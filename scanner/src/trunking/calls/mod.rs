//! The call book: every call from its grant to its close, and the only writer of call state.
//!
//! Inputs are the follower's grant decisions, the control channel's grant updates, what each
//! lane's traffic decoder reports and a 100 ms tick; outputs are call events (opened, source,
//! speaker, closed) and the open calls. The rules, as the field taught them:
//!
//! - Every grant is a new call, except a repeat of the on-air call's own grant (same talkgroup,
//!   channel and source, no end of transmission yet), which refreshes it. Repeats within 200 ms
//!   (the TSBKs of one TSDU) are dropped.
//! - The next talker's grant while this one is on the air waits for the hand-over: an HDU, voice
//!   after the end marker, the end grace, or 10 s.
//! - A call closes `end_grace` after its end-of-transmission marker unless voice resumes (two
//!   voice NIDs within 400 ms, or voice aired after the marker); otherwise `hang` after its last
//!   keep-alive (voice, an HDU, its grant or a grant update on its channel); or when another call
//!   takes its channel.
//! - A call the follower did not take is listed for its channel time: open while the control
//!   channel announces it, closed at its last announcement.
//!
//! Channels compare by frequency and timeslot. Timers run on the monotonic clock; wall time only
//! labels the records.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::hardware::p25core::Lane;
use crate::util::time::Stamp;

pub type CallId = u64;

/// Repeats of a grant this close together are one announcement.
const GRANT_DEDUP: Duration = Duration::from_millis(200);
/// Two voice NIDs this close together after an end marker mean voice resumed.
const VOICE_NID_PAIR: Duration = Duration::from_millis(400);
/// A queued grant is applied at the latest this long after it arrived.
const QUEUED_GRANT_MAX: Duration = Duration::from_secs(10);

/// A traffic channel: its downlink and, on a TDMA carrier or DMR, its timeslot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
pub struct ChannelKey {
    pub freq_hz: Option<u64>,
    pub slot: Option<u8>,
}

impl ChannelKey {
    /// Both known and the same channel.
    fn same(&self, other: &ChannelKey) -> bool {
        self.freq_hz.is_some() && self == other
    }
}

/// Why the follower did not take a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotFollowed {
    Encrypted,
    /// Every lane that could carry it is busy.
    Busy,
    MonitorList,
    Ignored,
    SpeakerOff,
    OutOfBand,
    /// P25 Phase 2 (TDMA).
    Phase2,
    /// DMR: a logical channel the site's channel plan does not name.
    UnknownLcn,
}

