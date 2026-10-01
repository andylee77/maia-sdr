//! The live site's trunking: one task owning the follower, the call book and each lane's traffic
//! decoder. Grants come from the control receivers; dibits and gateware NIDs from the lanes; a
//! 100 ms tick drives the call book's timers. The follower's commands move the lanes through the
//! tuner, and the call book's events go to the event log and the calls view.
//!
//! Voice is attributed by air time: a frame aired before the lane's current call opened belongs
//! to the call before it, unless that call's transmission had already ended. Dibits aired before
//! the lane's last retune are dropped (they are the old channel's).

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::mpsc;

use super::calls::{CallBook, CallEvent, CallId, CallPolicy, Closed, Opened, SourceVia};
use super::follow::routing::Routing;
use super::follow::{Command, Follower, Record};
use crate::audio::live::{Audio, VoiceBatch};
use crate::services::history::store::CallRow;
use crate::services::history::HistoryTx;
use crate::services::notices::{Notice, Notices};
use crate::services::packet_data::PacketData;
use crate::services::recordings::{CallEnd, CallStart, RecorderTx};
use crate::hardware::p25core::rings::mono_instant;
use crate::hardware::p25core::Lane;
use crate::protocol::dmr::demod::DmrDemodStats;
use crate::protocol::dmr::traffic::{DmrCall, DmrTraffic};
use crate::protocol::p25::framer::FramerStats;
use crate::protocol::events::{Grant, TrafficEvent, VoiceFrames};
use crate::protocol::p25::traffic::{CallContext, P25Traffic};
use crate::radio::streams::{LaneInput, LaneMode, StreamSource};
use crate::services::config::systems::Protocol;
use crate::radio::tuner::{RadioHw, Tuner};
use crate::services::events::EventLog;
use crate::trunking::learned::Learned;
use crate::util::time::Stamp;

/// Inputs queued for the task.
const QUEUE: usize = 1024;
/// Closed calls kept for the calls view.
pub const RECENT: usize = 100;
/// After a failed try for the data channel, before the next.
const DATA_PARK_RETRY: Duration = Duration::from_secs(10);
/// Raw IMBE frames kept for `/api/imbe_dump` (the bench compares them with the transmitted ones).
const FRAME_RING: usize = 128;

/// A raw IMBE frame as the lane decoded it, with the talkgroup of its lane's call.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub tg: u32,
    pub encrypted: bool,
    pub bits: [u8; 18],
}

pub type FrameRing = Arc<Mutex<VecDeque<RawFrame>>>;
const TICK: Duration = Duration::from_millis(100);
const STUCK_CHECK: Duration = Duration::from_secs(5);
/// A lane that carried voice this recently resumes on the same channel without a reset.
const COAST_MAX_IDLE: Duration = Duration::from_secs(1);
/// IQ received this soon after a lane's retune may be the old channel's (a sub-buffer is ~164 ms).
const IQ_SETTLE: Duration = Duration::from_millis(200);

/// What the trunking task receives.
#[derive(Debug)]
pub enum TrunkInput {
    /// A grant or grant update, with the control channel's NAC.
    Grant { grant: Grant, nac: u16, at: Stamp },
    Lane(LaneInput),
    /// The live site's profile changed.
    Routing(Box<Routing>),
    /// The receive window moved: every lane was reloaded and holds no channel.
    WindowMoved,
}

pub type TrunkTx = mpsc::Sender<TrunkInput>;

/// What the trunking task is started with.
pub struct Setup {
    pub site: String,
    pub protocol: Protocol,
    /// DMR: logical channel numbers to downlink Hz.
    pub lcn_hz: std::collections::HashMap<u16, u64>,
    pub lanes: Vec<Lane>,
    pub routing: Routing,
    pub encrypted: HashSet<u32>,
    pub policy: CallPolicy,
    /// Grant counts and encrypted talkgroups go into the site's learned state.
    pub learned: Option<Arc<Learned>>,
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
    pub not_followed: Option<String>,
    pub lane: Option<u8>,
    pub started_unix_ms: u64,
    pub ended_unix_ms: Option<u64>,
    pub close: Option<String>,
    pub end_lc: Option<String>,
    pub sources: Vec<u32>,
    pub voice_frames: u64,
}

