//! The pre-allocated URB slot ring — design §3.3, and the reason this crate has any `unsafe` at
//! all.
//!
//! # Why a ring and not a `Vec<Urb>`
//!
//! Two independent constraints force the same shape.
//!
//! **Memory Tagging Extension.** On arm64 Android 14+ the allocator tags heap pointers in the top
//! byte. A pointer handed to the kernel and returned through `REAPURB` cannot be assumed to
//! compare equal to, or be safely dereferenceable as, the tagged pointer we allocated. So the
//! completion path here **never dereferences a kernel-returned pointer and never compares pointer
//! identity**: it converts the returned address to an offset within one block whose base we own,
//! derives a slot *index*, and re-derives every pointer from our own base. A second, entirely
//! tag-free check — the slot index planted in `usercontext`, which travels as an integer — has to
//! agree before a completion is accepted.
//!
//! **Real-time behaviour.** One allocation at construction and none afterwards is what an audio
//! path wants anyway. `tests/no_alloc.rs` asserts it rather than trusting it.
//!
//! # Lifetime hazard
//!
//! While a URB is in flight the kernel holds a raw pointer into this block. Freeing it early is a
//! kernel-side use-after-free, so [`Ring::drop`] refuses to run while anything is unaccounted for
//! and [`Ring::leak`] exists for the one case — a device that vanished without completing its
//! URBs — where leaking a few hundred kilobytes is the only memory-safe option.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;

use crate::sys::{IsoPacketDesc, Urb, ISO_FRAME_DESC_OFFSET};
use crate::{Error, Result};

/// Alignment of each slot's data buffer.
///
/// A cache line, so a slot being filled by the producer never shares one with a slot the
/// controller is reading. Costs a few bytes per slot; removes a class of false sharing that would
/// otherwise show up as jitter in exactly the measurement WP7 is trying to make.
const DATA_ALIGN: usize = 64;

/// On aarch64 the top byte of a pointer is not part of the address (TBI), and under MTE it carries
/// an allocation tag. Masking it off makes offset arithmetic agree regardless of which tag a
/// pointer was carrying when it crossed the kernel boundary.
#[cfg(target_arch = "aarch64")]
const ADDR_MASK: usize = 0x00ff_ffff_ffff_ffff;
/// Everywhere else the address is the whole pointer.
#[cfg(not(target_arch = "aarch64"))]
const ADDR_MASK: usize = usize::MAX;

#[inline]
fn address_of<T>(p: *const T) -> usize {
    (p as usize) & ADDR_MASK
}

/// Where everything sits inside the single allocation. Pure arithmetic, so it is testable on any
/// host without allocating or touching a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RingLayout {
    /// Number of URB slots.
    pub slots: usize,
    /// Distance between consecutive URB headers.
    pub slot_stride: usize,
    /// Offset of the first data buffer.
    pub data_base: usize,
    /// Distance between consecutive data buffers.
    pub data_stride: usize,
    /// Payload bytes per packet.
    pub packet_bytes: usize,
    /// Packets per URB.
    pub packets_per_urb: usize,
    /// Total allocation size.
    pub total_bytes: usize,
    /// Allocation alignment.
    pub align: usize,
}

const fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

