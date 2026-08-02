//! The usbfs device node: open it, learn what it is, take an interface away from the kernel, and
//! give it back.
//!
//! Everything is an `ioctl(2)` on a character device at `/dev/bus/usb/BBB/DDD`. On Android the
//! path is unreachable but the *file descriptor* is not: `UsbDeviceConnection.getFileDescriptor()`
//! hands out exactly this fd once the user grants permission, which is what makes the whole
//! approach work from an unprivileged app.

use std::ffi::CString;
use std::os::unix::io::RawFd;
use std::path::Path;

use libc::{c_int, c_uint, c_void};

use crate::descriptor;
use crate::error::ErrnoContext;
use crate::{sys, Error, Result, Speed};

/// Run an `ioctl` and turn `-1` into a typed error.
///
/// The request number is cast with `as _` on purpose: bionic types the parameter as `c_int` and
/// glibc as `c_ulong`, and the values above `INT_MAX` wrap to negative `int` on the former exactly
/// as the C macros do.
fn ioctl(fd: RawFd, request: c_uint, arg: *mut c_void, context: ErrnoContext) -> Result<c_int> {
    // SAFETY: `fd` is a live usbfs descriptor owned by the caller, `request` is one of the
    // transcribed usbfs request numbers, and `arg` points at the structure that request expects
    // (checked at each call site). `ioctl` itself has no additional preconditions.
    let rc = unsafe { libc::ioctl(fd, request as _, arg) };
    if rc < 0 {
        let errno = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO);
        return Err(Error::from_errno(errno, context));
    }
    Ok(rc)
}

/// What the kernel says this usbfs implementation can do (`USBDEVFS_GET_CAPABILITIES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities(u32);

impl Capabilities {
    /// `USBDEVFS_CAP_ZERO_PACKET`
    pub fn zero_packet(&self) -> bool {
        self.0 & 0x01 != 0
    }
    /// `USBDEVFS_CAP_BULK_CONTINUATION`
    pub fn bulk_continuation(&self) -> bool {
        self.0 & 0x02 != 0
    }
    /// `USBDEVFS_CAP_NO_PACKET_SIZE_LIM`
    pub fn no_packet_size_limit(&self) -> bool {
        self.0 & 0x04 != 0
    }
    /// `USBDEVFS_CAP_REAP_AFTER_DISCONNECT` — completions can still be reaped after the device is
    /// gone. Without it, a disconnect can strand in-flight URBs, which is why teardown leaks the
    /// ring rather than freeing it when reaping does not converge.
    pub fn reap_after_disconnect(&self) -> bool {
        self.0 & 0x10 != 0
    }
    /// `USBDEVFS_CAP_MMAP` — buffers can be mapped from the fd, avoiding the copy usbfs otherwise
    /// makes on submit. Not used yet; recorded because it is the obvious next optimisation.
    pub fn mmap(&self) -> bool {
        self.0 & 0x20 != 0
    }
    /// The raw bitmask.
    pub fn bits(&self) -> u32 {
        self.0
    }
}

/// How hard to try when claiming an interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Fail with [`Error::InterfaceBusy`] if a kernel driver holds the interface.
    Shared,
    /// Detach the kernel driver first, and re-attach it when the guard drops.
    ///
    /// This is what `UsbDeviceConnection.claimInterface(iface, true)` does on Android. The detach
    /// is **per interface** — `usbdevfs_ioctl.ifno` names exactly one — so taking an audio
    /// streaming interface does not disturb the HID interface on the same composite device.
    Force,
}

/// Whether this handle closes the descriptor when it drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    Owned,
    Borrowed,
}

/// An open usbfs device.
#[derive(Debug)]
pub struct UsbFsDevice {
    fd: RawFd,
    ownership: Ownership,
}

