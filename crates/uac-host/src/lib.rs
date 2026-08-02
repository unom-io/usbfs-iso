//! A userspace USB Audio Class **host** driver: find a device's audio streams, open one, and play
//! PCM to it — unprivileged, on Linux and Android, with no libusb and no root.
//!
//! Almost every USB-audio library goes the other way (implementing a *device*, or capturing *from*
//! one). This one renders **to** a USB audio device from userspace, which is the side that has
//! been missing: the only prior userspace UAC playback host was `libmaru`, LGPL and dead since
//! 2012.
//!
//! # Why you would want this rather than the OS
//!
//! Normally you would not — ALSA and AAudio exist. You want this when the platform refuses to
//! expose the device. The motivating case: **Android denylists the DualSense's audio output by
//! VID/PID** in `UsbAlsaManager`, so the kernel enumerates the pad's 4-channel playback node and
//! then the framework throws it away. The endpoint is still there and still works; it is simply
//! unreachable through any supported API. Driving it directly is the only route left, and it is
//! permitted without root — Android's SELinux policy grants apps `ioctl` on `usb_device`.
//!
//! # Layering
//!
//! ```text
//! uac-host      terminals, alt settings, formats, channel maps, rate pacing   #![forbid(unsafe_code)]
//!    |
//! usbfs-iso     file descriptors, URBs, packets, bus speed                    all the unsafe
//! ```
//!
//! This crate contains **no `unsafe`** — it is enforced below — because everything it does is
//! parsing and arithmetic over a transport that already got the pointers right.
//!
//! # Example
//!
//! See `Playback` for a worked one. It is a doctest, and because the transfer machinery is
//! Linux-only it is compiled where it means something.
//!
//! # Class revisions
//!
//! UAC1 is complete and always available. UAC2 — descriptor layouts plus clock-entity rate
//! discovery — is behind the `uac2` feature, and a UAC2 device seen without it reports
//! [`Error::Uac2NotEnabled`] rather than silently looking like a device with no audio.

#![forbid(unsafe_code)]

mod error;
mod format;
mod parse;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod stream;

pub use error::{Error, Result};
pub use format::{Format, Rates};
pub use parse::{parse, parse_all, AudioFunction, AudioStream, UacVersion};

#[cfg(any(target_os = "linux", target_os = "android"))]
pub use stream::{OpenOptions, Playback};

// Re-exported so a consumer does not have to name `usbfs-iso` for the everyday knobs.
pub use usbfs_iso::descriptor::{Direction, SyncType};
pub use usbfs_iso::{Depth, Speed};
