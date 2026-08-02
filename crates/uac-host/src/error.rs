//! Errors that are about *audio*, not about USB.
//!
//! Transport failures pass straight through as [`Error::Transport`] with `usbfs-iso`'s own typed
//! variants intact, so a caller can still tell `ENOMEM`-the-budget from `EBUSY`-the-kernel-driver
//! without unwrapping a string.

use std::fmt;

use crate::{Format, Rates};

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong opening or driving a USB Audio Class stream.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The descriptors contain no audio function at all.
    NoAudioFunction,

    /// The device speaks UAC2 and the crate was built without the `uac2` feature.
    ///
    /// Distinct from [`Error::NoAudioFunction`] on purpose: "there is a device here that I was
    /// built not to understand" is a build-configuration problem with an obvious fix, and it
    /// should not look like an absent device.
    Uac2NotEnabled,

    /// The requested format is not the one this alternate setting provides.
    ///
    /// Formats are a property of the alt setting, not something negotiated at open time. Pick a
    /// different stream from `output_streams()` rather than a different argument.
    FormatMismatch {
        /// What the caller asked for.
        requested: Format,
        /// What this alternate setting actually carries.
        available: Format,
    },

    /// The rate is not among those the stream advertises.
    RateUnsupported {
        /// What the caller asked for.
        requested: u32,
        /// What the stream advertises.
        advertised: Rates,
    },

    /// The rate needs more bytes per service interval than the endpoint reserved.
    ///
    /// Isochronous bandwidth is reserved at `SETINTERFACE` time from `wMaxPacketSize`; a stream
    /// that needs more cannot simply send more.
    RateTooFastForEndpoint {
        /// The requested rate.
        requested: u32,
        /// Bytes per service interval it would need.
        needed: usize,
        /// Bytes per service interval the endpoint reserves.
        available: usize,
    },

    /// The stream's rates live in a clock entity that has not been read yet.
    ///
    /// Call `OutputStream::resolve_rates` with a live device first.
    RatesUnresolved,

    /// The device rejected the sample-rate control request.
    RateRejected {
        /// The rate we tried to set.
        requested: u32,
        /// The underlying transport error.
        source: usbfs_iso::Error,
    },

    /// A USB-level failure, with `usbfs-iso`'s typed variant preserved.
    Transport(usbfs_iso::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoAudioFunction => write!(f, "the device exposes no USB Audio Class function"),
            Error::Uac2NotEnabled => write!(
                f,
                "the device speaks UAC2; rebuild uac-host with the \"uac2\" feature"
            ),
            Error::FormatMismatch {
                requested,
                available,
            } => write!(
                f,
                "this alternate setting carries {available}, not {requested}; \
                 choose the alternate setting that provides the format you want"
            ),
            Error::RateUnsupported {
                requested,
                advertised,
            } => write!(
                f,
                "{requested} Hz is not advertised (supports {advertised})"
            ),
            Error::RateTooFastForEndpoint {
                requested,
                needed,
                available,
            } => write!(
                f,
                "{requested} Hz needs {needed} bytes per service interval but the endpoint \
                 reserves only {available}"
            ),
            Error::RatesUnresolved => write!(
                f,
                "this stream's rates live in a clock entity; resolve them against the device first"
            ),
            Error::RateRejected { requested, source } => {
                write!(f, "the device refused {requested} Hz: {source}")
            }
            Error::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Transport(e) | Error::RateRejected { source: e, .. } => Some(e),
            _ => None,
        }
    }
}

impl From<usbfs_iso::Error> for Error {
    fn from(e: usbfs_iso::Error) -> Self {
        Error::Transport(e)
    }
}
