//! Tier-1: drive a **real** USB Audio device end to end, with no hardware.
//!
//! `ci/gadget-rig.sh up` binds the kernel's own `f_uac1` gadget to a `dummy_hcd` virtual UDC, and
//! the host side of the same machine enumerates a genuine USB Audio Class device. These tests
//! claim it away from `snd-usb-audio`, arm its endpoint, and push isochronous audio at it — the
//! whole stack, against a real bus.
//!
//! Ignored by default and gated on `USB_ISO_GADGET=1`, because they need root-created state that
//! an ordinary `cargo test` must not depend on:
//!
//! ```text
//! ./ci/gadget-rig.sh check        # will this kernel do it at all?
//! sudo ./ci/gadget-rig.sh up
//! USB_ISO_GADGET=1 cargo test -p uac-host --test gadget -- --ignored --test-threads=1
//! sudo ./ci/gadget-rig.sh down
//! ```
//!
//! `--test-threads=1` is not optional: there is one gadget, and two tests claiming its interface
//! at once would produce a spurious `EBUSY` that looks like the OEM-kernel failure this project
//! actually cares about detecting.

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::time::Duration;

use uac_host::Format;
use usbfs_iso::{Claim, Depth, Speed, UsbFsDevice};

/// The Linux Foundation gadget VID and the composite PID `ci/gadget-rig.sh` uses.
const GADGET_VID: u16 = 0x1d6b;
const GADGET_PID: u16 = 0x0104;

fn gadget() -> Option<UsbFsDevice> {
    if std::env::var("USB_ISO_GADGET").ok().as_deref() != Some("1") {
        return None;
    }
    let buses = std::fs::read_dir("/dev/bus/usb").ok()?;
    for bus in buses.flatten() {
        let Ok(entries) = std::fs::read_dir(bus.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(dev) = UsbFsDevice::open(entry.path()) else {
                continue;
            };
            let Ok(d) = dev.device_descriptor() else {
                continue;
            };
            if d.vendor_id == GADGET_VID && d.product_id == GADGET_PID {
                return Some(dev);
            }
        }
    }
    panic!(
        "USB_ISO_GADGET=1 but no {GADGET_VID:04x}:{GADGET_PID:04x} device was found. \
         Run `sudo ci/gadget-rig.sh up` first, and check write access to /dev/bus/usb."
    );
}

#[test]
#[ignore = "needs the dummy_hcd + f_uac1 rig; see ci/gadget-rig.sh"]
fn the_gadget_parses_as_a_uac1_device_with_a_playback_stream() {
    let Some(dev) = gadget() else { return };
    let blob = dev.raw_descriptors().expect("descriptors");
    let function = uac_host::parse(&blob).expect("an audio function");

    let out = function
        .output_streams()
        .next()
        .expect("f_uac1 always exposes a playback stream");
    assert!(out.channels() >= 1);
    assert_ne!(out.alt_setting(), 0, "alt 0 carries no endpoint");
    assert!(out.rates().contains(48_000), "rates: {}", out.rates());

    // The property the parser must get right on a *second*, independently built device: the
    // schedule is derived from this bus, not from the one the fixtures came from.
    let speed = dev.speed().expect("speed");
    assert_ne!(speed, Speed::Unknown);
    let interval_us =
        usbfs_iso::packet_interval_us(speed, out.endpoint().interval).expect("interval");
    let needed = out
        .bytes_per_interval(48_000, interval_us)
        .expect("48 kHz must fit the gadget's endpoint");
    assert!(needed <= out.endpoint().bytes_per_interval());
}

