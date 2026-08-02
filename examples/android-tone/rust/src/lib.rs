//! JNI shim for the `android-tone` reference app — the Android half of WP0, WP7 and WP8.
//!
//! # The recipe this file exists to demonstrate
//!
//! Android hides `/dev/bus/usb` from apps, but not the file descriptor. Once
//! `UsbManager.requestPermission` succeeds, `UsbDeviceConnection.getFileDescriptor()` hands back
//! exactly the usbfs descriptor this crate wants. Pass that `int` down, wrap it with
//! [`UsbFsDevice::from_borrowed_fd`], and every `ioctl` in `usbfs-iso` works unprivileged — because
//! Android's SELinux policy grants ordinary apps `usb_device:chr_file { read write getattr ioctl }`
//! with no `allowxperm` narrowing.
//!
//! **Borrowed, not owned.** `UsbDeviceConnection` closes the descriptor in its own `close()`.
//! Adopting it here would double-close, and a double-close on a JVM surfaces as some unrelated
//! file descriptor being yanked away much later — one of the worst bugs to track down.
//!
//! # Interface style
//!
//! Every entry point takes and returns primitives only, and reports detail through `logcat`
//! (`adb logcat -s usb-iso`). That keeps the shim free of any JNI helper crate and free of string
//! marshalling, which is most of what makes hand-written JNI unpleasant.

use std::ffi::{c_char, c_int, c_void, CString};
use std::time::{Duration, Instant};

use uac_host::{AudioStream, Format, OpenOptions};
use usbfs_iso::{Claim, Depth, Speed, Underrun, UsbFsDevice};

// ---------------------------------------------------------------------------------------------
// Status codes shared with Native.kt. Keep them in sync — the Kotlin side maps each to a message.
// ---------------------------------------------------------------------------------------------

/// Everything worked.
const OK: i32 = 0;
/// The descriptors could not be read or parsed.
const ERR_DESCRIPTORS: i32 = -1;
/// The kernel refused to give up the interface. No app-side fix exists; degrade.
const ERR_CLAIM_REFUSED: i32 = -2;
/// A USB-level failure during the transfer.
const ERR_TRANSPORT: i32 = -3;
/// No playback stream matched.
const ERR_NO_STREAM: i32 = -4;
/// The stream ran but lost data.
const ERR_STREAM_HOLES: i32 = -5;
/// The Rust side panicked; the panic was caught rather than crossing into the JVM.
const ERR_PANIC: i32 = -6;

// ---------------------------------------------------------------------------------------------
// logcat
// ---------------------------------------------------------------------------------------------

const ANDROID_LOG_INFO: c_int = 4;
const ANDROID_LOG_ERROR: c_int = 6;

extern "C" {
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

fn log_at(priority: c_int, message: &str) {
    // A NUL in the message would truncate it; replacing beats dropping the line entirely.
    let text = CString::new(message.replace('\0', "?")).unwrap_or_default();
    let tag = CString::new("usb-iso").expect("static tag has no NUL");
    // SAFETY: both pointers are NUL-terminated C strings that outlive the call, and
    // `__android_log_write` neither retains nor frees them.
    unsafe { __android_log_write(priority, tag.as_ptr(), text.as_ptr()) };
}

macro_rules! log {
    ($($arg:tt)*) => { log_at(ANDROID_LOG_INFO, &format!($($arg)*)) };
}
macro_rules! log_err {
    ($($arg:tt)*) => { log_at(ANDROID_LOG_ERROR, &format!($($arg)*)) };
}

/// Wrap an entry point so a panic is reported rather than unwinding into the JVM, which is
/// undefined behaviour.
fn guard(name: &str, f: impl FnOnce() -> i32 + std::panic::UnwindSafe) -> i32 {
    match std::panic::catch_unwind(f) {
        Ok(code) => code,
        Err(_) => {
            log_err!("{name}: panicked (caught at the JNI boundary)");
            ERR_PANIC
        }
    }
}

/// Wrap a borrowed descriptor from `UsbDeviceConnection`.
///
/// # Safety
///
/// `fd` must be a live usbfs descriptor whose owning `UsbDeviceConnection` outlives the call.
unsafe fn device(fd: i32) -> UsbFsDevice {
    // SAFETY: forwarded from the caller's contract, which the Kotlin side upholds by keeping the
    // connection open for the duration of every native call.
    unsafe { UsbFsDevice::from_borrowed_fd(fd) }
}

fn describe_speed(speed: Speed) -> &'static str {
    match speed {
        Speed::High => "high (125 us microframes; bInterval 4 means 1 ms per packet)",
        Speed::Full => "full (1 ms frames; bInterval 4 means 8 ms per packet)",
        Speed::Super | Speed::SuperPlus => "super (125 us bus intervals)",
        Speed::Low => "low (no isochronous endpoints exist at this speed)",
        _ => "unknown",
    }
}

