//! USB Audio Class descriptor parsing — WP5.
//!
//! Two class revisions, deliberately split rather than merged. UAC1 (`bInterfaceProtocol` 0x00)
//! puts everything a stream needs in its own descriptors: channel count, sample size, and the
//! literal list of supported rates. UAC2 (protocol 0x20) moves the rates out into a *clock entity*
//! that has to be interrogated with a control request, and rearranges most of the class-specific
//! layouts. Sharing one code path between them means constantly re-deciding which layout is in
//! play at each field offset; keeping them apart costs a little duplication and makes each one
//! readable against its own spec document.
//!
//! Only the walk and the shape are shared, which is what the design meant by "structured so the
//! parser split is clean from day one".

use usbfs_iso::descriptor::{self, Direction, Endpoint, SyncType, TransferType};

use crate::{Error, Format, Rates, Result};

/// USB descriptor type: class-specific interface.
const CS_INTERFACE: u8 = 0x24;
/// USB descriptor type: class-specific endpoint.
const CS_ENDPOINT: u8 = 0x25;

/// `bInterfaceClass` for audio.
const CLASS_AUDIO: u8 = 0x01;
/// `bInterfaceSubClass` for the control interface.
const SUBCLASS_AUDIOCONTROL: u8 = 0x01;
/// `bInterfaceSubClass` for a streaming interface.
const SUBCLASS_AUDIOSTREAMING: u8 = 0x02;

/// `bInterfaceProtocol` for UAC2. UAC1 uses 0x00.
const PROTOCOL_UAC2: u8 = 0x20;

// Class-specific AC interface descriptor subtypes. Only the UAC2 path reads the AC topology —
// UAC1 keeps everything a stream needs in the streaming interface's own descriptors.
#[cfg(feature = "uac2")]
const AC_INPUT_TERMINAL: u8 = 0x02;
#[cfg(feature = "uac2")]
const AC_OUTPUT_TERMINAL: u8 = 0x03;
#[cfg(feature = "uac2")]
const AC_CLOCK_SOURCE: u8 = 0x0a;

// Class-specific AS interface descriptor subtypes.
const AS_GENERAL: u8 = 0x01;
const AS_FORMAT_TYPE: u8 = 0x02;

/// Which revision of the class an audio function speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UacVersion {
    /// USB Audio Class 1.0.
    Uac1,
    /// USB Audio Class 2.0.
    Uac2,
}

/// One audio function: a control interface plus the streaming interfaces it owns.
#[derive(Debug, Clone)]
pub struct AudioFunction {
    version: UacVersion,
    control_interface: u8,
    streams: Vec<AudioStream>,
}

impl AudioFunction {
    /// Which class revision this function speaks.
    pub fn version(&self) -> UacVersion {
        self.version
    }

    /// The AudioControl interface number — the `wIndex` of every entity control request.
    pub fn control_interface(&self) -> u8 {
        self.control_interface
    }

    /// Every streaming alternate setting, both directions.
    pub fn streams(&self) -> &[AudioStream] {
        &self.streams
    }

    /// Playback streams only: one entry per usable alternate setting.
    ///
    /// Alt 0 is deliberately absent — it carries no endpoints by design and exists only as the
    /// zero-bandwidth parking setting.
    pub fn output_streams(&self) -> impl Iterator<Item = &AudioStream> {
        self.streams
            .iter()
            .filter(|s| s.direction == Direction::Out)
    }

    /// Capture streams only.
    pub fn input_streams(&self) -> impl Iterator<Item = &AudioStream> {
        self.streams.iter().filter(|s| s.direction == Direction::In)
    }
}

/// One alternate setting of one AudioStreaming interface: a single concrete way to move audio.
#[derive(Debug, Clone)]
pub struct AudioStream {
    pub(crate) interface: u8,
    pub(crate) alt_setting: u8,
    pub(crate) direction: Direction,
    pub(crate) channels: u8,
    pub(crate) format: Format,
    pub(crate) rates: Rates,
    pub(crate) endpoint: Endpoint,
    pub(crate) feedback_endpoint: Option<u8>,
    pub(crate) terminal_link: u8,
    pub(crate) control_interface: u8,
    pub(crate) version: UacVersion,
}

