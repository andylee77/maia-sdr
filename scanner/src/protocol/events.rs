//! What a control channel decoder reports, the same for every protocol. Decoders are pure: they
//! return these and never call into trunking or the services.

use std::time::Instant;

use super::p25::pdu::PduFrame;
use super::p25::tsbk::FrequencyBand;
use super::p25::voice_frame::ImbeFrameRaw;

/// A logical channel as the control channel names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelId {
    P25 { iden: u8, number: u16 },
    /// A DMR Tier III logical channel number.
    DmrLcn(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalChannel {
    pub id: ChannelId,
    /// The timeslot: DMR 1 or 2; P25 on a TDMA carrier.
    pub slot: Option<u8>,
    /// Known once the channel plan names the channel.
    pub freq_hz: Option<u64>,
    /// A P25 Phase 2 (TDMA) channel.
    pub tdma: bool,
}

/// A voice grant, or its refresh while the call goes on (`update`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// The talkgroup; for a private call, the called radio.
    pub tg: u32,
    pub source: Option<u32>,
    /// Unit to unit.
    pub private: bool,
    pub channel: LogicalChannel,
    pub encrypted: bool,
    pub emergency: bool,
    pub update: bool,
}

/// The site's identity as broadcast; fields not heard yet are `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct P25Identity {
    pub nac: Option<u16>,
    pub wacn: Option<u32>,
    pub system: Option<u16>,
    pub rfss: Option<u8>,
    pub site: Option<u8>,
    pub lra: Option<u8>,
}

/// A DMR Tier III site's identity (ALOHA): colour code and system identity code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DmrIdentity {
    pub colour_code: u8,
    /// TINY, SMALL, LARGE or HUGE.
    pub model: &'static str,
    pub network: u32,
    pub site: u32,
}

/// What a DMR Tier III channel says of itself in its CACH (the short LC's system parameters):
/// whose network and site it is, and whether it is a control or a traffic channel. Its colour
/// code once a burst carried one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ChannelIdentity {
    /// TINY, SMALL, LARGE or HUGE.
    pub model: &'static str,
    pub network: u32,
    pub site: u32,
    pub colour_code: Option<u8>,
    pub control: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum SiteIdentity {
    P25(P25Identity),
    Dmr(DmrIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanEntry {
    P25Band(FrequencyBand),
}

/// A neighbour site (P25 adjacent status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Neighbour {
    pub system: u16,
    pub rfss: u8,
    pub site: u8,
    pub lra: u8,
    pub control: LogicalChannel,
    pub service_class: u8,
    pub conventional: bool,
    pub failure: bool,
    pub valid: bool,
    pub active: bool,
}

/// The site time (P25 SYNC_BCST).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiteSync {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hours: u8,
    pub minutes: u8,
    pub microslots: u16,
    pub microslot_locked: bool,
    /// The site's clock is locked to an external reference (GPS).
    pub ext_locked: bool,
    pub local_offset_min: Option<i16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    GroupAffiliation,
    Registration,
    Deregistration,
}

/// One decoded message in SDRTrunk's text form, for the events box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// SDRTrunk's message name (`GRP_VCH_GRANT`, ...).
    pub class: &'static str,
    pub text: String,
    /// Housekeeping broadcast many times a second (identity, channel plan, site time).
    pub routine: bool,
    /// Passed its checks (P25 hands on only messages that did).
    pub valid: bool,
    /// The DMR timeslot.
    pub slot: Option<u8>,
    pub tg: Option<u32>,
    pub unit: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum ControlEvent {
    Grant(Grant),
    /// The identity changed.
    Identity(SiteIdentity),
    /// A channel plan entry was added or changed.
    ChannelPlan(PlanEntry),
    /// A neighbour was heard for the first time or changed.
    Neighbour(Neighbour),
    /// The announced secondary control channels changed.
    SecondaryControl(Vec<LogicalChannel>),
    /// The announced packet data channel changed.
    DataChannel(LogicalChannel),
    SiteTime(SiteSync),
    /// An accepted affiliation or (de)registration.
    Unit { unit: u32, group: Option<u32>, kind: UnitKind },
    Pdu(PduFrame),
    Message(LogLine),
}

/// Voice frames as decoded, before the vocoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceFrames {
    /// P25 Phase 1: the nine IMBE frames of an LDU.
    Imbe([ImbeFrameRaw; 9]),
    /// DMR: the three AMBE+2 frames of a voice burst.
    Ambe2([[u8; 9]; 3]),
}

impl VoiceFrames {
    /// 20 ms frames.
    pub fn len(&self) -> usize {
        match self {
            VoiceFrames::Imbe(_) => 9,
            VoiceFrames::Ambe2(_) => 3,
        }
    }
}

/// What a traffic channel decoder reports about the call it follows.
#[derive(Debug, Clone)]
pub enum TrafficEvent {
    /// P25: the NID of a voice unit (an HDU when `header`, else an LDU) passed, aired at `air`.
    VoiceNid { header: bool, nac: u16, air: Instant },
    /// Voice, aired at `air` (the dibit that completed it).
    Voice { frames: VoiceFrames, encrypted: bool, air: Instant },
    /// The talking radio, from the voice link control (agreed over several frames).
    Source(u32),
    /// A terminator naming the radio that talked.
    TalkComplete(Option<u32>),
    /// The end of a transmission: the first valid terminator after the call's voice.
    End { lc: &'static str, air: Instant },
    /// Packet data (a lane waiting on the data channel).
    Pdu(PduFrame),
    Message(LogLine),
}

