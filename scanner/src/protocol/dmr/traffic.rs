//! DMR Tier III traffic channel: the DMR receiver on a lane's IQ, decoding the followed call's
//! timeslot. Voice bursts go out as three AMBE+2 frames; the voice link control (header or
//! embedded) for the call's talkgroup names the talking radio and its encryption; a terminator
//! or a CLEAR ends the transmission.

use std::collections::HashMap;
use std::time::Instant;

use super::demod::DmrDemodulator;
use super::framer::{DmrMessageFramer, FramerEvent};
use super::message::csbk::CsbkKind;
use super::message::lc::{FullLc, FullLcKind};
use super::message::processor::DmrMessageProcessor;
use super::message::types::Address;
use super::message::DmrMessage;
use crate::protocol::events::{LogLine, TrafficEvent, VoiceFrames};

/// The call a lane follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmrCall {
    pub call: u64,
    pub tg: u32,
    /// 1 or 2.
    pub slot: u8,
    pub encrypted: bool,
}

pub struct DmrTraffic {
    demod: DmrDemodulator,
    framer: DmrMessageFramer,
    processor: DmrMessageProcessor,
    lcn_hz: HashMap<u16, u64>,
    call: Option<DmrCall>,
    source: Option<u32>,
    /// Bursts decoded, for the diagnostics pages.
    pub bursts: u64,
}

impl DmrTraffic {
    pub fn new(lcn_hz: HashMap<u16, u64>) -> Self {
        let mut demod = DmrDemodulator::new();
        demod.symbols.set_base_station_mode();
        DmrTraffic {
            demod,
            framer: DmrMessageFramer::default(),
            processor: DmrMessageProcessor::new(lcn_hz.clone()),
            lcn_hz,
            call: None,
            source: None,
            bursts: 0,
        }
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

    /// The lane was retuned: timing and equaliser belong to the old channel.
    pub fn retuned(&mut self) {
        *self = DmrTraffic { call: self.call, source: self.source, bursts: self.bursts, ..DmrTraffic::new(self.lcn_hz.clone()) };
    }

    /// 50 kSPS interleaved IQ received at `now`.
    pub fn push(&mut self, iq: &[i16], now: Instant, out: &mut Vec<TrafficEvent>) {
        self.demod.process_iq_i16(iq, &mut self.framer);
        let events: Vec<FramerEvent> = self.framer.drain().collect();
        for event in events {
            if matches!(event, FramerEvent::Burst(_)) {
                self.bursts += 1;
            }
            for m in self.processor.process(event) {
                self.message(&m, now, out);
            }
        }
    }

    fn message(&mut self, m: &DmrMessage, now: Instant, out: &mut Vec<TrafficEvent>) {
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

    /// Offline: the 20:57 call in unit A's control capture (`DMR_CAPTURE_DIR`): 81921 on TG 87925,
    /// granted LCN 5 TS2, the control repeater itself, so the capture is its traffic channel too.
    #[test]
    fn the_control_repeaters_second_timeslot_carries_a_call() {
        let Ok(dir) = std::env::var("DMR_CAPTURE_DIR") else { return };
        let bytes = std::fs::read(std::path::Path::new(&dir).join("cc_454368750_20260930_205714_60s.wav")).unwrap();
        let iq: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let mut t = DmrTraffic::new(HashMap::from([(5, 454_368_750), (6, 451_087_500)]));
        t.follow(DmrCall { call: 1, tg: 87925, slot: 2, encrypted: false });
        let mut out = Vec::new();
        for chunk in iq.chunks(2 * 1250) {
            t.push(chunk, Instant::now(), &mut out);
        }
        let voice = out.iter().filter(|e| matches!(e, TrafficEvent::Voice { .. })).count();
        let ends = out.iter().filter(|e| matches!(e, TrafficEvent::End { .. })).count();
        let sources: Vec<u32> = out.iter().filter_map(|e| if let TrafficEvent::Source(s) = e { Some(*s) } else { None }).collect();
        eprintln!("{voice} voice bursts, {ends} ends, sources {sources:?}");
        assert!(voice >= 30, "{voice}");
        assert!(ends >= 1);
        assert_eq!(sources.first(), Some(&81921));
    }
}
