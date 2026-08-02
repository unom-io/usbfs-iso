//! The isochronous OUT stream: submit, reap, discard.
//!
//! The shape of the loop is forced by what isochronous transfers are. There is no retry and no
//! flow control — a packet either goes out in its service interval or that interval is silence
//! forever. So the ring is kept full ahead of the bus, and "being late" is a measurable statistic
//! rather than an error you can recover from after the fact.
//!
//! ```no_run
//! # use std::time::Duration;
//! # use usbfs_iso::{Claim, Depth, IsoOut, UsbFsDevice, Underrun};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dev = UsbFsDevice::open("/dev/bus/usb/001/002")?;
//! // Declared before the stream so it drops *after* it: the interface must outlive the URBs.
//! let iface = dev.claim_interface(1, Claim::Force)?;
//! iface.set_alt_setting(1)?;
//!
//! let mut out = IsoOut::builder(&dev, 0x01)
//!     .from_descriptors(1, 1)?     // reads wMaxPacketSize and bInterval off the device
//!     .depth(Depth::Millis(6))
//!     .on_underrun(Underrun::FillSilence)
//!     .build()?;
//!
//! out.start()?;
//! while let Some(mut slot) = out.next_slot(Duration::from_millis(20))? {
//!     let n = slot.bytes_mut().len();     // borrowed straight from the pre-allocated ring
//!     slot.commit(n)?;                    // submits; no copy, no allocation
//! }
//! # Ok(()) }
//! ```

use std::ffi::c_void;
use std::time::{Duration, Instant};

use libc::c_uint;

use crate::descriptor::{self, Direction, TransferType};
use crate::device::UsbFsDevice;
use crate::error::ErrnoContext;
use crate::ring::{Ring, RingLayout};
use crate::schedule::{Depth, Schedule};
use crate::{sys, Error, Result, Speed};

/// What to do when the producer fails to keep the pipeline full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Underrun {
    /// Count it and carry on. The next commit restarts the stream via `ISO_ASAP`.
    Continue,
    /// Count it and immediately submit silent URBs to refill the pipeline.
    ///
    /// "Silent" means every byte set to [`IsoOutBuilder::silence_byte`] — zero by default, which
    /// is silence for signed PCM. For formats where it is not (unsigned 8-bit, where silence is
    /// `0x80`), set the byte, or use [`Underrun::Continue`] and push your own.
    FillSilence,
    /// Surface [`Error::Underrun`] from [`IsoOut::next_slot`].
    Error,
}

/// Running totals for one stream. Cheap to read; the input to WP7's characterisation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IsoStats {
    /// URBs handed to the kernel.
    pub urbs_submitted: u64,
    /// URBs reaped back.
    pub urbs_completed: u64,
    /// Payload bytes offered.
    pub bytes_submitted: u64,
    /// Payload bytes the controller actually moved.
    pub bytes_transferred: u64,
    /// Bytes lost to short packets — the direct measure of stream holes.
    pub short_bytes: u64,
    /// The kernel's own count of failed isochronous packets.
    pub packet_errors: u64,
    /// URBs that came back with a non-zero status.
    pub urb_errors: u64,
    /// Times the ring drained completely while the stream was running.
    pub underruns: u64,
}

/// Builder for [`IsoOut`].
#[derive(Debug)]
pub struct IsoOutBuilder<'d> {
    device: &'d UsbFsDevice,
    endpoint: u8,
    max_packet_size: Option<usize>,
    interval: Option<u8>,
    speed: Option<Speed>,
    packet_bytes: Option<usize>,
    packets_per_urb: Option<usize>,
    depth: Depth,
    underrun: Underrun,
    silence_byte: u8,
}