impl RingLayout {
    pub(crate) fn new(slots: usize, packets_per_urb: usize, packet_bytes: usize) -> Result<Self> {
        if slots < 2 {
            return Err(Error::Config("a ring needs at least 2 slots"));
        }
        if packets_per_urb == 0 || packet_bytes == 0 {
            return Err(Error::Config(
                "packets_per_urb and packet_bytes must be non-zero",
            ));
        }
        if packets_per_urb > crate::sys::MAX_ISO_PACKETS_PER_URB {
            return Err(Error::TooManyPackets {
                requested: packets_per_urb,
                max: crate::sys::MAX_ISO_PACKETS_PER_URB,
            });
        }

        let align = if std::mem::align_of::<Urb>() > DATA_ALIGN {
            std::mem::align_of::<Urb>()
        } else {
            DATA_ALIGN
        };

        let descs = packets_per_urb
            .checked_mul(std::mem::size_of::<IsoPacketDesc>())
            .ok_or(Error::Config("packet descriptor array overflows"))?;
        let slot_stride = align_up(
            ISO_FRAME_DESC_OFFSET
                .checked_add(descs)
                .ok_or(Error::Config("urb slot overflows"))?,
            std::mem::align_of::<Urb>(),
        );
        let data_stride = align_up(
            packets_per_urb
                .checked_mul(packet_bytes)
                .ok_or(Error::Config("urb payload overflows"))?,
            DATA_ALIGN,
        );

        let headers = slots
            .checked_mul(slot_stride)
            .ok_or(Error::Config("ring headers overflow"))?;
        let data_base = align_up(headers, DATA_ALIGN);
        let total_bytes = data_base
            .checked_add(
                slots
                    .checked_mul(data_stride)
                    .ok_or(Error::Config("ring payload overflows"))?,
            )
            .ok_or(Error::Config("ring overflows"))?;

        Ok(RingLayout {
            slots,
            slot_stride,
            data_base,
            data_stride,
            packet_bytes,
            packets_per_urb,
            total_bytes,
            align,
        })
    }

    /// Payload bytes one slot can carry.
    pub(crate) fn slot_capacity(&self) -> usize {
        self.packets_per_urb * self.packet_bytes
    }
}

/// What a slot is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotState {
    /// Available to hand to the producer.
    Free,
    /// Handed out, not yet committed.
    Filling,
    /// Submitted; the kernel owns the memory.
    InFlight,
    /// `DISCARDURB` issued; the kernel still owes us a completion.
    Discarding,
}

/// The slot ring: one allocation, indices not pointers.
pub(crate) struct Ring {
    block: NonNull<u8>,
    layout: RingLayout,
    alloc_layout: Layout,
    state: Box<[SlotState]>,
    /// Circular queue of free slot indices. Fixed capacity, so pushing and popping never allocate.
    free: Box<[u32]>,
    free_head: usize,
    free_len: usize,
    in_flight: usize,
}

impl std::fmt::Debug for Ring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ring")
            .field("slots", &self.layout.slots)
            .field("packets_per_urb", &self.layout.packets_per_urb)
            .field("packet_bytes", &self.layout.packet_bytes)
            .field("bytes", &self.layout.total_bytes)
            .field("in_flight", &self.in_flight)
            .field("free", &self.free_len)
            .finish()
    }
}

// SAFETY: `Ring` owns its allocation exclusively — the only other holder of a pointer into it is
// the kernel, for the duration of a URB, and that reference is not affected by which thread the
// `Ring` lives on. There is no interior sharing, so moving one between threads is sound. It is
// deliberately NOT `Sync`: two threads submitting into the same ring would race the free list.
unsafe impl Send for Ring {}

impl Ring {
    /// Allocate and initialise the ring. This is the crate's only allocation on the data path.
    pub(crate) fn new(layout: RingLayout, endpoint: u8, urb_type: u8, flags: u32) -> Result<Ring> {
        let alloc_layout = Layout::from_size_align(layout.total_bytes, layout.align)
            .map_err(|_| Error::Config("ring layout is not a valid allocation"))?;

        // SAFETY: `alloc_layout` has a non-zero size (a ring always has at least 2 slots of at
        // least one packet) and a power-of-two alignment, which is `alloc_zeroed`'s contract.
        let raw = unsafe { alloc_zeroed(alloc_layout) };
        let block = NonNull::new(raw).ok_or(Error::Config("ring allocation failed"))?;

        let mut ring = Ring {
            block,
            layout,
            alloc_layout,
            state: vec![SlotState::Free; layout.slots].into_boxed_slice(),
            free: (0..layout.slots as u32)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            free_head: 0,
            free_len: layout.slots,
            in_flight: 0,
        };
        ring.init_headers(endpoint, urb_type, flags);
        Ok(ring)
    }