impl AudioStream {
    /// The interface number to claim.
    pub fn interface(&self) -> u8 {
        self.interface
    }

    /// The alternate setting to select — the call that reserves isochronous bandwidth.
    pub fn alt_setting(&self) -> u8 {
        self.alt_setting
    }

    /// Direction of travel.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Channel count. A hard gate for some consumers: a DualSense's voice coils are channels 3 and
    /// 4, so a stereo stream into it is silently useless rather than an error.
    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Sample format.
    pub fn format(&self) -> Format {
        self.format
    }

    /// Advertised sample rates. May be [`Rates::Clock`] on UAC2 until resolved.
    pub fn rates(&self) -> &Rates {
        &self.rates
    }

    /// The isochronous data endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Synchronisation type.
    ///
    /// [`SyncType::Adaptive`] is the easy case: the device slaves to whatever rate we feed it, so
    /// there is no feedback endpoint to service and no rate calibration to run.
    pub fn sync_type(&self) -> SyncType {
        self.endpoint.sync_type()
    }

    /// The explicit feedback endpoint an asynchronous stream publishes its true rate on, if any.
    ///
    /// Parsed and reported; **not serviced** by `Playback` yet. An asynchronous device
    /// fed a fixed rate will drift, so treat its presence as a warning that the stream needs rate
    /// adjustment the caller must implement.
    pub fn feedback_endpoint(&self) -> Option<u8> {
        self.feedback_endpoint
    }

    /// `bTerminalLink` — the AC entity this stream connects to.
    pub fn terminal_link(&self) -> u8 {
        self.terminal_link
    }

    /// The AudioControl interface that owns this stream — the `wIndex` of any entity request.
    pub fn control_interface(&self) -> u8 {
        self.control_interface
    }

    /// Which class revision this stream was described with.
    pub fn version(&self) -> UacVersion {
        self.version
    }

    /// Bytes on the wire for one sample frame across all channels.
    pub fn frame_bytes(&self) -> usize {
        self.format.bytes_per_sample() * self.channels as usize
    }

    /// Bytes per service interval needed to sustain `rate`.
    ///
    /// For an adaptive endpoint this **is** the rate control: the device consumes exactly what it
    /// is handed. Returns `None` if the endpoint's own `wMaxPacketSize` cannot carry it.
    pub fn bytes_per_interval(&self, rate: u32, interval_us: u32) -> Option<usize> {
        let frames = (u64::from(rate) * u64::from(interval_us)).div_ceil(1_000_000);
        let bytes = frames as usize * self.frame_bytes();
        (bytes <= self.endpoint.bytes_per_interval()).then_some(bytes)
    }
}

/// Parse the first audio function in a raw descriptor blob.
pub fn parse(blob: &[u8]) -> Result<AudioFunction> {
    parse_all(blob)?
        .into_iter()
        .next()
        .ok_or(Error::NoAudioFunction)
}

