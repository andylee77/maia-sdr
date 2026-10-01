//! Change 075: the DMR Tier III call follower's decisions, without I/O.
//!
//! Control-channel grants pick a call; the traffic chain is tuned to the
//! granted LCN's frequency and its bursts on the granted timeslot carry the
//! call (voice, link control, terminator, CLEAR). `DmrFollower` turns the
//! messages of both channels into `FollowerAction`s; `dmr_task` carries them
//! out (NCO writes, call-boundary events, AMBE frames to the vocoder).
//!
//! One call at a time on traffic chain 1 (the chain with an IQ tap).
//! Tier III grants every transmission, so a busy follower records grants for
//! other talkgroups as not followed; the same talkgroup's next grant moves
//! the follower with it.

use crate::protocol::dmr::message::csbk::CsbkKind;
use crate::protocol::dmr::message::lc::FullLcKind;
use crate::protocol::dmr::message::types::Address;
use crate::protocol::dmr::message::DmrMessage;

/// A transmission is over when nothing of it (voice, its grant repeated, its
/// link control) has been seen for this long.
pub const HANG_MS: u64 = 3000;

/// A voice channel grant as the follower uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmrGrant {
    pub talkgroup: u32,
    /// The radio, when the grant names one.
    pub source: Option<u32>,
    /// Unit-to-unit (private) call: `talkgroup` holds the called radio.
    pub private: bool,
    pub lcn: u16,
    /// 1 or 2.
    pub timeslot: u8,
    /// From the LCN map (or MBC absolute parameters); `None` if unknown.
    pub freq_hz: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowerAction {
    /// Move traffic chain 1 to this frequency.
    Tune { freq_hz: u64 },
    /// A grant: open (or, for the talkgroup already open, bundle into) a
    /// call; `not_followed` names why the chain does not take it.
    Grant { grant: DmrGrant, not_followed: Option<&'static str> },
    /// The followed call is still live (repeated grant, voice, link control).
    KeepAlive { talkgroup: u32, freq_hz: Option<u64>, lcn: u16 },
    /// Voice of the followed call: three AMBE+2 frames (one burst).
    Voice { talkgroup: u32, source: Option<u32>, frames: [[u8; 9]; 3] },
    /// The followed call's link control names its talking radio.
    Source { source: u32 },
    /// The link control says the voice is encrypted.
    Encrypted,
    /// End of the followed transmission ("terminator", "clear", "timeout").
    End { reason: &'static str },
}

#[derive(Debug, Clone, Copy)]
struct Following {
    grant: DmrGrant,
    /// Wall time of the last thing seen of this call.
    last_ms: u64,
    source: Option<u32>,
    encrypted: bool,
}

/// The follower's state: idle, or following one transmission.
#[derive(Debug)]
pub struct DmrFollower {
    following: Option<Following>,
    tuned_hz: Option<u64>,
    /// Follow at all (`false`: grants are only recorded).
    pub enabled: bool,
}

/// The voice grant in a control-channel message, if it is one.
pub fn voice_grant(message: &DmrMessage) -> Option<DmrGrant> {
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
    let talkgroup = csbk.destination()?.value();
    let source = csbk.source().map(Address::value);
    Some(DmrGrant {
        talkgroup,
        source,
        private,
        lcn: channel.lcn,
        timeslot: channel.timeslot,
        freq_hz: channel.downlink_hz,
    })
}

impl Default for DmrFollower {
    fn default() -> Self {
        Self::new()
    }
}

impl DmrFollower {
    pub fn new() -> Self {
        DmrFollower { following: None, tuned_hz: None, enabled: true }
    }

    /// The talkgroup being followed.
    pub fn following(&self) -> Option<DmrGrant> {
        self.following.map(|f| f.grant)
    }

    /// Where traffic chain 1 was last tuned.
    pub fn tuned_hz(&self) -> Option<u64> {
        self.tuned_hz
    }

    /// The traffic chain was moved by someone else (preset, retune).
    pub fn chain_moved(&mut self) {
        self.tuned_hz = None;
    }

    /// A control-channel message.
    pub fn on_control(&mut self, message: &DmrMessage, now_ms: u64) -> Vec<FollowerAction> {
        let Some(grant) = voice_grant(message) else { return Vec::new() };
        let mut out = Vec::new();
        let Some(freq_hz) = grant.freq_hz else {
            out.push(FollowerAction::Grant { grant, not_followed: Some("unknown_lcn") });
            return out;
        };
        if !self.enabled {
            out.push(FollowerAction::Grant { grant, not_followed: Some("follower_off") });
            return out;
        }
        match self.following {
            Some(mut f) if f.grant.talkgroup == grant.talkgroup && f.grant.private == grant.private => {
                if f.grant.freq_hz == grant.freq_hz && f.grant.timeslot == grant.timeslot {
                    // The grant repeated: the call is still live.
                    f.last_ms = now_ms;
                    self.following = Some(f);
                    out.push(FollowerAction::KeepAlive {
                        talkgroup: grant.talkgroup,
                        freq_hz: grant.freq_hz,
                        lcn: grant.lcn,
                    });
                } else {
                    // The talkgroup's next transmission on another channel or
                    // timeslot: go with it (the open call bundles it).
                    self.tune(freq_hz, &mut out);
                    self.following = Some(Following { grant, last_ms: now_ms, source: grant.source, encrypted: false });
                    out.push(FollowerAction::Grant { grant, not_followed: None });
                }
            }
            Some(_) => out.push(FollowerAction::Grant { grant, not_followed: Some("busy") }),
            None => {
                self.tune(freq_hz, &mut out);
                self.following = Some(Following { grant, last_ms: now_ms, source: grant.source, encrypted: false });
                out.push(FollowerAction::Grant { grant, not_followed: None });
            }
        }
        out
    }

    fn tune(&mut self, freq_hz: u64, out: &mut Vec<FollowerAction>) {
        if self.tuned_hz != Some(freq_hz) {
            self.tuned_hz = Some(freq_hz);
            out.push(FollowerAction::Tune { freq_hz });
        }
    }

    /// A message from the traffic chain (any timeslot).
    pub fn on_traffic(&mut self, message: &DmrMessage, now_ms: u64) -> Vec<FollowerAction> {
        let mut out = Vec::new();
        let Some(mut f) = self.following else { return out };
        if message.timeslot() != f.grant.timeslot {
            return out;
        }
        match message {
            DmrMessage::Voice(v) => {
                f.last_ms = now_ms;
                if !f.encrypted {
                    out.push(FollowerAction::Voice {
                        talkgroup: f.grant.talkgroup,
                        source: f.source,
                        frames: v.ambe_frames(),
                    });
                }
            }
            DmrMessage::VoiceHeader(_, lc) | DmrMessage::FullLc(lc) if lc.valid => {
                if matches!(lc.kind, FullLcKind::GroupVoiceChannelUser | FullLcKind::UnitToUnitVoiceChannelUser)
                    && lc.destination().map(Address::value) == Some(f.grant.talkgroup)
                {
                    f.last_ms = now_ms;
                    if let Some(source) = lc.source().map(Address::value) {
                        if f.source != Some(source) {
                            f.source = Some(source);
                            out.push(FollowerAction::Source { source });
                        }
                    }
                    if !f.encrypted && lc.service_options().is_some_and(|o| o.is_encrypted()) {
                        f.encrypted = true;
                        out.push(FollowerAction::Encrypted);
                    }
                    out.push(FollowerAction::KeepAlive {
                        talkgroup: f.grant.talkgroup,
                        freq_hz: f.grant.freq_hz,
                        lcn: f.grant.lcn,
                    });
                }
            }
            DmrMessage::Terminator(_, lc) if lc.valid => {
                self.following = None;
                out.push(FollowerAction::End { reason: "terminator" });
                return out;
            }
            DmrMessage::Csbk(csbk) if message.is_valid() && csbk.kind == CsbkKind::Clear => {
                self.following = None;
                out.push(FollowerAction::End { reason: "clear" });
                return out;
            }
            _ => {}
        }
        self.following = Some(f);
        out
    }

    /// Ends a transmission nothing has been heard of for `HANG_MS`.
    pub fn tick(&mut self, now_ms: u64) -> Vec<FollowerAction> {
        match self.following {
            Some(f) if now_ms.saturating_sub(f.last_ms) >= HANG_MS => {
                self.following = None;
                vec![FollowerAction::End { reason: "timeout" }]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
#[path = "dmr_follower_tests.rs"]
mod tests;
