//! The follower: which lane follows a grant, protocol-neutral and pure. It sees each grant and
//! the lanes' state, and returns the call book's record of the grant and the lanes' commands.
//!
//! The gates, in order: the ignore list, Phase 2, the monitor list, the speaker groups, an
//! unknown channel, the receive window, channel reuse (a lane on that channel with another
//! talkgroup is released), encryption (a lane on that talkgroup is paused), then the lane:
//! the one already on the talkgroup, an idle one, one whose call has ended (its end marker is
//! 600 ms old) or holds a lower-priority group, else none (busy).
//!
//! A grant update never takes a lane (it carries no encryption flag), except to follow again a
//! call its lane closed for lack of keep-alive within 30 s, or a grant refused as busy within
//! 2 s once a lane is free: the control channel still announcing it means it did not end.
//!
//! With two lanes, lane one serves the left speaker's groups and lane two the right's; a
//! talkgroup on "both" uses either. A busy side does not borrow the other side's lane.

pub mod routing;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::hardware::p25core::Lane;
use crate::protocol::events::{ChannelId, Grant};
use crate::services::config::profiles::Side;
use crate::trunking::calls::{CallId, ChannelKey, CloseReason, Closed, Decision, GrantIn, NotFollowed};
use routing::Routing;

/// After the call's end marker, another talkgroup may take its lane (long enough for the resumed
/// voice check to cancel a contradicted marker, still before SDRTrunk frees its channel).
pub const END_PREEMPT_AFTER: Duration = Duration::from_millis(600);
/// A grant update follows again a call its lane closed by timeout within this.
pub const REFOLLOW_WINDOW: Duration = Duration::from_secs(30);
/// A grant update follows again a grant refused as busy within this (later updates are mostly the
/// system's channel hang).
pub const REFOLLOW_BUSY: Duration = Duration::from_secs(2);

/// What the follower knows about one lane when a grant arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneView {
    pub lane: Lane,
    /// The talkgroup the lane follows (`None`: idle).
    pub locked_tg: Option<u32>,
    /// Where the lane is tuned, idle or not.
    pub tuned_hz: Option<u64>,
    /// The followed call's pending end marker: (talkgroup, receipt time).
    pub end_marker: Option<(u32, Instant)>,
}

/// Where a grant goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneChoice {
    /// The lane already follows the talkgroup (a repeat, the next talker, or the talkgroup moving
    /// channel). Calls never move between lanes.
    Stay(Lane),
    Take(Lane),
    /// The lane's call yields: its transmission ended, or a higher-priority group.
    Preempt(Lane, Preemption),
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preemption {
    EndMarker,
    Priority,
}

impl LaneChoice {
    pub fn lane(self) -> Option<Lane> {
        match self {
            LaneChoice::Stay(l) | LaneChoice::Take(l) | LaneChoice::Preempt(l, _) => Some(l),
            LaneChoice::Reject => None,
        }
    }
}

/// The lanes a grant on `side` may use, most preferred first.
pub fn candidates(side: Side, grant_hz: Option<u64>, lanes: &[LaneView], routing: &Routing) -> Vec<Lane> {
    if lanes.len() <= 1 {
        return lanes.iter().map(|v| v.lane).collect();
    }
    let has = |l: Lane| lanes.iter().any(|v| v.lane == l);
    match side {
        Side::Left => [Lane::One].into_iter().filter(|&l| has(l)).collect(),
        Side::Right => [Lane::Two].into_iter().filter(|&l| has(l)).collect(),
        Side::Off => Vec::new(),
        Side::Both => {
            // The lane already tuned to the grant's frequency (it resumes without a retune), then
            // the side that carries no groups (the other stays free for them), then lane one.
            let side_of = |l: Lane| if l == Lane::One { Side::Left } else { Side::Right };
            let mut v: Vec<&LaneView> = lanes.iter().collect();
            v.sort_by_key(|x| {
                let parked = grant_hz.is_some() && x.tuned_hz == grant_hz;
                (!parked, routing.side_has_groups(side_of(x.lane)), x.lane.index())
            });
            v.into_iter().map(|x| x.lane).collect()
        }
    }
}