/// Parse every audio function in a raw descriptor blob.
pub fn parse_all(blob: &[u8]) -> Result<Vec<AudioFunction>> {
    let mut functions: Vec<AudioFunction> = Vec::new();
    // The AC interface most recently seen, and the terminal-to-clock map built from its
    // class-specific descriptors. Descriptors are positional: everything up to the next interface
    // descriptor belongs to the one before it.
    let mut current: Option<PendingFunction> = None;
    let mut pending_stream: Option<PendingStream> = None;
    let mut saw_uac2_we_skipped = false;

    for d in descriptor::Iter::new(blob) {
        match d.descriptor_type {
            descriptor::DT_INTERFACE => {
                flush_stream(&mut pending_stream, &mut current);
                let Some(iface) = parse_interface(d.bytes) else {
                    continue;
                };
                if iface.class != CLASS_AUDIO {
                    // A non-audio interface ends the current function's descriptor run, but the
                    // function itself stays open: composite devices interleave in practice.
                    continue;
                }
                match iface.subclass {
                    SUBCLASS_AUDIOCONTROL => {
                        if let Some(done) = current.take() {
                            functions.push(done.finish());
                        }
                        let version = if iface.protocol == PROTOCOL_UAC2 {
                            UacVersion::Uac2
                        } else {
                            UacVersion::Uac1
                        };
                        if version == UacVersion::Uac2 && !cfg!(feature = "uac2") {
                            saw_uac2_we_skipped = true;
                            current = None;
                            continue;
                        }
                        current = Some(PendingFunction {
                            version,
                            control_interface: iface.number,
                            terminal_clocks: Vec::new(),
                            streams: Vec::new(),
                        });
                    }
                    SUBCLASS_AUDIOSTREAMING => {
                        // Alt 0 has no endpoints by design; nothing to describe.
                        if iface.alt_setting == 0 {
                            continue;
                        }
                        if let Some(f) = &current {
                            pending_stream = Some(PendingStream::new(
                                iface.number,
                                iface.alt_setting,
                                f.version,
                                f.control_interface,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            CS_INTERFACE => {
                if let Some(s) = pending_stream.as_mut() {
                    s.class_descriptor(d.bytes);
                } else if let Some(f) = current.as_mut() {
                    f.control_descriptor(d.bytes);
                }
            }
            descriptor::DT_ENDPOINT => {
                if let Some(s) = pending_stream.as_mut() {
                    if let Some(ep) = parse_endpoint(d.bytes) {
                        s.endpoint(ep, d.bytes);
                    }
                }
            }
            CS_ENDPOINT => { /* EP_GENERAL: only carries controls we do not use yet. */ }
            _ => {}
        }
    }
    flush_stream(&mut pending_stream, &mut current);
    if let Some(done) = current.take() {
        functions.push(done.finish());
    }

    if functions.is_empty() && saw_uac2_we_skipped {
        return Err(Error::Uac2NotEnabled);
    }
    Ok(functions)
}

fn flush_stream(pending: &mut Option<PendingStream>, function: &mut Option<PendingFunction>) {
    let (Some(s), Some(f)) = (pending.take(), function.as_mut()) else {
        return;
    };
    if let Some(stream) = s.finish(f) {
        f.streams.push(stream);
    }
}

struct InterfaceHeader {
    number: u8,
    alt_setting: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
}

fn parse_interface(bytes: &[u8]) -> Option<InterfaceHeader> {
    if bytes.len() < 9 {
        return None;
    }
    Some(InterfaceHeader {
        number: bytes[2],
        alt_setting: bytes[3],
        class: bytes[5],
        subclass: bytes[6],
        protocol: bytes[7],
    })
}

fn parse_endpoint(bytes: &[u8]) -> Option<Endpoint> {
    if bytes.len() < 7 {
        return None;
    }
    Some(Endpoint {
        address: bytes[2],
        attributes: bytes[3],
        max_packet_size: u16::from_le_bytes([bytes[4], bytes[5]]),
        interval: bytes[6],
    })
}

struct PendingFunction {
    version: UacVersion,
    control_interface: u8,
    /// UAC2 only: `bTerminalID` to `bCSourceID`.
    terminal_clocks: Vec<(u8, u8)>,
    streams: Vec<AudioStream>,
}

impl PendingFunction {
    fn control_descriptor(&mut self, bytes: &[u8]) {
        if bytes.len() < 3 {
            return;
        }
        match self.version {
            UacVersion::Uac1 => { /* Terminals and units carry nothing a stream needs. */ }
            UacVersion::Uac2 => self.control_descriptor_uac2(bytes),
        }
    }

    #[cfg(feature = "uac2")]
    fn control_descriptor_uac2(&mut self, bytes: &[u8]) {
        match bytes[2] {
            // Both terminal kinds name their clock in bCSourceID, but at DIFFERENT offsets:
            // an Input Terminal (UAC2 Table 4-9) has no bSourceID, so bCSourceID sits at 7 right
            // after bAssocTerminal, while an Output Terminal (Table 4-10) carries bSourceID at 7
            // and pushes bCSourceID to 8. Using one offset for both silently reads bNrChannels as
            // a clock id.
            AC_INPUT_TERMINAL if bytes.len() >= 17 => {
                self.terminal_clocks.push((bytes[3], bytes[7]));
            }
            AC_OUTPUT_TERMINAL if bytes.len() >= 12 => {
                self.terminal_clocks.push((bytes[3], bytes[8]));
            }
            AC_CLOCK_SOURCE => { /* Rates come from a control request, not this descriptor. */ }
            _ => {}
        }
    }

    #[cfg(not(feature = "uac2"))]
    fn control_descriptor_uac2(&mut self, _bytes: &[u8]) {
        // Unreachable: UAC2 functions are skipped in `parse_all` when the feature is off.
    }

    fn clock_for_terminal(&self, terminal: u8) -> Option<u8> {
        self.terminal_clocks
            .iter()
            .find(|(id, _)| *id == terminal)
            .map(|(_, clock)| *clock)
    }

    fn finish(self) -> AudioFunction {
        AudioFunction {
            version: self.version,
            control_interface: self.control_interface,
            streams: self.streams,
        }
    }
}

struct PendingStream {
    interface: u8,
    alt_setting: u8,
    version: UacVersion,
    control_interface: u8,
    terminal_link: Option<u8>,
    /// UAC2 carries the channel count in AS_GENERAL; UAC1 in FORMAT_TYPE.
    channels: Option<u8>,
    format: Option<Format>,
    rates: Option<Rates>,
    data_endpoint: Option<Endpoint>,
    feedback_endpoint: Option<u8>,
    /// UAC1's 9-byte endpoint descriptor names its feedback endpoint in `bSynchAddress`.
    synch_address: Option<u8>,
}

impl PendingStream {
    fn new(interface: u8, alt_setting: u8, version: UacVersion, control_interface: u8) -> Self {
        PendingStream {
            interface,
            alt_setting,
            version,
            control_interface,
            terminal_link: None,
            channels: None,
            format: None,
            rates: None,
            data_endpoint: None,
            feedback_endpoint: None,
            synch_address: None,
        }
    }

    fn class_descriptor(&mut self, bytes: &[u8]) {
        if bytes.len() < 3 {
            return;
        }
        match self.version {
            UacVersion::Uac1 => self.class_descriptor_uac1(bytes),
            UacVersion::Uac2 => self.class_descriptor_uac2(bytes),
        }
    }

    /// UAC1 §4.5.2 and §4.5.3: `AS_GENERAL` links the terminal, `FORMAT_TYPE_I` carries channels,
    /// sample size, and either a discrete rate list or a continuous range.
    fn class_descriptor_uac1(&mut self, bytes: &[u8]) {
        match bytes[2] {
            AS_GENERAL if bytes.len() >= 7 => {
                self.terminal_link = Some(bytes[3]);
            }
            AS_FORMAT_TYPE if bytes.len() >= 8 => {
                // bFormatType at [3]; only Type I (PCM-ish) is handled.
                if bytes[3] != 1 {
                    return;
                }
                self.channels = Some(bytes[4]);
                self.format = Format::from_descriptor(bytes[5], bytes[6]);
                let freq_type = bytes[7];
                let tail = &bytes[8..];
                self.rates = Some(if freq_type == 0 {
                    // Continuous: a lower and an upper bound, each a 24-bit little-endian value.
                    if tail.len() < 6 {
                        return;
                    }
                    Rates::Continuous {
                        min: u24(&tail[0..3]),
                        max: u24(&tail[3..6]),
                    }
                } else {
                    let n = freq_type as usize;
                    if tail.len() < n * 3 {
                        return;
                    }
                    Rates::Discrete((0..n).map(|i| u24(&tail[i * 3..i * 3 + 3])).collect())
                });
            }
            _ => {}
        }
    }

    /// UAC2 §4.9.2 and §4.9.3: `AS_GENERAL` gained the channel count and format bitmap, and
    /// `FORMAT_TYPE_I` shrank to subslot size plus bit resolution. Rates moved to the clock.
    #[cfg(feature = "uac2")]
    fn class_descriptor_uac2(&mut self, bytes: &[u8]) {
        match bytes[2] {
            AS_GENERAL if bytes.len() >= 16 => {
                self.terminal_link = Some(bytes[3]);
                self.channels = Some(bytes[10]);
            }
            AS_FORMAT_TYPE if bytes.len() >= 6 => {
                if bytes[3] != 1 {
                    return;
                }
                self.format = Format::from_descriptor(bytes[4], bytes[5]);
            }
            _ => {}
        }
    }

    #[cfg(not(feature = "uac2"))]
    fn class_descriptor_uac2(&mut self, _bytes: &[u8]) {}

    fn endpoint(&mut self, ep: Endpoint, raw: &[u8]) {
        if ep.transfer_type() != TransferType::Isochronous {
            return;
        }
        // An IN endpoint inside an OUT stream's alt setting, marked with usage type "feedback"
        // (bmAttributes bits 5:4 == 01), is the explicit feedback endpoint.
        let usage = (ep.attributes >> 4) & 0x03;
        if usage == 1 {
            self.feedback_endpoint = Some(ep.address);
            return;
        }
        if self.data_endpoint.is_none() {
            // UAC1's audio endpoint descriptor is 9 bytes; byte 8 is bSynchAddress.
            if raw.len() >= 9 && raw[8] != 0 {
                self.synch_address = Some(raw[8]);
            }
            self.data_endpoint = Some(ep);
        }
    }

    fn finish(self, function: &PendingFunction) -> Option<AudioStream> {
        let endpoint = self.data_endpoint?;
        let format = self.format?;
        let channels = self.channels?;
        if channels == 0 {
            return None;
        }
        let terminal_link = self.terminal_link.unwrap_or(0);
        let rates = match self.rates {
            Some(r) => r,
            None => {
                // UAC2: the descriptors do not carry rates, so point at the clock entity the
                // linked terminal names and let the caller resolve it against the device.
                let id = function.clock_for_terminal(terminal_link)?;
                Rates::Clock { id }
            }
        };
        Some(AudioStream {
            interface: self.interface,
            alt_setting: self.alt_setting,
            direction: endpoint.direction(),
            channels,
            format,
            rates,
            endpoint,
            feedback_endpoint: self.feedback_endpoint.or(self.synch_address),
            terminal_link,
            control_interface: self.control_interface,
            version: self.version,
        })
    }
}

/// A 24-bit little-endian sample rate, the form UAC1 stores rates in.
fn u24(b: &[u8]) -> u32 {
    u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use usbfs_iso::fixtures;

    #[test]
    fn dualsense_playback_stream_is_four_channel_sixteen_bit_forty_eight_kilohertz() {
        let f = parse(fixtures::DUALSENSE_DESCRIPTORS).unwrap();
        assert_eq!(f.version(), UacVersion::Uac1);
        assert_eq!(f.control_interface(), 0);

        let out: Vec<_> = f.output_streams().collect();
        assert_eq!(out.len(), 1, "exactly one playback alt setting");
        let s = out[0];
        assert_eq!(s.interface(), 1);
        assert_eq!(s.alt_setting(), 1);
        assert_eq!(s.channels(), 4);
        assert_eq!(s.format(), Format::S16Le);
        assert!(s.rates().contains(48_000));
        assert_eq!(s.endpoint().address, 0x01);
        assert_eq!(s.endpoint().interval, 4);
        assert_eq!(s.endpoint().bytes_per_interval(), 392);
    }

    #[test]
    fn the_dualsense_endpoint_is_adaptive_so_there_is_no_feedback_to_service() {
        let f = parse(fixtures::DUALSENSE_DESCRIPTORS).unwrap();
        let s = f.output_streams().next().unwrap();
        assert_eq!(s.sync_type(), SyncType::Adaptive);
        assert_eq!(s.feedback_endpoint(), None);
    }

    #[test]
    fn one_millisecond_of_dualsense_audio_is_384_bytes_and_fits_the_endpoint() {
        let f = parse(fixtures::DUALSENSE_DESCRIPTORS).unwrap();
        let s = f.output_streams().next().unwrap();
        // 48 frames x 4 channels x 2 bytes. The 392-byte wMaxPacketSize leaves exactly one sample
        // frame of slack, which is the room an adaptive endpoint needs to absorb clock drift.
        assert_eq!(s.frame_bytes(), 8);
        assert_eq!(s.bytes_per_interval(48_000, 1000), Some(384));
        assert_eq!(s.endpoint().bytes_per_interval() - 384, 8);
    }

    #[test]
    fn a_rate_the_endpoint_cannot_carry_is_refused_rather_than_truncated() {
        let f = parse(fixtures::DUALSENSE_DESCRIPTORS).unwrap();
        let s = f.output_streams().next().unwrap();
        // 192 kHz would need 1536 bytes per millisecond; the endpoint reserves 392.
        assert_eq!(s.bytes_per_interval(192_000, 1000), None);
    }

    #[test]
    fn the_capture_stream_is_found_too_and_is_not_mistaken_for_playback() {
        let f = parse(fixtures::DUALSENSE_DESCRIPTORS).unwrap();
        let ins: Vec<_> = f.input_streams().collect();
        assert_eq!(ins.len(), 1);
        assert_eq!(ins[0].interface(), 2);
        assert_eq!(ins[0].channels(), 2);
        assert_eq!(ins[0].direction(), Direction::In);
        // And it must not appear in the playback list.
        assert!(f.output_streams().all(|s| s.interface() != 2));
    }

    #[test]
    fn alt_zero_is_never_offered_as_a_stream() {
        let f = parse(fixtures::DUALSENSE_DESCRIPTORS).unwrap();
        assert!(f.streams().iter().all(|s| s.alt_setting() != 0));
    }

    #[test]
    fn a_truncated_blob_does_not_panic() {
        for cut in 0..fixtures::DUALSENSE_DESCRIPTORS.len() {
            let _ = parse_all(&fixtures::DUALSENSE_DESCRIPTORS[..cut]);
        }
    }

    #[cfg(feature = "uac2")]
    #[test]
    fn uac2_dac_parses_with_rates_deferred_to_its_clock_entity() {
        let f = parse(fixtures::UAC2_DAC_DESCRIPTORS).unwrap();
        assert_eq!(f.version(), UacVersion::Uac2);
        let s = f.output_streams().next().expect("a playback stream");
        assert_eq!(s.channels(), 2);
        assert_eq!(s.format(), Format::S24Le3);
        // The rates are NOT in the descriptors; they live in clock entity 9.
        assert_eq!(s.rates(), &Rates::Clock { id: 9 });
        assert!(!s.rates().is_resolved());
        assert!(
            !s.rates().contains(48_000),
            "an unresolved clock must not claim support"
        );
    }

    #[cfg(feature = "uac2")]
    #[test]
    fn uac2_terminal_clock_offsets_differ_between_input_and_output() {
        // Input Terminal: bCSourceID at 7 (no bSourceID field).
        // Output Terminal: bSourceID at 7, bCSourceID at 8.
        // Reading one offset for both makes an Input Terminal report bNrChannels as its clock.
        let mut f = PendingFunction {
            version: UacVersion::Uac2,
            control_interface: 0,
            terminal_clocks: Vec::new(),
            streams: Vec::new(),
        };
        let input = [
            0x11,
            0x24,
            AC_INPUT_TERMINAL,
            0x01,
            0x01,
            0x01,
            0x00,
            0x09,
            0x02,
            0x03,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];
        let output = [
            0x0c,
            0x24,
            AC_OUTPUT_TERMINAL,
            0x02,
            0x01,
            0x03,
            0x00,
            0x01,
            0x07,
            0x00,
            0x00,
            0x00,
        ];
        f.control_descriptor(&input);
        f.control_descriptor(&output);
        assert_eq!(
            f.clock_for_terminal(1),
            Some(9),
            "input terminal bCSourceID is at byte 7"
        );
        assert_eq!(
            f.clock_for_terminal(2),
            Some(7),
            "output terminal bCSourceID is at byte 8"
        );
    }

    #[cfg(feature = "uac2")]
    #[test]
    fn uac2_async_stream_reports_its_feedback_endpoint() {
        let f = parse(fixtures::UAC2_DAC_DESCRIPTORS).unwrap();
        let s = f.output_streams().next().unwrap();
        assert_eq!(s.sync_type(), SyncType::Asynchronous);
        assert_eq!(
            s.feedback_endpoint(),
            Some(0x81),
            "an async endpoint's feedback must be surfaced, not silently ignored"
        );
    }

    #[cfg(not(feature = "uac2"))]
    #[test]
    fn without_the_feature_a_uac2_device_says_so_instead_of_looking_empty() {
        assert!(matches!(
            parse(fixtures::UAC2_DAC_DESCRIPTORS),
            Err(Error::Uac2NotEnabled)
        ));
    }
}
