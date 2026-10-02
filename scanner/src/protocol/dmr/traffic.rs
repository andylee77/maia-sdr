//! DMR Tier III traffic: a carrier's receiver and the calls followed on its timeslots.
//!
//! `DmrReceiver` is the software receiver on a lane's IQ (demodulator, framer, message
//! processor); it reports the carrier's own identity (its short LC's network and site, its
//! bursts' colour code) with or without a call. `DmrTraffic` follows one call on one timeslot
//! from a carrier's messages: lane one's receiver's, or the control channel's (a control
//! repeater carries calls on its other timeslot). Voice bursts go out as three AMBE+2 frames; the
//! voice link control (header or embedded) for the call's talkgroup names the talking radio and
//! its encryption; a terminator or a CLEAR ends the transmission. A carrier's two timeslots are
//! two calls at once.

use std::collections::HashMap;
use std::time::Instant;

use super::demod::DmrDemodulator;
use super::framer::{DmrMessageFramer, FramerEvent};
use super::message::csbk::CsbkKind;
use super::message::lc::{FullLc, FullLcKind, ShortLcKind};
use super::message::processor::DmrMessageProcessor;
use super::message::types::Address;
use super::message::DmrMessage;
use crate::protocol::events::{ChannelIdentity, LogLine, TrafficEvent, VoiceFrames};

/// The call a lane follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmrCall {
    pub call: u64,
    pub tg: u32,
    /// 1 or 2.
    pub slot: u8,
    pub encrypted: bool,
}

/// A DMR carrier's receiver on a lane's IQ.
pub struct DmrReceiver {
    demod: DmrDemodulator,
    framer: DmrMessageFramer,
    processor: DmrMessageProcessor,
    lcn_hz: HashMap<u16, u64>,
    /// The colour code of the carrier's last valid data burst.
    colour_code: Option<u8>,
    /// The identity last reported.
    identity: Option<ChannelIdentity>,
    /// Bursts decoded, for the diagnostics pages.
    pub bursts: u64,
}

impl DmrReceiver {
    pub fn new(lcn_hz: HashMap<u16, u64>) -> Self {
        let mut demod = DmrDemodulator::new();
        demod.symbols.set_base_station_mode();
        DmrReceiver {
            demod,
            framer: DmrMessageFramer::default(),
            processor: DmrMessageProcessor::new(lcn_hz.clone()),
            lcn_hz,
            colour_code: None,
            identity: None,
            bursts: 0,
        }
    }

    pub fn demod_stats(&self) -> super::demod::DmrDemodStats {
        self.demod.symbols.stats
    }

    /// The lane was retuned: timing, equaliser and identity belong to the old carrier.
    pub fn retuned(&mut self) {
        *self = DmrReceiver { bursts: self.bursts, ..DmrReceiver::new(self.lcn_hz.clone()) };
    }

    /// 50 kSPS interleaved IQ: the messages decoded, and `identity` set when the carrier first
    /// names itself or names something new.
    pub fn push(&mut self, iq: &[i16], messages: &mut Vec<DmrMessage>, identity: &mut Option<ChannelIdentity>) {
        self.demod.process_iq_i16(iq, &mut self.framer);
        let events: Vec<FramerEvent> = self.framer.drain().collect();
        for event in events {
            if matches!(event, FramerEvent::Burst(_)) {
                self.bursts += 1;
            }
            for m in self.processor.process(event) {
                if let Some(id) = self.identify(&m) {
                    *identity = Some(id);
                }
                messages.push(m);
            }
        }
    }

    /// The carrier's network and site, from its short LC; again when its colour code becomes
    /// known.
    fn identify(&mut self, m: &DmrMessage) -> Option<ChannelIdentity> {
        if let Some(d) = m.data_burst().filter(|d| d.valid) {
            self.colour_code = Some(d.color_code);
        }
        let DmrMessage::ShortLc(slc) = m else { return None };
        let code = slc.system_identity_code().filter(|_| slc.valid)?;
        let id = ChannelIdentity {
            model: code.model_label(),
            network: code.network,
            site: code.site,
            colour_code: self.colour_code,
            control: slc.kind == ShortLcKind::ControlChannelSystemParameters,
        };
        (self.identity != Some(id)).then(|| {
            self.identity = Some(id);
            id
        })
    }
}

/// What a call on a carrier needs of its messages: those that carry or end voice.
pub fn carries_calls(m: &DmrMessage) -> bool {
    match m {
        DmrMessage::Voice(_)
        | DmrMessage::VoiceHeader(..)
        | DmrMessage::FullLc(_)
        | DmrMessage::Terminator(..)
        | DmrMessage::PiHeader(..) => true,
        DmrMessage::Csbk(c) => c.kind == CsbkKind::Clear,
        _ => false,
    }
}

/// One call followed on a carrier's timeslot.
#[derive(Default)]
pub struct DmrTraffic {
    call: Option<DmrCall>,
    source: Option<u32>,
}

impl DmrTraffic {
    pub fn new() -> Self {
        DmrTraffic::default()
    }

    pub fn follow(&mut self, call: DmrCall) {
        self.call = Some(call);
        self.source = None;
    }

    pub fn release(&mut self) {
        self.call = None;
    }

    pub fn call(&self) -> Option<DmrCall> {
        self.call
    }

