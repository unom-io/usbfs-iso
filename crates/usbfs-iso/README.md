# usbfs-iso

**Isochronous USB transfers on Linux and Android, straight to `usbfs`, from an unprivileged
process.** No libusb, no root, one dependency (`libc`).

Isochronous is the transfer type USB reserves bandwidth for and never retries. It is also the one
the Rust ecosystem cannot do: `nusb` has it in no release, `rusb` never wrapped libusb's isochronous
API, and libusb itself carries an open bug against Android isochronous OUT whose reporter's use case
is PCM audio to a USB DAC.

This crate knows about file descriptors, URBs, endpoints, packets, and bus speed. It knows nothing
about audio — that is [`uac-host`](../uac-host) — so a caller driving a UVC output or a custom
isochronous peripheral pays nothing for USB Audio Class machinery.

```rust
use std::time::Duration;
use usbfs_iso::{Claim, Depth, IsoOut, Underrun, UsbFsDevice};

let dev = UsbFsDevice::open("/dev/bus/usb/001/002")?;
// Declared before the stream so it drops *after* it: the interface must outlive the URBs.
let iface = dev.claim_interface(1, Claim::Force)?;
iface.set_alt_setting(1)?;                  // this is what reserves isochronous bandwidth

let mut out = IsoOut::builder(&dev, 0x01)
    .from_descriptors(1, 1)?                // reads wMaxPacketSize and bInterval off the device
    .depth(Depth::Millis(6))
    .on_underrun(Underrun::FillSilence)
    .build()?;

out.start()?;
while let Some(mut slot) = out.next_slot(Duration::from_millis(20))? {
    let n = fill(slot.bytes_mut());          // borrowed straight from the pre-allocated ring
    slot.commit(n)?;                         // submits; no copy, no allocation
}
```

On Android, wrap the descriptor from `UsbDeviceConnection.getFileDescriptor()` with
`UsbFsDevice::from_borrowed_fd` — borrowed, because the Java object still owns it.

Guarantees worth stating: **zero allocation after `start()`** (asserted by a counting allocator);
**MTE-safe completion** (kernel-returned pointers are resolved to slot indices by offset arithmetic
and an integer cookie, never dereferenced or compared for identity); and a `Drop` that discards and
reaps in-flight URBs before releasing memory, leaking rather than freeing when a vanished device
makes that impossible.

Linux and Android only — this is a Linux-kernel-ABI crate. The pure-logic modules (`sys`,
`descriptor`, `fixtures`, and the `Schedule` arithmetic) compile and test on any host.

MIT or Apache-2.0, at your option.
