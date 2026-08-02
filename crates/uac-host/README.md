# uac-host

**A userspace USB Audio Class *host* driver**: find a device's audio streams, open one, and play PCM
to it — unprivileged, on Linux and Android, with no libusb and no root.

Almost every USB-audio library goes the other way (implementing a *device*, or capturing *from*
one). This renders **to** a USB audio device from userspace, which is the side that has been
missing: the only prior userspace UAC playback host was `libmaru`, LGPL and dead since 2012.

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

Contains **no `unsafe`** — `#![forbid(unsafe_code)]` — because everything it does is parsing and
arithmetic over [`usbfs-iso`](../usbfs-iso), which already got the pointers right.

**Rate is a cadence.** An adaptive endpoint consumes exactly what arrives each service interval, so
the bytes per packet *are* the sample rate. 44.1 kHz on a 1 ms bus is 44.1 frames per packet: a
fixed 44 runs slow forever and a fixed 45 runs fast. This crate paces with a fractional accumulator
and submits an explicit per-packet length plan, so the long-run average is exact at any rate.

**Not handled:** asynchronous endpoints drift. Their feedback endpoint is parsed and reported
(`AudioStream::feedback_endpoint`) but not serviced — closing that loop is the caller's job.
Adaptive and synchronous endpoints need nothing.

UAC1 is complete and always available. UAC2 — descriptor layouts plus clock-entity rate discovery —
is behind the `uac2` feature; a UAC2 device seen without it reports `Error::Uac2NotEnabled` rather
than silently looking like a device with no audio.

MIT or Apache-2.0, at your option.