    /// Write the fields that never change again, so the steady-state path only ever touches
    /// per-packet lengths.
    fn init_headers(&mut self, endpoint: u8, urb_type: u8, flags: u32) {
        for idx in 0..self.layout.slots {
            let data = self.data_ptr(idx);
            let urb = self.urb_ptr(idx);
            // SAFETY: `idx < slots`, so `urb` points at a whole `Urb` inside our own zeroed block
            // and is correctly aligned by construction (`slot_stride` is a multiple of
            // `align_of::<Urb>()`). Nothing else aliases it: we hold `&mut self`.
            unsafe {
                (*urb).typ = urb_type;
                (*urb).endpoint = endpoint;
                (*urb).flags = flags;
                (*urb).buffer = data.cast();
                (*urb).buffer_length = self.layout.slot_capacity() as i32;
                (*urb).number_of_packets = self.layout.packets_per_urb as i32;
                // The tag-free identity check. This value travels to the kernel and back as an
                // integer, never as a pointer, so MTE cannot perturb it.
                (*urb).usercontext = idx as *mut std::ffi::c_void;
            }
        }
    }

    pub(crate) fn layout(&self) -> &RingLayout {
        &self.layout
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight
    }

    pub(crate) fn free_count(&self) -> usize {
        self.free_len
    }

    pub(crate) fn state(&self, idx: usize) -> SlotState {
        self.state[idx]
    }

    /// Raw pointer to a slot's URB header.
    pub(crate) fn urb_ptr(&self, idx: usize) -> *mut Urb {
        debug_assert!(idx < self.layout.slots);
        // SAFETY: `idx < slots` keeps the offset inside the single allocation, and `slot_stride`
        // is a multiple of `align_of::<Urb>()`, so the result is aligned.
        unsafe {
            self.block
                .as_ptr()
                .add(idx * self.layout.slot_stride)
                .cast()
        }
    }

    /// Raw pointer to a slot's trailing isochronous packet descriptor array.
    pub(crate) fn packet_descs_ptr(&self, idx: usize) -> *mut IsoPacketDesc {
        debug_assert!(idx < self.layout.slots);
        // SAFETY: as `urb_ptr`, plus `ISO_FRAME_DESC_OFFSET` which the layout reserved
        // `packets_per_urb` descriptors of space at.
        unsafe {
            self.block
                .as_ptr()
                .add(idx * self.layout.slot_stride + ISO_FRAME_DESC_OFFSET)
                .cast()
        }
    }

    /// Raw pointer to a slot's payload buffer.
    pub(crate) fn data_ptr(&self, idx: usize) -> *mut u8 {
        debug_assert!(idx < self.layout.slots);
        // SAFETY: `data_base + idx * data_stride + slot_capacity() <= total_bytes` by
        // construction in `RingLayout::new`.
        unsafe {
            self.block
                .as_ptr()
                .add(self.layout.data_base + idx * self.layout.data_stride)
        }
    }

    /// A slot's payload buffer as a mutable slice.
    ///
    /// Only valid while the slot is `Free` or `Filling`: once submitted, the kernel may be reading
    /// it. Callers inside this crate uphold that; it is not exposed publicly except through
    /// [`crate::Slot`], which the state machine only hands out for a `Filling` slot.
    pub(crate) fn data_mut(&mut self, idx: usize) -> &mut [u8] {
        let len = self.layout.slot_capacity();
        // SAFETY: the pointer is in-bounds for `len` bytes (see `data_ptr`), the memory was
        // zero-initialised so it is valid `u8`, and `&mut self` guarantees no other Rust reference
        // aliases it. The caller-facing contract above covers the kernel's side.
        unsafe { std::slice::from_raw_parts_mut(self.data_ptr(idx), len) }
    }