impl NotFollowed {
    pub fn as_str(self) -> &'static str {
        match self {
            NotFollowed::Encrypted => "encrypted",
            NotFollowed::Busy => "busy",
            NotFollowed::MonitorList => "monitor_list",
            NotFollowed::Ignored => "ignored",
            NotFollowed::SpeakerOff => "speaker_off",
            NotFollowed::OutOfBand => "out_of_band",
            NotFollowed::Phase2 => "phase2",
            NotFollowed::UnknownLcn => "unknown_lcn",
        }
    }

    /// The names p25-httpd stored ("sticky_lock" is today's `Busy`).
    #[cfg(test)]
    pub fn parse(s: &str) -> Option<NotFollowed> {
        Some(match s {
            "encrypted" => NotFollowed::Encrypted,
            "busy" | "sticky_lock" => NotFollowed::Busy,
            "monitor_list" => NotFollowed::MonitorList,
            "ignored" => NotFollowed::Ignored,
            "speaker_off" => NotFollowed::SpeakerOff,
            "out_of_band" => NotFollowed::OutOfBand,
            "phase2" => NotFollowed::Phase2,
            "unknown_lcn" => NotFollowed::UnknownLcn,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Followed(Lane),
    NotFollowed(NotFollowed),
}

/// A grant with the follower's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantIn {
    pub tg: u32,
    pub source: Option<u32>,
    pub channel: ChannelKey,
    /// As the control channel names it ("0-1189", "LCN 6").
    pub channel_label: Option<String>,
    pub encrypted: bool,
    /// P25 NAC of the control channel (0 for DMR).
    pub nac: u16,
    pub decision: Decision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenReason {
    /// A grant with no open call on its lane.
    CcGrant,
    /// A grant that ended the lane's previous call.
    TgChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// No keep-alive for `hang`; also a not-followed call no longer announced.
    Timeout,
    /// `end_grace` after the end-of-transmission marker.
    CallEnd,
    /// Another call took the channel (or the lane).
    TgChange,
    /// The live site changed.
    SiteSwitch,
}

impl CloseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            CloseReason::Timeout => "timeout",
            CloseReason::CallEnd => "call_end",
            CloseReason::TgChange => "tg_change",
            CloseReason::SiteSwitch => "site_switch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceVia {
    /// A repeat of the grant named it.
    CcRefresh,
    /// The voice link control (LDU1).
    LinkControl,
    /// A terminator (Motorola talk complete).
    TalkComplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub call: CallId,
    pub site: String,
    pub tg: u32,
    pub nac: u16,
    pub source: Option<u32>,
    pub channel: ChannelKey,
    pub channel_label: Option<String>,
    pub encrypted: bool,
    pub not_followed: Option<NotFollowed>,
    pub via: OpenReason,
    pub lane: Option<Lane>,
    pub at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    pub call: CallId,
    pub lane: Option<Lane>,
    pub reason: CloseReason,
    pub source: Option<u32>,
    /// The talking radio the voice link control named last.
    pub speaker: Option<u32>,
    pub started_unix_ms: u64,
    pub ended_unix_ms: u64,
    pub first_voice_unix_ms: Option<u64>,
    pub first_hdu_unix_ms: Option<u64>,
    /// Every radio heard in the call, in order.
    pub sources: Vec<u32>,
    pub last_update_unix_ms: u64,
    pub open_ms: u64,
    /// The end-of-transmission marker pending at the close.
    pub end_lc: Option<&'static str>,
    pub voice_frames: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallEvent {
    Opened(Opened),
    Source { call: CallId, lane: Option<Lane>, source: u32, via: SourceVia },
    Speaker { call: CallId, lane: Option<Lane>, speaker: u32, agrees_with_grant: bool },
    Closed(Closed),
}

/// Close timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallPolicy {
    /// No keep-alive for this long closes a call.
    pub hang: Duration,
    /// After the end-of-transmission marker.
    pub end_grace: Duration,
}

impl Default for CallPolicy {
    fn default() -> Self {
        CallPolicy { hang: Duration::from_millis(3_000), end_grace: Duration::from_millis(2_000) }
    }
}

/// A pending end of transmission.
#[derive(Debug, Clone, Copy)]
struct EndMarker {
    at: Instant,
    /// Air time of the terminator.
    air: Instant,
    lc: &'static str,
}

#[derive(Debug, Clone)]
struct Queued {
    grant: GrantIn,
    at: Instant,
}

/// One call.
#[derive(Debug, Clone)]
pub struct Call {
    pub id: CallId,
    pub tg: u32,
    pub nac: u16,
    pub source: Option<u32>,
    pub speaker: Option<u32>,
    pub channel: ChannelKey,
    pub channel_label: Option<String>,
    pub encrypted: bool,
    pub not_followed: Option<NotFollowed>,
    pub lane: Option<Lane>,
    pub started: Stamp,
    pub sources: Vec<u32>,
    pub voice_frames: u64,
    first_voice: Option<Stamp>,
    last_voice: Option<Instant>,
    first_hdu: Option<Stamp>,
    /// Voice or an HDU: the voice keep-alive.
    last_audio: Option<Instant>,
    /// The newest grant or grant update for the call.
    last_update: Stamp,
    last_voice_nid: Option<Instant>,
    end: Option<EndMarker>,
    queued: Option<Queued>,
}

impl Call {
    fn open(id: CallId, g: &GrantIn, lane: Option<Lane>, at: Stamp) -> Call {
        Call {
            id,
            tg: g.tg,
            nac: g.nac,
            source: g.source,
            speaker: None,
            channel: g.channel,
            channel_label: g.channel_label.clone(),
            encrypted: g.encrypted,
            not_followed: match g.decision {
                Decision::NotFollowed(r) => Some(r),
                Decision::Followed(_) => None,
            },
            lane,
            started: at,
            sources: g.source.into_iter().collect(),
            voice_frames: 0,
            first_voice: None,
            last_voice: None,
            first_hdu: None,
            last_audio: None,
            last_update: at,
            last_voice_nid: None,
            end: None,
            queued: None,
        }
    }

    /// Voice was seen (an HDU, a voice NID or decoded voice): the transmission can still be on
    /// the air.
    fn voice_seen(&self) -> bool {
        self.first_hdu.is_some() || self.voice_frames > 0 || self.last_voice_nid.is_some()
    }

    fn last_keepalive(&self) -> Instant {
        let mut t = self.started.mono.max(self.last_update.mono);
        if let Some(a) = self.last_audio {
            t = t.max(a);
        }
        t
    }

    fn cancel_end(&mut self) {
        self.end = None;
    }

    fn observe_source(&mut self, source: u32) -> bool {
        if source == 0 || self.sources.contains(&source) {
            return false;
        }
        self.sources.push(source);
        true
    }

    /// When the call closes if nothing changes, by which rule, and that rule's window.
    #[cfg(test)]
    pub fn close_plan(&self, policy: &CallPolicy) -> (Instant, &'static str, Duration) {
        let idle_at = self.last_keepalive() + policy.hang;
        if let Some(end) = self.end {
            let end_at = end.at + policy.end_grace;
            if end_at <= idle_at {
                return (end_at, "end", policy.end_grace);
            }
        }
        (idle_at, "timeout", policy.hang)
    }

    /// The pending end-of-transmission marker's receipt time.
    pub fn end_marker(&self) -> Option<Instant> {
        self.end.map(|e| e.at)
    }

    pub fn end_lc(&self) -> Option<&'static str> {
        self.end.map(|e| e.lc)
    }


    fn due(&self, now: Instant, policy: &CallPolicy) -> Option<CloseReason> {
        if self.end.is_some_and(|e| now.saturating_duration_since(e.at) >= policy.end_grace) {
            return Some(CloseReason::CallEnd);
        }
        if now.saturating_duration_since(self.last_keepalive()) > policy.hang {
            return Some(CloseReason::Timeout);
        }
        None
    }

    fn closed(&self, reason: CloseReason, ended: Stamp) -> Closed {
        Closed {
            call: self.id,
            lane: self.lane,
            reason,
            source: self.source,
            speaker: self.speaker,
            started_unix_ms: self.started.unix_ms,
            ended_unix_ms: ended.unix_ms,
            first_voice_unix_ms: self.first_voice.map(|s| s.unix_ms),
            first_hdu_unix_ms: self.first_hdu.map(|s| s.unix_ms),
            sources: self.sources.clone(),
            last_update_unix_ms: self.last_update.unix_ms,
            open_ms: ended.mono.saturating_duration_since(self.started.mono).as_millis() as u64,
            end_lc: self.end.map(|e| e.lc),
            voice_frames: self.voice_frames,
        }
    }

    fn opened(&self, site: &str, via: OpenReason) -> Opened {
        Opened {
            call: self.id,
            site: site.to_string(),
            tg: self.tg,
            nac: self.nac,
            source: self.source,
            channel: self.channel,
            channel_label: self.channel_label.clone(),
            encrypted: self.encrypted,
            not_followed: self.not_followed,
            via,
            lane: self.lane,
            at_unix_ms: self.started.unix_ms,
        }
    }
}