impl<'d> IsoOutBuilder<'d> {
    /// Read `wMaxPacketSize` and `bInterval` from the device's own descriptors.
    ///
    /// Strongly preferred over hand-entering them: a mis-transcribed `bInterval` produces a stream
    /// that is silently scheduled at the wrong rate rather than one that fails.
    pub fn from_descriptors(mut self, interface: u8, alt_setting: u8) -> Result<Self> {
        let blob = self.device.raw_descriptors()?;
        let ep = descriptor::find_endpoint(&blob, interface, alt_setting, self.endpoint)
            .ok_or(Error::InvalidEndpoint(self.endpoint))?;
        if ep.transfer_type() != TransferType::Isochronous {
            return Err(Error::InvalidEndpoint(self.endpoint));
        }
        if ep.direction() != Direction::Out {
            return Err(Error::InvalidEndpoint(self.endpoint));
        }
        self.max_packet_size = Some(ep.bytes_per_interval());
        self.interval = Some(ep.interval);
        Ok(self)
    }

    /// Set `wMaxPacketSize` manually (payload bytes per service interval).
    pub fn max_packet_size(mut self, bytes: usize) -> Self {
        self.max_packet_size = Some(bytes);
        self
    }

    /// Set `bInterval` manually.
    pub fn interval(mut self, b_interval: u8) -> Self {
        self.interval = Some(b_interval);
        self
    }

    /// Override the bus speed instead of asking the kernel. Mostly for testing.
    pub fn speed(mut self, speed: Speed) -> Self {
        self.speed = Some(speed);
        self
    }

    /// Bytes actually written per packet, if smaller than `wMaxPacketSize`.
    ///
    /// For an adaptive endpoint this is the knob that sets the *rate*: the device consumes what it
    /// is given, so 384 bytes per 1 ms packet is 48 kHz for 4-channel 16-bit, and the 8 bytes of
    /// slack in a 392-byte `wMaxPacketSize` are there to let the host add a sample frame
    /// occasionally when its clock drifts.
    pub fn packet_bytes(mut self, bytes: usize) -> Self {
        self.packet_bytes = Some(bytes);
        self
    }

    /// Packets per URB. Defaults to 1 — the finest granularity the bus offers.
    pub fn packets_per_urb(mut self, packets: usize) -> Self {
        self.packets_per_urb = Some(packets);
        self
    }

    /// How much audio to keep in flight. The latency knob.
    pub fn depth(mut self, depth: Depth) -> Self {
        self.depth = depth;
        self
    }

    /// Underrun policy.
    pub fn on_underrun(mut self, policy: Underrun) -> Self {
        self.underrun = policy;
        self
    }

    /// The byte [`Underrun::FillSilence`] writes. Zero by default.
    pub fn silence_byte(mut self, byte: u8) -> Self {
        self.silence_byte = byte;
        self
    }

    /// Allocate the ring and produce a stream ready to start.
    pub fn build(self) -> Result<IsoOut<'d>> {
        let speed = match self.speed {
            Some(s) => s,
            None => self.device.speed()?,
        };
        let interval = self.interval.ok_or(Error::Config(
            "bInterval unknown: set it or call from_descriptors",
        ))?;
        let max_packet_size = self.max_packet_size.ok_or(Error::Config(
            "wMaxPacketSize unknown: set it or call from_descriptors",
        ))?;
        let packet_bytes = self.packet_bytes.unwrap_or(max_packet_size);
        if packet_bytes > max_packet_size {
            return Err(Error::Config(
                "packet_bytes exceeds the endpoint's wMaxPacketSize",
            ));
        }
        if self.endpoint & 0x80 != 0 {
            return Err(Error::InvalidEndpoint(self.endpoint));
        }

        let schedule = Schedule::derive(
            speed,
            interval,
            packet_bytes,
            self.depth,
            self.packets_per_urb,
        )?;

        let layout = RingLayout::new(schedule.urbs, schedule.packets_per_urb, packet_bytes)?;
        let ring = Ring::new(layout, self.endpoint, sys::URB_TYPE_ISO, sys::URB_ISO_ASAP)?;

        Ok(IsoOut {
            device: self.device,
            endpoint: self.endpoint,
            ring,
            schedule,
            stats: IsoStats::default(),
            underrun: self.underrun,
            silence_byte: self.silence_byte,
            started: false,
            armed: false,
        })
    }
}

