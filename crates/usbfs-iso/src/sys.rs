//! The `usbfs` kernel ABI: `<linux/usbdevice_fs.h>` transcribed, plus the `_IOC` encoding.
//!
//! Everything here is a plain data definition or a `const fn`, so the whole module compiles and is
//! testable on **any** platform — macOS included. Only the code that actually calls `ioctl(2)`
//! (see the `device` module) is gated to Linux/Android. That split is what lets tier-0 tests
//! (§5 of the design) run in CI on hosts that have no USB stack at all.
//!
//! The structure layouts are `#[repr(C)]` over `libc` scalar types, so the 32-bit forms fall out
//! automatically: on a 32-bit target `usbdevfs_urb` is 44 bytes with 4-byte pointers, which is
//! exactly the kernel's `usbdevfs_urb32` compat layout. Because the ioctl request numbers below
//! encode `size_of` the local struct, a 32-bit build naturally emits `USBDEVFS_SUBMITURB32` and a
//! 64-bit build emits `USBDEVFS_SUBMITURB`. Nothing needs a `cfg` for pointer width.

use core::ffi::c_void;

use libc::{c_int, c_uchar, c_uint};

// ---------------------------------------------------------------------------------------------
// _IOC encoding (asm-generic/ioctl.h). Every architecture we target — x86, x86_64, arm, aarch64,
// riscv — uses the asm-generic encoding. The outliers (mips, powerpc, sparc, alpha) reorder the
// direction bits; usbfs on those is out of scope and `compile_error!`-ing would be unhelpful, so
// the constants below are simply documented as asm-generic.
// ---------------------------------------------------------------------------------------------

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

const IOC_NONE: u32 = 0;
/// "Userspace writes to the kernel" — the direction bit Linux's `_IOW` sets.
const IOC_WRITE: u32 = 1;
/// "Userspace reads from the kernel" — the direction bit Linux's `_IOR` sets.
///
/// Note the naming trap: `_IOR` means *the kernel reads the argument you point at* in usbfs's
/// usage. The direction bits are advisory; the kernel switch matches the whole 32-bit value, so
/// what matters is that we reproduce the header's macro exactly, not that the name reads sensibly.
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, ty: u32, nr: u32, size: usize) -> c_uint {
    debug_assert!(size < (1 << IOC_SIZEBITS));
    ((dir << IOC_DIRSHIFT)
        | (ty << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)
        | ((size as u32) << IOC_SIZESHIFT)) as c_uint
}

const fn io(nr: u32) -> c_uint {
    ioc(IOC_NONE, b'U' as u32, nr, 0)
}
const fn ior(nr: u32, size: usize) -> c_uint {
    ioc(IOC_READ, b'U' as u32, nr, size)
}
const fn iow(nr: u32, size: usize) -> c_uint {
    ioc(IOC_WRITE, b'U' as u32, nr, size)
}
const fn iowr(nr: u32, size: usize) -> c_uint {
    ioc(IOC_READ | IOC_WRITE, b'U' as u32, nr, size)
}

// ---------------------------------------------------------------------------------------------
// Structures
// ---------------------------------------------------------------------------------------------

/// One isochronous packet's slot in a URB's trailing descriptor array.
///
/// `length` is set by us (how many bytes this packet carries); `actual_length` and `status` are
/// filled in by the kernel on completion. For an OUT transfer a short `actual_length` means the
/// controller could not place the whole packet — the stream has a hole.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IsoPacketDesc {
    /// Bytes to transfer in this packet (in), set before submit.
    pub length: c_uint,
    /// Bytes actually transferred (out), written by the kernel.
    pub actual_length: c_uint,
    /// Per-packet completion status (out): 0, or a negative errno cast to `c_uint`.
    pub status: c_uint,
}