    /// Resolve a kernel-returned URB pointer back to a slot index — **the MTE-safe path**.
    ///
    /// Returns `None` for anything that is not exactly one of our slot headers, which is the
    /// correct response to `REAPURB` handing back a URB from a different stream on the same fd.
    pub(crate) fn slot_of_urb(&self, urb: *mut Urb) -> Option<usize> {
        let base = address_of(self.block.as_ptr());
        let addr = address_of(urb);
        let offset = addr.checked_sub(base)?;
        if offset >= self.layout.slots * self.layout.slot_stride {
            return None;
        }
        if offset % self.layout.slot_stride != 0 {
            return None;
        }
        let idx = offset / self.layout.slot_stride;

        // Cross-check against the cookie, which never travelled as a pointer. Dereferenced
        // through OUR base pointer, never through the kernel's.
        // SAFETY: `idx < slots` was just established, so this is our own initialised header.
        let cookie = unsafe { (*self.urb_ptr(idx)).usercontext as usize };
        if cookie != idx {
            return None;
        }
        Some(idx)
    }

    /// Take a free slot and mark it `Filling`.
    pub(crate) fn take_free(&mut self) -> Option<usize> {
        if self.free_len == 0 {
            return None;
        }
        let idx = self.free[self.free_head] as usize;
        self.free_head = (self.free_head + 1) % self.free.len();
        self.free_len -= 1;
        debug_assert_eq!(self.state[idx], SlotState::Free);
        self.state[idx] = SlotState::Filling;
        Some(idx)
    }

    /// Return a slot to the free list without submitting it.
    pub(crate) fn release(&mut self, idx: usize) {
        debug_assert_ne!(self.state[idx], SlotState::Free);
        self.state[idx] = SlotState::Free;
        let tail = (self.free_head + self.free_len) % self.free.len();
        self.free[tail] = idx as u32;
        self.free_len += 1;
    }

    /// Distribute `bytes` across the slot's packet descriptors.
    ///
    /// Packets are filled to `packet_bytes` in order; the first partial packet gets the remainder
    /// and any packets after it get length 0. A zero-length isochronous packet is legal and means
    /// "nothing this interval" — for audio it is a hole, which is why the normal path commits a
    /// full buffer and this only matters at end-of-stream.
    pub(crate) fn set_packet_lengths(&mut self, idx: usize, bytes: usize) -> Result<()> {
        let capacity = self.layout.slot_capacity();
        if bytes > capacity {
            return Err(Error::Config("commit larger than the slot"));
        }
        let descs = self.packet_descs_ptr(idx);
        let mut remaining = bytes;
        for p in 0..self.layout.packets_per_urb {
            let take = remaining.min(self.layout.packet_bytes);
            // SAFETY: `p < packets_per_urb`, and the layout reserved that many descriptors
            // immediately after this slot's header.
            unsafe {
                let d = descs.add(p);
                (*d).length = take as u32;
                (*d).actual_length = 0;
                (*d).status = 0;
            }
            remaining -= take;
        }
        // SAFETY: our own header, as in `urb_ptr`.
        unsafe {
            (*self.urb_ptr(idx)).buffer_length = bytes as i32;
            (*self.urb_ptr(idx)).actual_length = 0;
            (*self.urb_ptr(idx)).status = 0;
            (*self.urb_ptr(idx)).error_count = 0;
        }
        Ok(())
    }

