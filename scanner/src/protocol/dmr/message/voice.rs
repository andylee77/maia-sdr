//! Voice bursts: ports of SDRTrunk `message/voice/VoiceMessage`, `VoiceAMessage`,
//! `VoiceEMBMessage` and the frame F short bursts of `voice/embedded/*`.

use std::fmt;

use super::bits::{field, hex};
use crate::protocol::dmr::fec::bptc_16_2;
use crate::protocol::dmr::fec::cach::Cach;
use crate::protocol::dmr::fec::emb::{self, Emb};
use crate::protocol::dmr::sync::DmrSyncPattern;

/// Embedded signalling of voice burst F (single burst). Ports `voice/embedded/ShortBurst` and subclasses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortBurst {
    /// `NullShortBurst`; `valid` is its CRC-3.
    Null { bits: [u8; 32], valid: bool },
    /// `EmbeddedEncryptionParameters`.
    EncryptionParameters([u8; 32]),
    /// `TransmitInterrupt`.
    TransmitInterrupt([u8; 32]),
    /// `UnknownShortBurst`.
    Unknown([u8; 32]),
    /// `NonStandardShortBurst`: failed BPTC(16,2); the deinterleaved bits.
    NonStandard([u8; 32]),
}

impl ShortBurst {
    /// Ports `VoiceSuperFrameProcessor.extractShortBurst()`.
    pub fn extract(fragment: &[u8]) -> ShortBurst {
        let decoded = match bptc_16_2::decode_short_burst(fragment) {
            Some(decoded) => decoded,
            None => return ShortBurst::NonStandard(bptc_16_2::deinterleave(fragment)),
        };
        match field(&decoded, 8, 11) {
            0 => ShortBurst::Null {
                bits: decoded,
                valid: passes_crc3(&decoded),
            },
            1 | 4 | 5 => ShortBurst::EncryptionParameters(decoded),
            3 => ShortBurst::TransmitInterrupt(decoded),
            _ => ShortBurst::Unknown(decoded),
        }
    }
}

/// CRC-3 (x^3 + x + 1) over the 11 short burst bits. Ports `ShortBurst.passesCRC3()`.
fn passes_crc3(bits: &[u8]) -> bool {
    let mut checksum = field(bits, 0, 11);
    let mut polynomial = 0xB << 7;
    let mut check_bit = 1 << 10;
    for _ in 0..11 {
        if checksum == 0 {
            return true;
        }
        if checksum & check_bit == check_bit {
            checksum ^= polynomial;
        }
        polynomial >>= 1;
        check_bit >>= 1;
    }
    checksum == 0
}

impl fmt::Display for ShortBurst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShortBurst::Null { .. } => f.write_str("NULL SHORT BURST"),
            ShortBurst::EncryptionParameters(bits) => {
                f.write_str("ENCRYPTION ALGORITHM:")?;
                match field(bits, 8, 11) {
                    1 => f.write_str("DMRA RC4/EP")?,
                    4 => f.write_str("DMRA AES128")?,
                    5 => f.write_str("DMRA AES256")?,
                    other => write!(f, "{other}")?,
                }
                write!(f, " KEY:{}", field(bits, 0, 8))
            }
            ShortBurst::TransmitInterrupt(bits) => {
                let delay = field(bits, 3, 8);
                let at = match delay {
                    0 => "ANY TIME".to_string(),
                    2 => "FRAME E".to_string(),
                    4 => "FRAME D".to_string(),
                    6 => "FRAME C".to_string(),
                    8 => "FRAME B".to_string(),
                    _ => format!("UNKNOWN({delay})"),
                };
                write!(f, "TRANSMIT INTERRUPT (TXI) AT {at}")
            }
            ShortBurst::Unknown(bits) => write!(f, "UNKNOWN SHORT BURST:{}", hex(bits)),
            ShortBurst::NonStandard(bits) => write!(f, "NON-STANDARD SHORT BURST:{}", hex(bits)),
        }
    }
}

/// A voice burst: A (sync) or B-F (EMB + embedded signalling).
#[derive(Debug, Clone)]
pub struct Voice {
    pub pattern: DmrSyncPattern,
    pub timeslot: u8,
    pub timestamp_ms: u64,
    pub bits: [u8; 288],
    pub cach: Cach,
    /// The EMB of bursts B-F (`VoiceEMBMessage.getEMB()`), `None` for burst A.
    pub emb: Option<Emb>,
    /// Burst F's single-burst signalling, set by the superframe processor.
    pub embedded: Option<ShortBurst>,
}

/// Burst A patterns (`VoiceAMessage`).
pub fn is_voice_a(pattern: DmrSyncPattern) -> bool {
    matches!(
        pattern,
        DmrSyncPattern::BaseStationVoice
            | DmrSyncPattern::MobileStationVoice
            | DmrSyncPattern::DirectVoiceTimeslot1
            | DmrSyncPattern::DirectVoiceTimeslot2
    )
}

impl Voice {
    /// Ports `DMRMessageFactory.createVoiceMessage()`.
    pub fn new(
        pattern: DmrSyncPattern,
        bits: [u8; 288],
        cach: Cach,
        timestamp_ms: u64,
        timeslot: u8,
    ) -> Self {
        let emb = if is_voice_a(pattern) {
            None
        } else {
            Some(emb::decode_burst(&bits))
        };
        Voice {
            pattern,
            timeslot,
            timestamp_ms,
            bits,
            cach,
            emb,
            embedded: None,
        }
    }

    /// SDRTrunk's simple class name.
    pub fn class_name(&self) -> &'static str {
        if self.emb.is_some() {
            "VoiceEMBMessage"
        } else {
            "VoiceAMessage"
        }
    }

    /// The 32-bit embedded signalling fragment, bits 140..172 (`getFLCFragment()`).
    pub fn flc_fragment(&self) -> &[u8] {
        &self.bits[140..172]
    }

    /// The three 72-bit AMBE+2 frames, 9 bytes each, MSB first: bits 24..96,
    /// 96..132 + 180..216, and 216..288. Ports `VoiceMessage.getAMBEFrames()`
    /// (whose frame 2 byte 4 sign-extends bit 180 into the high nibble).
    pub fn ambe_frames(&self) -> [[u8; 9]; 3] {
        let mut frames = [[0u8; 9]; 3];
        let ranges: [Vec<usize>; 3] = [
            (24..96).collect(),
            (96..132).chain(180..216).collect(),
            (216..288).collect(),
        ];
        for (frame, range) in frames.iter_mut().zip(ranges.iter()) {
            for (i, &bit) in range.iter().enumerate() {
                frame[i / 8] |= self.bits[bit] << (7 - i % 8);
            }
        }
        frames
    }
}

impl fmt::Display for Voice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mobile = self.pattern.is_mobile_station_sync_pattern();
        match &self.emb {
            None => {
                if !mobile {
                    f.write_str("CC:- ")?;
                }
                f.write_str(self.pattern.label())
            }
            Some(emb) => {
                if !mobile && emb.valid {
                    write!(f, "CC:{} ", emb.color_code)?;
                }
                f.write_str(self.pattern.label())?;
                if let Some(burst) = &self.embedded {
                    write!(f, " {burst}")?;
                } else if emb.valid && emb.encrypted {
                    f.write_str(" ENCRYPTED")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