/// A lane and its open call.
#[derive(Debug)]
struct Slot {
    lane: Lane,
    call: Option<Call>,
}

/// Dedup key: talkgroup, source (0 when none), channel, encrypted.
type DedupKey = (u32, u32, ChannelKey, bool);

pub struct CallBook {
    site: String,
    policy: CallPolicy,
    slots: Vec<Slot>,
    /// Calls the follower did not take, open for their channel time.
    not_followed: Vec<Call>,
    next_id: CallId,
    dedup: HashMap<DedupKey, Instant>,
}

impl CallBook {
    /// `first_id` continues past the ids already stored (history, recordings).
    pub fn new(site: &str, lanes: &[Lane], policy: CallPolicy, first_id: CallId) -> Self {
        CallBook {
            site: site.to_string(),
            policy,
            slots: lanes.iter().map(|&lane| Slot { lane, call: None }).collect(),
            not_followed: Vec::new(),
            next_id: first_id.max(1),
            dedup: HashMap::new(),
        }
    }

    /// The open call on `lane`.
    pub fn on_lane(&self, lane: Lane) -> Option<&Call> {
        self.slots.iter().find(|s| s.lane == lane).and_then(|s| s.call.as_ref())
    }

    fn slot_index(&self, lane: Option<Lane>) -> usize {
        lane.and_then(|l| self.slots.iter().position(|s| s.lane == l)).unwrap_or(0)
    }