/// `struct usbdevfs_urb`, **without** its trailing flexible array member.
///
/// Rust has no representation for a C flexible array member, so this type is the *header* only and
/// the crate's slot ring allocates `size_of::<Urb>() + n * size_of::<IsoPacketDesc>()` bytes per slot,
/// treating this struct as a typed view onto the front of that block. [`ISO_FRAME_DESC_OFFSET`]
/// asserts the array really does start at `size_of::<Urb>()`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Urb {
    /// One of the `URB_TYPE_*` constants.
    pub typ: c_uchar,
    /// Endpoint address including the direction bit (`0x80` = IN).
    pub endpoint: c_uchar,
    /// Overall URB status (out): 0, or a negative errno.
    pub status: c_int,
    /// `URB_*` flag bits.
    pub flags: c_uint,
    /// Pointer to the data buffer. Must stay valid and unmoved until the URB is reaped.
    pub buffer: *mut c_void,
    /// Total size of `buffer` in bytes.
    pub buffer_length: c_int,
    /// Bytes transferred across the whole URB (out).
    pub actual_length: c_int,
    /// Frame the transfer started in (out, isochronous only).
    pub start_frame: c_int,
    /// Isochronous: number of trailing [`IsoPacketDesc`] entries. (Union with `stream_id` for
    /// bulk streams, which this crate does not use.)
    pub number_of_packets: c_int,
    /// Count of failed isochronous packets (out).
    pub error_count: c_int,
    /// Signal to deliver on completion; 0 for the reap-based flow this crate uses.
    pub signr: c_uint,
    /// Opaque cookie returned verbatim by `REAPURB`. This crate stores the **slot index** here so
    /// the completion path has a tag-free identity check that never dereferences a kernel-returned
    /// pointer (see the crate's slot ring and design §3.3, MTE).
    pub usercontext: *mut c_void,
}

/// Byte offset of the flexible `iso_frame_desc[]` array within a URB slot.
///
/// The C struct's array begins immediately after the last named member; because
/// [`IsoPacketDesc`]'s alignment (4) never exceeds [`Urb`]'s (pointer-sized), that is exactly
/// `size_of::<Urb>()` on every target we support.
pub const ISO_FRAME_DESC_OFFSET: usize = core::mem::size_of::<Urb>();

/// `struct usbdevfs_setinterface`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SetInterface {
    /// Interface number.
    pub interface: c_uint,
    /// Alternate setting to select.
    pub altsetting: c_uint,
}

/// `struct usbdevfs_getdriver`.
#[repr(C)]
pub struct GetDriver {
    /// Interface number to query.
    pub interface: c_uint,
    /// NUL-terminated driver name (out).
    pub driver: [libc::c_char; MAXDRIVERNAME + 1],
}

/// `struct usbdevfs_ioctl` — the wrapper that carries a per-interface ioctl (notably
/// [`IOCTL_DISCONNECT`] / [`IOCTL_CONNECT`]) down to one interface.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UsbFsIoctl {
    /// Interface the inner ioctl applies to. This per-interface targeting is why force-claiming
    /// one interface does **not** evict the kernel driver from the device's other interfaces.
    pub ifno: c_int,
    /// The inner ioctl request number.
    pub ioctl_code: c_int,
    /// Argument for the inner ioctl; NULL for connect/disconnect.
    pub data: *mut c_void,
}

/// `struct usbdevfs_disconnect_claim` — atomic "detach whatever driver is bound, then claim".
#[repr(C)]
pub struct DisconnectClaim {
    /// Interface number.
    pub interface: c_uint,
    /// `DISCONNECT_CLAIM_*` flags.
    pub flags: c_uint,
    /// Driver name the flags refer to; unused when `flags == 0`.
    pub driver: [libc::c_char; MAXDRIVERNAME + 1],
}

/// `struct usbdevfs_ctrltransfer`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CtrlTransfer {
    /// `bmRequestType`.
    pub request_type: u8,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
    /// `wLength`.
    pub length: u16,
    /// Timeout in milliseconds; 0 means wait forever.
    pub timeout: c_uint,
    /// Data stage buffer.
    pub data: *mut c_void,
}

/// Maximum length of a kernel driver name, per `USBDEVFS_MAXDRIVERNAME`.
pub const MAXDRIVERNAME: usize = 255;

// ---------------------------------------------------------------------------------------------
// Request numbers
// ---------------------------------------------------------------------------------------------

