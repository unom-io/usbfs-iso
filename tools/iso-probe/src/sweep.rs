//! **WP7 — latency characterisation.** The decision point.
//!
//! The question: *what is the lowest underrun-free in-flight depth on real hardware?* Nobody has
//! published it for Android, and the answer decides whether this route can serve haptics (which
//! need single-digit milliseconds) or only music (where the 80 ms that `decent-usb-audio-driver`
//! runs is perfectly fine).
//!
//! The sweep is depth × packets-per-URB × thread policy. Each cell runs a real stream for a real
//! duration and reports what actually happened, because the failure mode being measured — the
//! producer occasionally missing a deadline — does not show up in a short run or an average.
//!
//! There is a hard ceiling the sweep cannot exceed and does not try to: `usbcore.usbfs_memory_mb`,
//! default 16 MB, shared system-wide and unraisable without root. Cells that would cross it come
//! back as [`usbfs_iso::Error::UsbfsMemory`] and are reported as skipped rather than as failures.

use std::time::{Duration, Instant};

use usbfs_iso::{Depth, IsoStats};

use crate::cli::{self, Args, Result};

/// `SCHED_OTHER` — the ordinary time-sharing policy, 0 on every Linux ABI. The `libc` crate
/// exposes it for glibc but not for bionic, so it is spelled out here rather than `cfg`-ed.
const SCHED_OTHER: libc::c_int = 0;

/// Scheduling policy for the producer thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    /// Whatever the process already had.
    Default,
    /// `setpriority(-16)` — Android's `ANDROID_PRIORITY_AUDIO`. The realistic knob on Android,
    /// which does not hand `SCHED_FIFO` to ordinary app threads the way desktop Linux does.
    Audio,
    /// `SCHED_FIFO`. Usually needs `CAP_SYS_NICE`; the sweep reports whether it actually took.
    Fifo,
}

impl Policy {
    fn parse(s: &str) -> Option<Policy> {
        match s {
            "default" => Some(Policy::Default),
            "audio" => Some(Policy::Audio),
            "fifo" => Some(Policy::Fifo),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Policy::Default => "default",
            Policy::Audio => "audio(-16)",
            Policy::Fifo => "SCHED_FIFO",
        }
    }

    /// Apply the policy, reporting whether the kernel actually honoured it.
    ///
    /// Reporting rather than failing is the point: "we asked for -16 and got 0" is itself a
    /// finding, and a sweep that silently ran at the wrong priority would publish a wrong number.
    fn apply(self) -> bool {
        match self {
            Policy::Default => true,
            Policy::Audio => {
                // SAFETY: `setpriority` on the calling process; no pointers involved.
                unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, -16) };
                // Read it back rather than trusting the return code: on both desktop Linux and
                // Android a request to go negative is quietly refused without CAP_SYS_NICE.
                // `__errno_location` is glibc-only and bionic spells it differently, so this
                // deliberately does not disambiguate the -1-means-error case: -1 is not <= -16, so
                // an error reads as "not applied", which is the safe answer.
                // SAFETY: `getpriority` on the calling process; no pointers involved.
                let got = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
                got <= -16
            }
            Policy::Fifo => {
                let param = libc::sched_param { sched_priority: 10 };
                // SAFETY: `param` is a live, fully initialised `sched_param` for the duration of
                // the call, and 0 means "this thread".
                let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
                rc == 0
            }
        }
    }

    /// Put the thread back to a sane state between cells so one policy cannot contaminate the next.
    fn reset() {
        let param = libc::sched_param { sched_priority: 0 };
        // SAFETY: as above; restoring SCHED_OTHER at priority 0 is always valid.
        unsafe { libc::sched_setscheduler(0, SCHED_OTHER, &param) };
        // SAFETY: `setpriority` on the calling process.
        unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 0) };
    }
}

struct Cell {
    depth: usize,
    packets: usize,
    policy: Policy,
    policy_applied: bool,
    in_flight_us: u64,
    memory_bytes: usize,
    stats: IsoStats,
    late_wakeups: u64,
    worst_gap_us: u64,
    skipped: Option<String>,
}

impl Cell {
    /// A cell is clean when nothing was lost: no underruns, no short packets, no URB errors.
    fn clean(&self) -> bool {
        self.skipped.is_none()
            && self.stats.underruns == 0
            && self.stats.short_bytes == 0
            && self.stats.urb_errors == 0
            && self.stats.urbs_completed > 0
    }
}