impl UsbFsDevice {
    /// Open a device node by path, e.g. `/dev/bus/usb/001/002`.
    ///
    /// Opened read-write: usbfs only reports URB completions through `poll` for descriptors that
    /// have write access, so a read-only fd produces a stream that never appears to complete.
    pub fn open(path: impl AsRef<Path>) -> Result<UsbFsDevice> {
        let c = CString::new(path.as_ref().as_os_str().as_encoded_bytes())
            .map_err(|_| Error::Config("device path contains a NUL"))?;
        // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(UsbFsDevice {
            fd,
            ownership: Ownership::Owned,
        })
    }

    /// Adopt a raw descriptor and close it on drop.
    ///
    /// # Safety
    ///
    /// `fd` must be a live usbfs device descriptor opened read-write, and no other object may
    /// close it.
    pub unsafe fn from_raw_fd(fd: RawFd) -> UsbFsDevice {
        UsbFsDevice {
            fd,
            ownership: Ownership::Owned,
        }
    }

    /// Borrow a raw descriptor without taking responsibility for closing it.
    ///
    /// **This is the Android entry point.** `UsbDeviceConnection` owns its descriptor and closes
    /// it in `UsbDeviceConnection.close()`; adopting it here would produce a double close, and on
    /// a JVM that surfaces as an unrelated file descriptor being yanked out from under some other
    /// part of the app much later.
    ///
    /// ```no_run
    /// # use usbfs_iso::UsbFsDevice;
    /// # fn get_fd_from_jni() -> std::os::unix::io::RawFd { 0 }
    /// // int fd = connection.getFileDescriptor();  // passed down through JNI
    /// let fd = get_fd_from_jni();
    /// // SAFETY: the Java side keeps the UsbDeviceConnection alive for as long as this handle.
    /// let dev = unsafe { UsbFsDevice::from_borrowed_fd(fd) };
    /// ```
    ///
    /// # Safety
    ///
    /// `fd` must be a live usbfs device descriptor opened read-write, and must outlive the
    /// returned handle.
    pub unsafe fn from_borrowed_fd(fd: RawFd) -> UsbFsDevice {
        UsbFsDevice {
            fd,
            ownership: Ownership::Borrowed,
        }
    }

    /// The underlying descriptor.
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Bus speed, via `USBDEVFS_GET_SPEED`.
    ///
    /// **Read this before sizing anything.** It is the input that decides whether a `bInterval` of
    /// 4 means 1 ms or 8 ms (see [`crate::Schedule`]). It works on a descriptor handed over from
    /// Android with no sysfs access, which is why the whole crate can key off it.
    pub fn speed(&self) -> Result<Speed> {
        let raw = ioctl(
            self.fd,
            sys::GET_SPEED,
            std::ptr::null_mut(),
            ErrnoContext::Other,
        )?;
        Ok(Speed::from_raw(raw))
    }

    /// usbfs capabilities of the running kernel.
    pub fn capabilities(&self) -> Result<Capabilities> {
        let mut caps: u32 = 0;
        ioctl(
            self.fd,
            sys::GET_CAPABILITIES,
            (&mut caps as *mut u32).cast(),
            ErrnoContext::Other,
        )?;
        Ok(Capabilities(caps))
    }

    /// The device descriptor followed by every configuration descriptor, as raw bytes.
    ///
    /// Read with `pread` from offset 0 so the file offset is left alone — the same descriptor is
    /// also being used for URB traffic.
    pub fn raw_descriptors(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        let mut offset: libc::off_t = 0;
        loop {
            // SAFETY: `buf` is a live, writable array of exactly `buf.len()` bytes.
            let n = unsafe {
                libc::pread(
                    self.fd,
                    buf.as_mut_ptr().cast::<c_void>(),
                    buf.len(),
                    offset,
                )
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
            offset += n as libc::off_t;
            if (n as usize) < buf.len() {
                break;
            }
        }
        if out.len() < 18 {
            return Err(Error::MalformedDescriptor(
                "device descriptor short or unreadable",
            ));
        }
        Ok(out)
    }

    /// Vendor and product identity.
    pub fn device_descriptor(&self) -> Result<descriptor::Device> {
        descriptor::Device::parse(&self.raw_descriptors()?)
    }

    /// The kernel driver currently bound to an interface, if any.
    pub fn driver(&self, interface: u8) -> Result<Option<String>> {
        let mut gd = sys::GetDriver {
            interface: c_uint::from(interface),
            driver: [0; sys::MAXDRIVERNAME + 1],
        };
        match ioctl(
            self.fd,
            sys::GETDRIVER,
            (&mut gd as *mut sys::GetDriver).cast(),
            ErrnoContext::Other,
        ) {
            Ok(_) => {
                // `c_char` is signed on glibc/x86_64 and unsigned on bionic/aarch64, so this
                // cast is a no-op on some targets and a reinterpret on others. Both are correct;
                // only one of them looks redundant to clippy.
                #[allow(clippy::unnecessary_cast)]
                let bytes: Vec<u8> = gd
                    .driver
                    .iter()
                    .take_while(|&&c| c != 0)
                    .map(|&c| c as u8)
                    .collect();
                Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
            }
            // ENODATA is the kernel's "no driver bound", which is not an error here.
            Err(Error::Io(e)) if e.raw_os_error() == Some(libc::ENODATA) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Claim an interface, optionally taking it from the kernel driver that holds it.
    ///
    /// The returned guard releases the interface — and re-attaches the driver it displaced — when
    /// it drops, including while a panic unwinds. Leaving `snd-usb-audio` detached because a
    /// consumer panicked would silently cost the user their USB audio device until replug.
    pub fn claim_interface(&self, interface: u8, claim: Claim) -> Result<InterfaceGuard<'_>> {
        // Ask who holds it first: it is the only chance to name the driver in an EBUSY error, and
        // it tells the guard whether it has anything to re-attach.
        let existing = self.driver(interface).unwrap_or(None);
        // usbfs binds its own "usbfs" driver to interfaces claimed through this fd; that is not a
        // foreign driver we displaced and must not be "restored" on release.
        let displaced = existing.filter(|d| d != "usbfs");

        match claim {
            Claim::Shared => self.claim_raw(interface, displaced.clone())?,
            Claim::Force => {
                let mut dc = sys::DisconnectClaim {
                    interface: c_uint::from(interface),
                    // flags 0: detach whichever driver is bound, then claim, atomically.
                    flags: 0,
                    driver: [0; sys::MAXDRIVERNAME + 1],
                };
                let atomic = ioctl(
                    self.fd,
                    sys::DISCONNECT_CLAIM,
                    (&mut dc as *mut sys::DisconnectClaim).cast(),
                    ErrnoContext::Claim {
                        interface,
                        driver: displaced.clone(),
                    },
                );
                match atomic {
                    Ok(_) => {}
                    // Kernels before 3.7 have no DISCONNECT_CLAIM. Fall back to the two-step,
                    // which races (something could grab the interface in between) but is the only
                    // option there.
                    Err(Error::Unsupported(_)) => {
                        self.disconnect_driver(interface)?;
                        self.claim_raw(interface, displaced.clone())?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        Ok(InterfaceGuard {
            device: self,
            interface,
            displaced_driver: displaced,
            released: false,
        })
    }

    fn claim_raw(&self, interface: u8, driver: Option<String>) -> Result<()> {
        let mut ifno = c_uint::from(interface);
        ioctl(
            self.fd,
            sys::CLAIMINTERFACE,
            (&mut ifno as *mut c_uint).cast(),
            ErrnoContext::Claim { interface, driver },
        )?;
        Ok(())
    }

    /// Evict the kernel driver from one interface (`USBDEVFS_DISCONNECT` via `USBDEVFS_IOCTL`).
    fn disconnect_driver(&self, interface: u8) -> Result<()> {
        let mut wrapper = sys::UsbFsIoctl {
            ifno: c_int::from(interface),
            ioctl_code: sys::IOCTL_DISCONNECT,
            data: std::ptr::null_mut(),
        };
        ioctl(
            self.fd,
            sys::IOCTL,
            (&mut wrapper as *mut sys::UsbFsIoctl).cast(),
            ErrnoContext::Claim {
                interface,
                driver: None,
            },
        )?;
        Ok(())
    }

    /// Let the kernel driver rebind to an interface (`USBDEVFS_CONNECT`).
    fn reconnect_driver(&self, interface: u8) -> Result<()> {
        let mut wrapper = sys::UsbFsIoctl {
            ifno: c_int::from(interface),
            ioctl_code: sys::IOCTL_CONNECT,
            data: std::ptr::null_mut(),
        };
        ioctl(
            self.fd,
            sys::IOCTL,
            (&mut wrapper as *mut sys::UsbFsIoctl).cast(),
            ErrnoContext::Other,
        )?;
        Ok(())
    }

    /// Select an alternate setting.
    ///
    /// **This is the call that reserves isochronous bus bandwidth.** An audio streaming
    /// interface's alt 0 has no endpoints at all, by design; the endpoint only exists — and the
    /// host controller only budgets for it — once a non-zero alt setting is selected. Submitting
    /// a URB without this is `EINVAL` on an endpoint the kernel does not believe exists.
    pub fn set_alt_setting(&self, interface: u8, alt_setting: u8) -> Result<()> {
        let mut si = sys::SetInterface {
            interface: c_uint::from(interface),
            altsetting: c_uint::from(alt_setting),
        };
        ioctl(
            self.fd,
            sys::SETINTERFACE,
            (&mut si as *mut sys::SetInterface).cast(),
            ErrnoContext::Other,
        )?;
        Ok(())
    }

    /// A control transfer on endpoint 0.
    ///
    /// Present because class drivers need it — UAC1 sets the sampling frequency with a control
    /// request to the endpoint, and UAC2 reads the supported rates from a clock entity — and
    /// because `uac-host` is `#![forbid(unsafe_code)]` and so cannot issue one itself.
    ///
    /// Returns the number of bytes transferred in the data stage.
    pub fn control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
        timeout: std::time::Duration,
    ) -> Result<usize> {
        let len = u16::try_from(data.len())
            .map_err(|_| Error::Config("control transfers are limited to 65535 bytes"))?;
        let mut ct = sys::CtrlTransfer {
            request_type,
            request,
            value,
            index,
            length: len,
            timeout: timeout.as_millis().min(u128::from(c_uint::MAX)) as c_uint,
            data: data.as_mut_ptr().cast(),
        };
        let n = ioctl(
            self.fd,
            sys::CONTROL,
            (&mut ct as *mut sys::CtrlTransfer).cast(),
            ErrnoContext::Other,
        )?;
        Ok(n.max(0) as usize)
    }

    /// Clear a halt condition on an endpoint.
    pub fn clear_halt(&self, endpoint: u8) -> Result<()> {
        let mut ep = c_uint::from(endpoint);
        ioctl(
            self.fd,
            sys::CLEAR_HALT,
            (&mut ep as *mut c_uint).cast(),
            ErrnoContext::Other,
        )?;
        Ok(())
    }

    /// Reset the device. Every claim and alt setting is lost; the device re-enumerates.
    pub fn reset(&self) -> Result<()> {
        ioctl(
            self.fd,
            sys::RESET,
            std::ptr::null_mut(),
            ErrnoContext::Other,
        )?;
        Ok(())
    }

    /// Wait until a URB completion is queued, or the timeout expires.
    ///
    /// usbfs reports readiness through `poll`: `EPOLLOUT` means at least one completion is waiting
    /// to be reaped, `EPOLLHUP` means the device is gone. Polling rather than spinning is what
    /// keeps a low-latency stream from burning a core between 1 ms packets.
    pub(crate) fn wait_readable(&self, timeout: std::time::Duration) -> Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as c_int;
        // SAFETY: `pfd` is a single live `pollfd`, and 1 is its length.
        let rc = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, ms) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            // A signal is not a failure; the caller's loop will come back round.
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(false);
            }
            return Err(e.into());
        }
        if pfd.revents & libc::POLLHUP != 0 {
            return Err(Error::Disconnected);
        }
        Ok(rc > 0 && pfd.revents & libc::POLLOUT != 0)
    }
}

