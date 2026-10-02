//! Runs the live protocol's control decoders on the control chain's streams and publishes what
//! they report: messages to the event log, identity and health to the site card.
//!
//! One decode thread owns the decoders. At a P25 site it runs two: the HDL LSM demodulator's
//! dibits and the software C4FM demodulator on the IQ; only the chosen one's events are
//! published. In auto mode the one passing more TSBKs wins, with hysteresis and a dwell, so a
//! C4FM site moves to C4FM and an LSM site stays on LSM. At a DMR site the thread runs the DMR
//! receiver on the IQ, and the HDL chain's dibits are not read.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::protocol::dmr::control::{DmrControl, DmrStats};
use crate::protocol::dmr::demod::DmrDemodStats;
use crate::protocol::p25::framer::FramerStats;
use crate::protocol::events::{ControlEvent, SiteIdentity, UnitKind};
use crate::util::time::Stamp;
use crate::protocol::p25::c4fm::C4fmDecoder;
use crate::protocol::p25::control::P25Control;
use crate::radio::streams::{Input, StreamCounters, StreamSource, Wants};
use crate::services::config::systems::{Modulation, Protocol};
use crate::services::clock::Clock;
use crate::services::packet_data::PacketData;
use crate::services::events::EventLog;
use crate::services::iq::IqTap;
use crate::services::history::store::UnitEventKind;
use crate::services::history::HistoryTx;
use crate::trunking::learned::Learned;
use crate::trunking::trunk::{TrunkInput, TrunkTx};

/// Deliveries queued for the decode thread (about 6 s of IQ).
const QUEUE: usize = 64;
/// Health over this many one-second samples.
const RATE_WINDOW: usize = 10;

/// What the receivers need to know about the live site.
#[derive(Debug, Clone)]
pub struct Context {
    pub site: String,
    pub protocol: Protocol,
    pub modulation: Modulation,
    /// DMR: logical channel numbers to downlink Hz.
    pub lcn_hz: HashMap<u16, u64>,
    /// Where grants go.
    pub trunk: Option<TrunkTx>,
    /// What the site taught before; the channel plan heard goes back into it.
    pub learned: Option<Arc<Learned>>,
    /// Radio events (affiliations, registrations) go into the history.
    pub history: HistoryTx,
}

/// The control channel as the site card shows it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ControlStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<SiteIdentity>,
    /// P25: the demodulator whose messages are used ("lsm" or "c4fm").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modulation: Option<&'static str>,
    /// P25: TSBKs each demodulator passed in the last 20 s (what the choice compares).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsbks_20s: Option<TsbkWindow>,
    pub msgs_per_s: Option<f64>,
    /// P25: TSBKs passing their CRC; DMR: messages passing their checks.
    pub ok_pct: Option<f64>,
    pub last_message_age_ms: Option<u64>,
    /// Share of one core the decode thread uses.
    pub cpu_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier_offset_hz: Option<f64>,
    pub channel_plan_entries: usize,
    pub neighbours: usize,
    pub grants: u64,
    /// Grants the trunking task could not take (its queue was full).
    pub grants_dropped: u64,
    pub input: InputStatus,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct TsbkWindow {
    pub lsm: u64,
    pub c4fm: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct InputStatus {
    pub iq_chunks: u64,
    pub iq_dropped: u64,
    pub dibit_bytes: u64,
    pub dibit_resyncs: u64,
    pub dibit_lost: u64,
}

/// The control decoders' counters, for the diagnostics.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum Counters {
    /// Both demodulators run; `status.modulation` says whose messages are used.
    P25 { lsm: Box<FramerStats>, c4fm: Box<FramerStats> },
    Dmr { messages: DmrStats, demod: DmrDemodStats },
}

/// Shared between the decode thread and the API.
#[derive(Debug, Default)]
struct View {
    status: ControlStatus,
    last_message: Option<Instant>,
    counters: Option<Counters>,
}

/// Messages per second and the share passing, over the last `RATE_WINDOW` seconds.
#[derive(Debug, Default)]
struct RateWindow {
    last: Option<(u64, u64)>,
    samples: VecDeque<(u64, u64)>,
}

impl RateWindow {
    /// One-second sample of the cumulative (ok, total).
    fn sample(&mut self, ok: u64, total: u64) {
        if let Some((o, t)) = self.last {
            self.samples.push_back((ok.saturating_sub(o), total.saturating_sub(t)));
            while self.samples.len() > RATE_WINDOW {
                self.samples.pop_front();
            }
        }
        self.last = Some((ok, total));
    }

