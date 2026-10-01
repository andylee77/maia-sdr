//! The live site's trunking: one task owning the follower, the call book and each lane's traffic
//! decoder. Grants come from the control receivers; dibits and gateware NIDs from the lanes; a
//! 100 ms tick drives the call book's timers. The follower's commands move the lanes through the
//! tuner, and the call book's events go to the event log and the calls view.
//!
//! Voice is attributed by air time: a frame aired before the lane's current call opened belongs
//! to the call before it. Dibits aired before the lane's last retune are dropped (they are the old
//! channel's).

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::mpsc;

use super::calls::{CallBook, CallEvent, CallId, CallPolicy, Closed, Opened, SourceVia};
use super::follow::routing::Routing;
use super::follow::{Command, Follower, Record};
use crate::hardware::p25core::rings::mono_instant;
use crate::hardware::p25core::Lane;
use crate::protocol::events::{Grant, TrafficEvent};
use crate::protocol::p25::traffic::{CallContext, P25Traffic};
use crate::radio::streams::{LaneInput, StreamSource};
use crate::radio::tuner::{RadioHw, Tuner};
use crate::services::events::EventLog;
use crate::util::time::Stamp;

/// Inputs queued for the task.
const QUEUE: usize = 1024;
/// Closed calls kept for the calls view.
const RECENT: usize = 100;
const TICK: Duration = Duration::from_millis(100);
const STUCK_CHECK: Duration = Duration::from_secs(5);
/// A lane that carried voice this recently resumes on the same channel without a reset.
const COAST_MAX_IDLE: Duration = Duration::from_secs(1);

/// What the trunking task receives.
#[derive(Debug)]
pub enum TrunkInput {
    /// A grant or grant update, with the control channel's NAC.
    Grant { grant: Grant, nac: u16, at: Stamp },
    Lane(LaneInput),
}

pub type TrunkTx = mpsc::Sender<TrunkInput>;

/// What the trunking task is started with.
pub struct Setup {
    pub site: String,
    pub lanes: Vec<Lane>,
    pub routing: Routing,
    pub encrypted: HashSet<u32>,
    pub policy: CallPolicy,
    pub first_call: CallId,
}

/// A call as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct CallView {
    pub call: CallId,
    pub site: String,
    pub tg: u32,
    pub source: Option<u32>,
    pub speaker: Option<u32>,
    pub freq_hz: Option<u64>,
    pub slot: Option<u8>,
    pub channel: Option<String>,
    pub encrypted: bool,
    pub not_followed: Option<&'static str>,
    pub lane: Option<u8>,
    pub started_unix_ms: u64,
    pub ended_unix_ms: Option<u64>,
    pub close: Option<&'static str>,
    pub end_lc: Option<&'static str>,
    pub sources: Vec<u32>,
    pub voice_frames: u64,
}

/// Open calls and the newest closed ones.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CallsView {
    pub open: Vec<CallView>,
    pub recent: Vec<CallView>,
}

struct LaneSlot {
    lane: Lane,
    traffic: P25Traffic,
    /// The lane's calls, newest last: (call, opened at).
    calls: VecDeque<(CallId, Instant)>,
    /// Dibits aired before this are the old channel's.
    retuned_at: Option<Instant>,
    last_voice: Option<Instant>,
    tuned_hz: Option<u64>,
}

struct Task<H> {
    book: CallBook,
    follower: Follower,
    lanes: Vec<LaneSlot>,
    tuner: Arc<Tuner<H>>,
    log: Arc<EventLog>,
    view: Arc<Mutex<CallsView>>,
    open: Vec<Opened>,
    recent: VecDeque<CallView>,
    last_stuck_check: Instant,
}

pub struct Trunking {
    view: Arc<Mutex<CallsView>>,
    running: tokio::sync::Mutex<Option<Running>>,
}

struct Running {
    tx: TrunkTx,
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    sources: Vec<tokio::task::JoinHandle<()>>,
}

impl Default for Trunking {
    fn default() -> Self {
        Trunking { view: Arc::default(), running: tokio::sync::Mutex::new(None) }
    }
}