/// An isochronous OUT stream on one endpoint.
///
/// Drops cleanly: in-flight URBs are discarded and reaped before the ring is released, so a
/// panicking consumer cannot leave the kernel holding pointers into freed memory.
#[derive(Debug)]
pub struct IsoOut<'d> {
    device: &'d UsbFsDevice,
    endpoint: u8,
    ring: Ring,
    schedule: Schedule,
    stats: IsoStats,
    underrun: Underrun,
    silence_byte: u8,
    started: bool,
    /// True once at least one URB has been in flight, so a drained ring means the producer fell
    /// behind rather than that the stream has not begun. Cleared when an underrun is counted, so
    /// one stall counts once instead of once per call.
    armed: bool,
}

impl<'d> IsoOut<'d> {
    /// Start building a stream on `endpoint` (an OUT address, so bit 7 clear).
    pub fn builder(device: &'d UsbFsDevice, endpoint: u8) -> IsoOutBuilder<'d> {
        IsoOutBuilder {
            device,
            endpoint,
            max_packet_size: None,
            interval: None,
            speed: None,
            packet_bytes: None,
            packets_per_urb: None,
            depth: Depth::Millis(8),
            underrun: Underrun::Continue,
            silence_byte: 0,
        }
    }

    /// The derived schedule — every latency and memory figure for this stream.
    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    /// Running totals.
    pub fn stats(&self) -> IsoStats {
        self.stats
    }

    /// URBs the kernel currently owns.
    pub fn in_flight(&self) -> usize {
        self.ring.in_flight()
    }

    /// Slots available to fill right now. `0` means the producer is comfortably ahead of the bus;
    /// a value that keeps climbing towards the ring size means it is falling behind.
    pub fn free_slots(&self) -> usize {
        self.ring.free_count()
    }

    /// The endpoint address this stream drives.
    pub fn endpoint(&self) -> u8 {
        self.endpoint
    }

    /// Arm the stream.
    ///
    /// Deliberately does **not** submit anything. The bus starts moving on the first commit, and
    /// because commits are memcpy-fast the caller fills the whole ring well inside the first
    /// packet's service interval — priming with forced silence here would just add a ring's worth
    /// of latency to every stream that did not need it.
    pub fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        self.armed = false;
        Ok(())
    }