    /// One of the carrier's messages, decoded at `now`: the call's timeslot's voice, talker and
    /// end.
    pub fn message(&mut self, m: &DmrMessage, now: Instant, out: &mut Vec<TrafficEvent>) {
        let Some(call) = self.call else { return };
        if m.timeslot() != call.slot {
            return;
        }
        match m {
            DmrMessage::Voice(v) => {
                out.push(TrafficEvent::Voice { frames: VoiceFrames::Ambe2(v.ambe_frames()), encrypted: call.encrypted, air: now });
            }
            DmrMessage::VoiceHeader(_, lc) | DmrMessage::FullLc(lc) => {
                if self.link_control(lc, call, out) {
                    out.push(log(m));
                }
            }
            DmrMessage::Terminator(_, lc) if lc.valid => {
                out.push(TrafficEvent::End { lc: "call_termination", air: now });
                out.push(log(m));
            }
            DmrMessage::Csbk(c) if m.is_valid() && c.kind == CsbkKind::Clear => {
                out.push(TrafficEvent::End { lc: "network_teardown", air: now });
                out.push(log(m));
            }
            DmrMessage::PiHeader(..) => out.push(log(m)),
            _ => {}
        }
    }

    /// The call's voice link control: its talking radio and encryption. True when it is the call's.
    fn link_control(&mut self, lc: &FullLc, call: DmrCall, out: &mut Vec<TrafficEvent>) -> bool {
        let voice = matches!(lc.kind, FullLcKind::GroupVoiceChannelUser | FullLcKind::UnitToUnitVoiceChannelUser);
        if !lc.valid || !voice || lc.destination().map(Address::value) != Some(call.tg) {
            return false;
        }
        if let Some(source) = lc.source().map(Address::value) {
            if self.source != Some(source) {
                self.source = Some(source);
                out.push(TrafficEvent::Source(source));
            }
        }
        if !call.encrypted && lc.service_options().is_some_and(|o| o.is_encrypted()) {
            if let Some(c) = self.call.as_mut() {
                c.encrypted = true;
            }
        }
        true
    }
}

fn log(m: &DmrMessage) -> TrafficEvent {
    TrafficEvent::Message(LogLine {
        class: m.class_name(),
        text: m.to_string(),
        routine: false,
        valid: m.is_valid(),
        slot: Some(m.timeslot()),
        tg: None,
        unit: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capture of unit A's (`DMR_CAPTURE_DIR`) through a receiver: its messages and the
    /// identities it reported.
    fn receive(file: &str) -> Option<(Vec<DmrMessage>, Vec<ChannelIdentity>)> {
        let dir = std::env::var("DMR_CAPTURE_DIR").ok()?;
        let bytes = std::fs::read(std::path::Path::new(&dir).join(file)).unwrap();
        let iq: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let mut rx = DmrReceiver::new(HashMap::from([(5, 454_368_750), (6, 451_087_500)]));
        let (mut messages, mut ids) = (Vec::new(), Vec::new());
        for chunk in iq.chunks(2 * 1250) {
            let mut id = None;
            rx.push(chunk, &mut messages, &mut id);
            ids.extend(id);
        }
        Some((messages, ids))
    }

    /// Offline: the 20:57 call in unit A's control capture: 81921 on TG 87925, granted LCN 5 TS2,
    /// the control repeater itself, so the capture is its traffic channel too. Only the messages
    /// that carry calls are needed (what the control channel's decoder hands on).
    #[test]
    fn the_control_repeaters_second_timeslot_carries_a_call() {
        let Some((messages, _)) = receive("cc_454368750_20260930_205714_60s.wav") else { return };
        let mut t = DmrTraffic::new();
        t.follow(DmrCall { call: 1, tg: 87925, slot: 2, encrypted: false });
        let mut out = Vec::new();
        for m in messages.iter().filter(|m| carries_calls(m)) {
            t.message(m, Instant::now(), &mut out);
        }
        let voice = out.iter().filter(|e| matches!(e, TrafficEvent::Voice { .. })).count();
        let ends = out.iter().filter(|e| matches!(e, TrafficEvent::End { .. })).count();
        let sources: Vec<u32> = out.iter().filter_map(|e| if let TrafficEvent::Source(s) = e { Some(*s) } else { None }).collect();
        eprintln!("{voice} voice bursts, {ends} ends, sources {sources:?}");
        assert!(voice >= 30, "{voice}");
        assert!(ends >= 1);
        assert_eq!(sources.first(), Some(&81921));
    }

    /// Offline: a receiver on unit A's control capture hears the carrier name itself: Clay
    /// Electric's small network 0, site 2, colour code 0, a control channel; once, not on every
    /// short LC.
    #[test]
    fn a_carrier_names_its_network_and_site() {
        let Some((_, ids)) = receive("cc_454368750_20260930_204706_60s.wav") else { return };
        let clay = ChannelIdentity { model: "SMALL", network: 0, site: 2, colour_code: Some(0), control: true };
        assert_eq!(ids.last(), Some(&clay), "{ids:?}");
        assert!(ids.len() <= 2, "reported again only when the colour code came: {ids:?}");
    }

    /// A call is followed on its own timeslot: the other timeslot's messages are another call's.
    #[test]
    fn each_timeslot_is_its_own_call() {
        let Some((messages, _)) = receive("cc_454368750_20260930_205714_60s.wav") else { return };
        let mut other = DmrTraffic::new();
        other.follow(DmrCall { call: 2, tg: 87925, slot: 1, encrypted: false });
        let mut out = Vec::new();
        for m in &messages {
            other.message(m, Instant::now(), &mut out);
        }
        assert!(!out.iter().any(|e| matches!(e, TrafficEvent::Voice { .. } | TrafficEvent::Source(_))), "TS1 carries the control channel");
    }
}
