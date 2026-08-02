//! Typed, actionable errors.
//!
//! Design rule 4 (§4 of the plan): the failures a caller must *handle differently* are distinct
//! variants, not an opaque `io::Error`. Four of them decide product behaviour on Android —
//! [`Error::InterfaceBusy`] means an OEM kernel refused the detach, [`Error::UsbfsMemory`] means
//! the in-flight depth is over the unraisable `usbfs_memory_mb` budget, [`Error::Disconnected`]
//! is the documented mid-playback failure that must be recovered from rather than panicked on,
//! and [`Error::Underrun`] is a stream-quality signal rather than a fault.

use std::fmt;

use crate::Speed;

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong driving an isochronous endpoint through usbfs.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// `ENODEV` — the device went away (unplugged, or the kernel reset the port).
    ///
    /// Treat this as a **state**, not an exception: it is the documented failure mode of long
    /// isochronous playback sessions. Recovery is to drop everything, re-open the device node (or
    /// re-acquire the fd from `UsbDeviceConnection` on Android), re-claim, and re-arm.
    Disconnected,

    /// `ENOMEM` from `SUBMITURB` — the in-flight URB memory exceeded the kernel's usbfs budget.
    ///
    /// The budget is `usbcore.usbfs_memory_mb`, default **16 MB**, shared across every usbfs
    /// client on the system and **not raisable without root**. Reduce the depth or the packet
    /// size; see [`crate::Schedule::memory_bytes`].
    UsbfsMemory {
        /// Bytes this stream is asking the kernel to hold in flight.
        requested_bytes: usize,
    },

    /// `EBUSY` on claim — a kernel driver still owns the interface and would not let go.
    ///
    /// With `Claim::Shared` this just means "ask for `Claim::Force`". With
    /// `Claim::Force` it means the kernel refused the detach outright, which some OEM Android
    /// kernels do; there is no app-side fix and the caller must degrade.
    InterfaceBusy {
        /// The interface number that could not be claimed.
        interface: u8,
        /// The driver bound to it, if the kernel would name it.
        driver: Option<String>,
    },

    /// `EACCES` / `EPERM` — no permission for the device node.
    ///
    /// On desktop Linux this is a udev rule away. On Android it means the fd did not come from a
    /// granted `UsbManager.requestPermission` flow.
    PermissionDenied,

    /// The running kernel does not implement this request (`ENOTTY`), or the target platform has
    /// no usbfs at all.
    Unsupported(&'static str),

    /// The producer did not keep the pipeline full and the endpoint ran dry.
    ///
    /// Only returned when the stream was built with `Underrun::Error`; the other
    /// policies fold this into `IsoStats::underruns`.
    Underrun {
        /// Total underruns counted on this stream so far.
        count: u64,
    },

    /// A packet moved fewer bytes than we asked it to — the stream has a hole.
    ShortTransfer {
        /// Bytes offered.
        expected: usize,
        /// Bytes the controller actually moved.
        actual: usize,
    },

    /// The endpoint address is not an isochronous endpoint in the requested direction, or is not
    /// present in the selected alternate setting.
    InvalidEndpoint(u8),

    /// Isochronous transfers are not defined for this bus speed (low speed), or the speed could
    /// not be determined.
    UnsupportedSpeed(Speed),

    /// `bInterval` outside the 1..=16 the USB spec allows for isochronous endpoints.
    InvalidInterval(u8),

    /// More packets per URB than the kernel's `devio.c` limit.
    TooManyPackets {
        /// What was asked for.
        requested: usize,
        /// [`crate::sys::MAX_ISO_PACKETS_PER_URB`].
        max: usize,
    },

    /// A configuration value that cannot work, with the reason spelled out.
    Config(&'static str),

    /// Nothing completed within the caller's timeout.
    Timeout,

    /// A descriptor blob was truncated or self-inconsistent.
    MalformedDescriptor(&'static str),

    /// Anything else the kernel returned.
    Io(std::io::Error),
}

impl Error {
    /// Map a raw `errno` from a usbfs `ioctl` onto the typed variants.
    ///
    /// `context` distinguishes the calls where the same errno means different things — notably
    /// `ENOMEM`, which is the memory budget on submit and a plain allocation failure elsewhere.
    pub(crate) fn from_errno(errno: i32, context: ErrnoContext) -> Self {
        match (errno, context) {
            (libc::ENODEV | libc::ESHUTDOWN, _) => Error::Disconnected,
            (libc::ENOMEM, ErrnoContext::Submit { bytes }) => Error::UsbfsMemory {
                requested_bytes: bytes,
            },
            (libc::EBUSY, ErrnoContext::Claim { interface, driver }) => {
                Error::InterfaceBusy { interface, driver }
            }
            (libc::EACCES | libc::EPERM, _) => Error::PermissionDenied,
            (libc::ENOTTY, _) => {
                Error::Unsupported("this kernel does not implement the usbfs request")
            }
            (errno, _) => Error::Io(std::io::Error::from_raw_os_error(errno)),
        }
    }

    /// True when the device answered a control request with a STALL.
    ///
    /// usbfs surfaces a stalled control transfer as `EPIPE`. A stall is the device's legal way of
    /// saying "I do not implement this request" — for optional class controls it is an answer, not
    /// a fault, and callers routinely need to tell it apart from a real I/O failure.
    pub fn is_stall(&self) -> bool {
        matches!(self, Error::Io(e) if e.raw_os_error() == Some(libc::EPIPE))
    }

    /// True when the device is gone and the only recovery is a full re-open.
    ///
    /// Consumers use this to distinguish "re-arm the stream" from "tear the whole session down".
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Error::Disconnected)
    }
}

