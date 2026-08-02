//! Isochronous USB transfers on Linux and Android, straight to `usbfs`, from an unprivileged
//! process.
//!
//! # Why this crate exists
//!
//! Isochronous is the transfer type USB reserves bandwidth for and never retries: audio, video,
//! anything where a late packet is worse than a lost one. It is also the transfer type the Rust
//! ecosystem cannot do. `nusb` has had isochronous open as a tracking issue since 2024 with no
//! release; `rusb` never wrapped libusb's isochronous API at all; and libusb itself carries an
//! open bug against **Android isochronous OUT** whose reporter's use case is PCM audio to a USB
//! DAC — the exact thing this crate is for.
//!
//! Meanwhile the mechanism is proven: raw `usbfs` isochronous works on unrooted stock Android, and
//! Android's SELinux policy grants ordinary apps `usb_device:chr_file { read write getattr ioctl }`
//! with no `allowxperm` narrowing, so `USBDEVFS_SUBMITURB` with `USBDEVFS_URB_TYPE_ISO` is
//! permitted without root. What was missing was a Rust implementation.
//!
//! # What it does and does not do
//!
//! This crate knows about file descriptors, URBs, endpoints, packets, and bus speed. It knows
//! nothing about audio — no formats, no channel maps, no terminals. That belongs one layer up, in
//! `uac-host`, so a caller driving a UVC output or a custom isochronous peripheral pays nothing
//! for USB Audio Class machinery it will never use.
//!
//! # Getting a device
//!
//! On desktop Linux, open the node with `UsbFsDevice::open("/dev/bus/usb/001/002")`.
//!
//! On Android the path is unreachable but the descriptor is not. After
//! `UsbManager.requestPermission` succeeds, `UsbDeviceConnection.getFileDescriptor()` returns the
//! same usbfs descriptor; pass it down through JNI and wrap it with `UsbFsDevice::from_borrowed_fd`
//! — *borrowed*, because the Java object still owns it and will close it itself.
//!
//! Worked examples live on the types themselves (`UsbFsDevice`, `IsoOut`); they are doctests, and
//! because the transfer machinery is Linux-only they are compiled on the platforms where they
//! actually mean something.
//!
//! # The three things that decide whether a stream works
//!
//! 1. **Read the bus speed** (`UsbFsDevice::speed`). `bInterval` is an exponent whose unit changes
//!    with speed, so the same descriptor means 1 ms per packet on a high-speed bus and 8 ms on a
//!    full-speed one. Everything in [`Schedule`] derives from it; nothing is hardcoded.
//! 2. **Select a non-zero alternate setting** (`UsbFsDevice::set_alt_setting`). That call is what
//!    reserves isochronous bus bandwidth. An audio streaming interface's alt 0 has no endpoints at
//!    all, by design.
//! 3. **Keep the pipeline full.** There is no retry. Whatever is not queued in time is a hole in
//!    the stream, which `IsoStats::short_bytes` and `IsoStats::underruns` measure.
//!
//! # Platform support
//!
//! The transfer machinery is Linux and Android only — this is a Linux-kernel-ABI crate and says so
//! (`no_std` and Windows/macOS are explicit non-goals). The pure-logic modules ([`sys`],
//! [`descriptor`], [`fixtures`]) and the [`Schedule`] arithmetic compile and test everywhere, so
//! contributors on any host can work on the parsing and the packet maths.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod descriptor;
pub mod fixtures;
pub mod sys;

mod error;
mod schedule;

// Used only by the transfer machinery, but always compiled so its arithmetic and state machine are
// testable on hosts with no usbfs.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
mod ring;

pub use error::{Error, Result};
pub use schedule::{packet_interval_us, usbfs_memory_budget_bytes, Depth, Schedule, Speed};

#[cfg(any(target_os = "linux", target_os = "android"))]
mod device;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod iso;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub use device::{Capabilities, Claim, InterfaceGuard, UsbFsDevice};
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use iso::{plan, IsoOut, IsoOutBuilder, IsoStats, Slot, Underrun};

#[cfg(test)]
mod counting_allocator;