/// `USBDEVFS_CONTROL`
pub const CONTROL: c_uint = iowr(0, core::mem::size_of::<CtrlTransfer>());
/// `USBDEVFS_SETINTERFACE`
pub const SETINTERFACE: c_uint = ior(4, core::mem::size_of::<SetInterface>());
/// `USBDEVFS_GETDRIVER`
pub const GETDRIVER: c_uint = iow(8, core::mem::size_of::<GetDriver>());
/// `USBDEVFS_SUBMITURB`
pub const SUBMITURB: c_uint = ior(10, core::mem::size_of::<Urb>());
/// `USBDEVFS_DISCARDURB` — argument is the URB pointer itself, not a pointer to it.
pub const DISCARDURB: c_uint = io(11);
/// `USBDEVFS_REAPURB` — blocking.
pub const REAPURB: c_uint = iow(12, core::mem::size_of::<*mut c_void>());
/// `USBDEVFS_REAPURBNDELAY` — non-blocking; `EAGAIN` when nothing has completed.
pub const REAPURBNDELAY: c_uint = iow(13, core::mem::size_of::<*mut c_void>());
/// `USBDEVFS_CLAIMINTERFACE`
pub const CLAIMINTERFACE: c_uint = ior(15, core::mem::size_of::<c_uint>());
/// `USBDEVFS_RELEASEINTERFACE`
pub const RELEASEINTERFACE: c_uint = ior(16, core::mem::size_of::<c_uint>());
/// `USBDEVFS_IOCTL`
pub const IOCTL: c_uint = iowr(18, core::mem::size_of::<UsbFsIoctl>());
/// `USBDEVFS_RESET`
pub const RESET: c_uint = io(20);
/// `USBDEVFS_CLEAR_HALT`
pub const CLEAR_HALT: c_uint = ior(21, core::mem::size_of::<c_uint>());
/// `USBDEVFS_DISCONNECT` — only valid as the inner code of [`IOCTL`].
pub const IOCTL_DISCONNECT: c_int = io(22) as c_int;
/// `USBDEVFS_CONNECT` — only valid as the inner code of [`IOCTL`].
pub const IOCTL_CONNECT: c_int = io(23) as c_int;
/// `USBDEVFS_GET_CAPABILITIES`
pub const GET_CAPABILITIES: c_uint = ior(26, core::mem::size_of::<u32>());
/// `USBDEVFS_DISCONNECT_CLAIM`
pub const DISCONNECT_CLAIM: c_uint = ior(27, core::mem::size_of::<DisconnectClaim>());
/// `USBDEVFS_GET_SPEED` — returns the speed enum as the ioctl's return value.
pub const GET_SPEED: c_uint = io(31);

// ---------------------------------------------------------------------------------------------
// Flag / enum constants
// ---------------------------------------------------------------------------------------------

/// `USBDEVFS_URB_TYPE_ISO`
pub const URB_TYPE_ISO: c_uchar = 0;
/// `USBDEVFS_URB_TYPE_INTERRUPT`
pub const URB_TYPE_INTERRUPT: c_uchar = 1;
/// `USBDEVFS_URB_TYPE_CONTROL`
pub const URB_TYPE_CONTROL: c_uchar = 2;
/// `USBDEVFS_URB_TYPE_BULK`
pub const URB_TYPE_BULK: c_uchar = 3;

/// `USBDEVFS_URB_SHORT_NOT_OK`
pub const URB_SHORT_NOT_OK: c_uint = 0x01;
/// `USBDEVFS_URB_ISO_ASAP` — schedule at the next opportunity, or append seamlessly to a stream
/// that already has URBs queued on this endpoint.
pub const URB_ISO_ASAP: c_uint = 0x02;
/// `USBDEVFS_URB_BULK_CONTINUATION`
pub const URB_BULK_CONTINUATION: c_uint = 0x04;
/// `USBDEVFS_URB_ZERO_PACKET`
pub const URB_ZERO_PACKET: c_uint = 0x40;
/// `USBDEVFS_URB_NO_INTERRUPT`
pub const URB_NO_INTERRUPT: c_uint = 0x80;