    fn rates(&self) -> (Option<f64>, Option<f64>) {
        if self.samples.is_empty() {
            return (None, None);
        }
        let (ok, total) = self.samples.iter().fold((0, 0), |a, s| (a.0 + s.0, a.1 + s.1));
        let per_s = ok as f64 / self.samples.len() as f64;
        let pct = (total > 0).then(|| (ok as f64 * 1000.0 / total as f64).round() / 10.0);
        (Some((per_s * 10.0).round() / 10.0), pct)
    }
}

/// The choice between the LSM and C4FM decoders of a P25 control channel.
#[derive(Debug)]
pub struct ModulationChoice {
    mode: Modulation,
    c4fm: bool,
    last: Option<(u64, u64)>,
    /// Per second: (C4FM, LSM) TSBKs passed.
    window: VecDeque<(u64, u64)>,
    last_switch: Option<Instant>,
}

impl ModulationChoice {
    /// Auto: the other decoder must pass this many TSBKs in the window and beat the active one
    /// by `SWITCH_RATIO`. A short window flapped on a weak LSM site where the two are close;
    /// hence 20 s and a dwell after a switch.
    const WINDOW_S: usize = 20;
    /// Seconds of counts before the first decision: the two demodulators start a few hundred
    /// milliseconds apart.
    const MIN_SAMPLES: usize = 10;
    const MIN_TSBKS: u64 = 30;
    const SWITCH_RATIO: f64 = 1.25;
    const MIN_DWELL: Duration = Duration::from_secs(60);

    pub fn new(mode: Modulation) -> Self {
        ModulationChoice { mode, c4fm: mode == Modulation::C4fm, last: None, window: VecDeque::new(), last_switch: None }
    }

    pub fn c4fm(&self) -> bool {
        self.c4fm
    }

    /// The per-second decision from the cumulative TSBKs passed. True when it switched.
    pub fn tick(&mut self, c4fm_ok: u64, lsm_ok: u64, now: Instant) -> bool {
        if let Some((c, l)) = self.last {
            self.window.push_back((c4fm_ok.saturating_sub(c), lsm_ok.saturating_sub(l)));
            while self.window.len() > Self::WINDOW_S {
                self.window.pop_front();
            }
        }
        self.last = Some((c4fm_ok, lsm_ok));
        let want = match self.mode {
            Modulation::C4fm => true,
            Modulation::Lsm => false,
            Modulation::Auto if self.window.len() < Self::MIN_SAMPLES => self.c4fm,
            Modulation::Auto if self.last_switch.is_some_and(|t| now.duration_since(t) < Self::MIN_DWELL) => self.c4fm,
            Modulation::Auto => self.better(),
        };
        if want == self.c4fm {
            return false;
        }
        self.c4fm = want;
        self.last_switch = Some(now);
        true
    }

    fn better(&self) -> bool {
        let (c4fm, lsm) = self.window.iter().fold((0, 0), |a, w| (a.0 + w.0, a.1 + w.1));
        let (mine, other) = if self.c4fm { (c4fm, lsm) } else { (lsm, c4fm) };
        if other >= Self::MIN_TSBKS && other as f64 > mine as f64 * Self::SWITCH_RATIO {
            !self.c4fm
        } else {
            self.c4fm
        }
    }

    /// TSBKs passed in the window: (C4FM, LSM).
    pub fn window_totals(&self) -> (u64, u64) {
        self.window.iter().fold((0, 0), |a, w| (a.0 + w.0, a.1 + w.1))
    }
}

struct Running {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
    sources: Vec<tokio::task::JoinHandle<()>>,
}

pub struct Receivers {
    log: Arc<EventLog>,
    /// Takes the site's time broadcasts.
    clock: Mutex<Option<Arc<Clock>>>,
    /// Takes the PDUs the control channel carries.
    data: Mutex<Option<Arc<PacketData>>>,
    view: Arc<Mutex<View>>,
    counters: Mutex<Arc<StreamCounters>>,
    /// Copies of the control IQ for captures.
    iq_tap: Arc<IqTap>,
    running: tokio::sync::Mutex<Option<Running>>,
}