/// Log everything the device says about itself, and count its playback streams.
fn probe_inner(dev: &UsbFsDevice) -> i32 {
    match dev.device_descriptor() {
        Ok(d) => log!("device {:04x}:{:04x}", d.vendor_id, d.product_id),
        Err(e) => {
            log_err!("cannot read descriptors: {e}");
            return ERR_DESCRIPTORS;
        }
    }
    match dev.speed() {
        Ok(s) => log!("speed: {s:?} — {}", describe_speed(s)),
        Err(e) => log_err!("cannot read speed: {e}"),
    }
    if let Ok(caps) = dev.capabilities() {
        log!(
            "usbfs caps 0x{:02x} (reap-after-disconnect {}, mmap {})",
            caps.bits(),
            caps.reap_after_disconnect(),
            caps.mmap()
        );
    }
    for i in 0..8u8 {
        match dev.driver(i) {
            Ok(Some(d)) => log!("interface {i}: kernel driver {d}"),
            Ok(None) => {}
            Err(_) => break,
        }
    }

    let blob = match dev.raw_descriptors() {
        Ok(b) => b,
        Err(e) => {
            log_err!("cannot read descriptors: {e}");
            return ERR_DESCRIPTORS;
        }
    };
    let functions = match uac_host::parse_all(&blob) {
        Ok(f) => f,
        Err(e) => {
            log_err!("cannot parse audio descriptors: {e}");
            return ERR_DESCRIPTORS;
        }
    };

    let mut outputs = 0;
    for f in &functions {
        log!(
            "audio function on interface {} ({:?})",
            f.control_interface(),
            f.version()
        );
        for s in f.streams() {
            log!(
                "  {:?} if{}/alt{} {}ch {} {} ep 0x{:02x} {:?} {} B/interval bInterval {}",
                s.direction(),
                s.interface(),
                s.alt_setting(),
                s.channels(),
                s.format(),
                s.rates(),
                s.endpoint().address,
                s.sync_type(),
                s.endpoint().bytes_per_interval(),
                s.endpoint().interval
            );
            if s.direction() == uac_host::Direction::Out {
                outputs += 1;
            }
        }
    }
    if outputs == 0 {
        log_err!("no playback stream on this device");
        return ERR_NO_STREAM;
    }
    log!("{outputs} playback stream(s) available");
    outputs
}

/// Find the playback stream to use: most channels first, so a DualSense's 4-channel setting wins
/// over a 2-channel one. Feeding the 2-channel setting would leave the voice coils silent.
fn pick(function: &uac_host::AudioFunction, interface: i32, alt: i32) -> Option<&AudioStream> {
    let mut candidates: Vec<_> = function.output_streams().collect();
    if interface >= 0 {
        candidates.retain(|s| i32::from(s.interface()) == interface);
    }
    if alt >= 0 {
        candidates.retain(|s| i32::from(s.alt_setting()) == alt);
    }
    candidates.sort_by_key(|s| std::cmp::Reverse(s.channels()));
    candidates.first().copied()
}