pub fn run(args: &Args) -> Result<()> {
    let dev = cli::open(args)?;
    let blob = dev.raw_descriptors()?;
    let function = uac_host::parse(&blob)?;
    let stream = cli::pick_output_stream(&function, args)?;
    let rate = cli::pick_rate(stream, args)?;
    let speed = dev.speed()?;
    let interval_us = usbfs_iso::packet_interval_us(speed, stream.endpoint().interval)?;
    let seconds = args.seconds.unwrap_or(5);

    let depths = args
        .depths
        .clone()
        .unwrap_or_else(|| vec![2, 3, 4, 6, 8, 12, 20, 40, 80]);
    let packets = args.packets.clone().unwrap_or_else(|| vec![1, 2, 4, 8]);
    let policies: Vec<Policy> = match &args.policies {
        Some(list) => list
            .iter()
            .filter_map(|s| Policy::parse(s))
            .collect::<Vec<_>>(),
        None => vec![Policy::Default, Policy::Audio],
    };
    if policies.is_empty() {
        return Err("no valid --policies (choose from default,audio,fifo)".into());
    }

    println!(
        "sweeping {}ch {} at {} Hz on endpoint 0x{:02x}, {:?}-speed, {} us/packet, {} s per cell",
        stream.channels(),
        stream.format(),
        rate,
        stream.endpoint().address,
        speed,
        interval_us,
        seconds
    );
    if let Some(budget) = usbfs_iso::usbfs_memory_budget_bytes() {
        println!(
            "usbfs in-flight budget: {} bytes (shared system-wide)",
            budget
        );
    } else {
        println!("usbfs in-flight budget: unreadable; assuming the 16 MB default");
    }
    println!(
        "\n{:>6} {:>8} {:>12} {:>10} {:>9} {:>7} {:>7} {:>6} {:>10} {:>9}",
        "urbs",
        "pkts/urb",
        "policy",
        "in-flight",
        "mem",
        "under",
        "short",
        "err",
        "late",
        "worst gap"
    );

    let mut cells = Vec::new();
    for &policy in &policies {
        for &packets_per_urb in &packets {
            for &depth in &depths {
                let cell = run_cell(
                    &dev,
                    stream,
                    rate,
                    depth,
                    packets_per_urb,
                    policy,
                    Duration::from_secs(seconds),
                );
                print_cell(&cell);
                cells.push(cell);
            }
        }
    }
    Policy::reset();
    report(&cells, interval_us);
    Ok(())
}

fn print_cell(c: &Cell) {
    if let Some(why) = &c.skipped {
        println!(
            "{:>6} {:>8} {:>12} {:>10} {:>9} {:>7} {:>7} {:>6} {:>10} {:>9}   SKIPPED: {why}",
            c.depth,
            c.packets,
            c.policy.name(),
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-"
        );
        return;
    }
    println!(
        "{:>6} {:>8} {:>12} {:>9}u {:>8}B {:>7} {:>7} {:>6} {:>10} {:>8}u{}",
        c.depth,
        c.packets,
        c.policy.name(),
        c.in_flight_us,
        c.memory_bytes,
        c.stats.underruns,
        c.stats.short_bytes,
        c.stats.urb_errors,
        c.late_wakeups,
        c.worst_gap_us,
        if c.policy_applied {
            ""
        } else {
            "  (policy NOT applied)"
        }
    );
}