#[test]
#[ignore = "needs the dummy_hcd + f_uac1 rig; see ci/gadget-rig.sh"]
fn claiming_the_interface_displaces_snd_usb_audio_and_gives_it_back() {
    let Some(dev) = gadget() else { return };
    let blob = dev.raw_descriptors().expect("descriptors");
    let function = uac_host::parse(&blob).expect("an audio function");
    let out = function.output_streams().next().expect("a playback stream");
    let ifno = out.interface();

    let before = dev.driver(ifno).expect("driver query");
    {
        let guard = dev
            .claim_interface(ifno, Claim::Force)
            .expect("force-claim must work against an in-tree gadget");
        assert_eq!(guard.interface(), ifno);
        if before.is_some() {
            assert_eq!(
                guard.displaced_driver().map(str::to_owned),
                before,
                "the guard must name the driver it displaced so it can restore it"
            );
        }
        guard
            .set_alt_setting(out.alt_setting())
            .expect("alt setting");
    }
    // Dropping the guard must hand the interface back to the kernel driver.
    let after = dev.driver(ifno).expect("driver query");
    assert_eq!(
        after, before,
        "releasing must restore the kernel driver; leaving snd-usb-audio detached costs the \
         user their audio device until replug"
    );
}

#[test]
#[ignore = "needs the dummy_hcd + f_uac1 rig; see ci/gadget-rig.sh"]
fn a_second_of_audio_reaches_the_gadget_without_holes() {
    let Some(dev) = gadget() else { return };
    let blob = dev.raw_descriptors().expect("descriptors");
    let function = uac_host::parse(&blob).expect("an audio function");
    let out = function.output_streams().next().expect("a playback stream");

    let opts = uac_host::OpenOptions {
        depth: Depth::Millis(8),
        // Count underruns instead of papering over them: this test is about whether the transport
        // actually moves data, and silence-filling would hide a stream that never ran.
        underrun: usbfs_iso::Underrun::Continue,
        ..Default::default()
    };
    let mut playback = out
        .open_with(&dev, out.format(), 48_000, opts)
        .expect("open the stream");

    assert_eq!(playback.rate(), 48_000);
    assert!(playback.schedule().in_flight_us() >= 8_000);

    // One second of a quiet square wave. Not silence: an all-zero payload would still "succeed"
    // if the packet lengths were computed as zero, and this test is meant to catch that.
    let frames = 48_000usize;
    let channels = playback.channels() as usize;
    let mut pcm = vec![0i16; frames * channels];
    for (i, frame) in pcm.chunks_mut(channels).enumerate() {
        frame.fill(if (i / 120) % 2 == 0 { 4000 } else { -4000 });
    }
    if playback.format() == Format::S16Le {
        playback.write_interleaved(&pcm).expect("write");
    } else {
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        playback.write_all(&bytes).expect("write");
    }
    playback.drain(Duration::from_secs(2)).expect("drain");

    let stats = playback.stats();
    assert!(stats.urbs_completed > 0, "no URB ever completed");
    assert_eq!(stats.urb_errors, 0, "URB errors: {stats:?}");
    assert_eq!(stats.short_bytes, 0, "the stream had holes: {stats:?}");
    // A full second at 48 kHz must have moved a full second of frames.
    assert_eq!(
        playback.frames_written(),
        frames as u64,
        "frames written disagrees with frames offered"
    );
    // With no short packets and a completed drain, every byte offered must have moved.
    assert_eq!(
        stats.bytes_transferred, stats.bytes_submitted,
        "fewer bytes reached the bus than were offered: {stats:?}"
    );
}

#[test]
#[ignore = "needs the dummy_hcd + f_uac1 rig; see ci/gadget-rig.sh"]
fn a_rate_the_endpoint_cannot_carry_is_refused_before_anything_is_claimed() {
    let Some(dev) = gadget() else { return };
    let blob = dev.raw_descriptors().expect("descriptors");
    let function = uac_host::parse(&blob).expect("an audio function");
    let out = function.output_streams().next().expect("a playback stream");

    // 768 kHz cannot fit any sane wMaxPacketSize. The point is that it fails *before* the
    // interface is claimed, so a bad request never disturbs the kernel driver.
    let before = dev.driver(out.interface()).expect("driver query");
    let err = out
        .open(&dev, out.format(), 768_000)
        .expect_err("an impossible rate must be refused");
    let after = dev.driver(out.interface()).expect("driver query");
    assert_eq!(
        after, before,
        "a refused open must not disturb the kernel driver"
    );
    assert!(
        matches!(
            err,
            uac_host::Error::RateTooFastForEndpoint { .. }
                | uac_host::Error::RateUnsupported { .. }
        ),
        "unexpected error: {err}"
    );
}
