//! The live site's trunking: one task owning the follower, the call book and each lane's
//! demodulator and traffic decoder. Grants come from the control receivers; each lane's IQ from
//! the streams; a 100 ms tick drives the call book's timers. The follower's commands move the
//! lanes through the tuner, and the call book's events go to the event log and the calls view.
//!
//! A P25 lane demodulates with the control channel's modulation (LSM or C4FM, as the control
//! receivers choose). The streams deliver only the IQ of a lane's current tuning, each block
//! with its air time; a new tuning's first block starts the lane's demodulator over.
//!
//! Voice is attributed by air time: a frame aired before the lane's current call opened belongs
//! to the call before it, unless that call's transmission had already ended.
//!
//! At a DMR site a grant on a channel the plan lacks is followed on a candidate frequency, which
//! is kept once the call's link control is heard there (`lcn`). While a granted channel still
//! has no frequency, the idle lane identifies the carriers on the air: each names its network
//! and site in its CACH, so the site's own channels are known before a grant needs them.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::mpsc;

use super::calls::{CallBook, CallEvent, CallId, CallPolicy, Closed, Opened, SourceVia};
use super::follow::routing::Routing;
use super::follow::{Command, Follower, Record};
use super::lcn::{self, Around, LcnLearner, SiteCode, Usual};
use super::survey::{self, Carrier, Survey};
use crate::audio::live::{Audio, VoiceBatch};
use crate::services::history::store::{CallRow, Store};
use crate::services::history::HistoryTx;
use crate::services::notices::{Notice, Notices};
use crate::services::packet_data::PacketData;
use crate::services::recordings::{CallEnd, CallStart, RecorderTx};
use crate::radio::lane::Lane;
use crate::protocol::dmr::demod::DmrDemodStats;
use crate::protocol::dmr::message::DmrMessage;
use crate::protocol::dmr::traffic::{DmrCall, DmrReceiver, DmrTraffic};
use crate::protocol::p25::c4fm::{C4fmDecoder, DibitSink};
use crate::protocol::p25::framer::FramerStats;
use crate::protocol::p25::lsm::{self, LsmDecoder};
use crate::protocol::events::{ChannelId, ChannelIdentity, Grant, TrafficEvent, VoiceFrames};
use crate::services::discovery::carriers::power_db;
use crate::protocol::p25::traffic::{CallContext, P25Traffic};
use crate::radio::streams::{Block, LaneBlock, StreamSource};
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
/// One P25 symbol, 1/4800 s.
const SYMBOL: Duration = Duration::from_nanos(208_333);
/// Spectrometer frames gathered after a grant on an unmapped DMR channel, for the carrier that
/// keys up.
const KEYUP_WATCH: Duration = Duration::from_millis(800);
/// The survey's view is published this often.
const SURVEY_PUBLISH: Duration = Duration::from_secs(5);
/// DMR: an idle lane identifying a carrier waits this long for its short LC (it comes every
/// four bursts, 240 ms, once the demodulator has locked).
const PROBE_FOR: Duration = Duration::from_millis(1500);
/// ...starts one at most this often...
const PROBE_EVERY: Duration = Duration::from_secs(3);
/// ...and goes back to a carrier that named nothing (another protocol, or not DMR) after this.
const PROBE_AGAIN: Duration = Duration::from_secs(600);
/// A spectrum frame older than this does not say what is on the air now.
const FRAME_FRESH: Duration = Duration::from_millis(400);

/// DMR: lane one's receiver listening to a carrier for its network and site.
#[derive(Debug, Clone, Copy)]
struct Probe {
    freq_hz: u64,
    until: Instant,
}

/// The carriers heard in the live site's receive window (`survey`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SurveyView {
    pub site: String,
    pub lo_hz: Option<u64>,
    pub sample_rate_hz: Option<u32>,
    /// Spectrometer frames read since the window last moved (7.6 a second).
    pub frames: u64,
    pub carriers: Vec<Carrier>,
}

/// The newest spectrometer frame the live site read: what the spectrum page shows.
#[derive(Debug, Clone)]
pub struct Frame {
    pub db: Vec<f32>,
    pub at: Instant,
}

/// The spectrum watched after a grant on an unmapped DMR channel.
struct KeyupWatch {
    grant: Grant,
    nac: u16,
    usual: Vec<f32>,
    window: (u64, u32),
    until: Instant,
    after: Vec<Vec<f32>>,
}

/// What the trunking task receives.
#[derive(Debug)]
pub enum TrunkInput {
    /// A grant or grant update, with the control channel's NAC.
    Grant { grant: Grant, nac: u16, at: Stamp },
    /// DMR: the control channel's messages that carry calls (its other timeslot's).
    ControlCalls { messages: Vec<DmrMessage>, at: Stamp },
    /// P25: the control channel decodes as C4FM (or LSM); the lanes follow from their next
    /// tuning.
    Modulation { c4fm: bool },
    Lane(LaneBlock),
    /// The live site's aliases changed.
    Routing(Box<Routing>),
    /// The receive window moved: every lane was reloaded and holds no channel.
    WindowMoved,
    /// Follow only this talkgroup (None: release the hold).
    Hold(Option<u32>),
    /// This lane follows only this talkgroup (None: what the aliases say).
    LaneHold { lane: Lane, tg: Option<u32> },
}

pub type TrunkTx = mpsc::Sender<TrunkInput>;

/// What the trunking task is started with.
pub struct Setup {
    pub site: String,
    pub protocol: Protocol,
    /// DMR: logical channel numbers to downlink Hz, configured and learned.
    pub lcn_hz: std::collections::HashMap<u16, u64>,
    /// DMR: the site's known channels, where an unmapped channel is looked for.
    pub channels_hz: Vec<u64>,
    /// DMR: what the site's own channels name in their CACH, as configured.
    pub site_code: Option<SiteCode>,
    pub lanes: Vec<Lane>,
    /// P25: the lanes' first modulation, until the control receivers say (`Modulation`).
    pub c4fm: bool,
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
    pub emergency: bool,
    /// A unit-to-unit call (`tg` is the called radio).
    pub private: bool,
    pub not_followed: Option<String>,
    pub lane: Option<u8>,
    pub started_unix_ms: u64,
    pub first_voice_unix_ms: Option<u64>,
    pub ended_unix_ms: Option<u64>,
    /// From the grant to the close.
    pub open_ms: Option<u64>,
    /// From the grant to its last update on the control channel.
    pub grant_ms: Option<u64>,
    pub close: Option<String>,
    pub end_lc: Option<String>,
    pub sources: Vec<u32>,
    /// Decoded voice, 20 ms each.
    pub voice_frames: u64,
    /// `imbe` or `ambe2`, once there was voice.
    pub codec: Option<String>,
    /// Voice frames the vocoder found errors in (known once the call is stored).
    pub frame_errors: Option<u64>,
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
    /// P25: the lane's LSM carrier loop.
    #[serde(rename = "loop", skip_serializing_if = "Option::is_none")]
    pub carrier_loop: Option<lsm::Carrier>,
}

