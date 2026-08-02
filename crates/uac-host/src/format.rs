//! Sample formats and rate advertisements.

use std::fmt;

/// A PCM sample format, as a UAC Type I format descriptor describes it.
///
/// UAC1 spells a format as `bSubframeSize` (bytes on the wire per sample) plus `bBitResolution`
/// (meaningful bits within them); UAC2 uses `bSubslotSize` and the same `bBitResolution`. The pair
/// matters: a device can put 24 meaningful bits in a 4-byte subslot, which is a different wire
/// layout from 24 bits in 3 bytes and would silently produce noise if confused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// 8-bit unsigned. Silence is `0x80`, not zero — the one format where zero-filling is a buzz.
    U8,
    /// 16-bit signed little-endian. What the DualSense wants.
    S16Le,
    /// 24-bit signed little-endian packed into 3 bytes.
    S24Le3,
    /// 24-bit signed little-endian in a 4-byte subslot.
    S24Le4,
    /// 32-bit signed little-endian.
    S32Le,
}

impl Format {
    /// Bytes this format occupies on the wire per sample.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Format::U8 => 1,
            Format::S16Le => 2,
            Format::S24Le3 => 3,
            Format::S24Le4 | Format::S32Le => 4,
        }
    }

    /// The byte value that means silence.
    ///
    /// Zero for every signed format; `0x80` for `U8`. Getting this wrong does not fail — it plays
    /// a loud DC offset — which is exactly why the value is looked up rather than assumed.
    pub fn silence_byte(self) -> u8 {
        match self {
            Format::U8 => 0x80,
            _ => 0x00,
        }
    }

    /// Derive a format from a Type I descriptor's subframe/subslot size and bit resolution.
    pub fn from_descriptor(subframe_size: u8, bit_resolution: u8) -> Option<Format> {
        match (subframe_size, bit_resolution) {
            (1, 8) => Some(Format::U8),
            (2, 16) => Some(Format::S16Le),
            (3, 24) => Some(Format::S24Le3),
            (4, 24) => Some(Format::S24Le4),
            (4, 32) => Some(Format::S32Le),
            _ => None,
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Format::U8 => "U8",
            Format::S16Le => "S16_LE",
            Format::S24Le3 => "S24_3LE",
            Format::S24Le4 => "S24_LE",
            Format::S32Le => "S32_LE",
        };
        f.write_str(s)
    }
}

/// What sample rates a stream advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Rates {
    /// An explicit list, straight out of the descriptor.
    Discrete(Vec<u32>),
    /// A continuous range the device will lock to anywhere within.
    Continuous {
        /// Lowest supported rate.
        min: u32,
        /// Highest supported rate.
        max: u32,
    },
    /// UAC2: the rates live in a clock entity and take a control request to read.
    ///
    /// [`Rates::contains`] answers `false` for this variant — it genuinely does not know yet —
    /// so resolve it with `AudioStream::resolve_rates` before filtering on rate.
    Clock {
        /// `bClockID` of the entity to interrogate.
        id: u8,
    },
}

impl Rates {
    /// Whether a rate is known to be supported.
    ///
    /// **`false` for [`Rates::Clock`]**, which means "not known", not "not supported". Answering
    /// optimistically would turn an unresolved clock into a stream that opens and then produces
    /// silence at the wrong pitch, which is far harder to diagnose than a missing match.
    pub fn contains(&self, rate: u32) -> bool {
        match self {
            Rates::Discrete(list) => list.contains(&rate),
            Rates::Continuous { min, max } => rate >= *min && rate <= *max,
            Rates::Clock { .. } => false,
        }
    }

    /// True when the rates are known without asking the device.
    pub fn is_resolved(&self) -> bool {
        !matches!(self, Rates::Clock { .. })
    }

    /// The highest advertised rate, if known.
    pub fn max(&self) -> Option<u32> {
        match self {
            Rates::Discrete(list) => list.iter().copied().max(),
            Rates::Continuous { max, .. } => Some(*max),
            Rates::Clock { .. } => None,
        }
    }
}

impl fmt::Display for Rates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rates::Discrete(list) => {
                let joined: Vec<String> = list.iter().map(|r| r.to_string()).collect();
                write!(f, "{} Hz", joined.join(", "))
            }
            Rates::Continuous { min, max } => write!(f, "{min}..{max} Hz"),
            Rates::Clock { id } => write!(f, "clock entity {id} (unresolved)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_not_zero_for_unsigned_eight_bit() {
        assert_eq!(Format::U8.silence_byte(), 0x80);
        assert_eq!(Format::S16Le.silence_byte(), 0x00);
        assert_eq!(Format::S32Le.silence_byte(), 0x00);
    }

    #[test]
    fn twenty_four_bit_in_three_bytes_is_not_the_same_as_in_four() {
        assert_eq!(Format::from_descriptor(3, 24), Some(Format::S24Le3));
        assert_eq!(Format::from_descriptor(4, 24), Some(Format::S24Le4));
        assert_eq!(Format::S24Le3.bytes_per_sample(), 3);
        assert_eq!(Format::S24Le4.bytes_per_sample(), 4);
    }

    #[test]
    fn nonsense_descriptor_pairs_are_rejected_rather_than_guessed() {
        // More meaningful bits than the subframe can hold.
        assert_eq!(Format::from_descriptor(2, 24), None);
        assert_eq!(Format::from_descriptor(0, 0), None);
    }

    #[test]
    fn an_unresolved_clock_never_claims_to_support_a_rate() {
        let r = Rates::Clock { id: 9 };
        assert!(!r.contains(48_000));
        assert!(!r.is_resolved());
        assert_eq!(r.max(), None);
    }

    #[test]
    fn discrete_and_continuous_rates_match_as_expected() {
        let d = Rates::Discrete(vec![44_100, 48_000]);
        assert!(d.contains(48_000));
        assert!(!d.contains(96_000));
        assert_eq!(d.max(), Some(48_000));

        let c = Rates::Continuous {
            min: 8_000,
            max: 96_000,
        };
        assert!(c.contains(48_000));
        assert!(!c.contains(192_000));
    }
}
