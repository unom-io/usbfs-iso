//! Opening a stream and pushing PCM at it — WP6.
//!
//! # Rate is not a setting, it is a cadence
//!
//! For an **adaptive** endpoint (the DualSense's, and most cheap DACs') the device has no clock of
//! its own: it consumes exactly what arrives each service interval, so the *amount of data per
//! packet is the sample rate*. That makes one detail load-bearing. 48 kHz on a 1 ms bus is a clean
//! 48 frames per packet, but 44.1 kHz is 44.1 — and a host that sends a fixed 44 runs 0.2% slow
//! forever while one that sends 45 runs fast. [`Playback`] therefore paces with a fractional
//! accumulator and hands `usbfs-iso` an explicit per-packet length plan, so the long-run average
//! is exact for any rate.
//!
//! # What is not handled
//!
//! **Asynchronous endpoints drift.** They publish their true rate on a feedback endpoint, and this
//! crate parses that endpoint but does not service it. [`AudioStream::feedback_endpoint`] being
//! `Some` is a warning that the caller has to close that loop; adaptive and synchronous endpoints
//! need nothing.

use std::time::Duration;

use usbfs_iso::{Claim, Depth, InterfaceGuard, IsoOut, IsoStats, Underrun, UsbFsDevice};

use crate::parse::{AudioStream, UacVersion};
use crate::{Error, Format, Rates, Result};

// Audio class control requests (UAC1 §5.2.1, UAC2 §5.2.3).
const REQ_SET_CUR: u8 = 0x01;
const REQ_RANGE: u8 = 0x02;
/// `SAMPLING_FREQ_CONTROL` (UAC1, on an endpoint) and `CS_SAM_FREQ_CONTROL` (UAC2, on a clock).
const CTRL_SAMPLING_FREQ: u16 = 0x01;

/// Host-to-device, class request, endpoint recipient.
const RT_OUT_CLASS_ENDPOINT: u8 = 0x22;
/// Host-to-device, class request, interface recipient.
const RT_OUT_CLASS_INTERFACE: u8 = 0x21;
/// Device-to-host, class request, interface recipient.
const RT_IN_CLASS_INTERFACE: u8 = 0xa1;

const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);

/// Knobs for [`AudioStream::open_with`].
#[derive(Debug, Clone, Copy)]
pub struct OpenOptions {
    /// How much audio to keep in flight. The latency knob; see the crate's WP7 notes.
    pub depth: Depth,
    /// Packets per URB. `None` means one — the finest granularity the bus offers.
    pub packets_per_urb: Option<usize>,
    /// Whether to take the interface from the kernel driver that holds it.
    ///
    /// [`Claim::Force`] is almost always required: on Linux and Android `snd-usb-audio` binds any
    /// device with an audio function at enumeration. Note that it binds the function as a **unit**
    /// — detaching a playback interface can take the device's capture side down with it.
    pub claim: Claim,
    /// What to do when the producer falls behind.
    pub underrun: Underrun,
    /// Whether to issue the sample-rate control request. Off for devices with a single fixed rate
    /// that stall on the request.
    pub set_sample_rate: bool,
    /// How long a write waits for a free slot before reporting back-pressure.
    pub write_timeout: Duration,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            // 8 ms: deep enough not to underrun on a general-purpose scheduler, shallow enough to
            // be usable. Latency-sensitive callers should lower it and measure; music playback can
            // raise it a long way.
            depth: Depth::Millis(8),
            packets_per_urb: None,
            claim: Claim::Force,
            underrun: Underrun::FillSilence,
            set_sample_rate: true,
            write_timeout: Duration::from_millis(100),
        }
    }
}