/// The lane for a clear, followed grant of `tg` on `side`. `end_frees(locked_tg, marker)` says
/// whether a locked call's end marker lets another talkgroup take its lane.
pub fn choose_lane(
    tg: u32,
    side: Side,
    grant_hz: Option<u64>,
    lanes: &[LaneView],
    routing: &Routing,
    end_frees: impl Fn(u32, Option<(u32, Instant)>) -> bool,
) -> LaneChoice {
    if let Some(v) = lanes.iter().find(|v| v.locked_tg == Some(tg)) {
        return LaneChoice::Stay(v.lane);
    }
    let cands = candidates(side, grant_hz, lanes, routing);
    let views: Vec<LaneView> = cands.iter().filter_map(|&l| lanes.iter().find(|v| v.lane == l).copied()).collect();
    if let Some(v) = views.iter().find(|v| v.locked_tg.is_none()) {
        return LaneChoice::Take(v.lane);
    }
    if let Some(v) = views.iter().find(|v| v.locked_tg.is_some_and(|t| end_frees(t, v.end_marker))) {
        return LaneChoice::Preempt(v.lane, Preemption::EndMarker);
    }
    // The candidate whose call ranks lowest yields first (the more preferred one on a tie). A
    // call no longer followed ranks last.
    let rank = |t: u32| routing.route(t).map_or(u32::MAX, |r| u32::from(r.rank));
    let mut best: Option<(u32, Lane)> = None;
    for v in &views {
        let Some(t) = v.locked_tg else { continue };
        if routing.preempts(tg, t) && best.is_none_or(|(r, _)| rank(t) > r) {
            best = Some((rank(t), v.lane));
        }
    }
    best.map_or(LaneChoice::Reject, |(_, l)| LaneChoice::Preempt(l, Preemption::Priority))
}

/// May another talkgroup take the lane of the call of `locked_tg`? Once its end marker has been
/// pending `END_PREEMPT_AFTER`.
pub fn end_marker_frees(locked_tg: u32, marker: Option<(u32, Instant)>, now: Instant) -> bool {
    matches!(marker, Some((tg, at)) if tg == locked_tg && now.saturating_duration_since(at) >= END_PREEMPT_AFTER)
}

/// What the call book records of a grant or an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Grant(GrantIn),
    /// A keep-alive of the talkgroup's call on that channel.
    Update { tg: u32, channel: ChannelKey, channel_number: u16 },
}

/// What a lane must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Follow a call on the channel: tune there (the lane decides whether to reset or coast).
    Follow { lane: Lane, channel: ChannelKey },
    /// The lane's call is over on the air (another talkgroup took its channel, or its lane):
    /// the lane stays tuned, its decoder starts afresh.
    Release { lane: Lane },
    /// Stop decoding: the lane's talkgroup turned out encrypted.
    Pause { lane: Lane },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub record: Option<Record>,
    pub commands: Vec<Command>,
    /// Lines for the event log.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Locked {
    tg: u32,
    channel: ChannelKey,
    call: Option<CallId>,
}

#[derive(Debug, Clone)]
struct LaneState {
    lane: Lane,
    locked: Option<Locked>,
    tuned_hz: Option<u64>,
    /// (talkgroup, frequency, when) of the call the call book last closed on this lane by timeout.
    last_timeout: Option<(u32, u64, Instant)>,
    /// (talkgroup, frequency, when) of the newest clear grant refused while this lane was busy.
    last_busy: Option<(u32, u64, Instant)>,
    /// Consecutive checks that found the lane locked with no open call.
    stuck_looks: u8,
}

pub struct Follower {
    lanes: Vec<LaneState>,
    routing: Routing,
    /// Talkgroups seen encrypted at this site.
    encrypted: HashSet<u32>,
}

impl Follower {
    /// `lanes`: the lanes that can carry the site's protocol.
    pub fn new(lanes: &[Lane], routing: Routing, encrypted: HashSet<u32>) -> Self {
        Follower {
            lanes: lanes
                .iter()
                .map(|&lane| LaneState { lane, locked: None, tuned_hz: None, last_timeout: None, last_busy: None, stuck_looks: 0 })
                .collect(),
            routing,
            encrypted,
        }
    }

    pub fn set_routing(&mut self, routing: Routing) {
        self.routing = routing;
    }

    pub fn encrypted(&self) -> &HashSet<u32> {
        &self.encrypted
    }

    /// The talkgroup each lane follows.
    pub fn locked(&self) -> Vec<(Lane, Option<u32>)> {
        self.lanes.iter().map(|l| (l.lane, l.locked.map(|k| k.tg))).collect()
    }

    fn views(&self, markers: &dyn Fn(Lane) -> Option<(u32, Instant)>) -> Vec<LaneView> {
        self.lanes
            .iter()
            .map(|l| LaneView { lane: l.lane, locked_tg: l.locked.map(|k| k.tg), tuned_hz: l.tuned_hz, end_marker: markers(l.lane) })
            .collect()
    }

    fn index(&self, lane: Lane) -> Option<usize> {
        self.lanes.iter().position(|l| l.lane == lane)
    }