/// Which call an `errno` came from, so [`Error::from_errno`] can disambiguate.
///
/// Only the transfer machinery constructs the specific variants, and that is Linux-only, so on
/// other hosts they are legitimately unconstructed rather than dead.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) enum ErrnoContext {
    Submit {
        bytes: usize,
    },
    Claim {
        interface: u8,
        driver: Option<String>,
    },
    Other,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Disconnected => write!(f, "device disconnected (ENODEV); re-open and re-arm"),
            Error::UsbfsMemory { requested_bytes } => write!(
                f,
                "usbfs in-flight memory budget exceeded asking for {requested_bytes} bytes \
                 (usbcore.usbfs_memory_mb, default 16 MB, shared and not raisable without root)"
            ),
            Error::InterfaceBusy { interface, driver } => match driver {
                Some(d) => write!(
                    f,
                    "interface {interface} is held by kernel driver \"{d}\" and the detach was refused"
                ),
                None => write!(f, "interface {interface} is busy and the detach was refused"),
            },
            Error::PermissionDenied => write!(f, "permission denied on the usb device node"),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::Underrun { count } => {
                write!(f, "isochronous underrun (stream ran dry; {count} so far)")
            }
            Error::ShortTransfer { expected, actual } => {
                write!(f, "short isochronous transfer: {actual} of {expected} bytes")
            }
            Error::InvalidEndpoint(ep) => {
                write!(f, "endpoint 0x{ep:02x} is not a usable isochronous endpoint")
            }
            Error::UnsupportedSpeed(speed) => {
                write!(f, "isochronous transfers are not available at {speed:?} speed")
            }
            Error::InvalidInterval(i) => write!(
                f,
                "bInterval {i} is outside the 1..=16 range the spec allows for isochronous endpoints"
            ),
            Error::TooManyPackets { requested, max } => write!(
                f,
                "{requested} packets per URB exceeds the kernel limit of {max}"
            ),
            Error::Config(why) => write!(f, "invalid stream configuration: {why}"),
            Error::Timeout => write!(f, "timed out waiting for a URB to complete"),
            Error::MalformedDescriptor(why) => write!(f, "malformed usb descriptor: {why}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        match e.raw_os_error() {
            Some(errno) => Error::from_errno(errno, ErrnoContext::Other),
            None => Error::Io(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enomem_on_submit_is_the_budget_error_but_not_elsewhere() {
        let submit = Error::from_errno(libc::ENOMEM, ErrnoContext::Submit { bytes: 4096 });
        assert!(matches!(
            submit,
            Error::UsbfsMemory {
                requested_bytes: 4096
            }
        ));
        let other = Error::from_errno(libc::ENOMEM, ErrnoContext::Other);
        assert!(matches!(other, Error::Io(_)));
    }

    #[test]
    fn ebusy_on_claim_names_the_driver_it_lost_to() {
        let e = Error::from_errno(
            libc::EBUSY,
            ErrnoContext::Claim {
                interface: 1,
                driver: Some("snd-usb-audio".into()),
            },
        );
        match e {
            Error::InterfaceBusy { interface, driver } => {
                assert_eq!(interface, 1);
                assert_eq!(driver.as_deref(), Some("snd-usb-audio"));
            }
            other => panic!("expected InterfaceBusy, got {other:?}"),
        }
    }

    #[test]
    fn a_stalled_control_request_is_recognisable() {
        // Optional class controls answer with a STALL, and a caller must be able to tell that
        // apart from a real failure without string-matching an io::Error.
        assert!(Error::from_errno(libc::EPIPE, ErrnoContext::Other).is_stall());
        assert!(!Error::from_errno(libc::EIO, ErrnoContext::Other).is_stall());
        assert!(!Error::Disconnected.is_stall());
    }

    #[test]
    fn eshutdown_is_treated_as_disconnect() {
        assert!(Error::from_errno(libc::ESHUTDOWN, ErrnoContext::Other).is_disconnected());
        assert!(Error::from_errno(libc::ENODEV, ErrnoContext::Other).is_disconnected());
    }
}
