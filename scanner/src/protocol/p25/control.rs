//! P25 control channel: turns the framer's trunking signalling into control events, and keeps
//! what the site has announced (identity, channel plan, neighbours) for the status pages.

use std::collections::BTreeMap;
use std::time::Instant;

use super::c4fm::{C4fmDecoder, DibitSink};
use super::framer::{Framed, Framer, FramerConfig, FramerStats};
use super::pdu::PduFrame;
use super::tsbk::{service_options, FrequencyBand, TsbkMessage};
use super::types::Channel;
use crate::protocol::events::{
    ChannelId, ControlEvent, Grant, LogLine, LogicalChannel, Neighbour, Now, P25Identity, PlanEntry,
    SiteIdentity, SiteSync, UnitKind,
};

/// A neighbour as last announced.
#[derive(Debug, Clone)]
pub struct HeardNeighbour {
    pub neighbour: Neighbour,
    pub last_seen: Instant,
    pub count: u32,
}

/// What the site has announced since the decoder last started on it.
#[derive(Debug, Clone, Default)]
pub struct Announced {
    pub identity: P25Identity,
    pub control_channel: Option<Channel>,
    pub bands: BTreeMap<u8, FrequencyBand>,
    /// Keyed by (system, RFSS, site).
    pub neighbours: BTreeMap<(u16, u8, u8), HeardNeighbour>,
    pub secondary: Vec<Channel>,
    /// Packet data (SNDCP) downlink and uplink.
    pub data_channel: Option<(Channel, Channel)>,
    /// The site announces a TDMA band (IDEN_UP_TDMA), so it carries Phase 2.
    pub has_tdma_band: bool,
    pub last_sync: Option<SiteSync>,
}

impl Announced {
    pub fn frequency(&self, channel: Channel) -> Option<u64> {
        self.bands.get(&channel.identifier()).map(|b| b.channel_frequency(channel.number()))
    }

    pub fn logical(&self, channel: Channel) -> LogicalChannel {
        let band = self.bands.get(&channel.identifier());
        let tdma = band.is_some_and(FrequencyBand::is_tdma);
        LogicalChannel {
            id: ChannelId::P25 { iden: channel.identifier(), number: channel.number() },
            slot: band.filter(|b| b.is_tdma()).map(|b| (channel.number() % u16::from(b.slots)) as u8),
            freq_hz: self.frequency(channel),
            tdma,
        }
    }
}

pub struct P25Control {
    framer: Framer,
    announced: Announced,
    /// Names the decoder in PDU records ("control").
    chain: &'static str,
}

impl P25Control {
    pub fn new(chain: &'static str) -> Self {
        P25Control { framer: Framer::new(FramerConfig::default()), announced: Announced::default(), chain }
    }

    pub fn push(&mut self, dibits: &[u8], now: Now, out: &mut Vec<ControlEvent>) {
        let Self { framer, announced, chain } = self;
        for &d in dibits {
            framer.push(d, &mut |framed| match framed {
                Framed::Nid(nid) => announced.nac(nid.nac, out),
                Framed::Tsbk { index, opcode, message } => announced.tsbk(index, opcode, message, now, out),
                Framed::Pdu { header, blocks, expected } => out.push(ControlEvent::Pdu(PduFrame {
                    chain: *chain,
                    nac: announced.identity.nac.unwrap_or(0),
                    at_ms: now.unix_ms,
                    header,
                    blocks,
                    blocks_expected: expected,
                })),
                _ => {}
            });
        }
    }

    /// Soft frame sync from the C4FM demodulator.
    pub fn sync_detected(&mut self) {
        self.framer.sync_detected();
    }

    pub fn is_assembling(&self) -> bool {
        self.framer.is_assembling()
    }

    /// The channel moved within the system.
    pub fn retuned(&mut self) {
        self.framer.reset();
    }

    /// The channel now carries another site: forget what the old one announced.
    pub fn new_system(&mut self) {
        self.framer.reset();
        self.announced = Announced::default();
    }

    pub fn announced(&self) -> &Announced {
        &self.announced
    }

    pub fn stats(&self) -> &FramerStats {
        &self.framer.stats
    }

    pub fn locked_nac(&self) -> u16 {
        self.framer.locked_nac()
    }

    /// Decode 50 kSPS interleaved IQ through the software C4FM demodulator.
    pub fn push_c4fm(&mut self, demod: &mut C4fmDecoder, iq: &[i16], now: Now, out: &mut Vec<ControlEvent>) {
        demod.process_iq_i16(iq, &mut Fed { control: self, now, out });
    }
}