    fn take_id(&mut self) -> CallId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// True for a repeat within `GRANT_DEDUP`; records the announcement otherwise.
    fn duplicate(&mut self, key: DedupKey, now: Instant) -> bool {
        if self.dedup.get(&key).is_some_and(|&t| now.saturating_duration_since(t) < GRANT_DEDUP) {
            return true;
        }
        self.dedup.insert(key, now);
        self.dedup.retain(|_, t| now.saturating_duration_since(*t) < Duration::from_secs(60));
        false
    }

    /// The live site changed: every call ends.
    pub fn site_switch(&mut self, site: &str, at: Stamp, out: &mut Vec<CallEvent>) {
        for i in 0..self.slots.len() {
            self.close_slot(i, CloseReason::SiteSwitch, at, out);
        }
        for c in std::mem::take(&mut self.not_followed) {
            out.push(CallEvent::Closed(c.closed(CloseReason::SiteSwitch, at)));
        }
        self.dedup.clear();
        self.site = site.to_string();
    }

    fn close_slot(&mut self, i: usize, reason: CloseReason, at: Stamp, out: &mut Vec<CallEvent>) {
        if let Some(c) = self.slots[i].call.take() {
            out.push(CallEvent::Closed(c.closed(reason, at)));
        }
    }

    /// A grant with the follower's decision.
    pub fn grant(&mut self, g: GrantIn, at: Stamp, out: &mut Vec<CallEvent>) {
        let key = (g.tg, g.source.unwrap_or(0), g.channel, g.encrypted);
        if self.duplicate(key, at.mono) {
            return;
        }
        match g.decision {
            Decision::NotFollowed(_) => {
                // One voice channel per frequency: a followed call on it has ended.
                for i in 0..self.slots.len() {
                    if self.slots[i].call.as_ref().is_some_and(|c| c.channel.same(&g.channel)) {
                        self.close_slot(i, CloseReason::TgChange, at, out);
                    }
                }
                self.not_followed_grant(g, at, out);
            }
            Decision::Followed(lane) => {
                self.close_not_followed_on(&g.channel, out);
                let si = self.slot_index(Some(lane));
                for i in 0..self.slots.len() {
                    if i != si && self.slots[i].call.as_ref().is_some_and(|c| c.channel.same(&g.channel)) {
                        self.close_slot(i, CloseReason::TgChange, at, out);
                    }
                }
                self.followed_grant(si, g, at, out);
            }
        }
    }

    fn followed_grant(&mut self, si: usize, g: GrantIn, at: Stamp, out: &mut Vec<CallEvent>) {
        let Some(call) = self.slots[si].call.as_mut() else {
            self.open_next(si, g, None, OpenReason::CcGrant, at, out);
            return;
        };
        let same_channel = call.channel.same(&g.channel);
        if call.tg != g.tg || !same_channel || call.end.is_some() {
            self.open_next(si, g, Some(CloseReason::TgChange), OpenReason::TgChange, at, out);
            return;
        }
        let same_source = match (g.source, call.source) {
            (Some(n), Some(a)) => n == a,
            _ => true,
        };
        if same_source {
            // The control channel re-announcing the call on the air.
            call.last_update = at;
            if let Some(s) = g.source {
                if call.observe_source(s) {
                    out.push(CallEvent::Source { call: call.id, lane: call.lane, source: s, via: SourceVia::CcRefresh });
                }
                call.source.get_or_insert(s);
            }
        } else if call.voice_seen() {
            // The next talker, queued behind this one.
            call.queued = Some(Queued { grant: g, at: at.mono });
        } else {
            // Two talkers granted back to back before any voice: the later one is on the air.
            self.open_next(si, g, Some(CloseReason::TgChange), OpenReason::TgChange, at, out);
        }
    }

    fn open_next(&mut self, si: usize, g: GrantIn, close: Option<CloseReason>, via: OpenReason, at: Stamp, out: &mut Vec<CallEvent>) {
        if let Some(reason) = close {
            self.close_slot(si, reason, at, out);
        }
        let id = self.take_id();
        let call = Call::open(id, &g, Some(self.slots[si].lane), at);
        out.push(CallEvent::Opened(call.opened(&self.site, via)));
        self.slots[si].call = Some(call);
    }

