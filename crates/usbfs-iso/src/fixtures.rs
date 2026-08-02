//! Descriptor blobs for tests, in the exact byte form `read(2)` returns from a usbfs device node.
//!
//! These exist so the parsing half of both crates is testable by contributors who own none of the
//! hardware (design §5, tier 0). They are compiled into the library rather than hidden behind
//! `#[cfg(test)]` on purpose: `uac-host`'s tests need them too, and downstream crates writing
//! their own UAC handling find a known-good blob genuinely useful.
//!
//! # Provenance — read this before trusting a byte
//!
//! [`DUALSENSE_DESCRIPTORS`] is **synthesised, but every externally-observable value in it has now
//! been confirmed against a real pad** — a DualSense on a Nothing Phone (3) (A024, Android 16),
//! read back through `dumpsys usb` on 2026-08-02:
//!
//! | | fixture | measured |
//! |---|---|---|
//! | if0 alt0 | class 1 / subclass 1, no endpoints | same |
//! | if1 alt0 | class 1 / subclass 2, no endpoints | same |
//! | if1 alt1 endpoint 0x01 | isochronous OUT, 392 B, `bInterval` 4 | same |
//! | if2 alt1 endpoint 0x82 | isochronous IN, **196 B**, `bInterval` 4 | same (was 100 — corrected) |
//! | if3 endpoints 0x84 / 0x03 | interrupt, 64 B, `bInterval` 6 | same |
//! | VID:PID | `054c:0ce6` | same |
//!
//! What remains reconstructed is the part `dumpsys` does not expose: the class-specific descriptors
//! — terminal IDs, the feature unit, channel config, the format-type rate table — and the string
//! indices. Those are spec-shaped and self-consistent rather than captured.
//!
//! **To replace it with a byte-exact capture**, run
//! `iso-probe dump --device 054c:0ce6 --out ds5.bin` against a wired pad on desktop Linux and paste
//! the bytes here. Everything the tests assert is an observed value, so a real dump should slot in
//! without changing a single assertion — and if it does change one, that is a finding worth chasing
//! rather than a fixture to paper over.
//!
//! [`UAC2_DAC_DESCRIPTORS`] is likewise a synthesised but spec-faithful two-channel 24-bit UAC2
//! DAC with an asynchronous endpoint and a feedback endpoint — the shape the DualSense does *not*
//! have, included so the UAC2 and async-feedback paths are exercised rather than assumed.

