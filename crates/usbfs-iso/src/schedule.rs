//! Bus speed, service intervals, and the packets-per-URB arithmetic.
//!
//! This is design §3.2 — "hard thing #1" — in code. **Nothing here is hardcoded to a device.**
//! The caller reads the speed from the kernel (`UsbFsDevice::speed`) and the `bInterval`
//! from the endpoint descriptor, and every latency and buffer figure is derived from those two.
//!
//! The rule that makes the difference, from USB 2.0 §9.6.6: for an **isochronous** endpoint at any
//! speed, `bInterval` is an exponent, not a count — the period is `2^(bInterval-1)` service
//! intervals. The service interval itself is what changes with speed: a 1 ms *frame* at full
//! speed, a 125 µs *microframe* at high speed and above. So the same `bInterval 4` means 8 ms on a
//! full-speed bus and 1 ms on a high-speed one — an eightfold difference in how much audio one
//! packet carries, which is exactly why the speed must be read rather than assumed.
//!
//! Every function in this module is pure arithmetic and runs on any host, so the whole table is
//! covered by tier-0 tests.

use crate::{Error, Result};

/// Bus speed, as reported by `USBDEVFS_GET_SPEED` (`enum usb_device_speed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Speed {
    /// The kernel could not determine the speed.
    Unknown,
    /// 1.5 Mbit/s. Has no isochronous endpoints at all.
    Low,
    /// 12 Mbit/s. Service interval is a 1 ms frame.
    Full,
    /// 480 Mbit/s. Service interval is a 125 µs microframe.
    High,
    /// Wireless USB (2.5). Never shipped in a form this crate supports.
    Wireless,
    /// 5 Gbit/s. Service interval is a 125 µs bus interval.
    Super,
    /// 10 Gbit/s and above.
    SuperPlus,
}

impl Speed {
    /// Decode the kernel's `enum usb_device_speed` value.
    pub fn from_raw(raw: i32) -> Speed {
        match raw {
            1 => Speed::Low,
            2 => Speed::Full,
            3 => Speed::High,
            4 => Speed::Wireless,
            5 => Speed::Super,
            6 => Speed::SuperPlus,
            _ => Speed::Unknown,
        }
    }

    /// Length of one service interval in microseconds — 1000 µs (frame) or 125 µs (microframe).
    ///
    /// `None` for speeds that cannot carry isochronous traffic.
    pub fn service_interval_us(self) -> Option<u32> {
        match self {
            Speed::Full => Some(1000),
            Speed::High | Speed::Super | Speed::SuperPlus => Some(125),
            // Low speed has no isochronous endpoints; Wireless and Unknown are not supportable.
            Speed::Low | Speed::Wireless | Speed::Unknown => None,
        }
    }
}

/// Microseconds between two consecutive packets of an isochronous endpoint.
///
/// `bInterval` is an exponent: the period is `2^(bInterval-1)` service intervals. The spec allows
/// 1..=16; the kernel additionally clamps the resulting period (1024 frames at full speed, 8192
/// microframes at high speed), which only bites at intervals no real audio device uses.
pub fn packet_interval_us(speed: Speed, b_interval: u8) -> Result<u32> {
    let base = speed
        .service_interval_us()
        .ok_or(Error::UnsupportedSpeed(speed))?;
    if !(1..=16).contains(&b_interval) {
        return Err(Error::InvalidInterval(b_interval));
    }
    let exponent = u32::from(b_interval - 1);
    // Clamp exactly as `usb_submit_urb` does, so our arithmetic matches what the bus will really
    // do rather than what the descriptor literally asked for.
    let max_intervals: u32 = if base == 1000 { 1024 } else { 8192 };
    let intervals = (1u32 << exponent).min(max_intervals);
    Ok(base.saturating_mul(intervals))
}

/// How the caller wants to express the amount of audio kept in flight.
///
/// Depth is *the* latency knob: an isochronous OUT stream cannot be refilled retroactively, so
/// whatever is queued is the floor on how stale the newest sample can be. WP7 exists to measure
/// how low this can go before the producer stops keeping up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// An explicit URB count. At least 2 — one URB in flight and one being filled.
    Urbs(usize),
    /// A wall-clock target; rounded **up** to whole packets and then whole URBs.
    Micros(u32),
    /// A wall-clock target in milliseconds.
    Millis(u32),
}

/// A fully derived transfer schedule: everything the ring needs, and every latency figure the
/// caller wants to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Bus speed the figures were derived from.
    pub speed: Speed,
    /// The endpoint's `bInterval`.
    pub b_interval: u8,
    /// Microseconds between packets, from [`packet_interval_us`].
    pub packet_interval_us: u32,
    /// Packets in each URB. One URB is submitted and completes as a unit, so this is the
    /// granularity at which the producer is allowed to be late.
    pub packets_per_urb: usize,
    /// Number of URB slots in the ring.
    pub urbs: usize,
    /// Bytes reserved per packet (the endpoint's `wMaxPacketSize`, or a smaller nominal payload).
    pub packet_bytes: usize,
}

