//! What a control channel decoder reports, the same for every protocol. Decoders are pure: they
//! return these and never call into trunking or the services.

use std::time::Instant;

use super::p25::pdu::PduFrame;
use super::p25::tsbk::FrequencyBand;

/// A logical channel as the control channel names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelId {
    P25 { iden: u8, number: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalChannel {
    pub id: ChannelId,
    /// The timeslot on a TDMA carrier.
    pub slot: Option<u8>,
    /// Known once the channel plan names the channel's band.
    pub freq_hz: Option<u64>,
    pub tdma: bool,
}

/// A group voice grant, or its refresh while the call goes on (`update`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    pub tg: u32,
    pub source: Option<u32>,
    pub channel: LogicalChannel,
    pub encrypted: bool,
    pub emergency: bool,
    pub update: bool,
}

/// The site's identity as broadcast; fields not heard yet are `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct P25Identity {
    pub nac: Option<u16>,
    pub wacn: Option<u32>,
    pub system: Option<u16>,
    pub rfss: Option<u8>,
    pub site: Option<u8>,
    pub lra: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteIdentity {
    P25(P25Identity),
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

/// Input time of a batch: monotonic for ages, wall clock for records.
#[derive(Debug, Clone, Copy)]
pub struct Now {
    pub mono: Instant,
    pub unix_ms: u64,
}

impl Now {
    pub fn now() -> Self {
        Now { mono: Instant::now(), unix_ms: crate::util::time::unix_ms() }
    }
}