/// A DualSense-shaped composite device: UAC1 audio on interfaces 0..2, HID on interface 3.
///
/// See the module docs for provenance. The values that matter are observed; the topology is a
/// reconstruction.
pub static DUALSENSE_DESCRIPTORS: &[u8] = &[
    // ---- Device descriptor (18) ----
    0x12, 0x01, // bLength, bDescriptorType = DEVICE
    0x00, 0x02, // bcdUSB 2.00
    0x00, 0x00, 0x00, // class/subclass/protocol: defined per interface
    0x40, // bMaxPacketSize0 = 64
    0x4c, 0x05, // idVendor  = 0x054c (Sony)
    0xe6, 0x0c, // idProduct = 0x0ce6 (DualSense)
    0x00, 0x01, // bcdDevice
    0x01, 0x02, 0x00, // iManufacturer, iProduct, iSerialNumber
    0x01, // bNumConfigurations
    // ---- Configuration descriptor (9) ----
    0x09, 0x02, // bLength, bDescriptorType = CONFIGURATION
    0xda, 0x00, // wTotalLength = 218
    0x04, // bNumInterfaces
    0x01, 0x00, // bConfigurationValue, iConfiguration
    0xc0, // bmAttributes: self-powered
    0xfa, // bMaxPower = 500 mA
    // ---- Interface 0: AudioControl (9) ----
    0x09, 0x04, 0x00, 0x00, 0x00, // iface 0, alt 0, 0 endpoints
    0x01, 0x01, 0x00, 0x00, // class AUDIO, subclass AUDIOCONTROL, protocol 0, iInterface
    //      CS_INTERFACE / HEADER (10)
    0x0a, 0x24, 0x01, // bLength, CS_INTERFACE, HEADER
    0x00, 0x01, // bcdADC 1.00
    0x40, 0x00, // wTotalLength of the AC descriptors = 64
    0x02, 0x01, 0x02, // bInCollection = 2, streaming interfaces 1 and 2
    //      CS_INTERFACE / INPUT_TERMINAL - the microphone (12)
    0x0c, 0x24, 0x02, 0x01, // bTerminalID 1
    0x01, 0x02, // wTerminalType 0x0201 = microphone
    0x00, 0x01, // bAssocTerminal, bNrChannels = 1
    0x00, 0x00, 0x00, 0x00, // wChannelConfig, iChannelNames, iTerminal
    //      CS_INTERFACE / OUTPUT_TERMINAL - mic to USB (9)
    0x09, 0x24, 0x03, 0x02, // bTerminalID 2
    0x01, 0x01, // wTerminalType 0x0101 = USB streaming
    0x00, 0x01, 0x00, // bAssocTerminal, bSourceID = 1, iTerminal
    //      CS_INTERFACE / INPUT_TERMINAL - USB to the pad, 4 channels (12)
    0x0c, 0x24, 0x02, 0x03, // bTerminalID 3
    0x01, 0x01, // wTerminalType 0x0101 = USB streaming
    0x00, 0x04, // bAssocTerminal, bNrChannels = 4
    0x33, 0x00, // wChannelConfig = FL|FR|RL|RR
    0x00, 0x00, // iChannelNames, iTerminal
    //      CS_INTERFACE / FEATURE_UNIT (12)
    0x0c, 0x24, 0x06, 0x05, 0x03, // bUnitID 5, bSourceID 3
    0x01, // bControlSize
    0x01, 0x02, 0x02, 0x02, 0x02, // master mute, then per-channel volume
    0x00, // iFeature
    //      CS_INTERFACE / OUTPUT_TERMINAL - the speaker and voice coils (9)
    0x09, 0x24, 0x03, 0x04, // bTerminalID 4
    0x01, 0x03, // wTerminalType 0x0301 = speaker
    0x00, 0x05, 0x00, // bAssocTerminal, bSourceID = 5, iTerminal
    // ---- Interface 1 alt 0: AudioStreaming OUT, zero bandwidth (9) ----
    // No endpoints, by design: this is the setting the interface sits in until SETINTERFACE
    // reserves isochronous bus bandwidth by selecting alt 1.
    0x09, 0x04, 0x01, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00,
    // ---- Interface 1 alt 1: AudioStreaming OUT, operational (9) ----
    0x09, 0x04, 0x01, 0x01, 0x01, 0x01, 0x02, 0x00, 0x00,
    //      CS_INTERFACE / AS_GENERAL (7)
    0x07, 0x24, 0x01, 0x03, // bTerminalLink = 3
    0x01, // bDelay
    0x01, 0x00, // wFormatTag = PCM
    //      CS_INTERFACE / FORMAT_TYPE_I (11)
    0x0b, 0x24, 0x02, 0x01, // bFormatType = I
    0x04, // bNrChannels = 4
    0x02, 0x10, // bSubframeSize = 2, bBitResolution = 16
    0x01, // bSamFreqType = 1 discrete rate
    0x80, 0xbb, 0x00, // 48000 Hz
    //      Endpoint 0x01: isochronous, adaptive, 392 bytes, bInterval 4 (9)
    0x09, 0x05, 0x01, 0x09, // bEndpointAddress OUT ep1, bmAttributes = iso | adaptive
    0x88, 0x01, // wMaxPacketSize = 392
    0x04, // bInterval = 4 -> 2^3 microframes = 1 ms at high speed
    0x00, 0x00, // bRefresh, bSynchAddress
    //      CS_ENDPOINT / EP_GENERAL (7)
    0x07, 0x25, 0x01, 0x01, 0x00, 0x00, 0x00,
    // ---- Interface 2 alt 0: AudioStreaming IN, zero bandwidth (9) ----
    0x09, 0x04, 0x02, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00,
    // ---- Interface 2 alt 1: AudioStreaming IN, operational (9) ----
    0x09, 0x04, 0x02, 0x01, 0x01, 0x01, 0x02, 0x00, 0x00,
    //      CS_INTERFACE / AS_GENERAL (7)
    0x07, 0x24, 0x01, 0x02, 0x01, 0x01, 0x00,
    //      CS_INTERFACE / FORMAT_TYPE_I - 1 channel 16-bit 48 kHz (11)
    0x0b, 0x24, 0x02, 0x01, 0x01, 0x02, 0x10, 0x01, 0x80, 0xbb, 0x00,
    //      Endpoint 0x82: isochronous, asynchronous, 196 bytes (9)
    0x09, 0x05, 0x82, 0x05, 0xc4, 0x00, 0x04, 0x00, 0x00,
    //      CS_ENDPOINT / EP_GENERAL (7)
    0x07, 0x25, 0x01, 0x01, 0x00, 0x00, 0x00,
    // ---- Interface 3: HID - the gamepad, which must keep working (9) ----
    0x09, 0x04, 0x03, 0x00, 0x02, 0x03, 0x00, 0x00, 0x00, //      HID descriptor (9)
    0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x11, 0x01,
    //      Endpoint 0x84 IN, interrupt, bInterval 6 -> 4 ms (7)
    0x07, 0x05, 0x84, 0x03, 0x40, 0x00, 0x06, //      Endpoint 0x03 OUT, interrupt (7)
    0x07, 0x05, 0x03, 0x03, 0x40, 0x00, 0x06,
];

