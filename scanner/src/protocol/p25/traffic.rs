//! P25 traffic channel: the framer on a lane's dibits, decoding for the call the lane follows.
//!
//! - Voice goes out nine IMBE frames per LDU, with the call's encryption.
//! - The voice link control (LDU1) names the talking radio once three of the last four agree, and
//!   only a plausible radio (not 0, not a system controller address).
//! - An HDU that says encrypted, with an algorithm the standard knows, marks the call encrypted.
//!   An LDU2 cannot: its FEC accepts near-valid words on weak clear calls.
//! - The first valid TDULC after the call's voice ends the transmission.
//! - A Motorola talk complete names the radio that talked when it is plausible and agrees with
//!   the grant; a call termination from a system controller carries the grant's radio. One per
//!   1.5 s.
//! - Packet data goes out as it is read, call or not (a lane waits on the data channel between
//!   calls).
//! - Each voice unit's NID goes out as it passes, at its air time.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::framer::{Framed, Framer};
use super::tsbk::service_options;
use super::types::DataUnit;
use super::voice_frame::{self, TdulcLcw};
use super::pdu::PduFrame;
use crate::protocol::events::{LogLine, TrafficEvent, VoiceFrames};
use crate::util::time::unix_ms;

const VOTE_OF: usize = 4;
const VOTE_NEEDED: usize = 3;
const TALK_COMPLETE_COOLDOWN: Duration = Duration::from_millis(1_500);

/// The call a lane follows, as the traffic decoder needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallContext {
    pub call: u64,
    pub tg: u32,
    /// The radio the grant named.
    pub source: Option<u32>,
    /// The grant's flag (or the talkgroup is known encrypted).
    pub encrypted: bool,
}

pub struct P25Traffic {
    /// The decoder's name in packet data records.
    chain: &'static str,
    framer: Framer,
    /// The channel's NAC, from its last NID.
    nac: u16,
    call: Option<CallContext>,
    encrypted: bool,
    votes: VecDeque<u32>,
    last_voted: u32,
    /// LDUs of the call, and their count at the last end of transmission.
    ldus: u64,
    ldus_at_end: u64,
    last_talk_complete: Option<Instant>,
}