/// The control decoder as the C4FM demodulator's dibit sink.
struct Fed<'a> {
    control: &'a mut P25Control,
    now: Now,
    out: &'a mut Vec<ControlEvent>,
}

impl DibitSink for Fed<'_> {
    fn push_dibit(&mut self, dibit: u8) {
        self.control.push(&[dibit], self.now, self.out);
    }
    fn sync_detected(&mut self) {
        self.control.sync_detected();
    }
    fn is_assembling(&self) -> bool {
        self.control.is_assembling()
    }
}

/// SDRTrunk names of the housekeeping broadcasts.
const ROUTINE: [&str; 10] = [
    "TDMA_SYNC_BCST",
    "RFSS_STS_BCAST",
    "NET_STS_BCAST",
    "ADJ_STS_BCAST",
    "IDEN_UPDATE",
    "SCCB_EXP",
    "SNDCP_DCH_ANN_EX",
    "SNDCP_DCH_GRANT",
    "SNDCP_DCH_PAG_RQ",
    "MFR_SPECIFIC",
];

impl Announced {
    fn set_identity(&mut self, identity: P25Identity, out: &mut Vec<ControlEvent>) {
        if identity != self.identity {
            self.identity = identity;
            out.push(ControlEvent::Identity(SiteIdentity::P25(identity)));
        }
    }

    fn nac(&mut self, nac: u16, out: &mut Vec<ControlEvent>) {
        self.set_identity(P25Identity { nac: Some(nac), ..self.identity }, out);
    }

    fn grant(&self, channel: Channel, tg: u16, source: Option<u32>, options: Option<u8>, update: bool) -> ControlEvent {
        ControlEvent::Grant(Grant {
            tg: u32::from(tg),
            source,
            channel: self.logical(channel),
            encrypted: options.is_some_and(service_options::is_encrypted),
            emergency: options.is_some_and(service_options::is_emergency),
            update,
        })
    }

    pub(crate) fn tsbk(&mut self, index: u8, opcode: u8, msg: TsbkMessage, now: Now, out: &mut Vec<ControlEvent>) {
        use TsbkMessage as M;
        match &msg {
            M::NetworkStatus { wacn, system_id, channel } => {
                self.control_channel = Some(*channel);
                self.set_identity(P25Identity { wacn: Some(*wacn), system: Some(*system_id), ..self.identity }, out);
            }
            M::RfssStatus { lra, rfss_id, site_id, channel } => {
                self.control_channel = Some(*channel);
                let identity = P25Identity { lra: Some(*lra), rfss: Some(*rfss_id), site: Some(*site_id), ..self.identity };
                self.set_identity(identity, out);
            }
            M::IdentifierUpdate { .. } => {
                if opcode == 0x33 {
                    self.has_tdma_band = true;
                }
                if let Some(band) = FrequencyBand::from_tsbk(&msg) {
                    if self.bands.get(&band.identifier) != Some(&band) {
                        self.bands.insert(band.identifier, band.clone());
                        out.push(ControlEvent::ChannelPlan(PlanEntry::P25Band(band)));
                    }
                }
            }
            M::AdjacentStatus { lra, rfss_id, site_id, channel, system_id, conventional, failure, valid, active, service_class } => {
                let neighbour = Neighbour {
                    system: *system_id,
                    rfss: *rfss_id,
                    site: *site_id,
                    lra: *lra,
                    control: self.logical(*channel),
                    service_class: *service_class,
                    conventional: *conventional,
                    failure: *failure,
                    valid: *valid,
                    active: *active,
                };
                let key = (*system_id, *rfss_id, *site_id);
                let previous = self.neighbours.get(&key);
                if previous.map(|p| p.neighbour) != Some(neighbour) {
                    out.push(ControlEvent::Neighbour(neighbour));
                }
                let count = previous.map_or(0, |p| p.count).saturating_add(1);
                self.neighbours.insert(key, HeardNeighbour { neighbour, last_seen: now.mono, count });
            }
            M::GroupVoiceChannelGrant { channel, talkgroup, source, service_options } => {
                out.push(self.grant(*channel, talkgroup.0, Some(source.0), Some(*service_options), false));
            }
            M::GroupVoiceChannelGrantUpdateExplicit { transmit_channel, talkgroup, service_options, .. } => {
                // It carries the service options, so it counts as a grant, not a refresh.
                out.push(self.grant(*transmit_channel, talkgroup.0, None, Some(*service_options), false));
            }
            M::GroupVoiceChannelGrantUpdate { channel_a, talkgroup_a, channel_b, talkgroup_b } => {
                out.push(self.grant(*channel_a, talkgroup_a.0, None, None, true));
                if talkgroup_b.0 != 0 {
                    out.push(self.grant(*channel_b, talkgroup_b.0, None, None, true));
                }
            }
            M::SecondaryControlChannelBroadcast { rfss_id, site_id, channel_a, channel_b } => {
                let identity = P25Identity { rfss: Some(*rfss_id), site: Some(*site_id), ..self.identity };
                self.set_identity(identity, out);
                let secondary = vec![*channel_a, *channel_b];
                if secondary != self.secondary {
                    out.push(ControlEvent::SecondaryControl(secondary.iter().map(|c| self.logical(*c)).collect()));
                    self.secondary = secondary;
                }
            }
            M::SndcpDataChannelAnnouncementExplicit { downlink_channel, uplink_channel, .. } => {
                let data = Some((*downlink_channel, *uplink_channel));
                if data != self.data_channel {
                    self.data_channel = data;
                    out.push(ControlEvent::DataChannel(self.logical(*downlink_channel)));
                }
            }
            M::TdmaSyncBroadcast { time_locked, year, month, day, hours, minutes, microslots, microslot_locked, local_offset_min } => {
                let sync = SiteSync {
                    year: *year,
                    month: *month,
                    day: *day,
                    hours: *hours,
                    minutes: *minutes,
                    microslots: *microslots,
                    microslot_locked: *microslot_locked,
                    ext_locked: *time_locked,
                    local_offset_min: *local_offset_min,
                };
                self.last_sync = Some(sync);
                out.push(ControlEvent::SiteTime(sync));
            }
            M::GroupAffiliationResponse { response: 0, group, target, .. } if target.0 != 0 => {
                out.push(ControlEvent::Unit { unit: target.0, group: Some(u32::from(group.0)), kind: UnitKind::GroupAffiliation });
            }
            M::UnitRegistrationResponse { response: 0, source_address, .. } if source_address.0 != 0 => {
                out.push(ControlEvent::Unit { unit: source_address.0, group: None, kind: UnitKind::Registration });
            }
            M::UnitDeRegistrationAcknowledge { target, .. } if target.0 != 0 => {
                out.push(ControlEvent::Unit { unit: target.0, group: None, kind: UnitKind::Deregistration });
            }
            _ => {}
        }
        out.push(ControlEvent::Message(self.log_line(index, &msg)));
    }