/// A generic UAC2 DAC: 2 channels, 24-bit, asynchronous endpoint plus a feedback endpoint.
///
/// The control case for everything the DualSense does not exercise — UAC2 descriptor layouts, a
/// clock source whose rates come from a control request rather than the descriptor, and an
/// asynchronous endpoint that needs its feedback serviced.
pub static UAC2_DAC_DESCRIPTORS: &[u8] = &[
    // ---- Device descriptor (18) ----
    0x12, 0x01, 0x00, 0x02, // bcdUSB 2.00
    0xef, 0x02, 0x01, // Miscellaneous / common class / interface association
    0x40, // bMaxPacketSize0
    0x34, 0x12, // idVendor  = 0x1234
    0x78, 0x56, // idProduct = 0x5678
    0x00, 0x01, 0x01, 0x02, 0x03, 0x01, // ---- Configuration descriptor (9) ----
    0x09, 0x02, 0x86, 0x00, // wTotalLength = 134
    0x02, 0x01, 0x00, 0xc0, 0x32,
    // ---- Interface association: the audio function (8) ----
    0x08, 0x0b, 0x00, 0x02, 0x01, 0x00, 0x20, 0x00,
    // ---- Interface 0: AudioControl, protocol 0x20 = UAC2 (9) ----
    0x09, 0x04, 0x00, 0x00, 0x00, 0x01, 0x01, 0x20, 0x00,
    //      CS_INTERFACE / HEADER, UAC2 form (9)
    0x09, 0x24, 0x01, // bLength, CS_INTERFACE, HEADER
    0x00, 0x02, // bcdADC 2.00
    0x08, // bCategory = I/O box
    0x2e, 0x00, // wTotalLength = 46
    0x00, // bmControls
    //      CS_INTERFACE / CLOCK_SOURCE (8)
    0x08, 0x24, 0x0a, 0x09, // bClockID = 9
    0x03, // bmAttributes: internal programmable clock
    0x07, // bmControls: sampling frequency readable and writable
    0x00, 0x00, // bAssocTerminal, iClockSource
    //      CS_INTERFACE / INPUT_TERMINAL, UAC2 form (17)
    0x11, 0x24, 0x02, 0x01, // bTerminalID 1
    0x01, 0x01, // wTerminalType 0x0101 = USB streaming
    0x00, // bAssocTerminal
    0x09, // bCSourceID = clock 9
    0x02, // bNrChannels = 2
    0x03, 0x00, 0x00, 0x00, // bmChannelConfig = FL | FR
    0x00, // iChannelNames
    0x00, 0x00, // bmControls
    0x00, // iTerminal
    //      CS_INTERFACE / OUTPUT_TERMINAL, UAC2 form (12)
    0x0c, 0x24, 0x03, 0x02, // bTerminalID 2
    0x01, 0x03, // wTerminalType 0x0301 = speaker
    0x00, 0x01, // bAssocTerminal, bSourceID = 1
    0x09, // bCSourceID = clock 9
    0x00, 0x00, // bmControls
    0x00, // iTerminal
    // ---- Interface 1 alt 0: AudioStreaming, zero bandwidth (9) ----
    0x09, 0x04, 0x01, 0x00, 0x00, 0x01, 0x02, 0x20, 0x00,
    // ---- Interface 1 alt 1: operational (9) ----
    0x09, 0x04, 0x01, 0x01, 0x02, 0x01, 0x02, 0x20, 0x00,
    //      CS_INTERFACE / AS_GENERAL, UAC2 form (16)
    0x10, 0x24, 0x01, 0x01, // bTerminalLink = 1
    0x00, // bmControls
    0x01, // bFormatType = I
    0x01, 0x00, 0x00, 0x00, // bmFormats = PCM
    0x02, // bNrChannels
    0x03, 0x00, 0x00, 0x00, // bmChannelConfig
    0x00, // iChannelNames
    //      CS_INTERFACE / FORMAT_TYPE_I, UAC2 form (6)
    0x06, 0x24, 0x02, 0x01, // bFormatType = I
    0x03, // bSubslotSize = 3 bytes
    0x18, // bBitResolution = 24
    //      Endpoint 0x01 OUT: isochronous, asynchronous, 294 bytes, bInterval 4 (7)
    0x07, 0x05, 0x01, 0x05, 0x26, 0x01, 0x04,
    //      CS_ENDPOINT / EP_GENERAL, UAC2 form (8)
    0x08, 0x25, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
    //      Endpoint 0x81 IN: the explicit feedback endpoint (7)
    0x07, 0x05, 0x81, 0x11, 0x04, 0x00, 0x04,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::Iter;

    /// Both blobs must be self-consistent: every `bLength` lands exactly on the next descriptor,
    /// and the config descriptor's `wTotalLength` accounts for everything after the device
    /// descriptor. A hand-written fixture that fails this is a fixture that would mask a real
    /// parser bug.
    #[test]
    fn fixtures_are_internally_consistent() {
        for (name, blob) in [
            ("dualsense", DUALSENSE_DESCRIPTORS),
            ("uac2 dac", UAC2_DAC_DESCRIPTORS),
        ] {
            let consumed: usize = Iter::new(blob).map(|d| d.bytes.len()).sum();
            assert_eq!(
                consumed,
                blob.len(),
                "{name}: descriptor walk left a remainder"
            );

            let device = Iter::new(blob).next().unwrap();
            assert_eq!(device.bytes.len(), 18, "{name}: device descriptor length");

            let config = Iter::new(blob)
                .find(|d| d.descriptor_type == crate::descriptor::DT_CONFIG)
                .unwrap();
            let total = u16::from_le_bytes([config.bytes[2], config.bytes[3]]) as usize;
            assert_eq!(
                total,
                blob.len() - 18,
                "{name}: wTotalLength disagrees with the blob"
            );
        }
    }
}
