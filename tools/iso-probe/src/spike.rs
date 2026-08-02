//! **WP0 — spike zero.** The check that greenlights or kills the whole approach on a given kernel.
//!
//! The precondition nobody has prior art for: can an unprivileged process force-claim a composite
//! device's *audio* interface away from `snd-usb-audio`, submit an isochronous URB, and give it all
//! back — while the device's other functions (a gamepad's HID interface, say) keep working?
//!
//! Everything this can check in software, it checks and prints. The two things it cannot — that
//! the gamepad still reports, and that the device's microphone comes back — it prints as explicit
//! by-eye steps, because the kernel binds an audio function as a *unit* and detaching the playback
//! interface may take the capture side down with it.

use std::time::Duration;

use usbfs_iso::{Claim, Depth, IsoOut, Underrun};

use crate::cli::{self, Args, Result};

pub fn run(args: &Args) -> Result<()> {
    let dev = cli::open(args)?;

    println!("== device ==");
    let desc = dev.device_descriptor()?;
    println!("  {:04x}:{:04x}", desc.vendor_id, desc.product_id);

    // Step 1: the bus speed. Everything downstream is derived from it, and it is the one value
    // §3.2 flags as unconfirmed for the DualSense.
    let speed = dev.speed()?;
    println!("  speed: {speed:?}");
    match speed {
        usbfs_iso::Speed::High => {
            println!("    -> service interval 125 us; bInterval 4 means 1 ms per packet")
        }
        usbfs_iso::Speed::Full => {
            println!("    -> service interval 1 ms; bInterval 4 means 8 ms per packet")
        }
        other => println!("    -> {other:?}: check the schedule the library derives below"),
    }

    let caps = dev.capabilities()?;
    println!(
        "  usbfs caps: 0x{:02x} (reap-after-disconnect: {}, mmap: {})",
        caps.bits(),
        caps.reap_after_disconnect(),
        caps.mmap()
    );
    if !caps.reap_after_disconnect() {
        println!(
            "    ! without REAP_AFTER_DISCONNECT an unplug can strand in-flight URBs; the ring is\n\
             \x20     leaked rather than freed in that case, by design"
        );
    }

    let blob = dev.raw_descriptors()?;
    let function = uac_host::parse(&blob)?;
    let stream = cli::pick_output_stream(&function, args)?;
    let interface = stream.interface();
    let alt = stream.alt_setting();

    println!("\n== target stream ==");
    println!(
        "  interface {interface} alt {alt}: {}ch {} {} on endpoint 0x{:02x} ({:?})",
        stream.channels(),
        stream.format(),
        stream.rates(),
        stream.endpoint().address,
        stream.sync_type()
    );

    // Step 2: who owns what, before we touch anything. This is the baseline the "did we disturb
    // the rest of the device?" question is answered against.
    println!("\n== kernel drivers before ==");
    for i in 0..8u8 {
        match dev.driver(i) {
            Ok(Some(d)) => println!("  interface {i}: {d}"),
            Ok(None) => {}
            Err(_) => break,
        }
    }

    // Step 3: the claim. This is the step that fails on OEM kernels that refuse the detach, and
    // the failure has no app-side fix — the consumer must degrade.
    println!("\n== claim (force) ==");
    let guard = match dev.claim_interface(interface, Claim::Force) {
        Ok(g) => g,
        Err(e) => {
            println!("  FAILED: {e}");
            println!("\nVERDICT: this kernel will not give up interface {interface}.");
            println!("  There is no app-side workaround. A consumer must detect this and degrade.");
            return Err(e.into());
        }
    };
    println!("  ok");
    match guard.displaced_driver() {
        Some(d) => println!("  displaced kernel driver: {d}"),
        None => println!("  no kernel driver was bound (nothing to displace)"),
    }

    println!("\n== kernel drivers while claimed ==");
    for i in 0..8u8 {
        match dev.driver(i) {
            Ok(Some(d)) => println!("  interface {i}: {d}"),
            Ok(None) => println!("  interface {i}: <none>"),
            Err(_) => break,
        }
    }

    // Step 4: the alternate setting, which is what reserves isochronous bandwidth.
    println!("\n== set alternate setting {alt} ==");
    guard.set_alt_setting(alt)?;
    println!("  ok (isochronous bandwidth reserved)");

    // Step 5: one silent URB, submitted and reaped. The single fact that proves the transport.
    println!("\n== one isochronous URB ==");
    let mut out = IsoOut::builder(&dev, stream.endpoint().address)
        .speed(speed)
        .interval(stream.endpoint().interval)
        .max_packet_size(stream.endpoint().bytes_per_interval())
        .depth(Depth::Urbs(2))
        .on_underrun(Underrun::Continue)
        .silence_byte(stream.format().silence_byte())
        .build()?;

    let schedule = *out.schedule();
    println!(
        "  derived schedule: {} urbs x {} packet(s) x {} B, {} us/packet, {} us in flight, {} B \
         charged to the usbfs budget",
        schedule.urbs,
        schedule.packets_per_urb,
        schedule.packet_bytes,
        schedule.packet_interval_us,
        schedule.in_flight_us(),
        schedule.memory_bytes()
    );
    if let Some(budget) = usbfs_iso::usbfs_memory_budget_bytes() {
        println!("  usbfs_memory_mb budget: {} bytes", budget);
    }

    out.start()?;
    let silence = stream.format().silence_byte();
    {
        let mut slot = out
            .next_slot(Duration::from_millis(100))?
            .ok_or("no free slot in a freshly built ring")?;
        let byte = silence;
        slot.bytes_mut().fill(byte);
        slot.commit_full()?;
    }
    println!("  submitted 1 silent URB");

    // Reap it. `next_slot` drains completions as a side effect; when the ring is fully free again
    // the URB has come back.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while out.in_flight() > 0 && std::time::Instant::now() < deadline {
        let _ = out.next_slot(Duration::from_millis(20))?;
    }
    let stats = out.stats();
    println!(
        "  completed: {} of {} urbs, {} of {} bytes, {} short, {} packet errors, {} urb errors",
        stats.urbs_completed,
        stats.urbs_submitted,
        stats.bytes_transferred,
        stats.bytes_submitted,
        stats.short_bytes,
        stats.packet_errors,
        stats.urb_errors
    );

    let transported = stats.urbs_completed > 0 && stats.urb_errors == 0 && stats.short_bytes == 0;
    out.stop()?;
    drop(out);

    // Step 6: hand everything back and confirm the kernel took it.
    println!("\n== release ==");
    guard.release()?;
    println!("  ok");
    println!("\n== kernel drivers after ==");
    for i in 0..8u8 {
        match dev.driver(i) {
            Ok(Some(d)) => println!("  interface {i}: {d}"),
            Ok(None) => println!("  interface {i}: <none>"),
            Err(_) => break,
        }
    }

    println!("\n== VERDICT ==");
    if transported {
        println!("  PASS in software: the interface was force-claimed, one isochronous URB");
        println!("  completed cleanly, and the interface was handed back.");
    } else {
        println!("  FAIL: the URB did not complete cleanly. See the counters above.");
    }
    println!("\n  Two checks this program cannot make for you — do them now, by eye:");
    println!("    1. Is the device's other function still working? (For a gamepad: does it still");
    println!("       report input? `evtest` on desktop, `dumpsys input` on Android.)");
    println!("    2. Did the device's capture side come back after release? The kernel binds an");
    println!("       audio function as ONE ALSA card, so detaching playback can take the");
    println!("       microphone with it. `arecord -l`, or `dumpsys audio` on Android.");
    println!("\n  If either is broken, that is the cost of this route and the consumer has to");
    println!("  decide whether it is worth paying.");

    if !transported {
        return Err("the isochronous URB did not complete cleanly".into());
    }
    Ok(())
}