impl Trunking {
    /// Start following on the live site (stopping the previous site's trunking first).
    pub async fn start<H: RadioHw + StreamSource + Send + Sync + 'static>(&self, setup: Setup, tuner: Arc<Tuner<H>>, log: Arc<EventLog>) -> TrunkTx {
        self.stop().await;
        let (tx, rx) = mpsc::channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let (lane_tx, mut lane_rx) = mpsc::channel(QUEUE);
        let sources = tuner.hw().lane_streams(&setup.lanes, lane_tx, stop.clone());
        let forward = tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(input) = lane_rx.recv().await {
                if forward.send(TrunkInput::Lane(input)).await.is_err() {
                    return;
                }
            }
        });
        let mut sources = sources;
        sources.push(forwarder);
        let task = Task {
            book: CallBook::new(&setup.site, &setup.lanes, setup.policy, setup.first_call),
            follower: Follower::new(&setup.lanes, setup.routing, setup.encrypted),
            lanes: setup
                .lanes
                .iter()
                .map(|&lane| LaneSlot { lane, traffic: P25Traffic::new(), calls: VecDeque::new(), retuned_at: None, last_voice: None, tuned_hz: None })
                .collect(),
            tuner,
            log,
            view: self.view.clone(),
            open: Vec::new(),
            recent: VecDeque::new(),
            last_stuck_check: Instant::now(),
        };
        let task = tokio::spawn(task.run(rx, stop.clone()));
        *self.running.lock().await = Some(Running { tx: tx.clone(), stop, task, sources });
        tx
    }

    /// Stop following; the open calls close (`site_switch`).
    pub async fn stop(&self) {
        let Some(r) = self.running.lock().await.take() else { return };
        r.stop.store(true, Ordering::Relaxed);
        for s in &r.sources {
            s.abort();
        }
        drop(r.tx);
        let _ = r.task.await;
    }

    pub fn calls(&self) -> CallsView {
        self.view.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

fn lane_number(l: Option<Lane>) -> Option<u8> {
    l.map(|l| l.number())
}

impl<H: RadioHw + Send + Sync + 'static> Task<H> {
    async fn run(mut self, mut rx: mpsc::Receiver<TrunkInput>, stop: Arc<AtomicBool>) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                input = rx.recv() => match input {
                    Some(TrunkInput::Grant { grant, nac, at }) => self.grant(grant, nac, at).await,
                    Some(TrunkInput::Lane(LaneInput::Dibits { lane, bytes, first, reset, clock })) => {
                        self.dibits(lane, &bytes, first, reset, &clock);
                    }
                    Some(TrunkInput::Lane(LaneInput::Nid { lane, duid, nac, valid, at })) => self.nid(lane, duid, nac, valid, at),
                    None => break,
                },
                _ = tick.tick() => {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    self.tick().await;
                }
            }
            self.publish();
        }
        let mut out = Vec::new();
        self.book.site_switch("", Stamp::now(), &mut out);
        self.call_events(out);
        self.publish();
    }

    fn slot(&mut self, lane: Lane) -> Option<&mut LaneSlot> {
        self.lanes.iter_mut().find(|l| l.lane == lane)
    }

    async fn grant(&mut self, grant: Grant, nac: u16, at: Stamp) {
        let markers: Vec<(Lane, Option<(u32, Instant)>)> = self
            .lanes
            .iter()
            .map(|l| (l.lane, self.book.on_lane(l.lane).and_then(|c| c.end_marker().map(|m| (c.tg, m)))))
            .collect();
        let tuning = self.tuner.tuning();
        let outcome = self.follower.grant(
            &grant,
            nac,
            at.mono,
            &|lane| markers.iter().find(|m| m.0 == lane).and_then(|m| m.1),
            &|hz| tuning.in_window(hz),
        );
        for note in &outcome.notes {
            self.log.system("follow", note.clone());
        }
        for command in outcome.commands {
            self.command(command, at).await;
        }
        let mut out = Vec::new();
        match outcome.record {
            Some(Record::Grant(g)) => self.book.grant(g, at, &mut out),
            Some(Record::Update { tg, channel, channel_number }) => self.book.grant_update(tg, channel, channel_number, at),
            None => {}
        }
        self.call_events(out);
    }

    async fn command(&mut self, command: Command, at: Stamp) {
        match command {
            Command::Follow { lane, channel } => {
                let Some(freq) = channel.freq_hz else { return };
                let Some(slot) = self.slot(lane) else { return };
                let coast = slot.tuned_hz == Some(freq) && slot.last_voice.is_some_and(|t| at.mono.saturating_duration_since(t) <= COAST_MAX_IDLE);
                let moved = slot.tuned_hz != Some(freq);
                match self.tuner.retune_lane(lane, freq, !coast).await {
                    Ok(_) => {
                        let slot = self.slot(lane).expect("lane");
                        slot.tuned_hz = Some(freq);
                        if moved || !coast {
                            slot.retuned_at = Some(Instant::now());
                            slot.traffic.retuned();
                        }
                        self.follower.retuned(lane, Some(freq));
                    }
                    Err(e) => {
                        self.log.system("lane", format!("{lane} retune to {:.5} MHz failed: {e:#}", freq as f64 / 1e6));
                    }
                }
            }
            Command::Release { lane } => {
                if let Some(slot) = self.slot(lane) {
                    slot.traffic.release();
                }
            }
            Command::Pause { lane } => {
                if let Some(slot) = self.slot(lane) {
                    slot.traffic.release();
                }
                if let Err(e) = self.tuner.pause_lane(lane).await {
                    self.log.system("lane", format!("{lane} pause failed: {e:#}"));
                }
                if let Some(slot) = self.slot(lane) {
                    // A paused lane must be retuned (re-enabled) for its next call.
                    slot.tuned_hz = None;
                }
                self.follower.retuned(lane, None);
            }
        }
    }

    fn dibits(&mut self, lane: Lane, bytes: &[u8], first: u64, reset: bool, clock: &crate::radio::streams::dibit_ring::ClockView) {
        let now = Stamp::now();
        let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) else { return };
        if reset {
            slot.traffic.retuned();
        }
        let mut events = Vec::new();
        let mut index = first;
        for &b in bytes {
            for shift in [0, 2, 4, 6] {
                let air = clock.time_of(index).map(|us| mono_instant(us as u64)).unwrap_or(now.mono);
                index += 1;
                if slot.retuned_at.is_some_and(|t| air < t) {
                    continue;
                }
                let mut out = Vec::new();
                slot.traffic.push((b >> shift) & 3, air, now.mono, &mut out);
                events.extend(out.into_iter().map(|e| (air, e)));
            }
        }
        for (_, e) in events {
            self.traffic_event(lane, e, now);
        }
    }

    fn traffic_event(&mut self, lane: Lane, e: TrafficEvent, at: Stamp) {
        let mut out = Vec::new();
        match e {
            TrafficEvent::Voice { frames, encrypted, air } => {
                let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) else { return };
                slot.last_voice = Some(at.mono);
                if encrypted {
                    return;
                }
                // The call on the air when the frame was: the newest one opened before it.
                let call = slot.calls.iter().rev().find(|(_, opened)| *opened <= air).or(slot.calls.front()).map(|(id, _)| *id);
                for _ in 0..frames.len() {
                    self.book.voice(lane, call, air, at);
                }
            }
            TrafficEvent::Source(s) => self.book.link_control_source(lane, s, &mut out),
            TrafficEvent::TalkComplete(Some(s)) => self.book.talk_complete_source(lane, s, &mut out),
            TrafficEvent::TalkComplete(None) => {}
            TrafficEvent::End { lc, air } => {
                if let Some(call) = self.slot(lane).and_then(|s| s.traffic.call()) {
                    self.book.voice_end(lane, call.call, air, lc, at);
                }
            }
            TrafficEvent::Header { .. } => {}
            TrafficEvent::Message(line) => self.log.message("p25", at.unix_ms, &line),
        }
        self.call_events(out);
    }

    fn nid(&mut self, lane: Lane, duid: u8, nac: u16, valid: bool, at: Stamp) {
        // HDU, LDU1, LDU2.
        if !valid || !matches!(duid, 0x0 | 0x5 | 0xA) {
            return;
        }
        let mut out = Vec::new();
        self.book.nid(lane, true, at, &mut out);
        if duid == 0x0 && self.book.on_lane(lane).is_some() {
            self.book.hdu(lane, nac, at, &mut out);
        }
        self.call_events(out);
    }

    async fn tick(&mut self) {
        let at = Stamp::now();
        let mut out = Vec::new();
        self.book.tick(at, &mut out);
        self.call_events(out);
        if at.mono.saturating_duration_since(self.last_stuck_check) >= STUCK_CHECK {
            self.last_stuck_check = at.mono;
            let has: Vec<Lane> = self.lanes.iter().map(|l| l.lane).filter(|&l| self.book.on_lane(l).is_some()).collect();
            for command in self.follower.stuck_check(&|l| has.contains(&l)) {
                self.command(command, at).await;
            }
        }
    }

    /// The call book's events: the follower and the lanes learn of opens and closes; the log and
    /// the view get each call.
    fn call_events(&mut self, events: Vec<CallEvent>) {
        let now = Instant::now();
        for e in events {
            match e {
                CallEvent::Opened(o) => {
                    if let Some(lane) = o.lane {
                        self.follower.opened(lane, o.call);
                        if let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) {
                            slot.traffic.follow(CallContext { call: o.call, tg: o.tg, source: o.source, encrypted: o.encrypted });
                            slot.calls.push_back((o.call, now));
                            while slot.calls.len() > 4 {
                                slot.calls.pop_front();
                            }
                        }
                    }
                    self.log.system("call", opened_text(&o));
                    self.open.push(o);
                }
                CallEvent::Closed(c) => {
                    if let Some(Command::Release { lane }) = self.follower.closed(&c, now) {
                        if let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) {
                            if slot.traffic.call().is_some_and(|k| k.call == c.call) {
                                slot.traffic.release();
                            }
                        }
                    }
                    if let Some(i) = self.open.iter().position(|o| o.call == c.call) {
                        let o = self.open.remove(i);
                        if o.lane.is_some() {
                            self.log.system("call", closed_text(&o, &c));
                        }
                        self.recent.push_front(view_of(&o, Some(&c)));
                        self.recent.truncate(RECENT);
                    }
                }
                CallEvent::Source { call, lane: Some(lane), source, via: SourceVia::CcRefresh } => {
                    if let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) {
                        if slot.traffic.call().is_some_and(|k| k.call == call) {
                            slot.traffic.set_source(source);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn publish(&self) {
        let mut open: Vec<CallView> = Vec::new();
        for o in &self.open {
            let mut v = view_of(o, None);
            let call = o.lane.and_then(|l| self.book.on_lane(l)).filter(|c| c.id == o.call);
            if let Some(c) = call {
                v.source = c.source;
                v.speaker = c.speaker;
                v.sources = c.sources.clone();
                v.voice_frames = c.voice_frames;
                v.end_lc = c.end_lc();
            }
            open.push(v);
        }
        if let Ok(mut view) = self.view.lock() {
            view.open = open;
            view.recent = self.recent.iter().cloned().collect();
        }
    }
}

fn view_of(o: &Opened, c: Option<&Closed>) -> CallView {
    CallView {
        call: o.call,
        site: o.site.clone(),
        tg: o.tg,
        source: c.map_or(o.source, |c| c.source),
        speaker: c.and_then(|c| c.speaker),
        freq_hz: o.channel.freq_hz,
        slot: o.channel.slot,
        channel: o.channel_label.clone(),
        encrypted: o.encrypted,
        not_followed: o.not_followed.map(|n| n.as_str()),
        lane: lane_number(o.lane),
        started_unix_ms: o.at_unix_ms,
        ended_unix_ms: c.map(|c| c.ended_unix_ms),
        close: c.map(|c| match c.reason {
            super::calls::CloseReason::Timeout => "timeout",
            super::calls::CloseReason::CallEnd => "call_end",
            super::calls::CloseReason::TgChange => "tg_change",
            super::calls::CloseReason::SiteSwitch => "site_switch",
        }),
        end_lc: c.and_then(|c| c.end_lc),
        sources: c.map(|c| c.sources.clone()).unwrap_or_default(),
        voice_frames: c.map_or(0, |c| c.voice_frames),
    }
}

fn opened_text(o: &Opened) -> String {
    let freq = o.channel.freq_hz.map_or("? MHz".to_string(), |f| format!("{:.5} MHz", f as f64 / 1e6));
    let src = o.source.map_or(String::new(), |s| format!(" from {s}"));
    match (o.lane, o.not_followed) {
        (Some(lane), _) => format!("call {} TG {}{src} on {freq}, {lane}", o.call, o.tg),
        (None, Some(why)) => format!("call {} TG {}{src} on {freq} not followed ({})", o.call, o.tg, why.as_str()),
        (None, None) => format!("call {} TG {}{src} on {freq}", o.call, o.tg),
    }
}

fn closed_text(o: &Opened, c: &Closed) -> String {
    let secs = c.open_ms as f64 / 1e3;
    format!("call {} TG {} ended after {secs:.1} s ({:?}), {} voice frames", o.call, o.tg, c.reason, c.voice_frames)
}