    /// The message as SDRTrunk's `decoded_messages.log` writes it: `TSBK<n> <NAME> <fields>`.
    fn log_line(&self, index: u8, msg: &TsbkMessage) -> LogLine {
        use TsbkMessage as M;
        let mhz = |c: &Channel| self.frequency(*c).unwrap_or(0) as f64 / 1e6;
        let enc = |o: &u8| if service_options::is_encrypted(*o) { " [ENC]" } else { "" };
        let (class, fields, tg, unit): (&'static str, String, Option<u32>, Option<u32>) = match msg {
            M::GroupVoiceChannelGrant { channel, talkgroup, source, service_options } => (
                "GRP_VCH_GRANT",
                format!("TG:{:05} SRC:{:05} -> {channel} ({:.4} MHz){}", talkgroup.0, source.0, mhz(channel), enc(service_options)),
                Some(u32::from(talkgroup.0)),
                Some(source.0),
            ),
            M::GroupVoiceChannelGrantUpdate { channel_a, talkgroup_a, .. } => (
                "GRP_VCH_GRNT_UPD",
                format!("TG:{:05} -> {channel_a}", talkgroup_a.0),
                Some(u32::from(talkgroup_a.0)),
                None,
            ),
            M::GroupVoiceChannelGrantUpdateExplicit { transmit_channel, talkgroup, service_options, .. } => (
                "GRP_VCH_GRNT_UPD_EXP",
                format!("TG:{:05} -> {transmit_channel} ({:.4} MHz){}", talkgroup.0, mhz(transmit_channel), enc(service_options)),
                Some(u32::from(talkgroup.0)),
                None,
            ),
            M::NetworkStatus { wacn, system_id, channel } => {
                ("NET_STS_BCAST", format!("WACN:{wacn:05X} SYS:{system_id:03X} CH:{channel}"), None, None)
            }
            M::RfssStatus { rfss_id, site_id, .. } => ("RFSS_STS_BCAST", format!("RFSS:{rfss_id:02} SITE:{site_id:02}"), None, None),
            M::IdentifierUpdate { identifier, base_frequency, channel_spacing, .. } => (
                "IDEN_UPDATE",
                format!("Band:{identifier} base:{:.5} MHz spacing:{channel_spacing} Hz", *base_frequency as f64 / 1e6),
                None,
                None,
            ),
            M::AdjacentStatus { system_id, rfss_id, site_id, channel, .. } => {
                ("ADJ_STS_BCAST", format!("SYS:{system_id:03X} RFSS:{rfss_id} SITE:{site_id} CH:{channel}"), None, None)
            }
            M::SecondaryControlChannelBroadcast { channel_a, channel_b, .. } => {
                ("SCCB_EXP", format!("A:{channel_a} B:{channel_b}"), None, None)
            }
            M::SndcpDataChannelAnnouncementExplicit { downlink_channel, uplink_channel, .. } => {
                ("SNDCP_DCH_ANN_EX", format!("DL:{downlink_channel} UL:{uplink_channel}"), None, None)
            }
            M::TdmaSyncBroadcast { year, month, day, hours, minutes, time_locked, .. } => (
                "TDMA_SYNC_BCST",
                format!(
                    "{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02} {}",
                    if *time_locked { "LOCKED" } else { "UNLOCKED" }
                ),
                None,
                None,
            ),
            M::TelephoneInterconnectVoiceChannelGrantUpdate { channel, call_timer_secs, unit_id } => (
                "TEL_INT_V_CH_GRANT_UPD",
                format!("UNIT:{unit_id} CH:{channel} ({:.4} MHz) timer:{call_timer_secs}s", mhz(channel)),
                None,
                Some(unit_id.0),
            ),
            M::UnitToUnitAnswerRequest { target, source } => {
                ("UU_ANS_REQ", format!("TGT:{target} SRC:{source}"), None, Some(source.0))
            }
            M::RadioUnitMonitorCommand { source, target } => {
                ("RAD_MON_CMD", format!("SRC:{source} TGT:{target}"), None, Some(source.0))
            }
            M::SndcpDataChannelGrant { downlink_channel, uplink_channel, target, .. } => (
                "SNDCP_DCH_GRANT",
                format!("DL:{downlink_channel} UL:{uplink_channel} TGT:{target}"),
                None,
                Some(target.0),
            ),
            M::SndcpDataPageRequest { target, source, .. } => {
                ("SNDCP_DCH_PAG_RQ", format!("TGT:{target} SRC:{source}"), None, Some(source.0))
            }
            M::AcknowledgeResponseFne { service_type, source, target, .. } => {
                ("ACK_RESP", format!("SVC:0x{service_type:02X} SRC:{source} TGT:{target}"), None, Some(source.0))
            }
            M::GroupAffiliationResponse { response, group, target, .. } => (
                "GRP_AFF_RSP",
                format!("RSP:{response} TG:{group} TGT:{target}"),
                Some(u32::from(group.0)),
                Some(target.0),
            ),
            M::GroupAffiliationQuery { target, source } => {
                ("GRP_AFF_Q", format!("TGT:{target} SRC:{source}"), None, Some(source.0))
            }
            M::LocationRegistrationResponse { response, group, rfss_id, site_id, target } => (
                "LOC_RG_RSP",
                format!("RSP:{response} TG:{group} RFSS:{rfss_id:02} SITE:{site_id:02} TGT:{target}"),
                Some(u32::from(group.0)),
                Some(target.0),
            ),
            M::UnitRegistrationResponse { response, system_id, source_id, source_address } => (
                "U_REG_RSP",
                format!("RSP:{response} SYS:{system_id:03X} SRC_ID:{source_id} SRC_ADDR:{source_address}"),
                None,
                Some(source_address.0),
            ),
            M::UnitDeRegistrationAcknowledge { wacn, system_id, target } => {
                ("U_DE_REG_ACK", format!("WACN:{wacn:05X} SYS:{system_id:03X} TGT:{target}"), None, Some(target.0))
            }
            M::ManufacturerSpecific { mfid, opcode } => {
                let vendor = match mfid {
                    0x90 => "MOT",
                    0xA4 => "HAR",
                    0x68 => "DVSI",
                    _ => "VEN",
                };
                ("MFR_SPECIFIC", format!("{vendor} MFID:0x{mfid:02X} OP:0x{opcode:02X}"), None, None)
            }
        };
        LogLine {
            class,
            text: format!("TSBK{} {class} {fields}", index + 1),
            routine: ROUTINE.contains(&class),
            tg,
            unit,
        }
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