// ---------------------------------------------------------------------------------------------
// Entry points. Signatures must match Native.kt exactly.
// ---------------------------------------------------------------------------------------------

/// `Native.probe(fd)` — log what the device is and how many playback streams it has.
///
/// # Safety
///
/// `fd` must be a live descriptor from an open `UsbDeviceConnection`.
#[no_mangle]
pub unsafe extern "system" fn Java_io_unom_usbiso_tone_Native_probe(
    _env: *mut c_void,
    _class: *mut c_void,
    fd: i32,
) -> i32 {
    guard("probe", || {
        // SAFETY: the caller's contract, upheld by Kotlin holding the connection open.
        let dev = unsafe { device(fd) };
        probe_inner(&dev)
    })
}

/// `Native.spike(fd, interface, alt)` — **WP0 on Android.**
///
/// Force-claim the audio interface, arm the endpoint, move exactly one isochronous URB, and give
/// it all back. This is the check that decides whether the whole approach works on a given phone;
/// the two things it cannot check — that the gamepad still reports and that the device's
/// microphone comes back — are printed as by-eye steps.
///
/// # Safety
///
/// `fd` must be a live descriptor from an open `UsbDeviceConnection`.
#[no_mangle]
pub unsafe extern "system" fn Java_io_unom_usbiso_tone_Native_spike(
    _env: *mut c_void,
    _class: *mut c_void,
    fd: i32,
    interface: i32,
    alt: i32,
) -> i32 {
    guard("spike", || {
        // SAFETY: the caller's contract.
        let dev = unsafe { device(fd) };

        let Ok(blob) = dev.raw_descriptors() else {
            return ERR_DESCRIPTORS;
        };
        let Ok(function) = uac_host::parse(&blob) else {
            return ERR_DESCRIPTORS;
        };
        let Some(stream) = pick(&function, interface, alt) else {
            log_err!("no playback stream matched");
            return ERR_NO_STREAM;
        };
        let Ok(speed) = dev.speed() else {
            return ERR_DESCRIPTORS;
        };

        log!(
            "spike: interface {} alt {}, {}ch {} on ep 0x{:02x} ({:?}), {:?}-speed",
            stream.interface(),
            stream.alt_setting(),
            stream.channels(),
            stream.format(),
            stream.endpoint().address,
            stream.sync_type(),
            speed
        );

        let before = dev.driver(stream.interface()).ok().flatten();
        log!("interface {} driver before: {before:?}", stream.interface());

        let guard_iface = match dev.claim_interface(stream.interface(), Claim::Force) {
            Ok(g) => g,
            Err(e) => {
                log_err!("CLAIM REFUSED: {e}");
                log_err!("This kernel will not release the interface. There is no app-side fix;");
                log_err!("a consumer must detect this and fall back.");
                return ERR_CLAIM_REFUSED;
            }
        };
        log!(
            "claimed; displaced driver: {:?}",
            guard_iface.displaced_driver()
        );

        if let Err(e) = guard_iface.set_alt_setting(stream.alt_setting()) {
            log_err!("set_alt_setting failed: {e}");
            return ERR_TRANSPORT;
        }
        log!("alternate setting selected — isochronous bandwidth reserved");

        let mut out = match usbfs_iso::IsoOut::builder(&dev, stream.endpoint().address)
            .speed(speed)
            .interval(stream.endpoint().interval)
            .max_packet_size(stream.endpoint().bytes_per_interval())
            .depth(Depth::Urbs(2))
            .on_underrun(Underrun::Continue)
            .silence_byte(stream.format().silence_byte())
            .build()
        {
            Ok(o) => o,
            Err(e) => {
                log_err!("could not build the stream: {e}");
                return ERR_TRANSPORT;
            }
        };
        let schedule = *out.schedule();
        log!(
            "schedule: {} urbs x {} packet(s) x {} B, {} us/packet, {} us in flight, {} B charged",
            schedule.urbs,
            schedule.packets_per_urb,
            schedule.packet_bytes,
            schedule.packet_interval_us,
            schedule.in_flight_us(),
            schedule.memory_bytes()
        );

        if out.start().is_err() {
            return ERR_TRANSPORT;
        }
        match out.next_slot(Duration::from_millis(100)) {
            Ok(Some(mut slot)) => {
                let byte = stream.format().silence_byte();
                slot.bytes_mut().fill(byte);
                if let Err(e) = slot.commit_full() {
                    log_err!("submit failed: {e}");
                    return ERR_TRANSPORT;
                }
            }
            Ok(None) => {
                log_err!("no free slot in a fresh ring — this should be impossible");
                return ERR_TRANSPORT;
            }
            Err(e) => {
                log_err!("next_slot failed: {e}");
                return ERR_TRANSPORT;
            }
        }

        let deadline = Instant::now() + Duration::from_millis(500);
        while out.in_flight() > 0 && Instant::now() < deadline {
            let _ = out.next_slot(Duration::from_millis(20));
        }
        let stats = out.stats();
        log!(
            "urbs {}/{}, bytes {}/{}, short {}, packet errors {}, urb errors {}",
            stats.urbs_completed,
            stats.urbs_submitted,
            stats.bytes_transferred,
            stats.bytes_submitted,
            stats.short_bytes,
            stats.packet_errors,
            stats.urb_errors
        );
        let clean = stats.urbs_completed > 0 && stats.urb_errors == 0 && stats.short_bytes == 0;

        let _ = out.stop();
        drop(out);
        if let Err(e) = guard_iface.release() {
            log_err!("release failed: {e}");
        }
        let after = dev.driver(stream.interface()).ok().flatten();
        log!("interface {} driver after: {after:?}", stream.interface());
        if after != before {
            log_err!("! the kernel driver did not come back the same as it went in");
        }

        if clean {
            log!("SPIKE PASS (software). Now check by eye:");
            log!("  1. does the device's other function still work (a gamepad still reporting)?");
            log!("  2. did its microphone come back? `dumpsys audio` — the kernel binds an audio");
            log!("     function as ONE card, so detaching playback can take capture with it.");
            OK
        } else {
            log_err!("SPIKE FAIL: the URB did not complete cleanly");
            ERR_STREAM_HOLES
        }
    })
}