    /// A grant or grant update from the control channel. `markers` gives each lane's call end
    /// marker; `in_window` whether a frequency can be received in the current window.
    pub fn grant(
        &mut self,
        g: &Grant,
        nac: u16,
        now: Instant,
        markers: &dyn Fn(Lane) -> Option<(u32, Instant)>,
        in_window: &dyn Fn(u64) -> bool,
    ) -> Outcome {
        let channel = ChannelKey { freq_hz: g.channel.freq_hz, slot: g.channel.slot };
        if g.encrypted {
            self.encrypted.insert(g.tg);
        }
        let mut out = Outcome::default();
        if g.update {
            if self.lanes.iter().any(|l| l.locked.is_some_and(|k| k.tg == g.tg)) {
                out.record = Some(update(g, channel));
                return out;
            }
            let Some(why) = self.refollow(g.tg, g.channel.freq_hz, now, markers) else {
                out.record = Some(update(g, channel));
                return out;
            };
            out.notes.push(format!("follow TG {} again on its grant update ({why}, still announced)", g.tg));
            let regrant = Grant { update: false, source: None, ..*g };
            self.gates(&regrant, channel, nac, now, markers, in_window, &mut out);
            return out;
        }
        self.gates(g, channel, nac, now, markers, in_window, &mut out);
        out
    }