impl Receivers {
    pub fn new(log: Arc<EventLog>) -> Self {
        Receivers {
            log,
            clock: Mutex::default(),
            data: Mutex::default(),
            view: Arc::default(),
            counters: Mutex::default(),
            iq_tap: Arc::default(),
            running: tokio::sync::Mutex::new(None),
        }
    }

    pub fn set_clock(&self, clock: Arc<Clock>) {
        if let Ok(mut c) = self.clock.lock() {
            *c = Some(clock);
        }
    }

    pub fn set_packet_data(&self, data: Arc<PacketData>) {
        if let Ok(mut d) = self.data.lock() {
            *d = Some(data);
        }
    }

    /// Start the live site's receivers (stopping any running ones first).
    pub async fn start(&self, context: Context, source: &impl StreamSource) {
        self.stop().await;
        let counters = Arc::new(StreamCounters::default());
        if let Ok(mut c) = self.counters.lock() {
            *c = counters.clone();
        }
        if let Ok(mut v) = self.view.lock() {
            *v = View {
                status: ControlStatus { running: true, site: Some(context.site.clone()), ..Default::default() },
                last_message: None,
                counters: None,
            };
        }
        let wants = match context.protocol {
            Protocol::P25 => Wants { iq: true, dibits: true },
            Protocol::DmrTier3 => Wants { iq: true, dibits: false },
        };
        let (tx, rx) = sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let decoder = Decoder {
            log: self.log.clone(),
            view: self.view.clone(),
            stop: stop.clone(),
            trunk: context.trunk.clone(),
            learned: context.learned.clone(),
            history: context.history.clone(),
            site: context.site.clone(),
            clock: self.clock.lock().ok().and_then(|c| c.clone()),
            data: self.data.lock().ok().and_then(|d| d.clone()),
            iq_tap: self.iq_tap.clone(),
        };
        let name = match context.protocol {
            Protocol::P25 => "p25-cc",
            Protocol::DmrTier3 => "dmr-cc",
        };
        let thread = match std::thread::Builder::new().name(name.into()).spawn(move || decoder.run(context, rx)) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("control decode thread not started: {e}");
                return;
            }
        };
        let sources = source.control_streams(wants, tx, stop.clone(), counters);
        *self.running.lock().await = Some(Running { stop, thread, sources });
    }

    /// Stop the receivers and wait for the decode thread to finish.
    pub async fn stop(&self) {
        let Some(r) = self.running.lock().await.take() else { return };
        r.stop.store(true, Ordering::Relaxed);
        for s in &r.sources {
            s.abort();
        }
        let _ = tokio::task::spawn_blocking(move || r.thread.join()).await;
        if let Ok(mut v) = self.view.lock() {
            v.status.running = false;
        }
    }

    pub fn iq_tap(&self) -> &Arc<IqTap> {
        &self.iq_tap
    }

    /// The counters of the running decoders, updated each second.
    pub fn counters(&self) -> Option<Counters> {
        self.view.lock().ok().and_then(|v| v.counters.clone())
    }

    pub fn status(&self) -> ControlStatus {
        let (mut status, last) = match self.view.lock() {
            Ok(v) => (v.status.clone(), v.last_message),
            Err(_) => return ControlStatus::default(),
        };
        status.last_message_age_ms = last.map(|t| t.elapsed().as_millis() as u64);
        if let Ok(c) = self.counters.lock() {
            let l = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
            status.input = InputStatus {
                iq_chunks: l(&c.iq_chunks),
                iq_dropped: l(&c.iq_dropped),
                dibit_bytes: l(&c.dibit_bytes),
                dibit_resyncs: l(&c.dibit_resyncs),
                dibit_lost: l(&c.dibit_lost),
            };
        }
        status
    }
}

/// The decode thread's handles.
struct Decoder {
    log: Arc<EventLog>,
    view: Arc<Mutex<View>>,
    stop: Arc<AtomicBool>,
    trunk: Option<TrunkTx>,
    learned: Option<Arc<Learned>>,
    history: HistoryTx,
    site: String,
    clock: Option<Arc<Clock>>,
    data: Option<Arc<PacketData>>,
    iq_tap: Arc<IqTap>,
}

/// Busy time over the last few seconds, as a share of one core.
struct Cpu {
    busy: Duration,
    since: Instant,
    pct: f64,
}

impl Cpu {
    fn new() -> Self {
        Cpu { busy: Duration::ZERO, since: Instant::now(), pct: 0.0 }
    }