impl AudioStream {
    /// Read the real sample rates from the device when the descriptors deferred them to a clock.
    ///
    /// A no-op returning the existing rates for UAC1, whose descriptors carry them outright.
    pub fn resolve_rates(&self, device: &UsbFsDevice) -> Result<Rates> {
        let Rates::Clock { id } = self.rates else {
            return Ok(self.rates.clone());
        };
        // UAC2 §5.2.5.1: GET RANGE on the clock's sampling-frequency control returns
        // wNumSubRanges followed by (MIN, MAX, RES) triplets of 4 bytes each.
        let mut buf = [0u8; 2 + 12 * 16];
        let n = device.control(
            RT_IN_CLASS_INTERFACE,
            REQ_RANGE,
            CTRL_SAMPLING_FREQ << 8,
            (u16::from(id) << 8) | u16::from(self.control_interface),
            &mut buf,
            CONTROL_TIMEOUT,
        )?;
        if n < 2 {
            return Err(Error::RatesUnresolved);
        }
        let count = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        let mut discrete = Vec::new();
        let mut continuous: Option<(u32, u32)> = None;
        for i in 0..count {
            let off = 2 + i * 12;
            if off + 12 > n {
                break;
            }
            let min = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            let max = u32::from_le_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
            let res =
                u32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
            // A sub-range with RES 0 (or MIN == MAX) is a single discrete rate; anything else is a
            // genuine continuous range and collapsing it to a list would lose rates.
            if res == 0 || min == max {
                discrete.push(min);
            } else {
                let (lo, hi) = continuous.unwrap_or((min, max));
                continuous = Some((lo.min(min), hi.max(max)));
            }
        }
        match continuous {
            Some((min, max)) => Ok(Rates::Continuous { min, max }),
            None if !discrete.is_empty() => Ok(Rates::Discrete(discrete)),
            None => Err(Error::RatesUnresolved),
        }
    }