/// `Native.tone(fd, interface, alt, seconds, hz, depthMs, channelMask)` — play a sine.
///
/// `channelMask` is a bitmask of channel indices; on a DualSense `0b1100` drives the two voice
/// coils alone, which at a low frequency is the difference between hearing something and feeling
/// it. 0 means every channel.
///
/// # Safety
///
/// `fd` must be a live descriptor from an open `UsbDeviceConnection`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn Java_io_unom_usbiso_tone_Native_tone(
    _env: *mut c_void,
    _class: *mut c_void,
    fd: i32,
    interface: i32,
    alt: i32,
    seconds: i32,
    hz: i32,
    depth_ms: i32,
    channel_mask: i32,
) -> i32 {
    guard("tone", || {
        // SAFETY: the caller's contract.
        let dev = unsafe { device(fd) };
        let Ok(blob) = dev.raw_descriptors() else {
            return ERR_DESCRIPTORS;
        };
        let Ok(function) = uac_host::parse(&blob) else {
            return ERR_DESCRIPTORS;
        };
        let Some(stream) = pick(&function, interface, alt) else {
            return ERR_NO_STREAM;
        };
        if stream.format() != Format::S16Le {
            log_err!(
                "this example only writes S16_LE; stream is {}",
                stream.format()
            );
            return ERR_NO_STREAM;
        }
        let rate = match stream.rates().max() {
            Some(r) => r.min(48_000),
            None => 48_000,
        };

        // Audio priority, as a real consumer should take. Android does not hand SCHED_FIFO to
        // ordinary app threads, so -16 (ANDROID_PRIORITY_AUDIO) is the knob that exists.
        //
        // Measured, and NOT the fix it looks like: at 8 ms depth this generator underran 247 times
        // in 3 s without priority and 246 times with it (short bytes did drop to zero). So the
        // shortfall here is not scheduling — a generator that emits exactly one packet per call
        // and returns keeps the pipeline about one URB deep no matter how it is prioritised. The
        // sweep, which writes a pre-built buffer in a tight loop, is clean at this depth. Worth
        // knowing before reading a tone underrun as a transport problem.
        // SAFETY: `setpriority` on the calling thread; no pointers, no shared state.
        let applied = unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, -16);
            libc::getpriority(libc::PRIO_PROCESS, 0) <= -16
        };
        log!("ANDROID_PRIORITY_AUDIO (-16) applied: {applied}");

        let opts = OpenOptions {
            depth: Depth::Millis(depth_ms.clamp(1, 200) as u32),
            ..Default::default()
        };
        let mut playback = match stream.open_with(&dev, stream.format(), rate, opts) {
            Ok(p) => p,
            Err(uac_host::Error::Transport(e))
                if matches!(e, usbfs_iso::Error::InterfaceBusy { .. }) =>
            {
                log_err!("claim refused: {e}");
                return ERR_CLAIM_REFUSED;
            }
            Err(e) => {
                log_err!("open failed: {e}");
                return ERR_TRANSPORT;
            }
        };

        let channels = playback.channels() as usize;
        let targets: Vec<usize> = if channel_mask == 0 {
            (0..channels).collect()
        } else {
            (0..channels)
                .filter(|c| channel_mask & (1 << c) != 0)
                .collect()
        };
        log!(
            "tone: {hz} Hz for {seconds} s into channels {targets:?} of {channels} at {rate} Hz, \
             {} us in flight",
            playback.schedule().in_flight_us()
        );

        let frames_per_chunk = (rate as usize / 1000).max(1);
        let mut chunk = vec![0i16; frames_per_chunk * channels];
        let mut phase = 0.0f32;
        let step = std::f32::consts::TAU * hz as f32 / rate as f32;
        let total = rate as u64 * seconds.max(1) as u64;
        let mut written = 0u64;

        while written < total {
            for frame in chunk.chunks_mut(channels) {
                let sample = (phase.sin() * 16_384.0) as i16;
                phase += step;
                if phase >= std::f32::consts::TAU {
                    phase -= std::f32::consts::TAU;
                }
                frame.fill(0);
                for &c in &targets {
                    frame[c] = sample;
                }
            }
            if let Err(e) = playback.write_interleaved(&chunk) {
                log_err!("write failed: {e}");
                return ERR_TRANSPORT;
            }
            written += frames_per_chunk as u64;
        }
        let _ = playback.drain(Duration::from_millis(500));

        let stats = playback.stats();
        log!(
            "done: {} frames, {} urbs, {} underruns, {} short bytes, {} urb errors",
            playback.frames_written(),
            stats.urbs_completed,
            stats.underruns,
            stats.short_bytes,
            stats.urb_errors
        );
        if stats.underruns > 0 || stats.short_bytes > 0 {
            log_err!("the stream had holes — raise the depth");
            return ERR_STREAM_HOLES;
        }
        OK
    })
}