    fn add(&mut self, d: Duration) {
        self.busy += d;
        let elapsed = self.since.elapsed();
        if elapsed >= Duration::from_secs(5) {
            self.pct = (self.busy.as_secs_f64() / elapsed.as_secs_f64() * 1000.0).round() / 10.0;
            self.busy = Duration::ZERO;
            self.since = Instant::now();
        }
    }
}

/// Unpack dibit ring bytes: four dibits a byte, the first in the low bits.
fn unpack(bytes: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend(bytes.iter().flat_map(|b| [b & 3, (b >> 2) & 3, (b >> 4) & 3, (b >> 6) & 3]));
}

impl Decoder {
    fn run(self, context: Context, rx: Receiver<Input>) {
        tracing::info!("{} receivers on site {}", protocol_name(context.protocol), context.site);
        match context.protocol {
            Protocol::P25 => self.run_p25(&context, rx),
            Protocol::DmrTier3 => self.run_dmr(&context, rx),
        }
    }

    fn next(&self, rx: &Receiver<Input>) -> Option<Option<Input>> {
        if self.stop.load(Ordering::Relaxed) {
            return None;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(input) => Some(Some(input)),
            Err(RecvTimeoutError::Timeout) => Some(None),
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    fn run_p25(&self, context: &Context, rx: Receiver<Input>) {
        let mut lsm = P25Control::new("control");
        let mut c4fm = P25Control::new("control");
        if let Some(l) = &self.learned {
            let bands = l.bands();
            lsm.seed_bands(&bands);
            c4fm.seed_bands(&bands);
        }
        let mut demod = C4fmDecoder::new();
        let mut choice = ModulationChoice::new(context.modulation);
        let mut rates = RateWindow::default();
        let mut cpu = Cpu::new();
        let mut second = Instant::now();
        let mut dibits = Vec::new();
        let mut events = Vec::new();
        self.set(|s| s.modulation = Some(if choice.c4fm() { "c4fm" } else { "lsm" }));
        while let Some(input) = self.next(&rx) {
            let t0 = Instant::now();
            let now = Stamp::now();
            match input {
                Some(Input::Dibits { bytes, reset }) => {
                    if reset {
                        lsm.retuned();
                    }
                    unpack(&bytes, &mut dibits);
                    lsm.push(&dibits, now, &mut events);
                    if !choice.c4fm() {
                        self.publish("p25", now, &events);
                    }
                    events.clear();
                }
                Some(Input::Iq(iq)) => {
                    self.iq_tap.push(&iq);
                    c4fm.push_c4fm(&mut demod, &iq, now, &mut events);
                    if choice.c4fm() {
                        self.publish("p25", now, &events);
                    }
                    events.clear();
                }
                None => {}
            }
            cpu.add(t0.elapsed());
            if second.elapsed() >= Duration::from_secs(1) {
                second = Instant::now();
                if choice.tick(c4fm.stats().tsbk_ok(), lsm.stats().tsbk_ok(), second) {
                    let label = if choice.c4fm() { "C4FM" } else { "LSM" };
                    let (c, l) = choice.window_totals();
                    self.log.system("modulation", format!("control channel decoded as {label} (TSBKs in 20 s: C4FM {c}, LSM {l})"));
                }
                let active = if choice.c4fm() { &c4fm } else { &lsm };
                let s = active.stats();
                rates.sample(s.tsbk_ok(), s.tsbk_attempts());
                let (per_s, pct) = rates.rates();
                let a = active.announced();
                let modulation = if choice.c4fm() { "c4fm" } else { "lsm" };
                let (c, l) = choice.window_totals();
                self.set(|v| {
                    v.modulation = Some(modulation);
                    v.tsbks_20s = Some(TsbkWindow { lsm: l, c4fm: c });
                    v.msgs_per_s = per_s;
                    v.ok_pct = pct;
                    v.cpu_pct = cpu.pct;
                    v.channel_plan_entries = a.bands.len();
                    v.neighbours = a.neighbours.len();
                });
                self.set_counters(Counters::P25 { lsm: Box::new(lsm.stats().clone()), c4fm: Box::new(c4fm.stats().clone()) });
            }
        }
        tracing::info!("P25 receivers on site {} stopped", context.site);
    }

    fn run_dmr(&self, context: &Context, rx: Receiver<Input>) {
        let mut dmr = DmrControl::new(context.lcn_hz.clone());
        let mut rates = RateWindow::default();
        let mut cpu = Cpu::new();
        let mut second = Instant::now();
        let mut events = Vec::new();
        while let Some(input) = self.next(&rx) {
            let t0 = Instant::now();
            if let Some(Input::Iq(iq)) = input {
                self.iq_tap.push(&iq);
                dmr.push(&iq, &mut events);
                self.publish("dmr", Stamp::now(), &events);
                events.clear();
            }
            cpu.add(t0.elapsed());
            if second.elapsed() >= Duration::from_secs(1) {
                second = Instant::now();
                let s = &dmr.stats;
                rates.sample(s.msgs_valid, s.msgs_valid + s.msgs_invalid);
                let (per_s, pct) = rates.rates();
                let offset = dmr.carrier_offset_hz().map(f64::round);
                self.set(|v| {
                    v.msgs_per_s = per_s;
                    v.ok_pct = pct;
                    v.cpu_pct = cpu.pct;
                    v.carrier_offset_hz = offset;
                    v.channel_plan_entries = context.lcn_hz.len();
                });
                self.set_counters(Counters::Dmr { messages: dmr.stats.clone(), demod: dmr.demod_stats() });
            }
        }
        tracing::info!("DMR receivers on site {} stopped", context.site);
    }

    fn set(&self, f: impl FnOnce(&mut ControlStatus)) {
        if let Ok(mut v) = self.view.lock() {
            f(&mut v.status);
        }
    }

    fn set_counters(&self, c: Counters) {
        if let Ok(mut v) = self.view.lock() {
            v.counters = Some(c);
        }
    }

    /// The active decoder's events: messages to the event log, identity and counts to the view.
    fn publish(&self, source: &'static str, now: Stamp, events: &[ControlEvent]) {
        if events.is_empty() {
            return;
        }
        let Ok(mut v) = self.view.lock() else { return };
        for e in events {
            match e {
                ControlEvent::Message(line) => {
                    if line.valid {
                        v.last_message = Some(now.mono);
                    }
                    self.log.message(source, now.unix_ms, line);
                }
                ControlEvent::Identity(identity) => v.status.identity = Some(*identity),
                ControlEvent::ChannelPlan(crate::protocol::events::PlanEntry::P25Band(band)) => {
                    if let Some(l) = &self.learned {
                        l.band(band);
                    }
                }
                ControlEvent::SiteTime(sync) => {
                    if let Some(c) = &self.clock {
                        c.observe(*sync);
                    }
                }
                ControlEvent::Pdu(frame) => {
                    if let Some(d) = &self.data {
                        d.pdu(frame, &self.site);
                    }
                }
                ControlEvent::Neighbour(n) => {
                    if let Some(l) = &self.learned {
                        l.neighbour(n, now.unix_ms);
                    }
                }
                ControlEvent::SecondaryControl(channels) => {
                    if let Some(l) = &self.learned {
                        l.secondary_control(channels);
                    }
                }
                ControlEvent::DataChannel(channel) => {
                    if let Some(l) = &self.learned {
                        l.data_channel(channel);
                    }
                }
                ControlEvent::Unit { unit, group, kind } => {
                    let kind = match kind {
                        UnitKind::GroupAffiliation => UnitEventKind::GroupAffiliation,
                        UnitKind::Registration => UnitEventKind::Registration,
                        UnitKind::Deregistration => UnitEventKind::Deregistration,
                    };
                    self.history.unit(&self.site, *unit, group.unwrap_or(0), kind, now.unix_ms);
                }
                ControlEvent::Grant(grant) => {
                    v.status.grants += 1;
                    if let Some(trunk) = &self.trunk {
                        let nac = match v.status.identity {
                            Some(SiteIdentity::P25(id)) => id.nac.unwrap_or(0),
                            _ => 0,
                        };
                        if trunk.try_send(TrunkInput::Grant { grant: *grant, nac, at: now }).is_err() {
                            v.status.grants_dropped += 1;
                        }
                    }
                }
            }
        }
    }
}

fn protocol_name(p: Protocol) -> &'static str {
    match p {
        Protocol::P25 => "P25",
        Protocol::DmrTier3 => "DMR",
    }
}

#[cfg(test)]
#[path = "receivers_tests.rs"]
mod tests;