    /// Claim the interface, arm the endpoint, and open a playback stream with default options.
    pub fn open<'d>(
        &self,
        device: &'d UsbFsDevice,
        format: Format,
        rate: u32,
    ) -> Result<Playback<'d>> {
        self.open_with(device, format, rate, OpenOptions::default())
    }

    /// Claim the interface, arm the endpoint, and open a playback stream.
    pub fn open_with<'d>(
        &self,
        device: &'d UsbFsDevice,
        format: Format,
        rate: u32,
        opts: OpenOptions,
    ) -> Result<Playback<'d>> {
        if format != self.format {
            return Err(Error::FormatMismatch {
                requested: format,
                available: self.format,
            });
        }
        if self.rates.is_resolved() && !self.rates.contains(rate) {
            return Err(Error::RateUnsupported {
                requested: rate,
                advertised: self.rates.clone(),
            });
        }

        let speed = device.speed()?;
        let interval_us = usbfs_iso::packet_interval_us(speed, self.endpoint.interval)?;

        // Size packets for the busiest interval this rate can produce. With a fractional rate the
        // per-packet count alternates, so the reservation must cover the larger of the two.
        let frame_bytes = self.frame_bytes();
        let max_frames = (u64::from(rate) * u64::from(interval_us)).div_ceil(1_000_000) as usize;
        let packet_bytes = max_frames * frame_bytes;
        let available = self.endpoint.bytes_per_interval();
        if packet_bytes > available {
            return Err(Error::RateTooFastForEndpoint {
                requested: rate,
                needed: packet_bytes,
                available,
            });
        }

        // Order matters: claim, then select the alternate setting (which is what reserves the
        // isochronous bandwidth and makes the endpoint exist), and only then talk to the endpoint.
        let guard = device.claim_interface(self.interface, opts.claim)?;
        guard.set_alt_setting(self.alt_setting)?;

        if opts.set_sample_rate {
            self.write_sample_rate(device, rate)?;
        }

        let iso = IsoOut::builder(device, self.endpoint.address)
            .speed(speed)
            .interval(self.endpoint.interval)
            .max_packet_size(available)
            .packet_bytes(packet_bytes)
            .depth(opts.depth)
            .on_underrun(opts.underrun)
            .silence_byte(format.silence_byte())
            .packets_per_urb(opts.packets_per_urb.unwrap_or(1))
            .build()?;

        let packets_per_urb = iso.schedule().packets_per_urb;
        let mut playback = Playback {
            iso,
            _guard: guard,
            format,
            channels: self.channels,
            rate,
            frame_bytes,
            interval_us,
            frame_accum: 0,
            packet_plan: Vec::with_capacity(packets_per_urb),
            pending: Vec::with_capacity(packet_bytes * packets_per_urb),
            scratch: Vec::new(),
            write_timeout: opts.write_timeout,
            frames_written: 0,
        };
        playback.iso.start()?;
        Ok(playback)
    }

    /// Set the sample rate, tolerating the two legal ways a device can decline.
    ///
    /// Measured on a real DualSense: it answers `SET_CUR SAMPLING_FREQ_CONTROL` with a STALL
    /// (`EPIPE`). That is not a fault. The control is **optional** in UAC1 §5.2.3.2.3.1, the pad
    /// runs at one fixed rate, and there is nothing for the request to change. Treating the stall
    /// as fatal made every stream fail to open on a device whose audio endpoint works perfectly.
    ///
    /// So: skip the request when the endpoint does not claim the control, and when it does claim
    /// it but stalls anyway, accept that only if the rate asked for is the single rate the stream
    /// advertises. A stall while genuinely trying to *change* rate is still an error.
    fn write_sample_rate(&self, device: &UsbFsDevice, rate: u32) -> Result<()> {
        if self.version == UacVersion::Uac1 && !self.sampling_freq_control {
            // Nothing to set: the endpoint does not implement the control, so its rate is fixed.
            return Ok(());
        }

        let result = match self.version {
            // UAC1 §5.2.3.2.3.1: the control lives on the *endpoint*, and the value is 24-bit.
            UacVersion::Uac1 => {
                let mut data = [
                    (rate & 0xff) as u8,
                    ((rate >> 8) & 0xff) as u8,
                    ((rate >> 16) & 0xff) as u8,
                ];
                device.control(
                    RT_OUT_CLASS_ENDPOINT,
                    REQ_SET_CUR,
                    CTRL_SAMPLING_FREQ << 8,
                    u16::from(self.endpoint.address),
                    &mut data,
                    CONTROL_TIMEOUT,
                )
            }
            // UAC2 §5.2.5.1: the control lives on the clock entity behind the AC interface, and
            // the value is 32-bit.
            UacVersion::Uac2 => {
                let Rates::Clock { id } = self.rates else {
                    // Already-resolved rates lost the clock id; nothing to address the request to.
                    return Ok(());
                };
                let mut data = rate.to_le_bytes();
                device.control(
                    RT_OUT_CLASS_INTERFACE,
                    REQ_SET_CUR,
                    CTRL_SAMPLING_FREQ << 8,
                    (u16::from(id) << 8) | u16::from(self.control_interface),
                    &mut data,
                    CONTROL_TIMEOUT,
                )
            }
        };
        match result {
            Ok(_) => Ok(()),
            // A STALL is the device saying "I do not implement this". Harmless when the rate we
            // asked for is the only one it offers.
            Err(source) if source.is_stall() && self.only_rate() == Some(rate) => Ok(()),
            Err(source) => Err(Error::RateRejected {
                requested: rate,
                source,
            }),
        }
    }
}

/// A live playback stream. Write PCM at it; it paces the bus.
///
/// Dropping it stops the stream and hands the interface back to the kernel driver it displaced.
///
/// ```no_run
/// # use std::time::Duration;
/// # use uac_host::Format;
/// # use usbfs_iso::UsbFsDevice;
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let dev = UsbFsDevice::open("/dev/bus/usb/001/004")?;
/// let function = uac_host::parse(&dev.raw_descriptors()?)?;
///
/// let stream = function
///     .output_streams()
///     .find(|s| s.channels() == 4 && s.rates().contains(48_000))
///     .ok_or("no 4-channel 48 kHz playback stream")?;
///
/// let mut playback = stream.open(&dev, Format::S16Le, 48_000)?;
/// println!("{} us in flight", playback.schedule().in_flight_us());
///
/// let silence = vec![0i16; 48 * 4];   // 1 ms of 4-channel audio
/// playback.write_interleaved(&silence)?;
/// playback.drain(Duration::from_millis(200))?;
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct Playback<'d> {
    // DECLARATION ORDER IS LOAD-BEARING. Fields drop in declaration order, so the stream tears
    // down (discarding and reaping every in-flight URB) *before* the interface is released and the
    // kernel driver re-attached. The other order hands `snd-usb-audio` an endpoint that still has
    // our URBs queued on it.
    iso: IsoOut<'d>,
    _guard: InterfaceGuard<'d>,

    format: Format,
    channels: u8,
    rate: u32,
    frame_bytes: usize,
    interval_us: u32,
    /// Fractional-rate accumulator, in units of 1/1_000_000 of a frame.
    frame_accum: u64,
    /// Per-packet byte plan for the next URB. Held across a failed write so a retry does not
    /// advance the rate twice.
    packet_plan: Vec<usize>,
    /// PCM handed to us but not yet a whole URB's worth.
    pending: Vec<u8>,
    /// Reusable conversion buffer for the typed write helpers.
    scratch: Vec<u8>,
    write_timeout: Duration,
    frames_written: u64,
}