    /// Set each packet's length individually.
    ///
    /// Needed whenever the payload per service interval is not a whole number of sample frames.
    /// At 44.1 kHz on a 1 ms bus that is 44.1 frames per packet: a fixed 44 drifts slow and a
    /// fixed 45 drifts fast, so a correct host alternates. The kernel lays the packets out
    /// contiguously from the running sum of these lengths, which is exactly how the data ends up
    /// in the buffer.
    pub(crate) fn set_packet_lengths_exact(&mut self, idx: usize, lengths: &[usize]) -> Result<()> {
        if lengths.len() != self.layout.packets_per_urb {
            return Err(Error::Config(
                "packet length plan does not match packets_per_urb",
            ));
        }
        let mut total = 0usize;
        for &len in lengths {
            if len > self.layout.packet_bytes {
                return Err(Error::Config("a packet is larger than packet_bytes"));
            }
            total += len;
        }
        if total > self.layout.slot_capacity() {
            return Err(Error::Config("commit larger than the slot"));
        }
        let descs = self.packet_descs_ptr(idx);
        for (p, &len) in lengths.iter().enumerate() {
            // SAFETY: `p < packets_per_urb`, inside this slot's reserved descriptor array.
            unsafe {
                let d = descs.add(p);
                (*d).length = len as u32;
                (*d).actual_length = 0;
                (*d).status = 0;
            }
        }
        // SAFETY: our own header, as in `urb_ptr`.
        unsafe {
            (*self.urb_ptr(idx)).buffer_length = total as i32;
            (*self.urb_ptr(idx)).actual_length = 0;
            (*self.urb_ptr(idx)).status = 0;
            (*self.urb_ptr(idx)).error_count = 0;
        }
        Ok(())
    }

    /// Mark a slot as owned by the kernel. Call **after** a successful `SUBMITURB`.
    pub(crate) fn mark_in_flight(&mut self, idx: usize) {
        debug_assert_eq!(self.state[idx], SlotState::Filling);
        self.state[idx] = SlotState::InFlight;
        self.in_flight += 1;
    }

    /// Mark a slot as discard-issued. Call after a successful `DISCARDURB`; the kernel still owes
    /// a completion, so the slot does not become free here.
    pub(crate) fn mark_discarding(&mut self, idx: usize) {
        if self.state[idx] == SlotState::InFlight {
            self.state[idx] = SlotState::Discarding;
        }
    }

    /// Account for a completion the kernel handed back, returning the per-packet results.
    pub(crate) fn complete(&mut self, idx: usize) -> Completion {
        let was_discarded = self.state[idx] == SlotState::Discarding;
        if matches!(self.state[idx], SlotState::InFlight | SlotState::Discarding) {
            self.in_flight -= 1;
        }
        // SAFETY: our own header and descriptor array; `idx` came from `slot_of_urb`, which
        // validated it against both the layout and the cookie.
        let (status, actual, error_count) = unsafe {
            let u = self.urb_ptr(idx);
            ((*u).status, (*u).actual_length, (*u).error_count)
        };
        let mut short = 0usize;
        let mut requested = 0usize;
        for p in 0..self.layout.packets_per_urb {
            // SAFETY: `p < packets_per_urb`, within this slot's reserved descriptor array.
            let (len, actual_len) = unsafe {
                let d = self.packet_descs_ptr(idx).add(p);
                ((*d).length as usize, (*d).actual_length as usize)
            };
            requested += len;
            if actual_len < len {
                short += len - actual_len;
            }
        }
        self.release(idx);
        Completion {
            status,
            actual_bytes: actual.max(0) as usize,
            requested_bytes: requested,
            short_bytes: short,
            packet_errors: error_count.max(0) as u32,
            was_discarded,
        }
    }

    /// Give up ownership of the allocation without freeing it.
    ///
    /// The escape hatch for a device that disappeared while the kernel still held URB pointers
    /// into the block. Leaking is the only memory-safe option: freeing memory the kernel may still
    /// write to is a use-after-free that would corrupt an unrelated allocation.
    pub(crate) fn leak(&mut self) {
        self.alloc_layout = Layout::from_size_align(0, 1).expect("zero layout is always valid");
    }
}

/// The outcome of one URB, as the kernel filled it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Completion {
    /// URB-level status: 0, or a negative errno.
    pub status: i32,
    /// Bytes the controller actually moved.
    pub actual_bytes: usize,
    /// Bytes we asked it to move.
    pub requested_bytes: usize,
    /// Shortfall summed across packets.
    pub short_bytes: usize,
    /// The kernel's own count of failed packets.
    pub packet_errors: u32,
    /// True when this completion is the tail of a `DISCARDURB` rather than a real transfer.
    pub was_discarded: bool,
}