/// `Native.sweep(fd, interface, alt, secondsPerCell)` — **WP7 on Android.**
///
/// Sweeps in-flight depth against packets-per-URB and reports the lowest underrun-free
/// configuration to logcat. This is where the number that decides the product comes from, and it
/// has to be measured on the phone rather than inferred from a desktop.
///
/// # Safety
///
/// `fd` must be a live descriptor from an open `UsbDeviceConnection`.
#[no_mangle]
pub unsafe extern "system" fn Java_io_unom_usbiso_tone_Native_sweep(
    _env: *mut c_void,
    _class: *mut c_void,
    fd: i32,
    interface: i32,
    alt: i32,
    seconds_per_cell: i32,
) -> i32 {
    guard("sweep", || {
        // SAFETY: the caller's contract.
        let dev = unsafe { device(fd) };
        let Ok(blob) = dev.raw_descriptors() else {
            return ERR_DESCRIPTORS;
        };
        let Ok(function) = uac_host::parse(&blob) else {
            return ERR_DESCRIPTORS;
        };
        let Some(stream) = pick(&function, interface, alt) else {
            return ERR_NO_STREAM;
        };
        let rate = stream.rates().max().unwrap_or(48_000).min(48_000);
        let duration = Duration::from_secs(seconds_per_cell.clamp(1, 60) as u64);

        // Android does not hand SCHED_FIFO to ordinary app threads, so the realistic knob is
        // ANDROID_PRIORITY_AUDIO (-16) via setpriority. Whether it takes is itself a finding.
        // SAFETY: `setpriority`/`getpriority` on the calling process; no pointers involved.
        let applied = unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, -16);
            libc::getpriority(libc::PRIO_PROCESS, 0) <= -16
        };
        log!("ANDROID_PRIORITY_AUDIO (-16) applied: {applied}");

        let mut best: Option<(u64, usize, usize)> = None;
        for packets in [1usize, 2, 4, 8] {
            for depth in [2usize, 3, 4, 6, 8, 12, 20, 40, 80] {
                let opts = OpenOptions {
                    depth: Depth::Urbs(depth),
                    packets_per_urb: Some(packets),
                    // Count underruns rather than hiding them behind silence: they are the
                    // measurement.
                    underrun: Underrun::Continue,
                    ..Default::default()
                };
                let mut playback = match stream.open_with(&dev, stream.format(), rate, opts) {
                    Ok(p) => p,
                    Err(e) => {
                        log!("{depth:>3} urbs x {packets} pkt: skipped ({e})");
                        continue;
                    }
                };
                let in_flight = playback.schedule().in_flight_us();
                let urb_bytes = playback.schedule().packet_bytes * packets;
                let silence = vec![stream.format().silence_byte(); urb_bytes];

                let start = Instant::now();
                let mut failed = None;
                while start.elapsed() < duration {
                    if let Err(e) = playback.write(&silence) {
                        failed = Some(e.to_string());
                        break;
                    }
                }
                let stats = playback.stats();
                drop(playback);

                let clean = failed.is_none()
                    && stats.underruns == 0
                    && stats.short_bytes == 0
                    && stats.urb_errors == 0
                    && stats.urbs_completed > 0;
                log!(
                    "{depth:>3} urbs x {packets} pkt = {in_flight:>6} us: under {}, short {}, \
                     err {} -> {}",
                    stats.underruns,
                    stats.short_bytes,
                    stats.urb_errors,
                    if clean { "CLEAN" } else { "dirty" }
                );
                if clean && best.map(|(us, _, _)| in_flight < us).unwrap_or(true) {
                    best = Some((in_flight, depth, packets));
                }
            }
        }

        // SAFETY: `setpriority` on the calling process.
        unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 0) };

        match best {
            Some((us, depth, packets)) => {
                let ms = us as f64 / 1000.0;
                log!("LOWEST CLEAN DEPTH: {us} us ({ms:.1} ms) at {depth} urbs x {packets} packet(s)");
                if ms <= 8.0 {
                    log!(
                        "  inside single-digit milliseconds — usable for haptics as well as audio"
                    );
                } else if ms <= 15.0 {
                    log!(
                        "  marginal for haptics; a loaded or throttled device will likely lose it"
                    );
                } else {
                    log!(
                        "  above the ~15 ms haptics threshold — serves music well, haptics poorly"
                    );
                }
                (us / 1000) as i32
            }
            None => {
                log_err!("no configuration was clean at any depth tried");
                ERR_STREAM_HOLES
            }
        }
    })
}