impl Schedule {
    /// Derive a schedule from the bus facts plus a requested depth.
    ///
    /// `packets_per_urb` of `None` means "choose": one packet per URB, the finest granularity the
    /// bus offers, which is what a latency-sensitive stream wants. Bulk-ish consumers (music
    /// playback, where 80 ms of buffer is normal) should pass a larger value to cut the number of
    /// completions the CPU has to service.
    pub fn derive(
        speed: Speed,
        b_interval: u8,
        packet_bytes: usize,
        depth: Depth,
        packets_per_urb: Option<usize>,
    ) -> Result<Schedule> {
        let packet_interval_us = packet_interval_us(speed, b_interval)?;
        if packet_bytes == 0 {
            return Err(Error::Config("packet_bytes must be non-zero"));
        }

        let packets_per_urb = packets_per_urb.unwrap_or(1);
        if packets_per_urb == 0 {
            return Err(Error::Config("packets_per_urb must be non-zero"));
        }
        if packets_per_urb > crate::sys::MAX_ISO_PACKETS_PER_URB {
            return Err(Error::TooManyPackets {
                requested: packets_per_urb,
                max: crate::sys::MAX_ISO_PACKETS_PER_URB,
            });
        }

        let urbs = match depth {
            Depth::Urbs(n) => n,
            Depth::Micros(us) => {
                let packets = (us as usize).div_ceil(packet_interval_us as usize);
                packets.div_ceil(packets_per_urb)
            }
            Depth::Millis(ms) => {
                let us = (ms as usize).saturating_mul(1000);
                let packets = us.div_ceil(packet_interval_us as usize);
                packets.div_ceil(packets_per_urb)
            }
        };

        // Two is the hard floor: with a single URB the endpoint is guaranteed to starve between
        // that URB completing and its replacement being submitted, no matter how fast the
        // producer is. This is a correctness bound, not a tuning choice.
        let urbs = urbs.max(2);

        Ok(Schedule {
            speed,
            b_interval,
            packet_interval_us,
            packets_per_urb,
            urbs,
            packet_bytes,
        })
    }

    /// Wall-clock duration of one URB.
    pub fn urb_duration_us(&self) -> u64 {
        self.packet_interval_us as u64 * self.packets_per_urb as u64
    }

    /// Wall-clock audio held in flight when the ring is full — the stream's latency floor.
    pub fn in_flight_us(&self) -> u64 {
        self.urb_duration_us() * self.urbs as u64
    }

    /// Total payload bytes the kernel is asked to hold in flight.
    ///
    /// Compare against [`usbfs_memory_budget_bytes`]: exceeding the system-wide budget is
    /// [`Error::UsbfsMemory`] at submit time, and the budget is shared with every other usbfs
    /// client on the machine.
    pub fn memory_bytes(&self) -> usize {
        self.urbs * self.packets_per_urb * self.packet_bytes
    }
}