impl Drop for UsbFsDevice {
    fn drop(&mut self) {
        if self.ownership == Ownership::Owned {
            // SAFETY: we opened or adopted this descriptor and no other object closes it.
            unsafe { libc::close(self.fd) };
        }
    }
}

/// RAII ownership of one interface.
///
/// Dropping it releases the interface and re-attaches whatever kernel driver was displaced.
#[derive(Debug)]
pub struct InterfaceGuard<'d> {
    device: &'d UsbFsDevice,
    interface: u8,
    displaced_driver: Option<String>,
    released: bool,
}

impl InterfaceGuard<'_> {
    /// The interface number this guard holds.
    pub fn interface(&self) -> u8 {
        self.interface
    }

    /// The kernel driver that was detached to get here, if any.
    ///
    /// Worth surfacing to users: on a DualSense this is `snd-usb-audio`, and detaching it takes
    /// the pad's *microphone* down with the speaker, because the kernel binds the whole audio
    /// function as one ALSA card.
    pub fn displaced_driver(&self) -> Option<&str> {
        self.displaced_driver.as_deref()
    }

    /// Select an alternate setting on this interface.
    pub fn set_alt_setting(&self, alt: u8) -> Result<()> {
        self.device.set_alt_setting(self.interface, alt)
    }

    /// Release now instead of at drop, surfacing any error.
    pub fn release(mut self) -> Result<()> {
        self.released = true;
        self.release_inner()
    }

    fn release_inner(&mut self) -> Result<()> {
        let mut ifno = c_uint::from(self.interface);
        let released = ioctl(
            self.device.fd,
            sys::RELEASEINTERFACE,
            (&mut ifno as *mut c_uint).cast(),
            ErrnoContext::Other,
        );
        // Re-attach even if the release failed: on a disconnect the release is meaningless but the
        // rebind still matters for when the device comes back.
        let reconnected = if self.displaced_driver.is_some() {
            self.device.reconnect_driver(self.interface)
        } else {
            Ok(())
        };
        released?;
        reconnected
    }
}

impl Drop for InterfaceGuard<'_> {
    fn drop(&mut self) {
        if !self.released {
            // Nothing useful to do with an error here, and panicking in `drop` would abort during
            // an unwind. The device going away is the common case and is not actionable.
            let _ = self.release_inner();
        }
    }
}