/// Not zero and not a system controller address.
fn plausible(radio: u32) -> bool {
    radio != 0 && radio < 0xFF_FFFD
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn line(class: &'static str, text: String, routine: bool, tg: Option<u32>, unit: Option<u32>) -> TrafficEvent {
    TrafficEvent::Message(LogLine { class, text, routine, valid: true, slot: None, tg, unit })
}

impl P25Traffic {
    pub fn new(chain: &'static str) -> Self {
        P25Traffic {
            chain,
            framer: Framer::default(),
            nac: 0,
            call: None,
            encrypted: false,
            votes: VecDeque::with_capacity(VOTE_OF),
            last_voted: 0,
            ldus: 0,
            ldus_at_end: 0,
            last_talk_complete: None,
        }
    }

    /// The lane follows a new call.
    pub fn follow(&mut self, call: CallContext) {
        self.call = Some(call);
        self.encrypted = call.encrypted;
        self.votes.clear();
        self.last_voted = 0;
        self.ldus = 0;
        self.ldus_at_end = 0;
    }

    /// A repeat of the grant named the radio.
    pub fn set_source(&mut self, source: u32) {
        if let Some(c) = self.call.as_mut() {
            c.source = Some(source);
        }
    }

    /// The lane's call is over.
    pub fn release(&mut self) {
        self.call = None;
    }

    /// The lane was retuned: the frame in progress and the NAC lock belong to the old channel.
    pub fn retuned(&mut self) {
        self.framer.reset();
    }

    /// Soft frame sync from a software demodulator.
    pub fn sync_detected(&mut self) {
        self.framer.sync_detected();
    }

    pub fn is_assembling(&self) -> bool {
        self.framer.is_assembling()
    }

    pub fn call(&self) -> Option<CallContext> {
        self.call
    }

    pub fn stats(&self) -> &super::framer::FramerStats {
        &self.framer.stats
    }

    /// One dibit, aired at `air`.
    pub fn push(&mut self, dibit: u8, air: Instant, now: Instant, out: &mut Vec<TrafficEvent>) {
        let mut units = Vec::new();
        let mut pdus = Vec::new();
        let mut nac = None;
        let mut voice_nid = None;
        self.framer.push(dibit, &mut |f| match f {
            Framed::Nid(n) => {
                nac = Some(n.nac);
                if matches!(n.duid, DataUnit::Hdu | DataUnit::Ldu1 | DataUnit::Ldu2) {
                    voice_nid = Some(n.duid == DataUnit::Hdu);
                }
            }
            Framed::Pdu { header, blocks, expected } => pdus.push((header, blocks, expected)),
            Framed::Hdu(b) => units.push(Unit::Hdu(voice_frame::parse_hdu_body(b))),
            Framed::Ldu1(b) => {
                if let Some(frames) = voice_frame::extract_imbe_frames(b) {
                    units.push(Unit::Ldu1(frames, voice_frame::parse_ldu1_source(b), voice_frame::parse_ldu1_lcw(b)));
                }
            }
            Framed::Ldu2(b) => {
                if let Some(frames) = voice_frame::extract_imbe_frames(b) {
                    units.push(Unit::Ldu2(frames, voice_frame::parse_ldu2_ess(b)));
                }
            }
            Framed::TduLc(b) => units.push(Unit::TduLc(voice_frame::parse_tdulc_lcw_checked(b))),
            _ => {}
        });
        if let Some(n) = nac {
            self.nac = n;
        }
        if let Some(header) = voice_nid {
            out.push(TrafficEvent::VoiceNid { header, nac: self.nac, air });
        }
        for (header, blocks, blocks_expected) in pdus {
            out.push(TrafficEvent::Pdu(PduFrame { chain: self.chain, nac: self.nac, at_ms: unix_ms(), header, blocks, blocks_expected }));
        }
        for u in units {
            self.unit(u, air, now, out);
        }
    }

    fn unit(&mut self, u: Unit, air: Instant, now: Instant, out: &mut Vec<TrafficEvent>) {
        let tg = self.call.map(|c| c.tg);
        match u {
            Unit::Hdu(header) => {
                let Some(h) = header else { return };
                if h.is_encrypted() && h.is_spec_algorithm() {
                    self.encrypted = true;
                }
                let text = if h.is_encrypted() {
                    format!("HDU TALKGROUP:{} ENCRYPTION:0x{:02X} KEY:{} MI:{}", h.talkgroup, h.algorithm_id, h.key_id, hex(&h.message_indicator))
                } else {
                    format!("HDU TALKGROUP:{} UNENCRYPTED", h.talkgroup)
                };
                out.push(line("HDU", text, false, Some(u32::from(h.talkgroup)), None));
            }
            Unit::Ldu1(frames, source, lcw) => {
                self.ldus += 1;
                out.push(TrafficEvent::Voice { frames: VoiceFrames::Imbe(frames), encrypted: self.encrypted, air });
                let (Some(tg), Some(source)) = (tg, source) else { return };
                let options = match lcw {
                    Some(TdulcLcw::GroupVoiceChannelUser { service_options, .. }) => service_options,
                    _ => 0,
                };
                out.push(line(
                    "LDU1",
                    format!("LDU1 GROUP VOICE CHANNEL USER TG={tg} SRC={source} OPTS:{}", service_options::render(options)),
                    true,
                    Some(tg),
                    Some(source),
                ));
                if plausible(source) {
                    if let Some(voted) = self.vote(source) {
                        out.push(TrafficEvent::Source(voted));
                    }
                }
            }
            Unit::Ldu2(frames, ess) => {
                self.ldus += 1;
                out.push(TrafficEvent::Voice { frames: VoiceFrames::Imbe(frames), encrypted: self.encrypted, air });
                let Some(ess) = ess else { return };
                let text = if !ess.is_encrypted() {
                    "LDU2 VOICE UNENCRYPTED".to_string()
                } else if ess.is_spec_algorithm() && self.encrypted {
                    format!("LDU2 VOICE ENCRYPTED ENCRYPTION:0x{:02X} KEY:{} MI:{}", ess.algorithm_id, ess.key_id, hex(&ess.message_indicator))
                } else {
                    return;
                };
                out.push(line("LDU2", text, true, tg, None));
            }
            Unit::TduLc(checked) => {
                let Some(tg) = tg else { return };
                if let Some((lcw, true)) = checked.as_ref() {
                    if self.ldus > 0 && self.ldus != self.ldus_at_end {
                        self.ldus_at_end = self.ldus;
                        let lc = end_kind(lcw);
                        out.push(TrafficEvent::End { lc, air });
                        out.push(line("VOICE_END", format!("VOICE END TG={tg} LC={lc}"), false, Some(tg), None));
                    }
                }
                let Some((lcw, _)) = checked else { return };
                let source = self.call.and_then(|c| c.source);
                match lcw {
                    TdulcLcw::MotorolaTalkComplete { by_radio_id } => {
                        if plausible(by_radio_id) && source.is_none_or(|s| s == by_radio_id) && self.talk_complete_due(now) {
                            out.push(TrafficEvent::TalkComplete(Some(by_radio_id)));
                        }
                        out.push(line("TDULC", format!("TDULC MOTOROLA TALK COMPLETE TG={tg} SRC={by_radio_id}"), false, Some(tg), Some(by_radio_id)));
                    }
                    TdulcLcw::CallTermination { by_radio_id } => {
                        if matches!(by_radio_id, 0xFF_FFFD..=0xFF_FFFF) && self.talk_complete_due(now) {
                            out.push(TrafficEvent::TalkComplete(source));
                        }
                        let by = match by_radio_id {
                            0xFF_FFFD => "MOTOROLA SYS CTRL (0xFFFFFD)".to_string(),
                            0xFF_FFFF => "MOTOROLA SYS CTRL (0xFFFFFF)".to_string(),
                            0 => "HARRIS SYS CTRL".to_string(),
                            id => id.to_string(),
                        };
                        out.push(line("TDULC", format!("TDULC CALL TERMINATION BY:{by}"), false, Some(tg), None));
                    }
                    TdulcLcw::GroupVoiceChannelUser { talkgroup, .. } => {
                        out.push(line("TDULC", format!("TDULC GROUP VOICE CHANNEL USER TG={talkgroup}"), true, Some(tg), None));
                    }
                    other => out.push(line("TDULC", format!("TDULC {other:?}"), true, Some(tg), None)),
                }
            }
        }
    }

    /// Three of the last four link control sources agree on a radio not voted before.
    fn vote(&mut self, source: u32) -> Option<u32> {
        if self.votes.len() >= VOTE_OF {
            self.votes.pop_front();
        }
        self.votes.push_back(source);
        let agree = self.votes.iter().filter(|&&s| s == source).count();
        if agree >= VOTE_NEEDED && self.last_voted != source {
            self.last_voted = source;
            return Some(source);
        }
        None
    }

    fn talk_complete_due(&mut self, now: Instant) -> bool {
        if self.last_talk_complete.is_some_and(|t| now.saturating_duration_since(t) < TALK_COMPLETE_COOLDOWN) {
            return false;
        }
        self.last_talk_complete = Some(now);
        true
    }
}

enum Unit {
    Hdu(Option<voice_frame::HduHeader>),
    Ldu1([voice_frame::ImbeFrameRaw; 9], Option<u32>, Option<TdulcLcw>),
    Ldu2([voice_frame::ImbeFrameRaw; 9], Option<voice_frame::Ldu2Ess>),
    TduLc(Option<(TdulcLcw, bool)>),
}

/// The end-of-transmission marker's kind, by its link control.
fn end_kind(lcw: &TdulcLcw) -> &'static str {
    match lcw {
        TdulcLcw::MotorolaTalkComplete { .. } => "talk_complete",
        // SDRTrunk `LCCallTermination.isNetworkCommandedTeardown`: system controller addresses.
        TdulcLcw::CallTermination { by_radio_id } if matches!(*by_radio_id, 0 | 0xFF_FFFD..=0xFF_FFFF) => "network_teardown",
        TdulcLcw::CallTermination { .. } => "call_termination",
        TdulcLcw::GroupVoiceChannelUser { .. } => "channel_user",
        _ => "link_control",
    }
}

#[cfg(test)]
#[path = "traffic_tests.rs"]
mod tests;