/// A lane as the trunking task sees it.
#[derive(Debug, Clone, Serialize)]
pub struct LaneStatus {
    pub lane: u8,
    /// The channel the lane is tuned to.
    pub tuned_hz: Option<u64>,
    /// The call it carries.
    pub call: Option<CallId>,
    /// The talkgroup the follower keeps it for.
    pub following_tg: Option<u32>,
    /// Waiting on the site's data channel between calls.
    pub on_data_channel: bool,
    /// Voice frames decoded on it since the site went live.
    pub voice_frames: u64,
    pub last_voice_ms_ago: Option<u64>,
    pub counters: LaneCounters,
}

/// A lane's traffic decoder's counters.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum LaneCounters {
    P25(Box<FramerStats>),
    Dmr { bursts: u64, demod: DmrDemodStats },
}

/// Open calls and the newest closed ones.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CallsView {
    pub open: Vec<CallView>,
    pub recent: Vec<CallView>,
}

/// A lane's traffic decoder.
enum Decoder {
    P25(P25Traffic),
    Dmr(Box<DmrTraffic>),
}

impl Decoder {
    fn release(&mut self) {
        match self {
            Decoder::P25(t) => t.release(),
            Decoder::Dmr(t) => t.release(),
        }
    }

    fn retuned(&mut self) {
        match self {
            Decoder::P25(t) => t.retuned(),
            Decoder::Dmr(t) => t.retuned(),
        }
    }

    /// The call the decoder follows.
    fn call(&self) -> Option<CallId> {
        match self {
            Decoder::P25(t) => t.call().map(|c| c.call),
            Decoder::Dmr(t) => t.call().map(|c| c.call),
        }
    }
}

/// A call a lane followed, for attributing voice by air time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LaneCall {
    id: CallId,
    opened: Instant,
    /// When its end-of-transmission marker aired.
    ended_air: Option<Instant>,
}

/// The call a voice frame aired at `air` belongs to: the newest one opened before it, unless
/// that call's transmission had ended by then (a new talker's first frames can air before the
/// grant that names them is decoded), when it is the next one.
fn owner(calls: &VecDeque<LaneCall>, air: Instant) -> Option<CallId> {
    let i = calls.iter().rposition(|c| c.opened <= air).unwrap_or(0);
    let c = calls.get(i)?;
    match calls.get(i + 1) {
        Some(next) if c.ended_air.is_some_and(|e| e <= air) => Some(next.id),
        _ => Some(c.id),
    }
}

struct LaneSlot {
    lane: Lane,
    traffic: Decoder,
    /// The lane's calls, newest last.
    calls: VecDeque<LaneCall>,
    /// Dibits aired before this are the old channel's.
    retuned_at: Option<Instant>,
    last_voice: Option<Instant>,
    tuned_hz: Option<u64>,
    voice_frames: u64,
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
    learned: Option<Arc<Learned>>,
    audio: Arc<Audio>,
    recorder: RecorderTx,
    history: HistoryTx,
    notices: Notices,
    frames: FrameRing,
    /// The voice codec of the protocol, for the history.
    codec: &'static str,
    next_call: Arc<AtomicU64>,
    site: String,
    packet_data: Option<Arc<PacketData>>,
    /// The last time the last lane tried for the data channel.
    data_park_tried: Option<Instant>,
    lanes_view: Arc<Mutex<Vec<LaneStatus>>>,
    lanes_published: Instant,
}

pub struct Trunking {
    view: Arc<Mutex<CallsView>>,
    audio: Arc<Audio>,
    recorder: RecorderTx,
    history: HistoryTx,
    notices: Notices,
    frames: FrameRing,
    /// Call ids keep rising across site switches.
    next_call: Arc<AtomicU64>,
    /// Takes the PDUs a lane reads.
    packet_data: std::sync::Mutex<Option<Arc<PacketData>>>,
    lanes: Arc<Mutex<Vec<LaneStatus>>>,
    running: tokio::sync::Mutex<Option<Running>>,
}