/// A lane's traffic decoder's counters.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum LaneCounters {
    /// The framer of the lane's decoder, on the dibits of `demod` (`lsm` or `c4fm`).
    P25 {
        demod: &'static str,
        #[serde(flatten)]
        framer: Box<FramerStats>,
    },
    Dmr { bursts: u64, demod: DmrDemodStats },
}

/// Open calls and the newest closed ones.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CallsView {
    pub open: Vec<CallView>,
    pub recent: Vec<CallView>,
}

/// A lane's traffic decoder: P25's on its own chain's dibits; DMR's on the messages of the
/// carrier its call is on (`DmrHw`'s, or the control channel's).
enum Decoder {
    P25(P25Traffic),
    Dmr(DmrTraffic),
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
            Decoder::Dmr(_) => {}
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

/// A P25 lane's demodulator.
enum Demod {
    Lsm(Box<LsmDecoder>),
    C4fm(Box<C4fmDecoder>),
}

impl Demod {
    fn new(c4fm: bool) -> Demod {
        if c4fm {
            Demod::C4fm(Box::new(C4fmDecoder::new()))
        } else {
            Demod::Lsm(Box::new(LsmDecoder::new()))
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Demod::Lsm(_) => "lsm",
            Demod::C4fm(_) => "c4fm",
        }
    }

    fn process(&mut self, iq: &[i16], sink: &mut impl DibitSink) {
        match self {
            Demod::Lsm(d) => d.process_iq_i16(iq, sink),
            Demod::C4fm(d) => d.process_iq_i16(iq, sink),
        }
    }

    /// The LSM's carrier loop.
    fn carrier(&self) -> Option<lsm::Carrier> {
        match self {
            Demod::Lsm(d) => Some(d.demod.carrier()),
            Demod::C4fm(_) => None,
        }
    }
}

/// The demodulator's dibits into a lane's traffic decoder, each stamped a symbol after the last
/// from its block's air time.
struct LaneSink<'a> {
    traffic: &'a mut P25Traffic,
    air: Instant,
    now: Instant,
    out: &'a mut Vec<TrafficEvent>,
}

impl DibitSink for LaneSink<'_> {
    fn push_dibit(&mut self, dibit: u8) {
        self.traffic.push(dibit, self.air, self.now, self.out);
        self.air += SYMBOL;
    }
    fn sync_detected(&mut self) {
        self.traffic.sync_detected();
    }
    fn is_assembling(&self) -> bool {
        self.traffic.is_assembling()
    }
}

/// DMR: lane one's receiver. Both lanes' calls off the control channel are on its carrier, one
/// a timeslot (the control channel's calls come from its own receiver).
struct DmrHw {
    receiver: DmrReceiver,
    tuned_hz: Option<u64>,
}

