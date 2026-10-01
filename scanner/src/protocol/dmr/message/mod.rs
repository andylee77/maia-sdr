//! DMR messages: a port of SDRTrunk's `module/decode/dmr/message` layer and
//! `DMRMessageProcessor`, Tier III first.
//!
//! `DmrMessage` is one decoded message; `Display` prints exactly SDRTrunk's
//! `toString()` and `class_name()` its simple class name, so the output can be
//! diffed line by line with SDRTrunk's on the same capture.
//! `processor::DmrMessageProcessor` turns framer events into messages.

use std::fmt;

pub mod bits;
pub mod crc_mask;
pub mod csbk;
pub mod data;
pub mod lc;
pub mod processor;
pub mod types;
pub mod voice;

use self::crc_mask::DmrCrcMaskManager;
use self::csbk::Csbk;
use self::data::{DataBurst, DataRate};
use self::lc::{FullLc, ShortLc};
use self::voice::Voice;
use super::framer::DmrBurst;
use super::sync::DmrSyncPattern;

/// One DMR message, as SDRTrunk's decoder dispatches it.
#[derive(Debug, Clone)]
pub enum DmrMessage {
    /// Bits that went by without a burst (`SyncLossMessage`).
    SyncLoss {
        timestamp_ms: u64,
        timeslot: u8,
        bits: u32,
    },
    /// Direct mode: the other timeslot is empty (`EmptyTimeslotPlaceholderMessage`).
    EmptyTimeslot {
        timestamp_ms: u64,
        timeslot: u8,
        bits: u32,
    },
    /// `IDLEMessage`.
    Idle(DataBurst),
    /// Single and multi-block CSBKs and MBC headers.
    Csbk(Csbk),
    /// `VoiceHeader` with its full LC.
    VoiceHeader(DataBurst, FullLc),
    /// `Terminator` with its full LC.
    Terminator(DataBurst, FullLc),
    /// `PiHeader` with its encryption parameters.
    PiHeader(DataBurst, FullLc),
    /// Every data header (`DataHeader`; SDRTrunk has a class per packet format).
    DataHeader(DataBurst),
    /// `MBCContinuationBlock`.
    MbcContinuation(DataBurst),
    /// Rate 1/2, 3/4 and 1 data blocks (`DataBlock*`), payload not interpreted.
    DataBlock(DataBurst, DataRate),
    /// `USBData`.
    UsbData(DataBurst),
    /// `UnknownDataMessage`.
    UnknownData(DataBurst),
    /// Voice bursts A-F.
    Voice(Voice),
    /// Full LC assembled from voice bursts B-E (embedded signalling).
    FullLc(FullLc),
    /// Short LC assembled from CACH fragments.
    ShortLc(ShortLc),
    /// A reverse channel or reserved burst (`UnknownDMRMessage`).
    UnknownBurst {
        pattern: DmrSyncPattern,
        timeslot: u8,
        timestamp_ms: u64,
        bits: [u8; 288],
    },
}

impl DmrMessage {
    /// Builds the message for a burst by its sync pattern. Ports `DMRMessageFactory.create()`.
    pub fn from_burst(
        burst: &DmrBurst,
        timestamp_ms: u64,
        masks: &mut DmrCrcMaskManager,
    ) -> Option<DmrMessage> {
        use DmrSyncPattern::*;
        let pattern = burst.pattern;
        let voice = |timeslot| {
            Some(DmrMessage::Voice(Voice::new(
                pattern,
                burst.bits,
                burst.cach,
                timestamp_ms,
                timeslot,
            )))
        };
        let data = |timeslot, masks: &mut DmrCrcMaskManager| {
            Some(data::create(
                pattern,
                &burst.bits,
                burst.cach,
                timestamp_ms,
                timeslot,
                masks,
            ))
        };
        match pattern {
            BaseStationVoice | MobileStationVoice | BsVoiceFrameB | BsVoiceFrameC
            | BsVoiceFrameD | BsVoiceFrameE | BsVoiceFrameF | MsVoiceFrameB | MsVoiceFrameC
            | MsVoiceFrameD | MsVoiceFrameE | MsVoiceFrameF | DirectVoiceFrameB
            | DirectVoiceFrameC | DirectVoiceFrameD | DirectVoiceFrameE | DirectVoiceFrameF => {
                voice(burst.timeslot)
            }
            DirectVoiceTimeslot1 => voice(1),
            DirectVoiceTimeslot2 => voice(2),
            BaseStationData | MobileStationData => data(burst.timeslot, masks),
            DirectDataTimeslot1 => data(1, masks),
            DirectDataTimeslot2 => data(2, masks),
            DirectEmptyTimeslot => Some(DmrMessage::EmptyTimeslot {
                timestamp_ms,
                timeslot: burst.timeslot,
                bits: 288,
            }),
            Unknown => None,
            ReverseChannel | Reserved => Some(DmrMessage::UnknownBurst {
                pattern,
                timeslot: burst.timeslot,
                timestamp_ms,
                bits: burst.bits,
            }),
        }
    }

