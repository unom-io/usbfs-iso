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
  device the crates drive end to end. Two kernels measured, and the gap is narrower than it looks:
  - `ubuntu-latest` CI runner (6.17.0-1020-azure): `CONFIG_USB_GADGET` **not set at all**. The whole
    gadget stack is absent from the cloud image, so no amount of privilege helps.
  - A stock Ubuntu desktop kernel (7.0.0-28-generic): `CONFIG_USB_GADGET=m`,
    `CONFIG_USB_CONFIGFS_F_UAC1=y`, with `usb_f_uac1` and `libcomposite` both present — **only
    `CONFIG_USB_DUMMY_HCD` is missing.**

  So on an ordinary distro kernel the shortfall is one config option, not a whole subsystem: build
  `dummy_hcd` out of tree against the running kernel headers, or use a machine with a real UDC.
  `./ci/gadget-rig.sh check` reports exactly which pieces a candidate is missing, without root.
- **Tier 2 — hardware, manual.** `iso-probe spike` and `iso-probe sweep` on a real device.

`./scripts/check.sh` runs everything CI runs — fmt, tests, clippy across all four targets and both
feature sets, and docs — in one command.

## Status

### Verified on hardware

**The approach works.** Measured 2026-08-02 on a **Nothing Phone (3)** (Snapdragon 8s Gen 4,
Android 16 / SDK 36) with a **DualSense** on USB-C, from an ordinary unprivileged app:

- **The force-claim succeeds.** Interface 1 was taken from `snd-usb-audio` and handed back.
- **Isochronous OUT works.** One URB, 392 of 392 bytes, zero short packets, zero packet errors,
  zero URB errors — repeatable without replugging.
- **The gamepad is unaffected.** The pad stayed enumerated as an input device with its motion
  sensors and touchpad, because the detach is per-interface and HID sits on interface 3.
- **The microphone survives.** This was the feared trade-off, and it did not happen:
  `snd-usb-audio` stays bound to interfaces 0 and 2, so the ALSA card and the pad's capture device
  remain registered with the framework. Haptics do not have to cost the pad mic.
- **The bus is high-speed**, which settles the packet arithmetic: `bInterval` 4 means 1 ms per
  packet, so 384 bytes of 4-channel 16-bit 48 kHz audio fit with exactly one sample frame of slack.
- usbfs reports `REAP_AFTER_DISCONNECT` **and** `MMAP`, so the disconnect-stranding hazard is
  mitigated on this kernel and a future zero-copy path is available.

One caveat worth knowing: after release, `USBDEVFS_CONNECT` succeeds but `snd-usb-audio` does not
re-probe interface 1 on its own, so the playback interface stays driver-less until the device is
replugged. Harmless here — the card lives on the other interfaces, and Android denylists that
playback path anyway — but it is not a clean round trip.

### The latency floor, measured

Nobody had published what an isochronous OUT stream can actually achieve on Android. On the same
phone and pad, sweeping in-flight depth against packets-per-URB, two independent runs of 36 cells
each (3 s per cell, `ANDROID_PRIORITY_AUDIO` applied and confirmed):

| in-flight depth | idle 1 | idle 2 | under load |
|---|---|---|---|
| 2 ms — 2 URBs x 1 packet | 11 underruns | 5 underruns, 1 dropped packet | 5 underruns |
| 3 ms — 3 URBs x 1 packet | 1 underrun | clean | clean |
| **4 ms — 4 URBs x 1 packet** | **clean** | **clean** | **clean** |
| 6 ms — 6 URBs x 1 packet | clean | clean | 1 underrun |
| 8 ms and deeper | clean | clean | clean |

"Under load" is eight spinning threads on an 8-core phone with CPU0 at 105 °C reporting severe
thermal throttling — harsher than game streaming, and covering both load conditions at once.

**4 ms is the dependable floor: clean in all three runs, including the loaded one.** 3 ms is
marginal, and 2 ms never survives. That the number barely moves under saturation is the useful part
— the producer thread runs at `ANDROID_PRIORITY_AUDIO` (-16), which the sweep confirms actually
takes effect, and that is evidently enough to keep it scheduled against eight busy cores. That is comfortably inside single-digit milliseconds, so this route is
usable for **haptics**, not only for music. For reference, the C prior art this design took its
cues from runs 80 URBs deep (~80 ms) because that is what music playback needed.

One packet per URB is what buys the low floor: packing more multiplies the latency of a single
completion, so `2 URBs x 4 packets` is 8 ms of audio in flight, not 2 ms. Depth is the knob;
packets-per-URB trades latency for fewer completions to service.

Caveat worth stating, and it is the real one: across the three runs, isolated single-event blips
turn up at depths whose neighbours are clean — dropped packets at three mid-range depths in run 1,
a lone underrun at 6 ms under load. They are **not depth-dependent**; they look like roughly one
transient event per 3-second run wherever it lands. So "clean" here means "clean over 3 seconds",
not a guarantee, and a shipping consumer should watch `IsoStats::short_bytes` and
`IsoStats::underruns` rather than assume a depth is safe because a sweep said so once.

### Still open

- The **tier-1 rig**, which needs a kernel with the gadget stack — no hosted runner has one (above).
- The DualSense fixture is **synthesised**, though every externally-observable value in it has now
  been checked against the real pad. `iso-probe dump` emits a byte-exact replacement.
- The **desktop** path (`iso-probe`) is unexercised on hardware; everything measured so far came
  through the Android app.

## Licence

MIT or Apache-2.0, at your option.