impl Playback<'_> {
    /// Sample format on the wire.
    pub fn format(&self) -> Format {
        self.format
    }

    /// Channel count.
    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Sample rate being paced.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Bytes for one sample frame across all channels.
    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// Sample frames handed to the bus so far.
    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }

    /// Transport statistics.
    pub fn stats(&self) -> IsoStats {
        self.iso.stats()
    }

    /// The derived transfer schedule, including the true in-flight latency.
    pub fn schedule(&self) -> &usbfs_iso::Schedule {
        self.iso.schedule()
    }

    /// Write interleaved PCM bytes in this stream's format.
    ///
    /// Returns how many bytes it took. A short return means back-pressure — every slot is in
    /// flight and none freed within the write timeout — and the caller should retry with the
    /// remainder rather than dropping it. Bytes short of a whole URB are staged internally and
    /// count as taken.
    pub fn write(&mut self, pcm: &[u8]) -> Result<usize> {
        let mut off = 0;
        loop {
            self.plan_next_urb();
            let need: usize = self.packet_plan.iter().sum();

            if self.pending.len() < need {
                let take = (need - self.pending.len()).min(pcm.len() - off);
                self.pending.extend_from_slice(&pcm[off..off + take]);
                off += take;
                if self.pending.len() < need {
                    return Ok(off);
                }
            }

            let Some(mut slot) = self.iso.next_slot(self.write_timeout)? else {
                return Ok(off);
            };
            slot.bytes_mut()[..need].copy_from_slice(&self.pending[..need]);

            // Move the plan out so the borrow of `self.packet_plan` ends before `commit_packets`
            // consumes the slot, then put the buffer back to keep its capacity.
            let mut plan = std::mem::take(&mut self.packet_plan);
            let committed = slot.commit_packets(&plan);
            plan.clear();
            self.packet_plan = plan;
            committed?;

            self.pending.drain(..need);
            self.frames_written += (need / self.frame_bytes) as u64;
        }
    }

    /// Write every byte, blocking until the bus has taken it all.
    pub fn write_all(&mut self, pcm: &[u8]) -> Result<()> {
        let mut off = 0;
        while off < pcm.len() {
            let n = self.write(&pcm[off..])?;
            if n == 0 {
                // No slot freed within the timeout and nothing was staged. Waiting again is the
                // right move for a blocking write; the transport surfaces a real stall as an error.
                continue;
            }
            off += n;
        }
        Ok(())
    }

    /// Write interleaved 16-bit samples. Only valid on an `S16_LE` stream.
    pub fn write_interleaved(&mut self, samples: &[i16]) -> Result<usize> {
        if self.format != Format::S16Le {
            return Err(Error::FormatMismatch {
                requested: Format::S16Le,
                available: self.format,
            });
        }
        self.scratch.clear();
        self.scratch.reserve(samples.len() * 2);
        for s in samples {
            self.scratch.extend_from_slice(&s.to_le_bytes());
        }
        let scratch = std::mem::take(&mut self.scratch);
        let r = self.write(&scratch);
        self.scratch = scratch;
        r
    }

    /// Flush any staged partial URB, padding it with silence.
    ///
    /// Silence, not zeros: for an unsigned 8-bit stream a zero pad is a loud DC step.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.plan_next_urb();
        let need: usize = self.packet_plan.iter().sum();
        let pad = self.format.silence_byte();
        self.pending.resize(need, pad);
        let Some(mut slot) = self.iso.next_slot(self.write_timeout)? else {
            return Err(Error::Transport(usbfs_iso::Error::Timeout));
        };
        slot.bytes_mut()[..need].copy_from_slice(&self.pending[..need]);
        let mut plan = std::mem::take(&mut self.packet_plan);
        let committed = slot.commit_packets(&plan);
        plan.clear();
        self.packet_plan = plan;
        committed?;
        self.pending.clear();
        Ok(())
    }

    /// Wait for everything already queued to reach the device.
    pub fn drain(&mut self, timeout: Duration) -> Result<()> {
        self.flush()?;
        let deadline = std::time::Instant::now() + timeout;
        while self.iso.in_flight() > 0 {
            if std::time::Instant::now() >= deadline {
                return Err(Error::Transport(usbfs_iso::Error::Timeout));
            }
            // `next_slot` reaps completions as a side effect; the returned slot is dropped
            // uncommitted and goes straight back to the free list.
            let _ = self.iso.next_slot(Duration::from_millis(5))?;
        }
        Ok(())
    }

    /// Compute the byte count for each packet of the next URB, if it is not already planned.
    ///
    /// The fractional accumulator: each packet gets `floor((rate*interval + carry) / 1e6)` frames
    /// and the remainder carries, so the long-run rate is exact even when the per-interval frame
    /// count is not an integer.
    fn plan_next_urb(&mut self) {
        if !self.packet_plan.is_empty() {
            return;
        }
        let per_interval = u64::from(self.rate) * u64::from(self.interval_us);
        for _ in 0..self.iso.schedule().packets_per_urb {
            let total = self.frame_accum + per_interval;
            let frames = total / 1_000_000;
            self.frame_accum = total % 1_000_000;
            self.packet_plan.push(frames as usize * self.frame_bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    /// The pacing arithmetic, exercised without a device: a fractional rate must average out
    /// exactly rather than drifting in either direction.
    fn pace(rate: u32, interval_us: u32, frame_bytes: usize, packets: usize) -> Vec<usize> {
        let mut accum = 0u64;
        let per_interval = u64::from(rate) * u64::from(interval_us);
        (0..packets)
            .map(|_| {
                let total = accum + per_interval;
                accum = total % 1_000_000;
                (total / 1_000_000) as usize * frame_bytes
            })
            .collect()
    }

    #[test]
    fn forty_eight_kilohertz_is_a_constant_384_bytes_per_millisecond() {
        let plan = pace(48_000, 1000, 8, 10);
        assert!(plan.iter().all(|&b| b == 384), "{plan:?}");
    }

    #[test]
    fn forty_four_point_one_alternates_and_averages_exactly() {
        // 44.1 frames per millisecond: a fixed 44 runs slow forever and a fixed 45 runs fast.
        let plan = pace(44_100, 1000, 4, 1000);
        let frames: Vec<usize> = plan.iter().map(|b| b / 4).collect();
        assert!(frames.contains(&44) && frames.contains(&45));
        // One second of packets must carry exactly one second of audio.
        assert_eq!(frames.iter().sum::<usize>(), 44_100);
    }

    #[test]
    fn pacing_is_exact_across_rates_and_bus_speeds() {
        for (rate, interval_us, packets) in [
            (48_000, 1000, 1000), // high speed, 1 ms
            (44_100, 125, 8000),  // high speed, 125 us microframe
            (32_000, 1000, 1000), // full speed frame
            (96_000, 500, 2000),  // 2^2 microframes
            (22_050, 1000, 1000), // another fractional rate
        ] {
            let plan = pace(rate, interval_us, 2, packets);
            let frames: usize = plan.iter().map(|b| b / 2).sum();
            let expected = (u64::from(rate) * u64::from(interval_us) * packets as u64) / 1_000_000;
            assert_eq!(
                frames as u64, expected,
                "{rate} Hz at {interval_us} us drifted over {packets} packets"
            );
        }
    }
}
