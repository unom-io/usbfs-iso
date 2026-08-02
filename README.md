# usb-iso

**Isochronous USB, and USB Audio Class output, in Rust — unprivileged, on Linux and Android.**

Two crates:

| Crate | What it does | `unsafe` |
|---|---|---|
| [`usbfs-iso`](crates/usbfs-iso) | Drive an isochronous endpoint through raw `usbfs`. File descriptors, URBs, packets, bus speed. No audio concepts. | yes, concentrated in one ring |
| [`uac-host`](crates/uac-host) | Parse UAC1/UAC2 descriptors and play PCM to a USB audio device. | `#![forbid(unsafe_code)]` |

Plus [`iso-probe`](tools/iso-probe), a command-line harness, and
[`examples/android-tone`](examples/android-tone), a reference Android app.

## Why this exists

Isochronous is the transfer type USB reserves bandwidth for and never retries — audio, video,
anything where a late packet is worse than a lost one. It is also the transfer type the Rust
ecosystem cannot do:

- **`nusb`** has isochronous in no release. The tracking issue has been open since March 2024, and
  the PR that implements Linux/usbfs isochronous is unmerged — its author stepped away in June 2026
  and explicitly offered the work as a foundation for someone else.
- **`rusb`** never wrapped libusb's isochronous API at all. Using it means hand-built
  `libusb_transfer` structs and callbacks through `unsafe` FFI.
- **libusb itself** carries an open, unfixed bug against **Android isochronous OUT** — the
  reporter's use case is PCM audio to a USB DAC, which is exactly this crate's.

Meanwhile the mechanism is proven in C on exactly the platform that is hardest: raw usbfs
isochronous works on unrooted, stock Android, because Android's SELinux policy grants ordinary apps
`usb_device:chr_file { read write getattr ioctl }` with no `allowxperm` narrowing. What was missing
was a Rust implementation.

The motivating case is narrower and more annoying than "no crate does this". Android **denylists
the DualSense's audio output by VID/PID** in AOSP's `UsbAlsaManager` — the kernel enumerates the
pad's 4-channel playback node and the framework then throws it away. There is no `AudioDeviceInfo`
to target and `/dev/snd` is SELinux-closed to apps, so a device that works fine is unreachable
through every supported API. Driving the endpoint directly is what is left.

## Quick start

```rust
use std::time::Duration;
use uac_host::Format;
use usbfs_iso::UsbFsDevice;

let dev = UsbFsDevice::open("/dev/bus/usb/001/004")?;
let function = uac_host::parse(&dev.raw_descriptors()?)?;

let stream = function
    .output_streams()
    .find(|s| s.channels() == 4 && s.rates().contains(48_000))
    .ok_or("no 4-channel 48 kHz playback stream")?;

let mut playback = stream.open(&dev, Format::S16Le, 48_000)?;
playback.write_interleaved(&pcm)?;
playback.drain(Duration::from_millis(200))?;
```

On Android you cannot open a path, but you do not need to: after
`UsbManager.requestPermission` succeeds, `UsbDeviceConnection.getFileDescriptor()` returns the same
usbfs descriptor. Pass it through JNI and wrap it with `UsbFsDevice::from_borrowed_fd` — *borrowed*,
because the Java object still owns it and will close it itself. See
[`examples/android-tone`](examples/android-tone).

## The three things that decide whether a stream works

1. **Read the bus speed.** `bInterval` is an exponent whose *unit* changes with speed: the same
   descriptor value of 4 means 1 ms per packet on a high-speed bus and 8 ms on a full-speed one.
   Everything is derived from `USBDEVFS_GET_SPEED`; nothing is hardcoded to a device.
2. **Select a non-zero alternate setting.** That call is what reserves isochronous bus bandwidth.
   An audio streaming interface's alt 0 has no endpoints at all, by design.
3. **Keep the pipeline full.** There is no retry. Whatever is not queued in time is a hole in the
   stream, which `IsoStats::short_bytes` and `IsoStats::underruns` measure.

## Design notes worth knowing before you read the code

**The ring is addressed by index, never by pointer identity.** On arm64 Android 14+, Memory Tagging
tags heap pointers in the top byte, and a pointer that goes into the kernel and comes back through
`REAPURB` cannot be assumed to compare equal to the tagged pointer that was allocated. So the
completion path converts the returned address to an offset within one block whose base we own,
derives a slot *index*, re-derives every pointer from our own base, and cross-checks against an
integer cookie that never travelled as a pointer. This is an architectural invariant, not an
optimisation. It also yields the property a real-time path wants anyway: **zero allocation after
`start()`**, asserted by a counting allocator rather than assumed.

**Teardown leaks rather than risking a use-after-free.** In-flight URBs are discarded and reaped
before the ring is released. When that cannot converge — a device unplugged on a kernel without
`USBDEVFS_CAP_REAP_AFTER_DISCONNECT` — the ring is deliberately leaked. Freeing memory a host
controller may still write into would be the worst failure this crate could have.

**Rate is a cadence, not a setting.** An adaptive endpoint has no clock of its own; it consumes
exactly what arrives each service interval, so *the amount of data per packet is the sample rate*.
48 kHz on a 1 ms bus is a clean 48 frames, but 44.1 kHz is 44.1 — a host that sends a fixed 44 runs
0.2% slow forever. `uac-host` paces with a fractional accumulator and submits an explicit per-packet
length plan.

## Testing without the hardware

- **Tier 0 — no kernel.** Descriptor parsing against byte fixtures, packet-count derivation across
  speed × interval, ring index and wraparound arithmetic, underrun accounting, rate pacing. Runs on
  any host including macOS: `cargo test --workspace`.
- **Tier 1 — a real virtual USB device.** `ci/gadget-rig.sh` binds the kernel's `f_uac1` gadget to a
  `dummy_hcd` virtual UDC, and the host side of the same machine enumerates a genuine USB Audio
  device the crates drive end to end. **Whether a given kernel can do this is unverified** — distro
  kernels frequently ship `# CONFIG_USB_DUMMY_HCD is not set`. Run `./ci/gadget-rig.sh check`
  first; it needs no root and changes nothing.
- **Tier 2 — hardware, manual.** `iso-probe spike` and `iso-probe sweep` on a real device.

`./scripts/check.sh` runs everything CI runs — fmt, tests, clippy across all four targets and both
feature sets, and docs — in one command.

## Status

Implemented and green: the transport, the class layer, the harnesses, and the Android reference app
(it builds and packages all three ABIs).

**Not yet verified on hardware**, because that needs a device on a bus:

- The **claim spike** (`iso-probe spike`, or the app's *Spike* button). It answers whether a given
  kernel will hand over an audio interface at all — some OEM Android kernels refuse, and there is no
  app-side fix.
- The **latency floor** (`iso-probe sweep`). Nobody has published what the achievable in-flight
  depth is on Android. If it lands above ~15 ms the route serves music and video output well and
  haptics poorly — which is a real answer, and the harness prints it as one.
- The **tier-1 CI rig**, per the note above.
- The DualSense descriptor fixture is **synthesised** from measured values, not a byte-exact
  capture. `iso-probe dump` emits a replacement in the right form.

## Licence

MIT or Apache-2.0, at your option.