    /// Hand the lane to its call's queued grant.
    fn start_queued(&mut self, si: usize, at: Stamp, out: &mut Vec<CallEvent>) {
        let Some(q) = self.slots[si].call.as_mut().and_then(|c| c.queued.take()) else { return };
        self.open_next(si, q.grant, Some(CloseReason::TgChange), OpenReason::TgChange, at, out);
    }

    fn not_followed_grant(&mut self, g: GrantIn, at: Stamp, out: &mut Vec<CallEvent>) {
        let hang = self.policy.hang;
        if let Some(c) = self.not_followed.iter_mut().find(|c| c.tg == g.tg && c.channel == g.channel) {
            let same_source = match (g.source, c.source) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };
            if same_source && at.mono.saturating_duration_since(c.last_update.mono) <= hang {
                c.last_update = at;
                c.source = c.source.or(g.source);
                return;
            }
        }
        self.close_not_followed_on(&g.channel, out);
        let id = self.take_id();
        let call = Call::open(id, &g, None, at);
        out.push(CallEvent::Opened(call.opened(&self.site, OpenReason::CcGrant)));
        self.not_followed.push(call);
    }

    /// Another call holds `channel`: the not-followed calls on it end at their last announcement.
    fn close_not_followed_on(&mut self, channel: &ChannelKey, out: &mut Vec<CallEvent>) {
        if channel.freq_hz.is_some() {
            self.close_not_followed_where(|c| c.channel == *channel, out);
        }
    }

    fn close_not_followed_where(&mut self, pred: impl Fn(&Call) -> bool, out: &mut Vec<CallEvent>) {
        let mut i = 0;
        while i < self.not_followed.len() {
            if pred(&self.not_followed[i]) {
                let c = self.not_followed.remove(i);
                out.push(CallEvent::Closed(c.closed(CloseReason::Timeout, c.last_update)));
            } else {
                i += 1;
            }
        }
    }

    /// A grant update (a refresh; it never opens a call). `channel_number` stands in for an
    /// unknown frequency in the repeat filter.
    pub fn grant_update(&mut self, tg: u32, channel: ChannelKey, channel_number: u16, at: Stamp) {
        let key_channel = ChannelKey { freq_hz: channel.freq_hz.or(Some(u64::from(channel_number))), slot: channel.slot };
        if self.duplicate((tg, 0, key_channel, false), at.mono) {
            return;
        }
        for c in self.not_followed.iter_mut().filter(|c| c.tg == tg && (channel.freq_hz.is_none() || c.channel == channel)) {
            c.last_update = at;
        }
        for c in self.slots.iter_mut().filter_map(|s| s.call.as_mut()) {
            // An update for the talkgroup on another channel says nothing about this call.
            let same_channel = match (c.channel.freq_hz, channel.freq_hz) {
                (Some(_), Some(_)) => c.channel == channel,
                _ => true,
            };
            if c.tg == tg && same_channel {
                c.last_update = at;
            }
        }
    }

    /// The lane's decoder read an HDU.
    pub fn hdu(&mut self, lane: Lane, nac: u16, at: Stamp, out: &mut Vec<CallEvent>) {
        let si = self.slot_index(Some(lane));
        if self.slots[si].call.as_ref().is_some_and(|c| c.queued.is_some()) {
            self.start_queued(si, at, out);
        }
        if let Some(c) = self.slots[si].call.as_mut() {
            c.first_hdu.get_or_insert(at);
            c.first_voice.get_or_insert(at);
            c.last_audio = Some(at.mono);
            if c.nac == 0 && nac != 0 {
                c.nac = nac;
            }
        }
    }

    /// The voice link control names the talking radio.
    pub fn link_control_source(&mut self, lane: Lane, source: u32, out: &mut Vec<CallEvent>) {
        let si = self.slot_index(Some(lane));
        let Some(c) = self.slots[si].call.as_mut() else { return };
        let agrees = c.source == Some(source);
        if c.observe_source(source) {
            out.push(CallEvent::Source { call: c.id, lane: c.lane, source, via: SourceVia::LinkControl });
        }
        c.source.get_or_insert(source);
        if c.speaker != Some(source) {
            c.speaker = Some(source);
            out.push(CallEvent::Speaker { call: c.id, lane: c.lane, speaker: source, agrees_with_grant: agrees });
        }
    }

    /// A terminator names the radio that talked.
    pub fn talk_complete_source(&mut self, lane: Lane, source: u32, out: &mut Vec<CallEvent>) {
        let si = self.slot_index(Some(lane));
        let Some(c) = self.slots[si].call.as_mut() else { return };
        if c.observe_source(source) {
            out.push(CallEvent::Source { call: c.id, lane: c.lane, source, via: SourceVia::TalkComplete });
        }
        c.source.get_or_insert(source);
    }

    /// The lane's demodulator saw a valid NID (in real time, ahead of the decode). A pair of voice
    /// NIDs after the end marker means voice resumed: the queued talker takes over, or the end is
    /// cancelled. NIDs are not keep-alives (noise can fake one).
    pub fn nid(&mut self, lane: Lane, voice: bool, at: Stamp, out: &mut Vec<CallEvent>) {
        let si = self.slot_index(Some(lane));
        let mut hand_over = false;
        if let Some(c) = self.slots[si].call.as_mut() {
            if voice {
                let prev = c.last_voice_nid.replace(at.mono);
                if let (Some(end), Some(prev)) = (c.end, prev) {
                    if prev >= end.at && at.mono.saturating_duration_since(prev) <= VOICE_NID_PAIR {
                        if c.queued.is_some() {
                            hand_over = true;
                        } else {
                            c.cancel_end();
                        }
                    }
                }
            }
        }
        if hand_over {
            self.start_queued(si, at, out);
        }
    }

    /// End of a transmission of `call` (the first valid terminator after its voice), aired at
    /// `air`. Ignored for another call (a late terminator of a pre-empted one) and while a
    /// marker is pending.
    pub fn voice_end(&mut self, lane: Lane, call: CallId, air: Instant, lc: &'static str, at: Stamp) {
        let si = self.slot_index(Some(lane));
        if let Some(c) = self.slots[si].call.as_mut() {
            if c.id == call && c.end.is_none() {
                c.end = Some(EndMarker { at: at.mono, air, lc });
            }
        }
    }

    /// One voice frame decoded on `lane`, aired at `aired`; `call` when the decoder attributed
    /// it by air time.
    pub fn voice(&mut self, lane: Lane, call: Option<CallId>, aired: Instant, at: Stamp) {
        let Some(slot) = self.slots.iter_mut().find(|s| s.lane == lane) else { return };
        let Some(c) = slot.call.as_mut() else { return };
        // The previous call's in-flight tail is not this call's activity.
        if call.is_some_and(|id| id != c.id) {
            return;
        }
        // Voice aired after the end marker: the transmission did not end there. (With a queued
        // grant it is the next talker's, handed over by the NID and HDU paths.)
        if c.queued.is_none() && c.end.is_some_and(|e| aired > e.air) {
            c.cancel_end();
        }
        c.last_audio = Some(at.mono);
        c.voice_frames += 1;
        c.first_voice.get_or_insert(at);
        c.last_voice = Some(at.mono);
    }

    /// The 100 ms sweep: not-followed calls no longer announced, then each lane's call (its
    /// queued grant takes over when the call would close or after `QUEUED_GRANT_MAX`).
    pub fn tick(&mut self, at: Stamp, out: &mut Vec<CallEvent>) {
        let hang = self.policy.hang;
        self.close_not_followed_where(|c| at.mono.saturating_duration_since(c.last_update.mono) > hang, out);
        for si in 0..self.slots.len() {
            let Some(c) = self.slots[si].call.as_ref() else { continue };
            let due = c.due(at.mono, &self.policy);
            if let Some(q) = c.queued.as_ref() {
                if at.mono.saturating_duration_since(q.at) >= QUEUED_GRANT_MAX || due.is_some() {
                    self.start_queued(si, at, out);
                }
                continue;
            }
            if let Some(reason) = due {
                self.close_slot(si, reason, at, out);
            }
        }
    }
}

#[cfg(test)]
#[path = "book_tests.rs"]
mod book_tests;

#[cfg(test)]
#[path = "replay_tests.rs"]
mod replay_tests;