struct LaneSlot {
    lane: Lane,
    traffic: Decoder,
    /// P25: the lane's demodulator, started with each tuning.
    demod: Option<Demod>,
    /// The lane's calls, newest last.
    calls: VecDeque<LaneCall>,
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
    /// DMR: the site's channel plan, and the learning of the channels it lacks.
    lcn: LcnLearner,
    /// DMR: the receive window's bins at their usual level, read every `lcn::USUAL_EVERY`.
    usual: Usual,
    usual_read: Instant,
    /// The receive window over time, from every spectrometer frame (the task reads them all
    /// while the site is live).
    survey: Survey,
    survey_published: Instant,
    survey_view: Arc<Mutex<SurveyView>>,
    latest: Arc<Mutex<Option<Frame>>>,
    /// DMR: the spectrum watched after a grant on an unmapped channel (one watch at a time).
    keyup: Option<KeyupWatch>,
    dmr: bool,
    /// DMR: what the site's own channels name.
    site_code: Option<SiteCode>,
    /// DMR: the idle lane identifying a carrier, when one does.
    probe: Option<Probe>,
    probe_last: Option<Instant>,
    /// When each carrier was last listened to without naming itself.
    probed: std::collections::HashMap<u64, Instant>,
    /// DMR: lane one's receiver, and the control channel (whose calls its own receiver hears).
    dmr_hw: Option<DmrHw>,
    control_hz: u64,
    /// P25: the control channel's modulation, which each lane takes at its next tuning.
    c4fm: bool,
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
    /// Where a site's newest calls come from when it goes live.
    store: std::sync::Mutex<Option<Arc<Store>>>,
    /// (site, talkgroup): the hold outlives a restart of its site, not a switch to another.
    hold: std::sync::Mutex<Option<(String, u32)>>,
    /// (site, each lane held and its talkgroup), likewise.
    lane_holds: std::sync::Mutex<Option<(String, Vec<(Lane, u32)>)>>,
    lanes: Arc<Mutex<Vec<LaneStatus>>>,
    survey: Arc<Mutex<SurveyView>>,
    latest: Arc<Mutex<Option<Frame>>>,
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
            store: std::sync::Mutex::new(None),
            hold: std::sync::Mutex::new(None),
            lane_holds: std::sync::Mutex::new(None),
            lanes: Arc::default(),
            survey: Arc::default(),
            latest: Arc::default(),
            running: tokio::sync::Mutex::new(None),
        }
    }

    /// Each lane of the live site, updated each second.
    pub fn lanes(&self) -> Vec<LaneStatus> {
        self.lanes.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// The carriers heard in the live site's receive window, updated every 5 s.
    pub fn survey(&self) -> SurveyView {
        self.survey.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// The newest spectrometer frame the live site read.
    pub fn latest_frame(&self) -> Option<Frame> {
        self.latest.lock().ok().and_then(|f| f.clone())
    }

    pub fn set_packet_data(&self, data: Arc<PacketData>) {
        if let Ok(mut d) = self.packet_data.lock() {
            *d = Some(data);
        }
    }

    pub fn set_history_store(&self, store: Arc<Store>) {
        if let Ok(mut s) = self.store.lock() {
            *s = Some(store);
        }
    }

    /// The talkgroup `site` is held on.
    pub fn hold(&self, site: &str) -> Option<u32> {
        self.hold.lock().ok().and_then(|h| h.as_ref().filter(|(s, _)| s == site).map(|(_, tg)| *tg))
    }

    /// Hold the live site `site` on `tg`, or release it (None).
    pub async fn set_hold(&self, site: &str, tg: Option<u32>) {
        if let Ok(mut h) = self.hold.lock() {
            *h = tg.map(|tg| (site.to_string(), tg));
        }
        let tx = self.running.lock().await.as_ref().map(|r| r.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(TrunkInput::Hold(tg)).await;
        }
    }

    /// The lanes of `site` held on a talkgroup, each with it (released lanes as `None`).
    pub fn lane_holds(&self, site: &str) -> Vec<(Lane, Option<u32>)> {
        let held: Vec<(Lane, u32)> =
            self.lane_holds.lock().ok().and_then(|h| h.as_ref().filter(|(s, _)| s == site).map(|(_, v)| v.clone())).unwrap_or_default();
        Lane::ALL.iter().map(|&l| (l, held.iter().find(|(x, _)| *x == l).map(|(_, tg)| *tg))).collect()
    }

    /// Hold `lane` of the live site `site` on `tg`, or release it (None).
    pub async fn set_lane_hold(&self, site: &str, lane: Lane, tg: Option<u32>) {
        if let Ok(mut h) = self.lane_holds.lock() {
            let mut held: Vec<(Lane, u32)> = h.as_ref().filter(|(s, _)| s == site).map(|(_, v)| v.clone()).unwrap_or_default();
            held.retain(|(l, _)| *l != lane);
            held.extend(tg.map(|tg| (lane, tg)));
            *h = Some((site.to_string(), held));
        }
        let tx = self.running.lock().await.as_ref().map(|r| r.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(TrunkInput::LaneHold { lane, tg }).await;
        }
    }

    /// The site's newest calls: those still listed, and the stored ones (a call that closed just
    /// before a switch may not be stored yet).
    async fn recent_of(&self, site: &str) -> VecDeque<CallView> {
        let listed: Vec<CallView> =
            self.view.lock().map(|v| v.recent.iter().filter(|c| c.site == site).cloned().collect()).unwrap_or_default();
        let store = self.store.lock().ok().and_then(|s| s.clone());
        let stored = match store {
            Some(store) => {
                let site = site.to_string();
                match tokio::task::spawn_blocking(move || store.latest_calls(&site, RECENT)).await {
                    Ok(Ok(rows)) => rows,
                    Ok(Err(e)) => {
                        tracing::warn!("history: recent calls not read: {e:#}");
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("history: recent calls not read: {e}");
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };
        merge_recent(listed, stored)
    }

    /// Start following on the live site (stopping the previous site's trunking first).
    pub async fn start<H: RadioHw + StreamSource + Send + Sync + 'static>(&self, setup: Setup, tuner: Arc<Tuner<H>>, log: Arc<EventLog>) -> TrunkTx {
        self.stop().await;
        let hold = self.hold(&setup.site);
        if hold.is_none() {
            if let Ok(mut h) = self.hold.lock() {
                *h = None;
            }
        }
        if self.lane_holds(&setup.site).iter().all(|(_, tg)| tg.is_none()) {
            if let Ok(mut h) = self.lane_holds.lock() {
                *h = None;
            }
        }
        let recent = self.recent_of(&setup.site).await;
        if let Ok(mut v) = self.view.lock() {
            *v = CallsView { open: Vec::new(), recent: recent.iter().cloned().collect() };
        }
        let (tx, rx) = mpsc::channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let (lane_tx, mut lane_rx) = mpsc::channel(QUEUE);
        // P25: each lane's IQ; DMR: lane one's, whose carrier both lanes share.
        let streamed: Vec<Lane> = match setup.protocol {
            Protocol::P25 => setup.lanes.clone(),
            Protocol::DmrTier3 => setup.lanes.iter().copied().filter(|&l| l == Lane::One).collect(),
        };
        tuner.hw().lane_streams(&streamed, lane_tx, stop.clone());
        let forward = tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(input) = lane_rx.recv().await {
                if forward.send(TrunkInput::Lane(input)).await.is_err() {
                    return;
                }
            }
        });
        let sources = vec![forwarder];
        let control_hz = tuner.tuning().control_hz;
        let task = Task {
            book: CallBook::new(&setup.site, &setup.lanes, setup.policy, self.next_call.load(Ordering::Relaxed)),
            follower: {
                let mut f = Follower::new(&setup.lanes, setup.routing, setup.encrypted);
                f.set_hold(hold);
                for (lane, tg) in self.lane_holds(&setup.site) {
                    f.set_lane_hold(lane, tg);
                }
                if setup.protocol == Protocol::DmrTier3 {
                    f.share_tuner(control_hz);
                }
                f
            },
            lanes: setup
                .lanes
                .iter()
                .map(|&lane| LaneSlot {
                    lane,
                    traffic: match setup.protocol {
                        Protocol::P25 => Decoder::P25(P25Traffic::new(lane.name())),
                        Protocol::DmrTier3 => Decoder::Dmr(DmrTraffic::new()),
                    },
                    demod: None,
                    calls: VecDeque::new(),
                    last_voice: None,
                    tuned_hz: None,
                    voice_frames: 0,
                })
                .collect(),
            tuner,
            log,
            view: self.view.clone(),
            open: Vec::new(),
            recent,
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
            lcn: {
                let mut l = LcnLearner::new(setup.lcn_hz.clone(), control_hz, setup.channels_hz.clone());
                if let Some(state) = setup.learned.as_ref().map(|x| x.state()) {
                    for &lcn in &state.lcns_granted {
                        l.granted(lcn);
                    }
                    for (&hz, c) in &state.channels_heard {
                        // Judged again against the site's identity as configured now.
                        let own = setup.site_code.as_ref().and_then(|code| code.owns(&c.model, c.network, c.site, c.colour_code));
                        if let Some(own) = own {
                            l.heard(hz, own);
                        }
                    }
                }
                l
            },
            usual: Usual::default(),
            usual_read: Instant::now(),
            survey: Survey::default(),
            survey_published: Instant::now(),
            survey_view: self.survey.clone(),
            latest: self.latest.clone(),
            keyup: None,
            dmr: setup.protocol == Protocol::DmrTier3,
            site_code: setup.site_code.clone(),
            probe: None,
            probe_last: None,
            probed: std::collections::HashMap::new(),
            dmr_hw: (setup.protocol == Protocol::DmrTier3)
                .then(|| DmrHw { receiver: DmrReceiver::new(setup.lcn_hz.clone()), tuned_hz: None }),
            control_hz,
            c4fm: setup.c4fm,
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
        if let Ok(mut s) = self.survey.lock() {
            *s = SurveyView::default();
        }
        if let Ok(mut f) = self.latest.lock() {
            *f = None;
        }
    }

    /// New aliases for the calls to come (the open ones keep their lanes).
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

    /// The newest raw IMBE frames, oldest first.
    pub fn frames(&self) -> Vec<RawFrame> {
        self.frames.lock().map(|f| f.iter().cloned().collect()).unwrap_or_default()
    }

    pub fn calls(&self) -> CallsView {
        self.view.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

/// The listed calls and the stored ones as one list, newest first, each call once (the listed
/// one wins: it is the fresher).
fn merge_recent(listed: Vec<CallView>, stored: Vec<CallRow>) -> VecDeque<CallView> {
    let mut seen: HashSet<CallId> = listed.iter().map(|c| c.call).collect();
    let mut all = listed;
    all.extend(stored.into_iter().filter(|r| seen.insert(r.call_id)).map(view_of_row));
    all.sort_by(|a, b| b.started_unix_ms.cmp(&a.started_unix_ms));
    all.truncate(RECENT);
    all.into()
}

/// A stored call in the live view's shape.
pub fn view_of_row(r: CallRow) -> CallView {
    CallView {
        call: r.call_id,
        site: r.site,
        tg: r.tg,
        source: r.source,
        speaker: None,
        freq_hz: r.freq_hz,
        slot: r.timeslot,
        channel: r.channel,
        encrypted: r.encrypted,
        emergency: r.emergency,
        private: r.private,
        not_followed: r.not_followed,
        lane: (r.lane > 0).then_some(r.lane),
        started_unix_ms: r.started_ms,
        first_voice_unix_ms: r.first_voice_ms,
        ended_unix_ms: Some(r.ended_ms),
        open_ms: Some(r.ended_ms.saturating_sub(r.started_ms)),
        grant_ms: Some(r.grant_ms),
        close: Some(r.close_reason),
        end_lc: r.end_kind,
        sources: r.sources,
        voice_frames: r.voice_ms / 20,
        codec: r.codec,
        frame_errors: Some(r.frame_errors),
    }
}

/// The grant to its last update; a call with no update was shorter than their period.
fn grant_ms(c: &Closed) -> u64 {
    match c.last_update_unix_ms.saturating_sub(c.started_unix_ms) {
        0 => c.ended_unix_ms.saturating_sub(c.started_unix_ms),
        ms => ms,
    }
}

/// The codec a call's voice was decoded with, once it had voice.
fn codec_of(o: &Opened, voice_frames: u64, codec: &'static str) -> Option<String> {
    (o.lane.is_some() && voice_frames > 0).then(|| codec.to_string())
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
                    Some(TrunkInput::ControlCalls { messages, at }) => {
                        let control = self.control_hz;
                        self.dmr_messages(control, &messages, at);
                    }
                    Some(TrunkInput::LaneHold { lane, tg }) => self.lane_hold(lane, tg).await,
                    Some(TrunkInput::Modulation { c4fm }) => self.c4fm = c4fm,
                    Some(TrunkInput::Lane(LaneBlock { lane, block })) => self.iq(lane, block),
                    Some(TrunkInput::Routing(r)) => self.follower.set_routing(*r),
                    Some(TrunkInput::WindowMoved) => self.window_moved(),
                    Some(TrunkInput::Hold(tg)) => self.hold(tg).await,
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

    /// A grant. A DMR channel the plan lacks is looked for: the grant waits a moment for the
    /// carrier that keys up for it (`keyup_done`); when the spectrum cannot be watched, it is
    /// followed at once on the learner's next candidate.
    async fn grant(&mut self, mut grant: Grant, nac: u16, at: Stamp) {
        let mut trial = false;
        if let ChannelId::DmrLcn(lcn) = grant.channel.id {
            self.lcn.granted(lcn);
            if let Some(l) = &self.learned {
                l.lcn_granted(lcn);
            }
            if grant.channel.freq_hz.is_none() {
                grant.channel.freq_hz = self.lcn.freq_for(lcn, grant.tg);
            }
            if grant.channel.freq_hz.is_none() && self.lcn.idle() {
                // Repeated while its watch runs: the watch follows it.
                if self.keyup.as_ref().is_some_and(|k| k.grant.channel.id == grant.channel.id && k.grant.tg == grant.tg) {
                    return;
                }
                if self.keyup.is_none() && self.watch_keyup(&grant, nac, at) {
                    return;
                }
                // No watch: what the newest frame shows on the air.
                let on: Vec<u64> = self.fresh_frame(at.mono).map(|db| survey::on_air(&[db], self.window()).into_iter().map(|c| c.0).collect()).unwrap_or_default();
                let intermittent = self.survey.intermittent();
                if let Some(f) = self.lcn.candidate(lcn, &Around { keyed: &[], on: &on, intermittent: &intermittent }) {
                    self.lcn.start(lcn, f, grant.tg, at.mono);
                    grant.channel.freq_hz = Some(f);
                    trial = true;
                }
            }
        }
        self.follow_grant(grant, nac, at, trial).await;
    }

    /// The watch after a grant on an unmapped channel is over: follow it on the first candidate
    /// not ruled out (`keyed`: the carriers that keyed up; `on`: those on the air meanwhile).
    async fn keyed(&mut self, mut grant: Grant, nac: u16, keyed: Vec<u64>, on: Vec<u64>) {
        let mut trial = false;
        if let ChannelId::DmrLcn(lcn) = grant.channel.id {
            grant.channel.freq_hz = self.lcn.freq_for(lcn, grant.tg);
            if grant.channel.freq_hz.is_none() && self.lcn.idle() {
                let intermittent = self.survey.intermittent();
                match self.lcn.candidate(lcn, &Around { keyed: &keyed, on: &on, intermittent: &intermittent }) {
                    Some(f) => {
                        self.lcn.start(lcn, f, grant.tg, Instant::now());
                        grant.channel.freq_hz = Some(f);
                        trial = true;
                    }
                    None => self.log.system("lcn", format!("LCN {lcn}: nothing left to try for TG {}'s call", grant.tg)),
                }
            }
        }
        self.follow_grant(grant, nac, Stamp::now(), trial).await;
    }

    /// Gather the spectrum for a moment after a grant on an unmapped channel, to compare with the
    /// bins' usual level (`keyup_done`). False: no usual level to compare with yet.
    fn watch_keyup(&mut self, grant: &Grant, nac: u16, at: Stamp) -> bool {
        let t = self.tuner.tuning();
        let window = (t.lo_hz, t.sample_rate_hz);
        let Some(usual) = self.usual.level(window) else { return false };
        self.keyup = Some(KeyupWatch { grant: *grant, nac, usual, window, until: at.mono + KEYUP_WATCH, after: Vec::new() });
        true
    }

    /// The watch is over: the carriers that rose are the next candidates.
    async fn keyup_done(&mut self, now: Instant) {
        if !self.keyup.as_ref().is_some_and(|k| now >= k.until) {
            return;
        }
        let Some(k) = self.keyup.take() else { return };
        let keyed = lcn::keyed_up(&k.usual, &k.after, k.window.0, k.window.1);
        let on = survey::on_air(&k.after, k.window).into_iter().map(|c| c.0).collect();
        self.keyed(k.grant, k.nac, keyed, on).await;
    }

    /// The receive window: (centre, sample rate).
    fn window(&self) -> (u64, u32) {
        let t = self.tuner.tuning();
        (t.lo_hz, t.sample_rate_hz)
    }

    /// The newest spectrometer frame, while it says what is on the air now.
    fn fresh_frame(&self, now: Instant) -> Option<Vec<f32>> {
        let l = self.latest.lock().ok()?;
        l.as_ref().filter(|f| now.saturating_duration_since(f.at) < FRAME_FRESH).map(|f| f.db.clone())
    }

    /// One spectrometer frame of the receive window: into the survey, the usual level, a
    /// key-up watch, and the spectrum page.
    fn frame(&mut self, db: Vec<f32>, now: Instant) {
        let t = self.tuner.tuning();
        let window = (t.lo_hz, t.sample_rate_hz);
        self.survey.add(&db, window);
        if let Some(k) = self.keyup.as_mut().filter(|k| k.window == window && k.usual.len() == db.len()) {
            k.after.push(db.clone());
        }
        if self.dmr && now.saturating_duration_since(self.usual_read) >= lcn::USUAL_EVERY {
            self.usual_read = now;
            self.usual.push(db.clone(), window);
        }
        if let Ok(mut l) = self.latest.lock() {
            *l = Some(Frame { db, at: now });
        }
    }

    fn publish_survey(&mut self, now: Instant) {
        if now.saturating_duration_since(self.survey_published) < SURVEY_PUBLISH {
            return;
        }
        self.survey_published = now;
        let view = SurveyView {
            site: self.site.clone(),
            lo_hz: self.survey.window().map(|w| w.0),
            sample_rate_hz: self.survey.window().map(|w| w.1),
            frames: self.survey.read,
            carriers: self.survey.carriers(),
        };
        if let Ok(mut v) = self.survey_view.lock() {
            *v = view;
        }
    }

    /// Follow a grant; `trial`: its frequency is a candidate for its DMR channel, kept once the
    /// call's link control is heard there.
    async fn follow_grant(&mut self, grant: Grant, nac: u16, at: Stamp, trial: bool) {
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
        if trial {
            let lane = outcome.commands.iter().find_map(|c| match c {
                Command::Follow { lane, channel } if channel.freq_hz == grant.channel.freq_hz => Some(*lane),
                _ => None,
            });
            match (lane, self.lcn.trial()) {
                (Some(lane), Some(t)) => {
                    self.lcn.following(lane, Instant::now());
                    self.log.system("lcn", format!("LCN {}: trying {:.5} MHz with TG {}'s call on {lane}", t.lcn, t.freq_hz as f64 / 1e6, t.tg));
                }
                _ => self.lcn.abandon(),
            }
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

    async fn hold(&mut self, tg: Option<u32>) {
        let at = Stamp::now();
        for command in self.follower.set_hold(tg) {
            self.command(command, at).await;
        }
        self.log.system("hold", tg.map_or("hold released".to_string(), |tg| format!("holding TG {tg}: no other talkgroup is followed")));
    }

    async fn command(&mut self, command: Command, at: Stamp) {
        match command {
            Command::Follow { lane, channel } => {
                let Some(freq) = channel.freq_hz else { return };
                if self.dmr_hw.is_some() {
                    return self.follow_dmr(lane, freq).await;
                }
                let Some(slot) = self.slot(lane) else { return };
                let moved = slot.tuned_hz != Some(freq);
                let since_voice = slot.last_voice.map(|t| at.mono.saturating_duration_since(t));
                let carrier = slot.demod.as_ref().and_then(Demod::carrier);
                let coast = !moved && !resume_needs_reset(carrier, since_voice);
                match self.tuner.retune_lane(lane, freq, !coast).await {
                    Ok(_) => {
                        let slot = self.slot(lane).expect("lane");
                        slot.tuned_hz = Some(freq);
                        if !coast {
                            // The tuning's first block starts the demodulator over.
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
                // A DMR lane's receiver may carry the other lane's call.
                if self.dmr_hw.is_some() {
                    return;
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

    /// DMR: `lane` follows a call on `freq`. The control channel's own receiver hears it there;
    /// elsewhere lane one's receiver goes to the carrier, unless it is there already (the other
    /// lane's call on its other timeslot).
    async fn follow_dmr(&mut self, lane: Lane, freq: u64) {
        let on_hw = freq != self.control_hz;
        if on_hw && self.dmr_hw.as_ref().is_some_and(|hw| hw.tuned_hz != Some(freq)) {
            // A call takes the receiver from a carrier being identified.
            self.probe = None;
            if let Err(e) = self.tuner.retune_lane(Lane::One, freq, true).await {
                self.log.system("lane", format!("{} retune to {:.5} MHz failed: {e:#}", Lane::One, freq as f64 / 1e6));
                return;
            }
            if let Some(hw) = self.dmr_hw.as_mut() {
                hw.receiver.retuned();
                hw.tuned_hz = Some(freq);
            }
        }
        if let Some(slot) = self.slot(lane) {
            slot.tuned_hz = Some(freq);
        }
        self.follower.retuned(lane, Some(freq));
    }

    async fn lane_hold(&mut self, lane: Lane, tg: Option<u32>) {
        let at = Stamp::now();
        for command in self.follower.set_lane_hold(lane, tg) {
            self.command(command, at).await;
        }
        let text = match tg {
            Some(tg) => format!("{lane} held on TG {tg}: it follows only that talkgroup, and the talkgroup only it"),
            None => format!("{lane} released: it follows what the aliases say"),
        };
        self.log.system("hold", text);
    }

    /// A key-up watch keeps its grant (it is followed when the watch ends); it compares only the
    /// frames of the window it began in.
    fn window_moved(&mut self) {
        self.usual.clear();
        self.survey.clear();
        if let Some(hw) = self.dmr_hw.as_mut() {
            hw.receiver.retuned();
            hw.tuned_hz = None;
        }
        for slot in &mut self.lanes {
            slot.traffic.retuned();
            slot.demod = None;
            slot.tuned_hz = None;
            self.follower.retuned(slot.lane, None);
        }
    }

    /// A block of a lane's IQ (only ever of its current tuning). At a P25 site the lane's
    /// demodulator decodes it whenever the lane is tuned. At a DMR site lane one's is decoded
    /// while a lane's call is on its carrier, or while it identifies a carrier.
    fn iq(&mut self, lane: Lane, block: Block) {
        let now = Stamp::now();
        if let Some(hw) = self.dmr_hw.as_mut() {
            if lane != Lane::One {
                return;
            }
            let Some(freq) = hw.tuned_hz else { return };
            if block.retuned {
                hw.receiver.retuned();
            }
            let wanted = self.probe.is_some() || self.lanes.iter().any(|s| s.tuned_hz == Some(freq) && s.traffic.call().is_some());
            if !wanted {
                return;
            }
            let (mut messages, mut identity) = (Vec::new(), None);
            hw.receiver.push(&block.iq, &mut messages, &mut identity);
            if let Some(id) = identity {
                self.identified(freq, id, block.at);
            }
            self.dmr_messages(freq, &messages, block.at);
            return;
        }
        let c4fm = self.c4fm;
        let Some(slot) = self.lanes.iter_mut().find(|s| s.lane == lane) else { return };
        if slot.tuned_hz.is_none() {
            return;
        }
        let Decoder::P25(traffic) = &mut slot.traffic else { return };
        if block.retuned || slot.demod.is_none() {
            slot.demod = Some(Demod::new(c4fm));
        }
        if block.retuned || block.gap {
            // The frames in progress are lost.
            traffic.retuned();
        }
        let Some(demod) = slot.demod.as_mut() else { return };
        let mut events = Vec::new();
        let mut sink = LaneSink { traffic, air: block.at.mono, now: now.mono, out: &mut events };
        demod.process(&block.iq, &mut sink);
        for e in events {
            self.traffic_event(lane, e, now);
        }
    }

    /// A DMR carrier's messages (lane one's, or the control channel's) to the lanes whose calls
    /// are on it.
    fn dmr_messages(&mut self, freq: u64, messages: &[DmrMessage], at: Stamp) {
        let mut events = Vec::new();
        for slot in self.lanes.iter_mut().filter(|s| s.tuned_hz == Some(freq)) {
            let Decoder::Dmr(traffic) = &mut slot.traffic else { continue };
            if traffic.call().is_none() {
                continue;
            }
            let mut out = Vec::new();
            for m in messages {
                traffic.message(m, at.mono, &mut out);
            }
            events.extend(out.into_iter().map(|e| (slot.lane, e)));
        }
        for (lane, e) in events {
            self.traffic_event(lane, e, at);
        }
    }

    fn traffic_event(&mut self, lane: Lane, e: TrafficEvent, at: Stamp) {
        let mut out = Vec::new();
        match e {
            TrafficEvent::VoiceNid { header, nac, air } => {
                let before = at.mono.saturating_duration_since(air).as_millis() as u64;
                self.voice_nid(lane, header, nac, Stamp { mono: air, unix_ms: at.unix_ms.saturating_sub(before) });
            }
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
            TrafficEvent::Source(s) => {
                // The link control named the call's talkgroup: a trial's frequency is its channel's.
                if let Some(t) = self.lcn.confirmed(lane) {
                    if let Some(l) = &self.learned {
                        l.lcn(t.lcn, t.freq_hz);
                        l.grant(t.freq_hz);
                    }
                    self.log.system("lcn", format!("LCN {} is {:.5} MHz: TG {}'s voice header was heard there", t.lcn, t.freq_hz as f64 / 1e6, t.tg));
                }
                self.book.link_control_source(lane, s, &mut out);
            }
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

    /// A voice NID (HDU, LDU1, LDU2) the lane's decoder read, at its air time.
    fn voice_nid(&mut self, lane: Lane, header: bool, nac: u16, at: Stamp) {
        let mut out = Vec::new();
        self.book.nid(lane, true, at, &mut out);
        if header && self.book.on_lane(lane).is_some() {
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
        if let Some(t) = self.lcn.expire(at.mono) {
            self.log.system(
                "lcn",
                format!("LCN {} is not {:.5} MHz: no voice header for TG {} there in {} s", t.lcn, t.freq_hz as f64 / 1e6, t.tg, lcn::TRIAL.as_secs()),
            );
        }
        // Every frame the spectrometer finished since the last tick (one every 131 ms).
        if let Some(bytes) = self.tuner.hw().spectrum().await {
            let db = power_db(&bytes);
            if !db.is_empty() {
                self.frame(db, at.mono);
            }
        }
        self.keyup_done(at.mono).await;
        self.probe(at).await;
        self.publish_survey(at.mono);
        if at.mono.saturating_duration_since(self.lanes_published) >= Duration::from_secs(1) {
            self.lanes_published = at.mono;
            self.publish_lanes(at.mono);
        }
    }

    fn publish_lanes(&self, now: Instant) {
        let following = self.follower.locked();
        let data_hz = self.learned.as_ref().and_then(|l| l.data_channel_hz());
        // DMR: both lanes' counters are lane one's receiver's.
        let dmr = self.dmr_hw.as_ref().map(|hw| (hw.receiver.bursts, hw.receiver.demod_stats()));
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
                    Decoder::P25(t) => LaneCounters::P25 {
                        demod: s.demod.as_ref().map_or(if self.c4fm { "c4fm" } else { "lsm" }, Demod::name),
                        framer: Box::new(t.stats().clone()),
                    },
                    Decoder::Dmr(_) => {
                        let (bursts, demod) = dmr.unwrap_or_default();
                        LaneCounters::Dmr { bursts, demod }
                    }
                },
                carrier_loop: s.demod.as_ref().and_then(Demod::carrier),
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
                self.follower.retuned(lane, Some(data_hz));
                self.log.system("lane", format!("{lane} waits on the data channel, {:.4} MHz", data_hz as f64 / 1e6));
            }
            Err(e) => self.log.system("lane", format!("{lane} could not wait on the data channel: {e:#}")),
        }
    }

    /// The channel on `freq` named its network and site: it is kept in the learned state, the
    /// learner hears whether it is the site's, the receiver listening for it is done, and a trial
    /// on another site's channel is over at once.
    fn identified(&mut self, freq: u64, id: ChannelIdentity, at: Stamp) {
        let code = self.site_code.as_ref();
        let (own, changed) = match self.learned.as_ref().and_then(|l| l.channel_heard(freq, &id, code, at.unix_ms)) {
            Some((entry, changed)) => (entry.own, changed),
            None => (code.and_then(|c| c.owns(id.model, id.network, id.site, id.colour_code)), false),
        };
        if let Some(own) = own {
            self.lcn.heard(freq, own);
        }
        if self.probe.is_some_and(|p| p.freq_hz == freq) {
            self.probe = None;
            self.probed.remove(&freq);
        }
        if changed {
            let whose = match own {
                Some(true) => ": this site's",
                Some(false) => ": another site's",
                None => "",
            };
            let colour = id.colour_code.map(|c| format!(", colour code {c}")).unwrap_or_default();
            let kind = if id.control { "control" } else { "traffic" };
            self.log.system(
                "lcn",
                format!("{:.5} MHz is a {kind} channel of {} network {} site {}{colour}{whose}", freq as f64 / 1e6, id.model.to_lowercase(), id.network, id.site),
            );
        }
        if own == Some(false) {
            if let Some(t) = self.lcn.wrong_site(freq) {
                self.log.system("lcn", format!("LCN {} is not {:.5} MHz: that is network {} site {}", t.lcn, t.freq_hz as f64 / 1e6, id.network, id.site));
            }
        }
    }

    /// At a DMR site still learning its channels, lane one's receiver, while no call is on it,
    /// identifies a carrier on the air that no lane has heard name itself: it listens there until
    /// the channel's short LC names its network and site (`PROBE_FOR` at most). A grant takes the
    /// receiver at once.
    async fn probe(&mut self, at: Stamp) {
        if let Some(p) = self.probe {
            if at.mono >= p.until {
                self.probe = None;
            }
            return;
        }
        let control = self.control_hz;
        let receiver_free = self.lanes.iter().all(|s| s.tuned_hz == Some(control) || (self.follower.idle(s.lane) && self.book.on_lane(s.lane).is_none()));
        let idle = self.dmr_hw.is_some() && self.lcn.learning() && self.lcn.idle() && self.keyup.is_none() && receiver_free;
        if !idle || self.probe_last.is_some_and(|t| at.mono.saturating_duration_since(t) < PROBE_EVERY) {
            return;
        }
        let Some(db) = self.fresh_frame(at.mono) else { return };
        let tuning = self.tuner.tuning();
        let heard = self.learned.as_ref().map(|l| l.channels_heard_hz()).unwrap_or_default();
        let near = |list: &[u64], f: u64| list.iter().any(|&h| h.abs_diff(f) <= 3_000);
        let target = survey::on_air(&[db], self.window()).into_iter().map(|c| c.0).find(|&f| {
            !near(&[tuning.control_hz], f)
                && tuning.in_window(f)
                && !near(&heard, f)
                && !self.probed.get(&f).is_some_and(|&t| at.mono.saturating_duration_since(t) < PROBE_AGAIN)
        });
        let Some(f) = target else { return };
        self.probe_last = Some(at.mono);
        self.probed.insert(f, at.mono);
        match self.tuner.retune_lane(Lane::One, f, true).await {
            Ok(_) => {
                if let Some(hw) = self.dmr_hw.as_mut() {
                    hw.receiver.retuned();
                    hw.tuned_hz = Some(f);
                }
                self.probe = Some(Probe { freq_hz: f, until: Instant::now() + PROBE_FOR });
            }
            Err(e) => self.log.system("lane", format!("{} could not listen to {:.5} MHz: {e:#}", Lane::One, f as f64 / 1e6)),
        }
    }

    /// The call book's events: the follower and the lanes learn of opens and closes; the log and
    /// the view get each call.
    fn call_events(&mut self, events: Vec<CallEvent>) {
        let now = Instant::now();
        // Announced after the view has the calls, so a listener can read them at once.
        let mut notices = Vec::new();
        for e in events {
            match e {
                CallEvent::Opened(o) => {
                    self.next_call.fetch_max(o.call + 1, Ordering::Relaxed);
                    notices.push(Notice::CallOpened { call: o.call, tg: o.tg, followed: o.lane.is_some() });
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
                            record: self.follower.record(o.tg),
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
                    // A frequency on trial counts as the site's channel once confirmed.
                    if let (Some(l), Some(f)) = (&self.learned, o.channel.freq_hz) {
                        if !self.lcn.trying(f, o.tg) {
                            l.grant(f);
                        }
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
                        notices.push(Notice::CallClosed { call: o.call, tg: o.tg });
                        self.recent.push_front(view_of(&o, Some(&c), self.codec));
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
        if !notices.is_empty() {
            self.publish();
            for n in notices {
                self.notices.send(n);
            }
        }
    }

    fn publish(&self) {
        let mut open: Vec<CallView> = Vec::new();
        for o in &self.open {
            let mut v = view_of(o, None, self.codec);
            let call = o.lane.and_then(|l| self.book.on_lane(l)).filter(|c| c.id == o.call);
            if let Some(c) = call {
                v.source = c.source;
                v.speaker = c.speaker;
                v.sources = c.sources.clone();
                v.voice_frames = c.voice_frames;
                v.first_voice_unix_ms = c.first_voice_unix_ms();
                v.codec = codec_of(o, c.voice_frames, self.codec);
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
/// up on noise), or its LSM's carrier loop ran to half its limit or more.
pub fn resume_needs_reset(carrier: Option<lsm::Carrier>, since_voice: Option<Duration>) -> bool {
    let stale = since_voice.is_none_or(|d| d > COAST_MAX_IDLE);
    stale || carrier.is_some_and(|c| c.hot())
}

fn view_of(o: &Opened, c: Option<&Closed>, codec: &'static str) -> CallView {
    let voice_frames = c.map_or(0, |c| c.voice_frames);
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
        emergency: o.emergency,
        private: o.private,
        not_followed: o.not_followed.map(|n| n.as_str().to_string()),
        lane: lane_number(o.lane),
        started_unix_ms: o.at_unix_ms,
        first_voice_unix_ms: c.and_then(|c| c.first_voice_unix_ms),
        ended_unix_ms: c.map(|c| c.ended_unix_ms),
        open_ms: c.map(|c| c.open_ms),
        grant_ms: c.map(grant_ms),
        close: c.map(|c| c.reason.as_str().to_string()),
        end_lc: c.and_then(|c| c.end_lc).map(str::to_string),
        sources: c.map(|c| c.sources.clone()).unwrap_or_default(),
        voice_frames,
        codec: codec_of(o, voice_frames, codec),
        frame_errors: None,
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
        emergency: o.emergency,
        private: o.private,
        first_voice_ms: c.first_voice_unix_ms,
        followed,
        not_followed: o.not_followed.map(|n| n.as_str().to_string()),
        voice_ms: c.voice_frames * 20,
        grant_ms: grant_ms(c),
        codec: codec_of(o, c.voice_frames, codec),
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
    use crate::radio::tuner::{Readback, TuningPlan};
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
        fn control_streams(&self, _: std::sync::mpsc::SyncSender<Block>, _: Arc<AtomicBool>, _: Arc<crate::radio::streams::StreamCounters>) {}
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
            channels_hz: Vec::new(),
            site_code: None,
            lanes: lanes.to_vec(),
            c4fm: false,
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
    fn lane_counters_keep_the_framer_fields_at_the_top() {
        let stats = FramerStats { nid_ok: 7, ..Default::default() };
        let v = serde_json::to_value(LaneCounters::P25 { demod: "c4fm", framer: Box::new(stats) }).unwrap();
        assert_eq!(v["protocol"], "p25");
        assert_eq!(v["demod"], "c4fm");
        assert_eq!(v["nid_ok"], 7);
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
        let carrier = |pll: f32| Some(lsm::Carrier { pll, offset_hz: 0.0, limit: 0.65, held: false });
        // Voice a moment ago, loop near centre: coast.
        assert!(!resume_needs_reset(carrier(0.05), Some(ms(400))));
        assert!(!resume_needs_reset(carrier(-0.3), Some(COAST_MAX_IDLE)));
        // The loop at or past half its limit: reset, however fresh.
        assert!(resume_needs_reset(carrier(0.65), Some(ms(100))));
        assert!(resume_needs_reset(carrier(-0.325), Some(ms(100))));
        // Gone longer than the coast window, or never any voice: reset.
        assert!(resume_needs_reset(carrier(0.0), Some(COAST_MAX_IDLE + ms(1))));
        assert!(resume_needs_reset(carrier(0.0), None));
        // No loop (a C4FM lane): the voice rule alone.
        assert!(!resume_needs_reset(None, Some(ms(100))));
    }

    #[test]
    fn the_recent_calls_merge_listed_and_stored_newest_first() {
        let row = |call_id: u64, started_ms: u64| CallRow {
            site: "cec_gcs".into(),
            call_id,
            started_ms,
            ended_ms: started_ms + 1_000,
            tg: 1,
            source: None,
            sources: Vec::new(),
            freq_hz: None,
            channel: None,
            timeslot: None,
            lane: 0,
            encrypted: false,
            emergency: false,
            private: false,
            first_voice_ms: None,
            followed: true,
            not_followed: None,
            voice_ms: 200,
            grant_ms: 1_000,
            codec: None,
            frames: 10,
            frame_errors: 0,
            close_reason: "end".into(),
            end_kind: None,
        };
        let listed = |call_id: u64, started_ms: u64| CallView { voice_frames: 99, ..view_of_row(row(call_id, started_ms)) };
        let merged = merge_recent(vec![listed(5, 500), listed(3, 300)], vec![row(5, 500), row(4, 400), row(2, 200)]);
        assert_eq!(merged.iter().map(|c| c.call).collect::<Vec<_>>(), vec![5, 4, 3, 2]);
        assert_eq!(merged[0].voice_frames, 99, "the listed call wins over its stored row");
        let many: Vec<CallRow> = (0..RECENT as u64 + 10).map(|i| row(i + 10, i)).collect();
        assert_eq!(merge_recent(Vec::new(), many).len(), RECENT);
    }
}