    /// Borrow the next free slot, waiting up to `timeout` for one to come back.
    ///
    /// `Ok(None)` means the timeout expired with the ring still full — every slot is in flight,
    /// which for a healthy stream simply means the producer is ahead of the bus.
    pub fn next_slot(&mut self, timeout: Duration) -> Result<Option<Slot<'_, 'd>>> {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain_completions()?;

            if self.started && self.armed && self.ring.in_flight() == 0 {
                self.armed = false;
                self.stats.underruns += 1;
                match self.underrun {
                    Underrun::Error => {
                        return Err(Error::Underrun {
                            count: self.stats.underruns,
                        })
                    }
                    Underrun::FillSilence => self.fill_silence()?,
                    Underrun::Continue => {}
                }
            }

            if let Some(idx) = self.ring.take_free() {
                return Ok(Some(Slot {
                    out: self,
                    idx,
                    committed: false,
                }));
            }

            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            // Wait for a completion rather than spinning; a 1 ms packet stream would otherwise
            // burn a core between URBs and pollute the very latency numbers WP7 is measuring.
            self.device.wait_readable(deadline - now)?;
        }
    }

    /// Submit zero-filled URBs to refill a drained pipeline.
    fn fill_silence(&mut self) -> Result<()> {
        let want = self.ring.layout().slots.min(2);
        for _ in 0..want {
            let Some(idx) = self.ring.take_free() else {
                break;
            };
            let byte = self.silence_byte;
            self.ring.data_mut(idx).fill(byte);
            let capacity = self.ring.layout().slot_capacity();
            self.ring.set_packet_lengths(idx, capacity)?;
            self.submit(idx)?;
        }
        Ok(())
    }

    /// Hand a filled slot to the kernel.
    fn submit(&mut self, idx: usize) -> Result<()> {
        let urb = self.ring.urb_ptr(idx);
        // SAFETY: `urb` is this ring's own header for `idx`, fully initialised by `Ring::new` and
        // by `set_packet_lengths`. `SUBMITURB` takes the URB pointer directly. The kernel retains
        // the pointer until the URB is reaped, which the slot state machine tracks and `Drop`
        // enforces before the block is freed.
        let rc = unsafe { libc::ioctl(self.device.as_raw_fd(), sys::SUBMITURB as _, urb) };
        if rc < 0 {
            let errno = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO);
            // Failed submits leave the slot ours; hand it straight back rather than stranding it.
            self.ring.release(idx);
            return Err(Error::from_errno(
                errno,
                ErrnoContext::Submit {
                    bytes: self.schedule.memory_bytes(),
                },
            ));
        }
        // SAFETY: read-back of the length we just set on our own live header, for accounting.
        let bytes = unsafe { (*urb).buffer_length.max(0) as u64 };
        self.ring.mark_in_flight(idx);
        self.stats.urbs_submitted += 1;
        self.stats.bytes_submitted += bytes;
        self.armed = true;
        Ok(())
    }

    /// Reap one completion, if any is waiting.
    fn reap(&mut self) -> Result<Reaped> {
        let mut urb: *mut sys::Urb = std::ptr::null_mut();
        // SAFETY: `REAPURBNDELAY` writes one URB pointer through the `void **` we pass; `&mut urb`
        // is a live, correctly typed, correctly aligned destination.
        let rc = unsafe {
            libc::ioctl(
                self.device.as_raw_fd(),
                sys::REAPURBNDELAY as _,
                (&mut urb as *mut *mut sys::Urb).cast::<c_void>(),
            )
        };
        if rc < 0 {
            let errno = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO);
            if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK {
                return Ok(Reaped::Nothing);
            }
            return Err(Error::from_errno(errno, ErrnoContext::Other));
        }
        // REAPURB hands back *any* completed URB on this descriptor, not necessarily one of ours.
        // Resolving it through the ring's offset arithmetic — never by pointer identity — is both
        // the MTE-safe path and the thing that keeps a second stream on the same fd from
        // corrupting this one's accounting.
        match self.ring.slot_of_urb(urb) {
            Some(idx) => Ok(Reaped::Slot(idx)),
            None => Ok(Reaped::Foreign),
        }
    }

    /// Reap and account for everything currently completed.
    fn drain_completions(&mut self) -> Result<usize> {
        let mut n = 0;
        loop {
            match self.reap()? {
                Reaped::Nothing => return Ok(n),
                // Someone else's URB on this fd. Nothing to account, and dropping it is correct:
                // the other owner reaps its own completions.
                Reaped::Foreign => continue,
                Reaped::Slot(idx) => {
                    let c = self.ring.complete(idx);
                    n += 1;
                    if !c.was_discarded {
                        self.stats.urbs_completed += 1;
                        self.stats.bytes_transferred += c.actual_bytes as u64;
                        self.stats.short_bytes += c.short_bytes as u64;
                        self.stats.packet_errors += u64::from(c.packet_errors);
                        if c.status != 0 {
                            self.stats.urb_errors += 1;
                            if c.status == -libc::ENODEV || c.status == -libc::ESHUTDOWN {
                                return Err(Error::Disconnected);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Stop the stream: cancel everything in flight and reap it back.
    ///
    /// Returns `Ok(())` only when the kernel has handed back every URB. If it has not — the
    /// realistic case being a device unplugged mid-stream on a kernel without
    /// `USBDEVFS_CAP_REAP_AFTER_DISCONNECT` — the error is surfaced and the ring is leaked at drop
    /// rather than freed under the kernel's feet.
    pub fn stop(&mut self) -> Result<()> {
        self.started = false;
        self.armed = false;
        if self.ring.in_flight() == 0 {
            return Ok(());
        }

        for idx in 0..self.ring.layout().slots {
            if self.ring.state(idx) != crate::ring::SlotState::InFlight {
                continue;
            }
            let urb = self.ring.urb_ptr(idx);
            // SAFETY: `DISCARDURB` takes the URB pointer itself. `urb` is our own live header for
            // a slot the kernel currently owns.
            let rc = unsafe { libc::ioctl(self.device.as_raw_fd(), sys::DISCARDURB as _, urb) };
            // EINVAL means it already completed; either way the kernel still owes us the
            // completion, which the drain below collects.
            let _ = rc;
            self.ring.mark_discarding(idx);
        }

        // Bounded: a device that has gone will never complete them, and blocking forever in a
        // teardown path is worse than leaking a ring.
        let deadline = Instant::now() + Duration::from_millis(250);
        while self.ring.in_flight() > 0 {
            match self.drain_completions() {
                Ok(_) => {}
                // The device is gone; its URBs are unrecoverable. Fall through to the check below.
                Err(Error::Disconnected) => break,
                Err(e) => return Err(e),
            }
            if self.ring.in_flight() == 0 {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            let _ = self.device.wait_readable(Duration::from_millis(5));
        }

        if self.ring.in_flight() > 0 {
            return Err(Error::Disconnected);
        }
        Ok(())
    }
}

impl Drop for IsoOut<'_> {
    fn drop(&mut self) {
        let _ = self.stop();
        if self.ring.in_flight() > 0 {
            // The kernel still holds pointers into the ring and will never give them back. Leak
            // it: freeing memory a host controller may still DMA into corrupts whatever gets that
            // address next, which is the single worst failure mode this crate could have.
            self.ring.leak();
        }
    }
}

enum Reaped {
    Slot(usize),
    Foreign,
    Nothing,
}

/// A borrowed slot in the ring, ready to be filled.
///
/// The buffer is the ring's own memory: filling it is the only copy on the path, and committing
/// costs one `ioctl` with no allocation. Dropping a slot without committing returns it to the free
/// list, so an early `?` in a producer loop cannot slowly starve the ring.
#[derive(Debug)]
pub struct Slot<'a, 'd> {
    out: &'a mut IsoOut<'d>,
    idx: usize,
    committed: bool,
}

impl Slot<'_, '_> {
    /// The slot's payload buffer — `packets_per_urb * packet_bytes` bytes.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.out.ring.data_mut(self.idx)
    }

    /// Total bytes this slot can carry.
    pub fn capacity(&self) -> usize {
        self.out.ring.layout().slot_capacity()
    }

    /// Bytes per packet, and so the natural chunk size to write.
    pub fn packet_bytes(&self) -> usize {
        self.out.ring.layout().packet_bytes
    }

    /// Submit the first `bytes` bytes of the buffer.
    pub fn commit(mut self, bytes: usize) -> Result<()> {
        self.out.ring.set_packet_lengths(self.idx, bytes)?;
        self.committed = true;
        self.out.submit(self.idx)
    }

    /// Submit with an explicit length for each packet.
    ///
    /// The escape hatch for rates that are not a whole number of frames per service interval; see
    /// `Ring::set_packet_lengths_exact`. `lengths` must have exactly `packets_per_urb` entries.
    pub fn commit_packets(mut self, lengths: &[usize]) -> Result<()> {
        self.out.ring.set_packet_lengths_exact(self.idx, lengths)?;
        self.committed = true;
        self.out.submit(self.idx)
    }

    /// Submit the whole buffer.
    pub fn commit_full(self) -> Result<()> {
        let n = self.capacity();
        self.commit(n)
    }
}

impl Drop for Slot<'_, '_> {
    fn drop(&mut self) {
        if !self.committed {
            self.out.ring.release(self.idx);
        }
    }
}

/// Deriving a stream configuration without touching a device — used by `iso-probe` to print what
/// a given endpoint *would* do, and by tests.
pub fn plan(
    speed: Speed,
    endpoint: &descriptor::Endpoint,
    depth: Depth,
    packets_per_urb: Option<usize>,
) -> Result<Schedule> {
    if endpoint.transfer_type() != TransferType::Isochronous {
        return Err(Error::InvalidEndpoint(endpoint.address));
    }
    Schedule::derive(
        speed,
        endpoint.interval,
        endpoint.bytes_per_interval(),
        depth,
        packets_per_urb,
    )
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<Ring>();
}

const _: () = {
    // `c_uint` is what the request constants are typed as; the `as _` casts at the ioctl call
    // sites rely on that staying true.
    let _: c_uint = sys::SUBMITURB;
};