    /// Does an update for `tg` follow again a call it still announces?
    fn refollow(&mut self, tg: u32, freq: Option<u64>, now: Instant, markers: &dyn Fn(Lane) -> Option<(u32, Instant)>) -> Option<&'static str> {
        let freq = freq?;
        let matches = |m: Option<(u32, u64, Instant)>, window: Duration| {
            m.is_some_and(|(t, f, at)| t == tg && f == freq && now.saturating_duration_since(at) <= window)
        };
        for i in 0..self.lanes.len() {
            let l = &self.lanes[i];
            if l.locked.is_none() && matches(l.last_timeout, REFOLLOW_WINDOW) {
                self.lanes[i].last_timeout = None;
                return Some("its call closed by timeout");
            }
            let free = l.locked.is_none_or(|k| end_marker_frees(k.tg, markers(l.lane), now));
            if free && matches(l.last_busy, REFOLLOW_BUSY) {
                for l in &mut self.lanes {
                    l.last_busy = None;
                }
                return Some("refused while the lanes were busy");
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn gates(
        &mut self,
        g: &Grant,
        channel: ChannelKey,
        nac: u16,
        now: Instant,
        markers: &dyn Fn(Lane) -> Option<(u32, Instant)>,
        in_window: &dyn Fn(u64) -> bool,
        out: &mut Outcome,
    ) {
        let record = |decision| {
            Some(Record::Grant(GrantIn {
                tg: g.tg,
                source: g.source.filter(|&s| s != 0),
                channel,
                channel_label: Some(label(g)),
                encrypted: g.encrypted,
                nac,
                decision,
            }))
        };
        let refuse = |out: &mut Outcome, why: NotFollowed, note: String| {
            out.notes.push(note);
            out.record = record(Decision::NotFollowed(why));
        };
        if self.routing.ignored(g.tg) {
            return refuse(out, NotFollowed::Ignored, format!("TG {} not followed: on the ignore list", g.tg));
        }
        if g.channel.tdma {
            return refuse(out, NotFollowed::Phase2, format!("TG {} not followed: granted a Phase 2 (TDMA) channel {}", g.tg, label(g)));
        }
        if !self.routing.monitored(g.tg) {
            return refuse(out, NotFollowed::MonitorList, format!("TG {} not followed: not on the monitor list", g.tg));
        }
        let Some(route) = self.routing.route(g.tg) else {
            return refuse(out, NotFollowed::SpeakerOff, format!("TG {} not followed: not on a speaker", g.tg));
        };
        let Some(freq) = g.channel.freq_hz else {
            return refuse(out, NotFollowed::UnknownLcn, format!("TG {} not followed: channel {} not in the channel plan", g.tg, label(g)));
        };
        if !in_window(freq) {
            return refuse(out, NotFollowed::OutOfBand, format!("TG {} not followed: {:.5} MHz is outside the receive window", g.tg, freq as f64 / 1e6));
        }
        // The system reassigned a lane's channel: its call is over.
        for l in &mut self.lanes {
            if l.locked.is_some_and(|k| k.tg != g.tg && k.channel.freq_hz.is_some() && k.channel == channel) {
                out.notes.push(format!("{}: channel {} now carries TG {}, TG {} ended", l.lane, label(g), g.tg, l.locked.map_or(0, |k| k.tg)));
                l.locked = None;
                out.commands.push(Command::Release { lane: l.lane });
            }
        }
        if g.encrypted || self.encrypted.contains(&g.tg) {
            for l in &mut self.lanes {
                if l.locked.is_some_and(|k| k.tg == g.tg) {
                    l.locked = None;
                    out.commands.push(Command::Pause { lane: l.lane });
                }
            }
            return refuse(out, NotFollowed::Encrypted, format!("TG {} not followed: encrypted", g.tg));
        }
        let views = self.views(markers);
        let choice = choose_lane(g.tg, route.side, Some(freq), &views, &self.routing, |t, m| end_marker_frees(t, m, now));
        let lane = match choice {
            LaneChoice::Reject => {
                let cands = candidates(route.side, Some(freq), &views, &self.routing);
                for l in self.lanes.iter_mut().filter(|l| cands.contains(&l.lane)) {
                    l.last_busy = Some((g.tg, freq, now));
                }
                let busy: Vec<String> = views.iter().filter_map(|v| v.locked_tg.map(|t| format!("{} on TG {t}", v.lane))).collect();
                return refuse(out, NotFollowed::Busy, format!("TG {} not followed: busy ({})", g.tg, busy.join(", ")));
            }
            LaneChoice::Preempt(lane, why) => {
                let i = self.index(lane).expect("a chosen lane");
                let prev = self.lanes[i].locked.map_or(0, |k| k.tg);
                let why = match why {
                    Preemption::EndMarker => "its transmission ended",
                    Preemption::Priority => "a higher-priority group",
                };
                out.notes.push(format!("{lane}: TG {} takes over from TG {prev} ({why})", g.tg));
                self.lanes[i].locked = None;
                out.commands.push(Command::Release { lane });
                lane
            }
            LaneChoice::Stay(lane) | LaneChoice::Take(lane) => lane,
        };
        let i = self.index(lane).expect("a chosen lane");
        let l = &mut self.lanes[i];
        let already = l.locked.is_some_and(|k| k.tg == g.tg && k.channel == channel);
        if !already {
            l.locked = Some(Locked { tg: g.tg, channel, call: None });
            l.tuned_hz = Some(freq);
            out.commands.push(Command::Follow { lane, channel });
        }
        out.record = record(Decision::Followed(lane));
    }

    /// The call book opened `call` on `lane`.
    pub fn opened(&mut self, lane: Lane, call: CallId) {
        if let Some(l) = self.lanes.iter_mut().find(|l| l.lane == lane) {
            if let Some(k) = l.locked.as_mut() {
                k.call = Some(call);
            }
        }
    }

    /// The call book closed a call: its lane is released when that was the lane's call.
    pub fn closed(&mut self, c: &Closed, now: Instant) -> Option<Command> {
        let l = self.lanes.iter_mut().find(|l| Some(l.lane) == c.lane)?;
        let k = l.locked?;
        if k.call.is_some_and(|id| id != c.call) {
            return None;
        }
        l.last_timeout = match (c.reason, k.channel.freq_hz) {
            (CloseReason::Timeout, Some(f)) => Some((k.tg, f, now)),
            _ => None,
        };
        l.locked = None;
        Some(Command::Release { lane: l.lane })
    }

    /// Every few seconds: a lane locked while the call book has no call on it (a lost close) is
    /// released on the second look.
    pub fn stuck_check(&mut self, has_call: &dyn Fn(Lane) -> bool) -> Vec<Command> {
        let mut out = Vec::new();
        for l in &mut self.lanes {
            if l.locked.is_none() || has_call(l.lane) {
                l.stuck_looks = 0;
                continue;
            }
            l.stuck_looks += 1;
            if l.stuck_looks >= 2 {
                l.stuck_looks = 0;
                l.locked = None;
                out.push(Command::Release { lane: l.lane });
            }
        }
        out
    }

    /// The lane was tuned elsewhere (a preset change, a recentre): where it is now.
    pub fn retuned(&mut self, lane: Lane, tuned_hz: Option<u64>) {
        if let Some(l) = self.lanes.iter_mut().find(|l| l.lane == lane) {
            l.tuned_hz = tuned_hz;
        }
    }
}

fn update(g: &Grant, channel: ChannelKey) -> Record {
    let number = match g.channel.id {
        ChannelId::P25 { number, .. } => number,
        ChannelId::DmrLcn(lcn) => lcn,
    };
    Record::Update { tg: g.tg, channel, channel_number: number }
}

/// The channel as the control channel names it.
fn label(g: &Grant) -> String {
    match g.channel.id {
        ChannelId::P25 { iden, number } => format!("{iden}-{number}"),
        ChannelId::DmrLcn(lcn) => match g.channel.slot {
            Some(ts) => format!("LCN {lcn} TS{ts}"),
            None => format!("LCN {lcn}"),
        },
    }
}

#[cfg(test)]
#[path = "follow_tests.rs"]
mod tests;