/// `USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER`
pub const DISCONNECT_CLAIM_IF_DRIVER: c_uint = 0x01;
/// `USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER`
pub const DISCONNECT_CLAIM_EXCEPT_DRIVER: c_uint = 0x02;

/// The kernel's hard cap on `number_of_packets` for one isochronous URB (`devio.c`).
///
/// This is not a tunable: `proc_do_submiturb` rejects anything above it with `EINVAL`, so a
/// request for a deeper single URB has to become several URBs.
pub const MAX_ISO_PACKETS_PER_URB: usize = 128;

#[cfg(test)]
mod tests {
    use super::*;

    // The values a 64-bit Linux userspace computes from <linux/usbdevice_fs.h>. Hardcoded rather
    // than recomputed, so a mistake in the `_IOC` helpers cannot cancel itself out.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn request_numbers_match_the_header_on_lp64() {
        assert_eq!(SUBMITURB, 0x8038550a, "USBDEVFS_SUBMITURB");
        assert_eq!(REAPURBNDELAY, 0x4008550d, "USBDEVFS_REAPURBNDELAY");
        assert_eq!(REAPURB, 0x4008550c, "USBDEVFS_REAPURB");
        assert_eq!(DISCARDURB, 0x0000550b, "USBDEVFS_DISCARDURB");
        assert_eq!(CLAIMINTERFACE, 0x8004550f, "USBDEVFS_CLAIMINTERFACE");
        assert_eq!(RELEASEINTERFACE, 0x80045510, "USBDEVFS_RELEASEINTERFACE");
        assert_eq!(SETINTERFACE, 0x80085504, "USBDEVFS_SETINTERFACE");
        assert_eq!(IOCTL, 0xc0105512, "USBDEVFS_IOCTL");
        assert_eq!(GET_CAPABILITIES, 0x8004551a, "USBDEVFS_GET_CAPABILITIES");
        assert_eq!(DISCONNECT_CLAIM, 0x8108551b, "USBDEVFS_DISCONNECT_CLAIM");
        assert_eq!(GET_SPEED, 0x0000551f, "USBDEVFS_GET_SPEED");
        assert_eq!(RESET, 0x00005514, "USBDEVFS_RESET");
        assert_eq!(CONTROL, 0xc0185500, "USBDEVFS_CONTROL");
    }

    // The 32-bit request numbers are the kernel's `*32` compat codes; they differ from the LP64
    // ones only in the size field, which is what makes deriving them from `size_of` correct.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn request_numbers_match_the_compat_header_on_ilp32() {
        assert_eq!(SUBMITURB, 0x802c550a, "USBDEVFS_SUBMITURB32");
        assert_eq!(REAPURBNDELAY, 0x4004550d, "USBDEVFS_REAPURBNDELAY32");
        assert_eq!(IOCTL, 0xc00c5512, "USBDEVFS_IOCTL32");
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn urb_header_layout_is_the_kernel_layout() {
        use core::mem::{align_of, size_of};
        assert_eq!(size_of::<Urb>(), 56);
        assert_eq!(align_of::<Urb>(), 8);
        assert_eq!(size_of::<IsoPacketDesc>(), 12);
        assert_eq!(ISO_FRAME_DESC_OFFSET, 56);
        assert_eq!(size_of::<DisconnectClaim>(), 264);
        assert_eq!(size_of::<SetInterface>(), 8);
    }

    #[test]
    fn iso_packet_descriptors_never_need_more_alignment_than_the_header() {
        // This is the property that makes ISO_FRAME_DESC_OFFSET == size_of::<Urb>() correct: if
        // IsoPacketDesc were more strictly aligned than Urb, C would insert padding the Rust
        // definition does not model.
        assert!(core::mem::align_of::<IsoPacketDesc>() <= core::mem::align_of::<Urb>());
    }

    #[test]
    fn ioc_encoding_matches_worked_examples() {
        // _IO('U', 31) == 0x551f, the simplest case: no direction, no size.
        assert_eq!(io(31), 0x0000_551f);
        // _IOR('U', 15, unsigned int): dir=2, size=4, type='U'=0x55, nr=15.
        assert_eq!(ior(15, 4), (2 << 30) | (4 << 16) | (0x55 << 8) | 15);
    }
}