    /// SDRTrunk's simple class name (`getClass().getSimpleName()`).
    pub fn class_name(&self) -> &'static str {
        match self {
            DmrMessage::SyncLoss { .. } => "SyncLossMessage",
            DmrMessage::EmptyTimeslot { .. } => "EmptyTimeslotPlaceholderMessage",
            DmrMessage::Idle(_) => "IDLEMessage",
            DmrMessage::Csbk(csbk) => csbk.class_name(),
            DmrMessage::VoiceHeader(..) => "VoiceHeader",
            DmrMessage::Terminator(..) => "Terminator",
            DmrMessage::PiHeader(..) => "PiHeader",
            DmrMessage::DataHeader(_) => "DataHeader",
            DmrMessage::MbcContinuation(_) => "MBCContinuationBlock",
            DmrMessage::DataBlock(_, DataRate::Half) => "DataBlock1_2Rate",
            DmrMessage::DataBlock(_, DataRate::ThreeQuarter) => "DataBlock3_4Rate",
            DmrMessage::DataBlock(_, DataRate::Full) => "DataBlock1Rate",
            DmrMessage::UsbData(_) => "USBData",
            DmrMessage::UnknownData(_) => "UnknownDataMessage",
            DmrMessage::Voice(voice) => voice.class_name(),
            DmrMessage::FullLc(lc) => lc.class_name(),
            DmrMessage::ShortLc(slc) => slc.class_name(),
            DmrMessage::UnknownBurst { .. } => "UnknownDMRMessage",
        }
    }

    /// The data burst fields of a data message.
    pub fn data_burst(&self) -> Option<&DataBurst> {
        match self {
            DmrMessage::Idle(d)
            | DmrMessage::VoiceHeader(d, _)
            | DmrMessage::Terminator(d, _)
            | DmrMessage::PiHeader(d, _)
            | DmrMessage::DataHeader(d)
            | DmrMessage::MbcContinuation(d)
            | DmrMessage::DataBlock(d, _)
            | DmrMessage::UsbData(d)
            | DmrMessage::UnknownData(d) => Some(d),
            DmrMessage::Csbk(csbk) => Some(&csbk.burst),
            _ => None,
        }
    }

    /// 1 or 2; 0 when unknown (and for short LC, which is timeslot-agnostic).
    pub fn timeslot(&self) -> u8 {
        match self {
            DmrMessage::SyncLoss { timeslot, .. }
            | DmrMessage::EmptyTimeslot { timeslot, .. }
            | DmrMessage::UnknownBurst { timeslot, .. } => *timeslot,
            DmrMessage::Voice(voice) => voice.timeslot,
            DmrMessage::FullLc(lc) => lc.timeslot,
            DmrMessage::ShortLc(_) => 0,
            other => other.data_burst().map_or(0, |d| d.timeslot),
        }
    }

    /// Milliseconds since the stream started, from the burst's dibit position.
    pub fn timestamp_ms(&self) -> u64 {
        match self {
            DmrMessage::SyncLoss { timestamp_ms, .. }
            | DmrMessage::EmptyTimeslot { timestamp_ms, .. }
            | DmrMessage::UnknownBurst { timestamp_ms, .. } => *timestamp_ms,
            DmrMessage::Voice(voice) => voice.timestamp_ms,
            DmrMessage::FullLc(lc) => lc.timestamp_ms,
            DmrMessage::ShortLc(slc) => slc.timestamp_ms,
            other => other.data_burst().map_or(0, |d| d.timestamp_ms),
        }
    }

    /// SDRTrunk's `isValid()`: error checks passed.
    pub fn is_valid(&self) -> bool {
        match self {
            DmrMessage::SyncLoss { .. }
            | DmrMessage::EmptyTimeslot { .. }
            | DmrMessage::UnknownBurst { .. }
            | DmrMessage::Voice(_)
            | DmrMessage::MbcContinuation(_) => true,
            DmrMessage::FullLc(lc) => lc.valid,
            DmrMessage::ShortLc(slc) => slc.valid,
            other => other.data_burst().map_or(true, |d| d.valid),
        }
    }

    /// A burst on air (SDRTrunk `DMRBurst`), as opposed to sync loss or link control.
    pub fn is_burst(&self) -> bool {
        !matches!(
            self,
            DmrMessage::SyncLoss { .. }
                | DmrMessage::EmptyTimeslot { .. }
                | DmrMessage::FullLc(_)
                | DmrMessage::ShortLc(_)
        )
    }
}

impl fmt::Display for DmrMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DmrMessage::SyncLoss { bits, .. } => {
                write!(f, "<-> SYNC LOSS - BITS PROCESSED [{bits}]")
            }
            DmrMessage::EmptyTimeslot { bits, .. } => {
                write!(f, "EMPTY TIMESLOT - BITS PROCESSED [{bits}]")
            }
            DmrMessage::Idle(d) => data::fmt_idle(d, f),
            DmrMessage::Csbk(csbk) => csbk.fmt(f),
            DmrMessage::VoiceHeader(d, lc) => data::fmt_voice_header(d, lc, f),
            DmrMessage::Terminator(d, lc) => data::fmt_terminator(d, lc, f),
            DmrMessage::PiHeader(d, lc) => data::fmt_pi_header(d, lc, f),
            DmrMessage::DataHeader(d) => data::fmt_data_header(d, f),
            DmrMessage::MbcContinuation(d) => data::fmt_mbc_continuation(d, f),
            DmrMessage::DataBlock(d, rate) => data::fmt_data_block(d, *rate, f),
            DmrMessage::UsbData(d) => data::fmt_usb_data(d, f),
            DmrMessage::UnknownData(d) => data::fmt_unknown_data(d, f),
            DmrMessage::Voice(voice) => voice.fmt(f),
            DmrMessage::FullLc(lc) => lc.fmt(f),
            DmrMessage::ShortLc(slc) => slc.fmt(f),
            DmrMessage::UnknownBurst {
                pattern,
                timeslot,
                bits,
                ..
            } => {
                write!(
                    f,
                    "TS{timeslot} {} UNKNOWN DMR BURST {}",
                    pattern.label(),
                    bits::hex(bits)
                )
            }
        }
    }
}