struct Running {
    tx: TrunkTx,
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    sources: Vec<tokio::task::JoinHandle<()>>,
}

impl Trunking {
    /// `first_call` continues past the ids already stored (recordings, history).
    pub fn new(audio: Arc<Audio>, recorder: RecorderTx, history: HistoryTx, notices: Notices, first_call: CallId) -> Self {
        Trunking {
            view: Arc::default(),
            audio,
            recorder,
            history,
            notices,
            frames: Arc::default(),
            next_call: Arc::new(AtomicU64::new(first_call.max(1))),
            packet_data: std::sync::Mutex::new(None),
            lanes: Arc::default(),
            running: tokio::sync::Mutex::new(None),
        }
    }

    /// Each lane of the live site, updated each second.
    pub fn lanes(&self) -> Vec<LaneStatus> {
        self.lanes.lock().map(|l| l.clone()).unwrap_or_default()
    }

    pub fn set_packet_data(&self, data: Arc<PacketData>) {
        if let Ok(mut d) = self.packet_data.lock() {
            *d = Some(data);
        }
    }

    /// Start following on the live site (stopping the previous site's trunking first).
    pub async fn start<H: RadioHw + StreamSource + Send + Sync + 'static>(&self, setup: Setup, tuner: Arc<Tuner<H>>, log: Arc<EventLog>) -> TrunkTx {
        self.stop().await;
        let (tx, rx) = mpsc::channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let (lane_tx, mut lane_rx) = mpsc::channel(QUEUE);
        let mode = match setup.protocol {
            Protocol::P25 => LaneMode::Dibits,
            Protocol::DmrTier3 => LaneMode::Iq,
        };
        let sources = tuner.hw().lane_streams(&setup.lanes, mode, lane_tx, stop.clone());
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
            book: CallBook::new(&setup.site, &setup.lanes, setup.policy, self.next_call.load(Ordering::Relaxed)),
            follower: Follower::new(&setup.lanes, setup.routing, setup.encrypted),
            lanes: setup
                .lanes
                .iter()
                .map(|&lane| LaneSlot {
                    lane,
                    traffic: match setup.protocol {
                        Protocol::P25 => Decoder::P25(P25Traffic::new(lane.name())),
                        Protocol::DmrTier3 => Decoder::Dmr(Box::new(DmrTraffic::new(setup.lcn_hz.clone()))),
                    },
                    calls: VecDeque::new(),
                    retuned_at: None,
                    last_voice: None,
                    tuned_hz: None,
                    voice_frames: 0,
                })
                .collect(),
            tuner,
            log,
            view: self.view.clone(),
            open: Vec::new(),
            // The newest calls stay listed across a switch.
            recent: self.view.lock().map(|v| v.recent.iter().cloned().collect()).unwrap_or_default(),
            last_stuck_check: Instant::now(),
            lanes_view: self.lanes.clone(),
            lanes_published: Instant::now(),
            learned: setup.learned.clone(),
            audio: self.audio.clone(),
            recorder: self.recorder.clone(),
            history: self.history.clone(),
            notices: self.notices.clone(),
            frames: self.frames.clone(),
            codec: match setup.protocol {
                Protocol::P25 => "imbe",
                Protocol::DmrTier3 => "ambe2",
            },
            next_call: self.next_call.clone(),
            site: setup.site.clone(),
            packet_data: self.packet_data.lock().ok().and_then(|d| d.clone()),
            data_park_tried: None,
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
        if let Ok(mut l) = self.lanes.lock() {
            l.clear();
        }
    }

    /// A new profile for the calls to come (the open ones keep their lanes).
    pub async fn set_routing(&self, routing: Routing) {
        let tx = self.running.lock().await.as_ref().map(|r| r.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(TrunkInput::Routing(Box::new(routing))).await;
        }
    }

    /// The receive window moved under idle lanes (a recentre).
    pub async fn window_moved(&self) {
        let tx = self.running.lock().await.as_ref().map(|r| r.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(TrunkInput::WindowMoved).await;
        }
    }

    /// List the newest calls of the history (boot), newest first.
    pub fn seed_recent(&self, rows: Vec<CallRow>) {
        let recent = rows
            .into_iter()
            .take(RECENT)
            .map(|r| CallView {
                call: r.call_id,
                site: r.site,
                tg: r.tg,
                source: r.source,
                speaker: None,
                freq_hz: r.freq_hz,
                slot: r.timeslot,
                channel: r.channel,
                encrypted: r.encrypted,
                not_followed: r.not_followed,
                lane: (r.lane > 0).then_some(r.lane),
                started_unix_ms: r.started_ms,
                ended_unix_ms: Some(r.ended_ms),
                close: Some(r.close_reason),
                end_lc: r.end_kind,
                sources: r.sources,
                voice_frames: r.voice_ms / 20,
            })
            .collect();
        if let Ok(mut v) = self.view.lock() {
            v.recent = recent;
        }
    }

    /// The newest raw IMBE frames, oldest first.
    pub fn frames(&self) -> Vec<RawFrame> {
        self.frames.lock().map(|f| f.iter().cloned().collect()).unwrap_or_default()
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
                    Some(TrunkInput::Lane(LaneInput::Iq { lane, iq, at })) => self.iq(lane, &iq, at),
                    Some(TrunkInput::Routing(r)) => self.follower.set_routing(*r),
                    Some(TrunkInput::WindowMoved) => self.window_moved(),
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
        if grant.encrypted {
            if let Some(l) = &self.learned {
                l.encrypted(grant.tg);
            }
        }
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
                let moved = slot.tuned_hz != Some(freq);
                let since_voice = slot.last_voice.map(|t| at.mono.saturating_duration_since(t));
                let coast = !moved && !resume_needs_reset(self.tuner.lane_pll(lane).await, since_voice);
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

    fn window_moved(&mut self) {
        for slot in &mut self.lanes {
            slot.traffic.retuned();
            slot.tuned_hz = None;
            slot.retuned_at = Some(Instant::now());
            self.follower.retuned(slot.lane, None);
        }
    }

    fn dibits(&mut self, lane: Lane, bytes: &[u8], first: u64, reset: bool, clock: &crate::radio::streams::dibit_ring::ClockView) {
        let now = Stamp::now();
        let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) else { return };
        let Decoder::P25(traffic) = &mut slot.traffic else { return };
        if reset {
            traffic.retuned();
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
                traffic.push((b >> shift) & 3, air, now.mono, &mut out);
                events.extend(out.into_iter().map(|e| (air, e)));
            }
        }
        for (_, e) in events {
            self.traffic_event(lane, e, now);
        }
    }

    /// Lane one's IQ at a DMR site.
    fn iq(&mut self, lane: Lane, iq: &[i16], at: Stamp) {
        let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) else { return };
        let Decoder::Dmr(traffic) = &mut slot.traffic else { return };
        // A sub-buffer read just after a retune can still hold the old channel's samples.
        if traffic.call().is_none() || slot.retuned_at.is_some_and(|t| at.mono < t + IQ_SETTLE) {
            return;
        }
        let mut out = Vec::new();
        traffic.push(iq, at.mono, &mut out);
        for e in out {
            self.traffic_event(lane, e, at);
        }
    }

    fn traffic_event(&mut self, lane: Lane, e: TrafficEvent, at: Stamp) {
        let mut out = Vec::new();
        match e {
            TrafficEvent::Voice { frames, encrypted, air } => {
                if let (VoiceFrames::Imbe(f), Ok(mut ring)) = (&frames, self.frames.lock()) {
                    let tg = self.book.on_lane(lane).map_or(0, |c| c.tg);
                    for x in f {
                        ring.push_back(RawFrame { tg, encrypted, bits: x.bits });
                    }
                    while ring.len() > FRAME_RING {
                        ring.pop_front();
                    }
                }
                let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) else { return };
                slot.last_voice = Some(at.mono);
                slot.voice_frames += frames.len() as u64;
                if encrypted {
                    return;
                }
                let call = owner(&slot.calls, air);
                for _ in 0..frames.len() {
                    self.book.voice(lane, call, air, at);
                }
                // Voice of the lane's current call goes to the speakers.
                let current = self.book.on_lane(lane).filter(|c| Some(c.id) == call);
                if let Some(c) = current {
                    let speaker = self.follower.speaker(c.tg);
                    self.audio.voice(VoiceBatch { lane, call: c.id, tg: c.tg, source: c.speaker.or(c.source), speaker, frames });
                }
            }
            TrafficEvent::Source(s) => self.book.link_control_source(lane, s, &mut out),
            TrafficEvent::TalkComplete(Some(s)) => self.book.talk_complete_source(lane, s, &mut out),
            TrafficEvent::TalkComplete(None) => {}
            TrafficEvent::End { lc, air } => {
                let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) else { return };
                if let Some(call) = slot.traffic.call() {
                    if let Some(c) = slot.calls.iter_mut().find(|c| c.id == call) {
                        c.ended_air = Some(air);
                    }
                    self.book.voice_end(lane, call, air, lc, at);
                }
            }
            TrafficEvent::Pdu(frame) => {
                if let Some(d) = &self.packet_data {
                    d.pdu(&frame, &self.site);
                }
            }
            TrafficEvent::Message(line) => {
                let source = match self.slot(lane).map(|s| &s.traffic) {
                    Some(Decoder::Dmr(_)) => "dmr",
                    _ => "p25",
                };
                self.log.message(source, at.unix_ms, &line);
            }
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
        self.park_on_data(at).await;
        if at.mono.saturating_duration_since(self.lanes_published) >= Duration::from_secs(1) {
            self.lanes_published = at.mono;
            self.publish_lanes(at.mono);
        }
    }

    fn publish_lanes(&self, now: Instant) {
        let following = self.follower.locked();
        let data_hz = self.learned.as_ref().and_then(|l| l.data_channel_hz());
        let lanes = self
            .lanes
            .iter()
            .map(|s| LaneStatus {
                lane: s.lane.number(),
                tuned_hz: s.tuned_hz,
                call: self.book.on_lane(s.lane).map(|c| c.id),
                following_tg: following.iter().find(|(l, _)| *l == s.lane).and_then(|(_, tg)| *tg),
                on_data_channel: data_hz.is_some() && s.tuned_hz == data_hz,
                voice_frames: s.voice_frames,
                last_voice_ms_ago: s.last_voice.map(|t| now.saturating_duration_since(t).as_millis() as u64),
                counters: match &s.traffic {
                    Decoder::P25(t) => LaneCounters::P25(Box::new(t.stats().clone())),
                    Decoder::Dmr(t) => LaneCounters::Dmr { bursts: t.bursts, demod: t.demod_stats() },
                },
            })
            .collect();
        if let Ok(mut v) = self.lanes_view.lock() {
            *v = lanes;
        }
    }

    /// At a P25 site with two lanes, the last one waits for its next call on the data channel the
    /// site announces, where packet data flows. A voice grant still takes it.
    async fn park_on_data(&mut self, at: Stamp) {
        if self.lanes.len() < 2 || self.data_park_tried.is_some_and(|t| at.mono.saturating_duration_since(t) < DATA_PARK_RETRY) {
            return;
        }
        let Some(data_hz) = self.learned.as_ref().and_then(|l| l.data_channel_hz()) else { return };
        let Some(slot) = self.lanes.last() else { return };
        let lane = slot.lane;
        let idle = matches!(slot.traffic, Decoder::P25(_))
            && slot.tuned_hz != Some(data_hz)
            && self.book.on_lane(lane).is_none()
            && self.follower.idle(lane);
        if !idle || !self.tuner.tuning().in_window(data_hz) {
            return;
        }
        self.data_park_tried = Some(at.mono);
        match self.tuner.retune_lane(lane, data_hz, true).await {
            Ok(_) => {
                let slot = self.slot(lane).expect("lane");
                slot.traffic.retuned();
                slot.tuned_hz = Some(data_hz);
                slot.retuned_at = Some(Instant::now());
                self.follower.retuned(lane, Some(data_hz));
                self.log.system("lane", format!("{lane} waits on the data channel, {:.4} MHz", data_hz as f64 / 1e6));
            }
            Err(e) => self.log.system("lane", format!("{lane} could not wait on the data channel: {e:#}")),
        }
    }

    /// The call book's events: the follower and the lanes learn of opens and closes; the log and
    /// the view get each call.
    fn call_events(&mut self, events: Vec<CallEvent>) {
        let now = Instant::now();
        for e in events {
            match e {
                CallEvent::Opened(o) => {
                    self.next_call.fetch_max(o.call + 1, Ordering::Relaxed);
                    self.notices.send(Notice::CallOpened { call: o.call, tg: o.tg, followed: o.lane.is_some() });
                    if let Some(lane) = o.lane {
                        self.recorder.start(CallStart {
                            call: o.call,
                            site: o.site.clone(),
                            tg: o.tg,
                            source: o.source,
                            lane,
                            freq_hz: o.channel.freq_hz,
                            channel: o.channel_label.clone(),
                            started_unix_ms: o.at_unix_ms,
                        });
                        self.follower.opened(lane, o.call);
                        if let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) {
                            match &mut slot.traffic {
                                Decoder::P25(t) => t.follow(CallContext { call: o.call, tg: o.tg, source: o.source, encrypted: o.encrypted }),
                                Decoder::Dmr(t) => t.follow(DmrCall { call: o.call, tg: o.tg, slot: o.channel.slot.unwrap_or(1), encrypted: o.encrypted }),
                            }
                            slot.calls.push_back(LaneCall { id: o.call, opened: now, ended_air: None });
                            while slot.calls.len() > 4 {
                                slot.calls.pop_front();
                            }
                        }
                    }
                    self.log.system("call", opened_text(&o));
                    if let (Some(l), Some(f)) = (&self.learned, o.channel.freq_hz) {
                        l.grant(f);
                    }
                    self.open.push(o);
                }
                CallEvent::Closed(c) => {
                    if c.lane.is_some() {
                        self.recorder.end(CallEnd { call: c.call, source: c.source, sources: c.sources.clone() });
                    }
                    if let Some(Command::Release { lane }) = self.follower.closed(&c, now) {
                        if let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) {
                            if slot.traffic.call() == Some(c.call) {
                                slot.traffic.release();
                            }
                        }
                    }
                    if let Some(i) = self.open.iter().position(|o| o.call == c.call) {
                        let o = self.open.remove(i);
                        if o.lane.is_some() {
                            self.log.system("call", closed_text(&o, &c));
                        }
                        self.history.call(call_row(&o, &c, self.codec));
                        self.notices.send(Notice::CallClosed { call: o.call, tg: o.tg });
                        self.recent.push_front(view_of(&o, Some(&c)));
                        self.recent.truncate(RECENT);
                    }
                }
                CallEvent::Source { call, lane: Some(lane), source, via: SourceVia::CcRefresh } => {
                    if let Some(slot) = self.lanes.iter_mut().find(|l| l.lane == lane) {
                        if let Decoder::P25(t) = &mut slot.traffic {
                            if t.call().is_some_and(|k| k.call == call) {
                                t.set_source(source);
                            }
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
                v.end_lc = c.end_lc().map(str::to_string);
            }
            open.push(v);
        }
        if let Ok(mut view) = self.view.lock() {
            view.open = open;
            view.recent = self.recent.iter().cloned().collect();
        }
    }
}