/// The kernel's usbfs in-flight memory budget in bytes, read from sysfs.
///
/// `None` when the parameter is unreadable — which is the normal case for an unprivileged Android
/// app, where sysfs is largely SELinux-closed. Callers should treat `None` as "assume the 16 MB
/// default and stay well under it" rather than as a licence to allocate freely.
pub fn usbfs_memory_budget_bytes() -> Option<usize> {
    let raw = std::fs::read_to_string("/sys/module/usbcore/parameters/usbfs_memory_mb").ok()?;
    let mb: usize = raw.trim().parse().ok()?;
    Some(mb * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dualsense_high_speed_interval_is_one_millisecond() {
        // The DualSense's audio OUT endpoint: bInterval 4. At high speed that is 2^3 = 8
        // microframes = 1 ms, which is what makes 384 bytes (48 frames x 4 ch x 16-bit) the
        // per-packet payload and 392 (wMaxPacketSize) the reservation with one frame of slack.
        assert_eq!(packet_interval_us(Speed::High, 4).unwrap(), 1000);
    }

    #[test]
    fn the_same_binterval_means_eight_times_more_at_full_speed() {
        // The trap §3.2 exists to catch: identical descriptor, eightfold difference in packet
        // duration, so a schedule derived for the wrong speed is silently wrong rather than
        // failing loudly.
        assert_eq!(packet_interval_us(Speed::Full, 4).unwrap(), 8000);
        assert_eq!(packet_interval_us(Speed::High, 4).unwrap(), 1000);
    }

    #[test]
    fn interval_table_across_speeds() {
        for (speed, b_interval, expect) in [
            (Speed::High, 1, 125),
            (Speed::High, 2, 250),
            (Speed::High, 3, 500),
            (Speed::High, 4, 1000),
            (Speed::High, 5, 2000),
            (Speed::Super, 1, 125),
            (Speed::Super, 4, 1000),
            (Speed::Full, 1, 1000),
            (Speed::Full, 2, 2000),
            (Speed::Full, 3, 4000),
        ] {
            assert_eq!(
                packet_interval_us(speed, b_interval).unwrap(),
                expect,
                "{speed:?} bInterval {b_interval}"
            );
        }
    }

    #[test]
    fn kernel_period_clamps_are_reproduced() {
        // usb_submit_urb() caps the period at 1024 frames / 8192 microframes; beyond that the
        // descriptor asks for something the bus will not do, and our arithmetic must agree with
        // the bus rather than with the descriptor.
        assert_eq!(packet_interval_us(Speed::Full, 16).unwrap(), 1024 * 1000);
        assert_eq!(packet_interval_us(Speed::High, 16).unwrap(), 8192 * 125);
    }

    #[test]
    fn speeds_without_isochronous_are_refused() {
        assert!(matches!(
            packet_interval_us(Speed::Low, 1),
            Err(Error::UnsupportedSpeed(Speed::Low))
        ));
        assert!(matches!(
            packet_interval_us(Speed::Unknown, 1),
            Err(Error::UnsupportedSpeed(Speed::Unknown))
        ));
    }

    #[test]
    fn out_of_range_intervals_are_refused() {
        assert!(matches!(
            packet_interval_us(Speed::High, 0),
            Err(Error::InvalidInterval(0))
        ));
        assert!(matches!(
            packet_interval_us(Speed::High, 17),
            Err(Error::InvalidInterval(17))
        ));
    }

    #[test]
    fn six_millis_of_dualsense_audio_is_six_single_packet_urbs() {
        let s = Schedule::derive(Speed::High, 4, 392, Depth::Millis(6), None).unwrap();
        assert_eq!(s.packets_per_urb, 1);
        assert_eq!(s.urbs, 6);
        assert_eq!(s.urb_duration_us(), 1000);
        assert_eq!(s.in_flight_us(), 6000);
        assert_eq!(s.memory_bytes(), 6 * 392);
    }

    #[test]
    fn packing_more_packets_per_urb_holds_the_latency_and_cuts_completions() {
        let fine = Schedule::derive(Speed::High, 4, 392, Depth::Millis(8), Some(1)).unwrap();
        let coarse = Schedule::derive(Speed::High, 4, 392, Depth::Millis(8), Some(4)).unwrap();
        assert_eq!(fine.urbs, 8);
        assert_eq!(coarse.urbs, 2);
        assert_eq!(fine.in_flight_us(), coarse.in_flight_us());
        // Same audio in flight, a quarter of the completions to service — the trade WP7 sweeps.
        assert!(coarse.urbs < fine.urbs);
    }

    #[test]
    fn depth_rounds_up_never_down() {
        // 5.5 ms of a 1 ms packet must be 6 URBs, not 5: rounding down would silently ship a
        // shallower pipeline than the caller asked for.
        let s = Schedule::derive(Speed::High, 4, 392, Depth::Micros(5500), None).unwrap();
        assert_eq!(s.urbs, 6);
    }

    #[test]
    fn a_single_urb_is_never_accepted() {
        // One URB always gaps: the endpoint starves between completion and resubmission.
        for depth in [
            Depth::Urbs(0),
            Depth::Urbs(1),
            Depth::Micros(1),
            Depth::Millis(0),
        ] {
            let s = Schedule::derive(Speed::High, 4, 392, depth, None).unwrap();
            assert!(s.urbs >= 2, "{depth:?} produced {} urbs", s.urbs);
        }
    }

    #[test]
    fn packets_per_urb_above_the_kernel_limit_is_refused() {
        let e = Schedule::derive(Speed::High, 4, 392, Depth::Millis(6), Some(129));
        assert!(matches!(
            e,
            Err(Error::TooManyPackets {
                requested: 129,
                max: 128
            })
        ));
    }

    #[test]
    fn decent_style_music_depth_still_fits_the_default_budget() {
        // 80 URBs is what `decent-usb-audio-driver` runs for music. Confirm the arithmetic agrees
        // it is ~80 ms and that it is nowhere near the 16 MB usbfs budget, so the budget is a
        // constraint on *sweep ceilings* (WP7) rather than on ordinary use.
        let s = Schedule::derive(Speed::High, 4, 392, Depth::Urbs(80), Some(1)).unwrap();
        assert_eq!(s.in_flight_us(), 80_000);
        assert!(s.memory_bytes() < 16 * 1024 * 1024);
    }
}