#[allow(clippy::too_many_arguments)]
fn run_cell(
    dev: &usbfs_iso::UsbFsDevice,
    stream: &uac_host::AudioStream,
    rate: u32,
    depth: usize,
    packets_per_urb: usize,
    policy: Policy,
    duration: Duration,
) -> Cell {
    let policy_applied = policy.apply();

    let opts = uac_host::OpenOptions {
        depth: Depth::Urbs(depth),
        packets_per_urb: Some(packets_per_urb),
        // Count underruns; do not paper over them with silence, which is exactly the thing being
        // measured.
        underrun: usbfs_iso::Underrun::Continue,
        ..Default::default()
    };

    let mut cell = Cell {
        depth,
        packets: packets_per_urb,
        policy,
        policy_applied,
        in_flight_us: 0,
        memory_bytes: 0,
        stats: IsoStats::default(),
        late_wakeups: 0,
        worst_gap_us: 0,
        skipped: None,
    };

    let mut playback = match stream.open_with(dev, stream.format(), rate, opts) {
        Ok(p) => p,
        Err(e) => {
            cell.skipped = Some(e.to_string());
            Policy::reset();
            return cell;
        }
    };
    cell.in_flight_us = playback.schedule().in_flight_us();
    cell.memory_bytes = playback.schedule().memory_bytes();

    // One URB's worth of silence, written over and over. Silence is the right signal here: the
    // measurement is about the producer meeting deadlines, and generating content would put the
    // generator's own cost into the number.
    let urb_bytes = playback.schedule().packet_bytes * packets_per_urb;
    let silence = vec![stream.format().silence_byte(); urb_bytes];

    let start = Instant::now();
    let mut last = start;
    // A wake-up is "late" when the gap between two successful writes exceeded the audio actually
    // in flight — the point at which the pipeline would have drained.
    let budget_us = cell.in_flight_us;
    while start.elapsed() < duration {
        match playback.write(&silence) {
            Ok(0) => {}
            Ok(_) => {}
            Err(e) => {
                cell.skipped = Some(e.to_string());
                break;
            }
        }
        let now = Instant::now();
        let gap = now.duration_since(last).as_micros() as u64;
        last = now;
        cell.worst_gap_us = cell.worst_gap_us.max(gap);
        if budget_us > 0 && gap > budget_us {
            cell.late_wakeups += 1;
        }
    }
    cell.stats = playback.stats();
    drop(playback);
    Policy::reset();
    cell
}

fn report(cells: &[Cell], interval_us: u32) {
    println!("\n== result ==");
    let clean: Vec<&Cell> = cells.iter().filter(|c| c.clean()).collect();
    if clean.is_empty() {
        println!("  No cell was clean. Either the device rejected the stream, or this machine");
        println!("  cannot sustain isochronous output at any depth the sweep tried.");
        println!("  Re-run with --depths 80,120,200 before concluding it is impossible.");
        return;
    }

    let floor = clean.iter().min_by_key(|c| c.in_flight_us).unwrap();
    println!(
        "  Lowest underrun-free depth: {} us in flight ({} urbs x {} packet(s), {}), {} B charged\n\
         \x20 to the usbfs budget.",
        floor.in_flight_us,
        floor.depth,
        floor.packets,
        floor.policy.name(),
        floor.memory_bytes
    );
    println!("  One packet is {interval_us} us, so that is the granularity of any improvement.");

    // The honesty gate from the plan: say plainly which side of the haptics threshold this lands
    // on, rather than leaving the reader to decide what a number means.
    let ms = floor.in_flight_us as f64 / 1000.0;
    println!("\n  Interpretation:");
    if ms <= 8.0 {
        println!("    {ms:.1} ms — comfortably inside single-digit milliseconds. Usable for");
        println!("    haptics as well as for audio.");
    } else if ms <= 15.0 {
        println!("    {ms:.1} ms — marginal for haptics. Usable, but the headroom is thin and a");
        println!("    loaded or thermally throttled device will likely lose it.");
    } else {
        println!(
            "    {ms:.1} ms — above the ~15 ms haptics threshold. This route serves music and"
        );
        println!("    video output well, but a haptics consumer should stay on its fallback.");
        println!("    That is a negative result for the product and a fine one for the library.");
    }

    // Whether more packets per URB bought anything is the second question the sweep answers.
    let best_by_packets: Vec<(usize, u64)> = {
        let mut v: Vec<(usize, u64)> = Vec::new();
        for c in &clean {
            match v.iter_mut().find(|(p, _)| *p == c.packets) {
                Some((_, best)) => *best = (*best).min(c.in_flight_us),
                None => v.push((c.packets, c.in_flight_us)),
            }
        }
        v.sort();
        v
    };
    println!("\n  Lowest clean latency by packets-per-URB:");
    for (p, us) in best_by_packets {
        println!("    {p:>2} packet(s): {us} us");
    }

    println!("\n  Run this under load (a game streaming, a thermally throttled device) before");
    println!("  trusting it: an idle-machine floor is the best case, not the shipping case.");
}