impl Drop for Ring {
    fn drop(&mut self) {
        if self.alloc_layout.size() == 0 {
            // Deliberately leaked by `leak()`; the kernel may still hold pointers into it.
            return;
        }
        // A ring reaching `drop` with URBs outstanding means the owning stream could not get them
        // back — `IsoOut`'s own `Drop` discards and reaps first, and only gives up when the device
        // has gone. Leak rather than hand the kernel a dangling pointer. Deliberately not an
        // assertion: panicking inside `drop` during an unwind aborts the process, which would turn
        // a bounded leak into a crash.
        if self.in_flight != 0 {
            return;
        }
        // SAFETY: the block came from `alloc_zeroed` with exactly this layout, has not been freed
        // (the size-0 sentinel above is the only other path), and nothing is in flight, so the
        // kernel holds no pointer into it.
        unsafe { dealloc(self.block.as_ptr(), self.alloc_layout) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{URB_ISO_ASAP, URB_TYPE_ISO};

    fn ring(slots: usize, packets: usize, bytes: usize) -> Ring {
        let layout = RingLayout::new(slots, packets, bytes).unwrap();
        Ring::new(layout, 0x01, URB_TYPE_ISO, URB_ISO_ASAP).unwrap()
    }

    #[test]
    fn layout_reserves_a_descriptor_per_packet_and_keeps_slots_aligned() {
        let l = RingLayout::new(6, 4, 392).unwrap();
        assert!(l.slot_stride >= ISO_FRAME_DESC_OFFSET + 4 * std::mem::size_of::<IsoPacketDesc>());
        assert_eq!(l.slot_stride % std::mem::align_of::<Urb>(), 0);
        assert_eq!(l.data_base % DATA_ALIGN, 0);
        assert_eq!(l.data_stride % DATA_ALIGN, 0);
        assert!(l.data_stride >= 4 * 392);
        // Headers and payloads must not overlap, and everything must fit.
        assert!(l.data_base >= l.slots * l.slot_stride);
        assert!(l.total_bytes >= l.data_base + l.slots * l.data_stride);
    }

    #[test]
    fn a_ring_needs_two_slots() {
        assert!(RingLayout::new(1, 1, 392).is_err());
        assert!(RingLayout::new(0, 1, 392).is_err());
        assert!(RingLayout::new(2, 1, 392).is_ok());
    }

    #[test]
    fn headers_are_initialised_once_and_point_at_their_own_payload() {
        let r = ring(4, 2, 392);
        for idx in 0..4 {
            // SAFETY: test-local read of our own initialised header.
            let urb = unsafe { *r.urb_ptr(idx) };
            assert_eq!(urb.typ, URB_TYPE_ISO);
            assert_eq!(urb.endpoint, 0x01);
            assert_eq!(urb.flags, URB_ISO_ASAP);
            assert_eq!(urb.number_of_packets, 2);
            assert_eq!(urb.buffer_length, 2 * 392);
            assert_eq!(urb.usercontext as usize, idx);
            assert_eq!(urb.buffer as usize, r.data_ptr(idx) as usize);
        }
    }

    #[test]
    fn payload_buffers_do_not_overlap() {
        let mut r = ring(4, 1, 392);
        for idx in 0..4 {
            r.data_mut(idx).fill(idx as u8);
        }
        for idx in 0..4 {
            assert!(
                r.data_mut(idx).iter().all(|&b| b == idx as u8),
                "slot {idx} was clobbered by a neighbour"
            );
        }
    }

    #[test]
    fn slot_resolution_accepts_our_headers_and_rejects_everything_else() {
        let r = ring(4, 1, 392);
        for idx in 0..4 {
            assert_eq!(r.slot_of_urb(r.urb_ptr(idx)), Some(idx));
        }
        // One byte into a slot: not a header start.
        let misaligned = (r.urb_ptr(1) as usize + 1) as *mut Urb;
        assert_eq!(r.slot_of_urb(misaligned), None);
        // Past the end of the header region.
        let past = (r.urb_ptr(0) as usize + 4 * r.layout.slot_stride) as *mut Urb;
        assert_eq!(r.slot_of_urb(past), None);
        // Before the block.
        let before = (r.urb_ptr(0) as usize - r.layout.slot_stride) as *mut Urb;
        assert_eq!(r.slot_of_urb(before), None);
    }

    /// The property that makes the whole design MTE-proof: a pointer whose top byte has been
    /// rewritten — exactly what a tag change looks like — still resolves to the right slot on
    /// aarch64, and is rejected outright elsewhere rather than silently mis-resolving.
    #[test]
    fn a_retagged_pointer_still_resolves_to_its_slot() {
        let r = ring(4, 1, 392);
        let real = r.urb_ptr(2);
        let retagged = ((real as usize) | (0xa5 << 56)) as *mut Urb;
        if cfg!(target_arch = "aarch64") {
            assert_eq!(r.slot_of_urb(retagged), Some(2));
        } else {
            assert_eq!(r.slot_of_urb(retagged), None);
        }
    }

    #[test]
    fn free_list_wraps_without_losing_or_duplicating_slots() {
        let mut r = ring(3, 1, 64);
        // Cycle far past the ring length; every slot must come back exactly once per lap.
        let mut seen = vec![0usize; 3];
        for _ in 0..30 {
            let idx = r
                .take_free()
                .expect("a slot is always available at depth 1");
            seen[idx] += 1;
            r.release(idx);
        }
        assert_eq!(r.free_count(), 3);
        assert_eq!(seen.iter().sum::<usize>(), 30);
        assert!(
            seen.iter().all(|&n| n == 10),
            "unbalanced rotation: {seen:?}"
        );
    }

    #[test]
    fn exhausting_the_ring_returns_none_rather_than_wrapping_onto_live_slots() {
        let mut r = ring(2, 1, 64);
        let a = r.take_free().unwrap();
        let b = r.take_free().unwrap();
        assert_ne!(a, b);
        assert_eq!(r.take_free(), None);
        r.release(a);
        assert_eq!(r.take_free(), Some(a));
    }

    #[test]
    fn packet_lengths_fill_whole_packets_then_the_remainder() {
        let mut r = ring(2, 4, 100);
        r.take_free().unwrap();
        r.set_packet_lengths(0, 250).unwrap();
        let lens: Vec<u32> = (0..4)
            // SAFETY: test-local read of this slot's descriptor array.
            .map(|p| unsafe { (*r.packet_descs_ptr(0).add(p)).length })
            .collect();
        assert_eq!(lens, vec![100, 100, 50, 0]);
    }

    #[test]
    fn exact_packet_plans_survive_the_round_trip_and_reject_bad_ones() {
        let mut r = ring(2, 3, 100);
        let idx = r.take_free().unwrap();
        // The 44.1 kHz shape: alternating frame counts whose average is the true rate.
        r.set_packet_lengths_exact(idx, &[88, 90, 88]).unwrap();
        let lens: Vec<u32> = (0..3)
            // SAFETY: test-local read of this slot's descriptor array.
            .map(|p| unsafe { (*r.packet_descs_ptr(idx).add(p)).length })
            .collect();
        assert_eq!(lens, vec![88, 90, 88]);
        // SAFETY: test-local read of our own header.
        assert_eq!(unsafe { (*r.urb_ptr(idx)).buffer_length }, 266);

        assert!(
            r.set_packet_lengths_exact(idx, &[100, 100]).is_err(),
            "wrong packet count"
        );
        assert!(
            r.set_packet_lengths_exact(idx, &[101, 0, 0]).is_err(),
            "packet over packet_bytes"
        );
    }

    #[test]
    fn committing_more_than_a_slot_holds_is_refused() {
        let mut r = ring(2, 2, 100);
        r.take_free().unwrap();
        assert!(r.set_packet_lengths(0, 201).is_err());
        assert!(r.set_packet_lengths(0, 200).is_ok());
    }

    #[test]
    fn completion_accounts_short_packets_and_frees_the_slot() {
        let mut r = ring(2, 2, 100);
        let idx = r.take_free().unwrap();
        r.set_packet_lengths(idx, 200).unwrap();
        r.mark_in_flight(idx);
        assert_eq!(r.in_flight(), 1);

        // Stand in for the kernel: second packet moved only 60 of its 100 bytes.
        // SAFETY: test-local write to our own descriptor array, with no URB actually in flight.
        unsafe {
            (*r.packet_descs_ptr(idx).add(0)).actual_length = 100;
            (*r.packet_descs_ptr(idx).add(1)).actual_length = 60;
            (*r.urb_ptr(idx)).actual_length = 160;
            (*r.urb_ptr(idx)).error_count = 1;
        }

        let c = r.complete(idx);
        assert_eq!(c.requested_bytes, 200);
        assert_eq!(c.actual_bytes, 160);
        assert_eq!(c.short_bytes, 40);
        assert_eq!(c.packet_errors, 1);
        assert!(!c.was_discarded);
        assert_eq!(r.in_flight(), 0);
        assert_eq!(r.state(idx), SlotState::Free);
    }

    #[test]
    fn a_discarded_urb_completes_as_discarded_and_still_frees_its_slot() {
        let mut r = ring(2, 1, 64);
        let idx = r.take_free().unwrap();
        r.set_packet_lengths(idx, 64).unwrap();
        r.mark_in_flight(idx);
        r.mark_discarding(idx);
        let c = r.complete(idx);
        assert!(c.was_discarded);
        assert_eq!(r.in_flight(), 0);
        assert_eq!(r.state(idx), SlotState::Free);
    }

    #[test]
    fn state_machine_runs_a_full_lap_through_wraparound() {
        let mut r = ring(3, 1, 64);
        for round in 0..10 {
            let mut held = Vec::new();
            while let Some(idx) = r.take_free() {
                r.set_packet_lengths(idx, 64).unwrap();
                r.mark_in_flight(idx);
                held.push(idx);
            }
            assert_eq!(held.len(), 3, "round {round}");
            assert_eq!(r.in_flight(), 3);
            for idx in held {
                r.complete(idx);
            }
            assert_eq!(r.in_flight(), 0);
            assert_eq!(r.free_count(), 3);
        }
    }

    /// Design rule 2, asserted rather than trusted: once the ring exists, the steady-state path
    /// — take a slot, fill it, plan its packets, submit-equivalent, complete it — must not touch
    /// the allocator. An allocation here would be a page fault and a lock in the middle of a
    /// 1 ms audio deadline.
    #[test]
    fn the_steady_state_path_never_allocates() {
        let mut r = ring(4, 2, 392);
        // Warm-up lap outside the measurement: the first pass through any lazily-built state
        // would otherwise be counted as steady-state cost.
        for _ in 0..4 {
            let idx = r.take_free().unwrap();
            r.set_packet_lengths(idx, 784).unwrap();
            r.mark_in_flight(idx);
            r.complete(idx);
        }

        let plan = [384usize, 384];
        let (_, allocations) = crate::counting_allocator::allocations_during(|| {
            for _ in 0..1000 {
                let idx = r.take_free().expect("depth 4, one in flight at a time");
                r.data_mut(idx).fill(0x5a);
                r.set_packet_lengths_exact(idx, &plan).unwrap();
                r.mark_in_flight(idx);
                r.complete(idx);
            }
        });
        assert_eq!(allocations, 0, "the steady-state ring path allocated");
    }

    #[test]
    fn leak_suppresses_the_free_so_the_kernel_never_reads_freed_memory() {
        let mut r = ring(2, 1, 64);
        let idx = r.take_free().unwrap();
        r.mark_in_flight(idx);
        r.leak();
        drop(r); // Must not free, and must not trip the in-flight debug assertion.
    }
}
