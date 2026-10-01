//! DMR Tier III control channel: the software receiver (demodulator, framer, message processor)
//! on the control DDC's IQ, turned into control events.

use std::collections::{BTreeMap, HashMap};

use super::demod::DmrDemodulator;
use super::framer::{DmrMessageFramer, FramerEvent};
use super::message::csbk::CsbkKind;
use super::message::types::Address;
use super::message::DmrMessage;
use crate::protocol::events::{ChannelId, ControlEvent, DmrIdentity, Grant, LogLine, LogicalChannel, SiteIdentity};

/// A control channel's steady broadcasts.
const ROUTINE: [&str; 7] = [
    "Aloha",
    "IDLEMessage",
    "ControlChannelSystemParameters",
    "NullMessage",
    "VoteNowAdvice",
    "CallTimerParameters",
    "Announcement",
];

/// Counters for the diagnostics pages.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DmrStats {
    /// Bursts by timeslot: unknown, 1, 2.
    pub bursts: [u64; 3],
    pub voice_bursts: u64,
    pub cach_ok: u64,
    pub cach_bad: u64,
    pub sync_loss_bits: u64,
    pub msgs_valid: u64,
    pub msgs_invalid: u64,
    /// (valid, invalid) by SDRTrunk class name.
    pub classes: BTreeMap<&'static str, (u64, u64)>,
}

pub struct DmrControl {
    demod: DmrDemodulator,
    framer: DmrMessageFramer,
    processor: super::message::processor::DmrMessageProcessor,
    lcn_hz: HashMap<u16, u64>,
    identity: Option<DmrIdentity>,
    pub stats: DmrStats,
}

/// The voice grant in a control channel message, if it is one.
pub fn voice_grant(message: &DmrMessage) -> Option<Grant> {
    let DmrMessage::Csbk(csbk) = message else { return None };
    if !message.is_valid() {
        return None;
    }
    let private = match csbk.kind {
        CsbkKind::TalkgroupVoiceChannelGrant | CsbkKind::BroadcastTalkgroupVoiceChannelGrant => false,
        CsbkKind::PrivateVoiceChannelGrant | CsbkKind::DuplexPrivateVoiceChannelGrant => true,
        _ => return None,
    };
    let channel = csbk.channel?;
    Some(Grant {
        tg: csbk.destination()?.value(),
        source: csbk.source().map(Address::value),
        private,
        channel: LogicalChannel {
            id: ChannelId::DmrLcn(channel.lcn),
            slot: Some(channel.timeslot),
            freq_hz: channel.downlink_hz,
            tdma: false,
        },
        encrypted: false,
        emergency: false,
        update: false,
    })
}

impl DmrControl {
    /// `lcn_hz`: the site's logical channel numbers and their downlink frequencies.
    pub fn new(lcn_hz: HashMap<u16, u64>) -> Self {
        DmrControl {
            demod: DmrDemodulator::new(),
            framer: DmrMessageFramer::default(),
            processor: super::message::processor::DmrMessageProcessor::new(lcn_hz.clone()),
            lcn_hz,
            identity: None,
            stats: DmrStats::default(),
        }
    }

    /// Decode 50 kSPS interleaved IQ.
    pub fn push(&mut self, iq: &[i16], out: &mut Vec<ControlEvent>) {
        self.demod.process_iq_i16(iq, &mut self.framer);
        let events: Vec<FramerEvent> = self.framer.drain().collect();
        for event in events {
            self.count(&event);
            for message in self.processor.process(event) {
                self.message(&message, out);
            }
        }
    }

    /// The channel moved: timing and equaliser belong to the old one.
    pub fn retuned(&mut self) {
        self.demod = DmrDemodulator::new();
        self.framer = DmrMessageFramer::default();
        self.processor = super::message::processor::DmrMessageProcessor::new(self.lcn_hz.clone());
    }

    /// The channel now carries another site.
    pub fn new_system(&mut self) {
        self.retuned();
        self.identity = None;
    }

    pub fn identity(&self) -> Option<DmrIdentity> {
        self.identity
    }

    pub fn demod_stats(&self) -> super::demod::DmrDemodStats {
        self.demod.symbols.stats
    }

    /// The carrier offset the equaliser has learned, once it has a fine sync. The balance
    /// corrects the phase per symbol: offset = -balance x 4800 / 2 pi.
    pub fn carrier_offset_hz(&self) -> Option<f64> {
        (self.demod.symbols.stats.fine_syncs > 0)
            .then(|| -f64::from(self.demod.symbols.equalizer_balance()) * 4800.0 / std::f64::consts::TAU)
    }

    fn count(&mut self, event: &FramerEvent) {
        match event {
            FramerEvent::Burst(b) => {
                self.stats.bursts[(b.timeslot as usize).min(2)] += 1;
                if b.pattern.is_voice_pattern() {
                    self.stats.voice_bursts += 1;
                }
                if b.pattern.has_cach() {
                    if b.cach.valid {
                        self.stats.cach_ok += 1;
                    } else {
                        self.stats.cach_bad += 1;
                    }
                }
            }
            FramerEvent::SyncLoss { bits, .. } => self.stats.sync_loss_bits += u64::from(*bits),
        }
    }

    fn message(&mut self, message: &DmrMessage, out: &mut Vec<ControlEvent>) {
        let class = message.class_name();
        let valid = message.is_valid();
        let counts = self.stats.classes.entry(class).or_default();
        if valid {
            self.stats.msgs_valid += 1;
            counts.0 += 1;
        } else {
            self.stats.msgs_invalid += 1;
            counts.1 += 1;
        }
        if let DmrMessage::Csbk(c) = message {
            if valid && c.kind == CsbkKind::Aloha {
                let code = c.system_identity_code();
                let identity = DmrIdentity {
                    colour_code: c.burst.color_code,
                    model: code.model_label(),
                    network: code.network,
                    site: code.site,
                };
                if self.identity != Some(identity) {
                    self.identity = Some(identity);
                    out.push(ControlEvent::Identity(SiteIdentity::Dmr(identity)));
                }
            }
        }
        let grant = voice_grant(message);
        if let Some(g) = grant {
            out.push(ControlEvent::Grant(g));
        }
        out.push(ControlEvent::Message(LogLine {
            class,
            text: message.to_string(),
            routine: ROUTINE.contains(&class),
            valid,
            slot: Some(message.timeslot()),
            tg: grant.map(|g| g.tg),
            unit: grant.and_then(|g| g.source),
        }));
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