/// Should a lane resuming on the channel it is parked on be reset rather than coast on its
/// loops? Yes when it carried no voice within `COAST_MAX_IDLE` (the carrier dropped; its AGC wound
/// up on noise), or its PLL ran to half its clamp or more.
pub fn resume_needs_reset(pll: Option<crate::radio::tuner::LanePll>, since_voice: Option<Duration>) -> bool {
    let stale = since_voice.is_none_or(|d| d > COAST_MAX_IDLE);
    stale || pll.is_some_and(|p| p.hot())
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
        not_followed: o.not_followed.map(|n| n.as_str().to_string()),
        lane: lane_number(o.lane),
        started_unix_ms: o.at_unix_ms,
        ended_unix_ms: c.map(|c| c.ended_unix_ms),
        close: c.map(|c| c.reason.as_str().to_string()),
        end_lc: c.and_then(|c| c.end_lc).map(str::to_string),
        sources: c.map(|c| c.sources.clone()).unwrap_or_default(),
        voice_frames: c.map_or(0, |c| c.voice_frames),
    }
}

/// A closed call as the history stores it. Its vocoder counts follow from the recorder.
fn call_row(o: &Opened, c: &Closed, codec: &'static str) -> CallRow {
    let followed = o.lane.is_some();
    CallRow {
        site: o.site.clone(),
        call_id: o.call,
        started_ms: c.started_unix_ms,
        ended_ms: c.ended_unix_ms,
        tg: o.tg,
        source: c.source.or(c.speaker).filter(|&s| s != 0),
        sources: c.sources.clone(),
        freq_hz: o.channel.freq_hz,
        channel: o.channel_label.clone(),
        timeslot: o.channel.slot,
        lane: lane_number(o.lane).unwrap_or(0),
        encrypted: o.encrypted,
        followed,
        not_followed: o.not_followed.map(|n| n.as_str().to_string()),
        voice_ms: c.voice_frames * 20,
        // The grant to its last update; a call with no update was shorter than their period.
        grant_ms: match c.last_update_unix_ms.saturating_sub(c.started_unix_ms) {
            0 => c.ended_unix_ms.saturating_sub(c.started_unix_ms),
            ms => ms,
        },
        codec: (followed && c.voice_frames > 0).then(|| codec.to_string()),
        frames: c.voice_frames,
        frame_errors: 0,
        close_reason: c.reason.as_str().to_string(),
        end_kind: c.end_lc.map(str::to_string),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::presets::{find_preset, DdcPreset};
    use crate::protocol::events::{ChannelId, LogicalChannel};
    use crate::radio::tuner::{LanePll, Readback, TuningPlan};
    use crate::services::config::{Config, Paths};

    /// A radio with nothing behind it.
    struct Quiet;

    impl RadioHw for Quiet {
        async fn set_lo(&self, _: u64) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set_rate(&self, _: u32, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set_gain(&self, _: crate::hardware::ad9361::GainMode, _: Option<f64>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn configure_control(&self, _: &'static DdcPreset, _: f64) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set_control_nco(&self, _: f64, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        async fn configure_lanes(&self, _: &'static DdcPreset) -> anyhow::Result<()> {
            Ok(())
        }
        async fn retune_lane(&self, _: Lane, _: f64, _: u32, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        async fn pause_lane(&self, _: Lane) -> anyhow::Result<()> {
            Ok(())
        }
        async fn readback(&self, _: u32) -> Readback {
            Readback::default()
        }
    }

    impl StreamSource for Quiet {
        fn control_streams(
            &self,
            _: crate::radio::streams::Wants,
            _: std::sync::mpsc::SyncSender<crate::radio::streams::Input>,
            _: Arc<AtomicBool>,
            _: Arc<crate::radio::streams::StreamCounters>,
        ) -> Vec<tokio::task::JoinHandle<()>> {
            Vec::new()
        }
    }

    /// Clay with the data channel learned, its trunking started on `lanes`: where the lanes sit.
    async fn lanes_after_start(lanes: &[Lane]) -> [Option<u64>; 2] {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(&dir.path().join("flash"), &dir.path().join("sd"));
        let learned = Arc::new(Learned::new("clay", Config::site_state(&paths, "clay").unwrap()));
        learned.data_channel(&LogicalChannel { id: ChannelId::P25 { iden: 1, number: 1 }, slot: None, freq_hz: Some(859_212_500), tdma: false });
        let tuner = Arc::new(Tuner::new(Quiet, 0.0));
        tuner.apply(TuningPlan { preset: find_preset("12M").unwrap(), lo_hz: 855_996_875, control_hz: 860_962_500 }).await.unwrap();
        let trunking = Trunking::new(Audio::start(lanes), Default::default(), Default::default(), Default::default(), 1);
        let setup = Setup {
            site: "clay".into(),
            protocol: Protocol::P25,
            lcn_hz: Default::default(),
            lanes: lanes.to_vec(),
            routing: Default::default(),
            encrypted: Default::default(),
            policy: CallPolicy::default(),
            learned: Some(learned),
        };
        trunking.start(setup, tuner.clone(), Arc::new(EventLog::default())).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        trunking.stop().await;
        tuner.tuning().lanes
    }

    #[test]
    fn voice_after_an_end_marker_is_the_next_calls() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let calls: VecDeque<LaneCall> = [
            LaneCall { id: 1, opened: t0, ended_air: Some(t0 + s(3)) },
            LaneCall { id: 2, opened: t0 + s(5), ended_air: None },
        ]
        .into();
        assert_eq!(owner(&calls, t0 + s(2)), Some(1));
        // After call 1's transmission ended, before call 2's grant was decoded: call 2's talker.
        assert_eq!(owner(&calls, t0 + s(4)), Some(2));
        assert_eq!(owner(&calls, t0 + s(6)), Some(2));
        // Without an end marker, a frame before the new grant is the old call's tail.
        let open: VecDeque<LaneCall> = calls.iter().map(|c| LaneCall { ended_air: None, ..*c }).collect();
        assert_eq!(owner(&open, t0 + s(4)), Some(1));
        // Before every call kept: the oldest.
        assert_eq!(owner(&calls, t0 - s(1)), Some(1));
        assert_eq!(owner(&VecDeque::new(), t0), None);
    }

    #[tokio::test]
    async fn the_last_lane_waits_on_the_data_channel() {
        assert_eq!(lanes_after_start(&[Lane::One, Lane::Two]).await, [None, Some(859_212_500)]);
        // With one lane it stays free for voice.
        assert_eq!(lanes_after_start(&[Lane::One]).await, [None, None]);
    }

    // A parked lane resumed with its PLL at the clamp after the carrier had been gone for seconds
    // lost two whole transmissions on the bench.
    #[test]
    fn a_stale_or_runaway_lane_is_reset_on_resume() {
        let ms = Duration::from_millis;
        for clamp_q213 in [8579, 5325] {
            let pll = |q| Some(LanePll { pll_q213: q, clamp_q213 });
            // Voice a moment ago, PLL near centre: coast.
            assert!(!resume_needs_reset(pll(300), Some(ms(400))));
            assert!(!resume_needs_reset(pll(-2000), Some(COAST_MAX_IDLE)));
            // PLL at or past half the clamp: reset, however fresh.
            assert!(resume_needs_reset(pll(clamp_q213 as i16), Some(ms(100))));
            assert!(resume_needs_reset(pll(-(clamp_q213 / 2) as i16), Some(ms(100))));
            // Gone longer than the coast window, or never any voice: reset.
            assert!(resume_needs_reset(pll(0), Some(COAST_MAX_IDLE + ms(1))));
            assert!(resume_needs_reset(pll(0), None));
        }
        // 3000 is inside the pi/3 clamp's coast band but past half the 0.65 rad clamp.
        assert!(!resume_needs_reset(Some(LanePll { pll_q213: 3000, clamp_q213: 8579 }), Some(ms(100))));
        assert!(resume_needs_reset(Some(LanePll { pll_q213: 3000, clamp_q213: 5325 }), Some(ms(100))));
        // No PLL reading (no hardware): the voice rule alone.
        assert!(!resume_needs_reset(None, Some(ms(100))));
    }
}
