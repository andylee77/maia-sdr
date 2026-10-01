//! Change 075: DMR receive on the control IQ hub, first as a monitor.
//!
//! Off by default (`PUT /api/dmr?enabled=1`). When on, a thread runs the
//! software DMR receiver (`protocol::dmr`, SDRTrunk's: demodulator, framer,
//! message processor) on the control DDC's IQ, beside the P25 decoders. It
//! keeps what `/api/dmr` serves: syncs, bursts per timeslot, CACH, voice, the
//! carrier offset the equaliser learned, the CPU it costs, message counts by
//! class, recent grants; and `/api/dmr/messages` the last messages in
//! SDRTrunk's text.
//!
//! With `follow` on (`PUT /api/dmr?follow=1`), the call follower
//! (`app::dmr_follower`) takes the control channel's grants: an executor task
//! moves traffic chain 1 to the granted channel, and a second DMR thread on
//! chain 1's IQ feeds the call's bursts back to the follower.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::app::dmr_follower::{DmrFollower, FollowerAction};
use crate::protocol::dmr::framer::FramerEvent;
use crate::protocol::dmr::message::DmrMessage;

/// Messages kept for `/api/dmr/messages?all=1` (~12 s of a control channel).
const MESSAGE_RING: usize = 500;
/// Messages other than the control channel's filler, for `/api/dmr/messages`.
const EVENT_RING: usize = 2000;
/// A control channel's steady filler: counted, kept only in the short ring.
const FILLER_CLASSES: [&str; 4] = ["Aloha", "IDLEMessage", "ControlChannelSystemParameters", "NullMessage"];
/// Grants kept for `/api/dmr`.
const GRANT_RING: usize = 50;

/// LCN -> downlink frequency when the active site gives none (DMR switched
/// on by hand on a P25 site): Clay Electric Cooperative, Green Cove Springs
/// (control channel LCN 5, voice LCN 6). A DMR site's `lcn_map` replaces it.
pub const DEFAULT_LCN_MAP: [(u16, u64); 2] = [(5, 454_368_750), (6, 451_087_500)];

/// A decoded message as `/api/dmr/messages` lists it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MessageRecord {
    pub unix_ms: u64,
    pub timeslot: u8,
    pub valid: bool,
    pub class: &'static str,
    pub text: String,
}

/// A channel grant seen on the control channel.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GrantRecord {
    pub unix_ms: u64,
    pub class: &'static str,
    pub lcn: u16,
    pub timeslot: u8,
    pub downlink_hz: Option<u64>,
    pub text: String,
}

/// Counters of the DMR path. Demodulator figures count from the last reset
/// (enable or retune); burst figures from the last enable.
#[derive(Default)]
pub struct DmrRuntime {
    pub enabled: AtomicBool,
    pub chunks: AtomicU64,
    pub lagged: AtomicU64,
    pub resets: AtomicU64,
    /// Share of one core, x100.
    pub cpu_centi_pct: AtomicU64,
    /// Equaliser balance (the carrier offset), milliradians per symbol.
    pub balance_mrad: AtomicI64,
    /// Equaliser gain, x1000.
    pub gain_milli: AtomicU64,
    pub coarse_syncs: AtomicU64,
    pub fine_syncs: AtomicU64,
    pub fine_sync_losses: AtomicU64,
    /// Bursts by timeslot: unknown, 1, 2.
    pub bursts: [AtomicU64; 3],
    pub voice_bursts: AtomicU64,
    pub cach_ok: AtomicU64,
    pub cach_bad: AtomicU64,
    pub sync_loss_bits: AtomicU64,
    pub last_burst_unix_ms: AtomicU64,
    /// (valid, invalid) messages by SDRTrunk class name.
    pub classes: Mutex<BTreeMap<&'static str, (u64, u64)>>,
    pub messages: Mutex<VecDeque<MessageRecord>>,
    /// Everything but the filler (grants, PROTECT, CLEAR, voice, LC, ...).
    pub events: Mutex<VecDeque<MessageRecord>>,
    pub grants: Mutex<VecDeque<GrantRecord>>,
    /// Follow grants onto traffic chain 1.
    pub follow: AtomicBool,
    pub follower: Mutex<DmrFollower>,
    /// The follower's actions, to the executor task.
    pub actions_tx: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<FollowerAction>>,
    /// Bumped at every traffic-chain retune: the traffic thread restarts.
    pub traffic_epoch: AtomicU64,
    pub traffic_chunks: AtomicU64,
    pub traffic_bursts: AtomicU64,
    pub traffic_voice_bursts: AtomicU64,
    pub traffic_fine_syncs: AtomicU64,
    pub traffic_cpu_centi_pct: AtomicU64,
    pub tunes: AtomicU64,
    pub tune_errors: AtomicU64,
    /// The follower's actions, as text, newest last.
    pub follower_log: Mutex<VecDeque<(u64, String)>>,
    /// The active DMR site's LCN map (`DEFAULT_LCN_MAP` until one is set).
    pub lcn_map: Mutex<Option<HashMap<u16, u64>>>,
    /// Bumped when the LCN map changes: the threads rebuild their receivers.
    pub config_epoch: AtomicU64,
    /// AMBE+2 frames decoded, and those with bit errors.
    pub vocoder_frames: AtomicU64,
    pub vocoder_frame_errors: AtomicU64,
    /// Voice bursts dropped because the vocoder queue was full.
    pub voice_dropped: AtomicU64,
}

impl DmrRuntime {
    /// Zero the counters (a new enable).
    pub fn clear(&self) {
        for c in [
            &self.chunks,
            &self.lagged,
            &self.resets,
            &self.cpu_centi_pct,
            &self.gain_milli,
            &self.coarse_syncs,
            &self.fine_syncs,
            &self.fine_sync_losses,
            &self.voice_bursts,
            &self.cach_ok,
            &self.cach_bad,
            &self.sync_loss_bits,
            &self.last_burst_unix_ms,
        ] {
            c.store(0, Ordering::Relaxed);
        }
        for b in &self.bursts {
            b.store(0, Ordering::Relaxed);
        }
        self.balance_mrad.store(0, Ordering::Relaxed);
        if let Ok(mut c) = self.classes.lock() {
            c.clear();
        }
        if let Ok(mut m) = self.messages.lock() {
            m.clear();
        }
        if let Ok(mut e) = self.events.lock() {
            e.clear();
        }
        if let Ok(mut g) = self.grants.lock() {
            g.clear();
        }
    }

    /// Keeps a decoded message: class counts, the message ring, grants.
    pub fn record(&self, message: &DmrMessage) {
        let class = message.class_name();
        let valid = message.is_valid();
        let now = crate::app::now_unix_ms();
        if let Ok(mut c) = self.classes.lock() {
            let e = c.entry(class).or_default();
            if valid {
                e.0 += 1;
            } else {
                e.1 += 1;
            }
        }
        let text = message.to_string();
        if let DmrMessage::Csbk(csbk) = message {
            if let (true, true, Some(ch)) = (valid, class.contains("Grant"), csbk.channel) {
                if let Ok(mut g) = self.grants.lock() {
                    g.push_back(GrantRecord {
                        unix_ms: now,
                        class,
                        lcn: ch.lcn,
                        timeslot: ch.timeslot,
                        downlink_hz: ch.downlink_hz,
                        text: text.clone(),
                    });
                    while g.len() > GRANT_RING {
                        g.pop_front();
                    }
                }
            }
        }
        let record = MessageRecord { unix_ms: now, timeslot: message.timeslot(), valid, class, text };
        if !FILLER_CLASSES.contains(&class) {
            if let Ok(mut e) = self.events.lock() {
                e.push_back(record.clone());
                while e.len() > EVENT_RING {
                    e.pop_front();
                }
            }
        }
        if let Ok(mut m) = self.messages.lock() {
            m.push_back(record);
            while m.len() > MESSAGE_RING {
                m.pop_front();
            }
        }
    }

    /// The LCN map the receivers use now.
    pub fn lcn_map(&self) -> HashMap<u16, u64> {
        self.lcn_map
            .lock()
            .ok()
            .and_then(|m| m.clone())
            .unwrap_or_else(|| DEFAULT_LCN_MAP.iter().copied().collect())
    }

    /// Change 075: a site became active. A DMR site switches the receiver
    /// and the follower on with its LCN map; a P25 site switches them off.
    pub fn apply_site(&self, site: &crate::services::sites::Site) {
        if site.is_dmr() {
            let map: HashMap<u16, u64> = site.lcn_map.iter().map(|(k, v)| (*k, *v)).collect();
            if let Ok(mut m) = self.lcn_map.lock() {
                *m = Some(map);
            }
            self.config_epoch.fetch_add(1, Ordering::Relaxed);
            if !self.enabled.load(Ordering::Relaxed) {
                self.clear();
            }
            self.enabled.store(true, Ordering::Relaxed);
            self.follow.store(true, Ordering::Relaxed);
            tracing::info!("dmr: site {} is DMR: receiver and follower on, LCN map {:?}", site.name, site.lcn_map);
        } else {
            self.enabled.store(false, Ordering::Relaxed);
            self.follow.store(false, Ordering::Relaxed);
            if let Ok(mut m) = self.lcn_map.lock() {
                *m = None;
            }
            self.config_epoch.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut f) = self.follower.lock() {
            *f = DmrFollower::new();
        }
    }

    /// Hands follower actions to the executor and logs them.
    pub fn act(&self, actions: Vec<FollowerAction>) {
        if actions.is_empty() {
            return;
        }
        let now = crate::app::now_unix_ms();
        if let Ok(mut log) = self.follower_log.lock() {
            for a in &actions {
                if !matches!(a, FollowerAction::Voice { .. } | FollowerAction::KeepAlive { .. }) {
                    log.push_back((now, format!("{a:?}")));
                }
            }
            while log.len() > 100 {
                log.pop_front();
            }
        }
        if let Some(tx) = self.actions_tx.get() {
            for a in actions {
                let _ = tx.send(a);
            }
        }
    }

    /// The control channel's message, to the follower (when following).
    /// A call granted on the control repeater itself (its other timeslot)
    /// is in the control receiver's bursts too: they go to the follower as
    /// the call's traffic, and the traffic thread leaves that call alone.
    pub fn follow_control(&self, message: &DmrMessage, control_hz: u64) {
        if !self.follow.load(Ordering::Relaxed) {
            return;
        }
        let now = crate::app::now_unix_ms();
        let actions = match self.follower.lock() {
            Ok(mut f) => {
                let mut a = f.on_control(message, now);
                if f.following().and_then(|g| g.freq_hz) == Some(control_hz) {
                    a.extend(f.on_traffic(message, now));
                }
                a.extend(f.tick(now));
                a
            }
            Err(_) => return,
        };
        self.act(actions);
    }

    /// The last `n` messages (oldest first): every message when `all`, else
    /// those other than the filler; optionally only class names containing
    /// `class`.
    pub fn recent_messages(&self, n: usize, class: Option<&str>, all: bool) -> Vec<MessageRecord> {
        let ring = if all { &self.messages } else { &self.events };
        let Ok(m) = ring.lock() else { return Vec::new() };
        let mut out: Vec<MessageRecord> =
            m.iter().rev().filter(|r| class.map_or(true, |c| r.class.contains(c))).take(n).cloned().collect();
        out.reverse();
        out
    }

    /// Counts what the framer produced.
    pub fn count(&self, event: &FramerEvent) {
        match event {
            FramerEvent::Burst(b) => {
                self.bursts[(b.timeslot as usize).min(2)].fetch_add(1, Ordering::Relaxed);
                if b.pattern.is_voice_pattern() {
                    self.voice_bursts.fetch_add(1, Ordering::Relaxed);
                }
                if b.pattern.has_cach() {
                    let c = if b.cach.valid { &self.cach_ok } else { &self.cach_bad };
                    c.fetch_add(1, Ordering::Relaxed);
                }
                self.last_burst_unix_ms.store(crate::app::now_unix_ms(), Ordering::Relaxed);
            }
            FramerEvent::SyncLoss { bits, .. } => {
                self.sync_loss_bits.fetch_add(*bits as u64, Ordering::Relaxed);
            }
        }
    }

    /// The `/api/dmr` body.
    pub fn snapshot(&self) -> serde_json::Value {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let balance = self.balance_mrad.load(Ordering::Relaxed) as f64 / 1000.0;
        let (ok, bad) = (l(&self.cach_ok), l(&self.cach_bad));
        serde_json::json!({
            "enabled": self.enabled.load(Ordering::Relaxed),
            "cpu_pct": l(&self.cpu_centi_pct) as f64 / 100.0,
            "chunks": l(&self.chunks),
            "lagged": l(&self.lagged),
            "resets": l(&self.resets),
            "demod": {
                "coarse_syncs": l(&self.coarse_syncs),
                "fine_syncs": l(&self.fine_syncs),
                "fine_sync_losses": l(&self.fine_sync_losses),
                "balance_rad_per_symbol": balance,
                // The balance corrects the phase per symbol: offset = -balance x 4800 / 2 pi.
                "carrier_offset_hz": (-balance * 4800.0 / std::f64::consts::TAU).round(),
                "gain": l(&self.gain_milli) as f64 / 1000.0,
            },
            "bursts": { "ts_unknown": l(&self.bursts[0]), "ts1": l(&self.bursts[1]), "ts2": l(&self.bursts[2]) },
            "voice_bursts": l(&self.voice_bursts),
            "cach": { "ok": ok, "bad": bad, "ok_pct": if ok + bad > 0 { ok as f64 * 100.0 / (ok + bad) as f64 } else { 0.0 } },
            "sync_loss_bits": l(&self.sync_loss_bits),
            "last_burst_unix_ms": l(&self.last_burst_unix_ms),
            "classes": self.classes.lock().map(|c| {
                c.iter()
                    .map(|(k, (ok, bad))| (k.to_string(), serde_json::json!({ "valid": ok, "invalid": bad })))
                    .collect::<serde_json::Map<_, _>>()
            }).unwrap_or_default(),
            "recent_grants": self.grants.lock().map(|g| g.iter().rev().take(20).cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
            "follow": self.follow.load(Ordering::Relaxed),
            "following": self.follower.lock().ok().and_then(|f| f.following()).map(|g| serde_json::json!({
                "talkgroup": g.talkgroup, "source": g.source, "private": g.private,
                "lcn": g.lcn, "timeslot": g.timeslot, "freq_hz": g.freq_hz,
            })),
            "traffic": {
                "tuned_hz": self.follower.lock().ok().and_then(|f| f.tuned_hz()),
                "tunes": l(&self.tunes),
                "tune_errors": l(&self.tune_errors),
                "epoch": l(&self.traffic_epoch),
                "chunks": l(&self.traffic_chunks),
                "bursts": l(&self.traffic_bursts),
                "voice_bursts": l(&self.traffic_voice_bursts),
                "fine_syncs": l(&self.traffic_fine_syncs),
                "cpu_pct": l(&self.traffic_cpu_centi_pct) as f64 / 100.0,
            },
            "vocoder": {
                "frames": l(&self.vocoder_frames),
                "frames_with_errors": l(&self.vocoder_frame_errors),
                "dropped_bursts": l(&self.voice_dropped),
            },
            "follower_log": self.follower_log.lock().map(|g| g.iter().rev().take(30).cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
        })
    }
}

/// The DMR thread: control IQ -> DMR demodulator -> framer -> counters.
#[cfg(target_os = "linux")]
pub fn spawn_dmr_control(
    hub: std::sync::Arc<crate::app::iq_hub::IqHub>,
    control_freq: std::sync::Arc<AtomicU64>,
    rx_lo: std::sync::Arc<AtomicI64>,
    rt: std::sync::Arc<DmrRuntime>,
) {
    use crate::protocol::dmr::demod::DmrDemodulator;
    use crate::protocol::dmr::framer::DmrMessageFramer;
    use crate::protocol::dmr::message::processor::DmrMessageProcessor;
    use tokio::sync::broadcast::error::RecvError;
    let spawned = std::thread::Builder::new().name("dmr-cc".into()).spawn(move || {
        let mut rx = hub.subscribe();
        let mut chain: Option<(DmrDemodulator, DmrMessageFramer, DmrMessageProcessor)> = None;
        let mut tuned = (0u64, 0i64, 0u64);
        let mut busy = std::time::Duration::ZERO;
        let mut since = std::time::Instant::now();
        loop {
            let chunk = match rx.blocking_recv() {
                Ok(c) => c,
                Err(RecvError::Lagged(_)) => {
                    if chain.is_some() {
                        rt.lagged.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            if !rt.enabled.load(Ordering::Relaxed) {
                chain = None;
                continue;
            }
            // Enabled, or retuned: start from scratch (timing and equaliser
            // belong to the old channel).
            let now_tuned = (
                control_freq.load(Ordering::Relaxed),
                rx_lo.load(Ordering::Relaxed),
                rt.config_epoch.load(Ordering::Relaxed),
            );
            if chain.is_none() || now_tuned != tuned {
                if chain.is_some() {
                    rt.resets.fetch_add(1, Ordering::Relaxed);
                }
                tuned = now_tuned;
                chain = Some((DmrDemodulator::new(), DmrMessageFramer::default(), DmrMessageProcessor::new(rt.lcn_map())));
                busy = std::time::Duration::ZERO;
                since = std::time::Instant::now();
            }
            let Some((demod, framer, processor)) = chain.as_mut() else { continue };
            let t0 = std::time::Instant::now();
            demod.process_iq_i16(&chunk, framer);
            let events: Vec<FramerEvent> = framer.drain().collect();
            for event in events {
                rt.count(&event);
                let control_hz = control_freq.load(Ordering::Relaxed);
                for message in processor.process(event) {
                    rt.record(&message);
                    rt.follow_control(&message, control_hz);
                }
            }
            busy += t0.elapsed();
            rt.chunks.fetch_add(1, Ordering::Relaxed);
            let s = demod.symbols.stats;
            rt.coarse_syncs.store(s.coarse_syncs, Ordering::Relaxed);
            rt.fine_syncs.store(s.fine_syncs, Ordering::Relaxed);
            rt.fine_sync_losses.store(s.fine_sync_losses, Ordering::Relaxed);
            if since.elapsed() >= std::time::Duration::from_secs(5) {
                let pct = busy.as_secs_f64() / since.elapsed().as_secs_f64() * 100.0;
                rt.cpu_centi_pct.store((pct * 100.0) as u64, Ordering::Relaxed);
                rt.balance_mrad.store((demod.symbols.equalizer_balance() * 1000.0) as i64, Ordering::Relaxed);
                rt.gain_milli.store((demod.symbols.equalizer_gain() * 1000.0) as u64, Ordering::Relaxed);
                busy = std::time::Duration::ZERO;
                since = std::time::Instant::now();
            }
        }
        tracing::warn!("dmr control thread exiting (IQ hub closed)");
    });
    if let Err(e) = spawned {
        tracing::error!("dmr control thread not started: {e}");
    }
}

/// The DMR traffic thread: chain 1's IQ -> DMR receiver -> the follower.
/// Runs only while following; a retune (`traffic_epoch`) restarts the
/// receiver and drops the chunk that may straddle it.
#[cfg(target_os = "linux")]
pub fn spawn_dmr_traffic(
    hub: std::sync::Arc<crate::app::iq_hub::IqHub>,
    control_freq: std::sync::Arc<AtomicU64>,
    rt: std::sync::Arc<DmrRuntime>,
) {
    use crate::protocol::dmr::demod::DmrDemodulator;
    use crate::protocol::dmr::framer::DmrMessageFramer;
    use crate::protocol::dmr::message::processor::DmrMessageProcessor;
    use tokio::sync::broadcast::error::RecvError;
    let spawned = std::thread::Builder::new().name("dmr-traffic".into()).spawn(move || {
        let mut rx = hub.subscribe();
        let mut chain: Option<(DmrDemodulator, DmrMessageFramer, DmrMessageProcessor)> = None;
        let mut epoch = (u64::MAX, u64::MAX);
        let mut busy = std::time::Duration::ZERO;
        let mut since = std::time::Instant::now();
        loop {
            let chunk = match rx.blocking_recv() {
                Ok(c) => c,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            };
            if !rt.enabled.load(Ordering::Relaxed) || !rt.follow.load(Ordering::Relaxed) {
                chain = None;
                continue;
            }
            let now_epoch = (rt.traffic_epoch.load(Ordering::Relaxed), rt.config_epoch.load(Ordering::Relaxed));
            if now_epoch != epoch {
                // Retuned: this chunk may hold the old channel's samples.
                epoch = now_epoch;
                let mut demod = DmrDemodulator::new();
                demod.symbols.set_base_station_mode();
                chain = Some((demod, DmrMessageFramer::default(), DmrMessageProcessor::new(rt.lcn_map())));
                continue;
            }
            let Some((demod, framer, processor)) = chain.as_mut() else { continue };
            let t0 = std::time::Instant::now();
            demod.process_iq_i16(&chunk, framer);
            let events: Vec<FramerEvent> = framer.drain().collect();
            let now = crate::app::now_unix_ms();
            let mut actions = Vec::new();
            for event in events {
                if let FramerEvent::Burst(b) = &event {
                    rt.traffic_bursts.fetch_add(1, Ordering::Relaxed);
                    if b.pattern.is_voice_pattern() {
                        rt.traffic_voice_bursts.fetch_add(1, Ordering::Relaxed);
                    }
                }
                for message in processor.process(event) {
                    if let Ok(mut f) = rt.follower.lock() {
                        // A call on the control repeater comes from the
                        // control receiver (`follow_control`), not twice.
                        let on_control_repeater =
                            f.following().and_then(|g| g.freq_hz) == Some(control_freq.load(Ordering::Relaxed));
                        if !on_control_repeater {
                            actions.extend(f.on_traffic(&message, now));
                        }
                    }
                }
            }
            if let Ok(mut f) = rt.follower.lock() {
                actions.extend(f.tick(now));
            }
            rt.act(actions);
            busy += t0.elapsed();
            rt.traffic_chunks.fetch_add(1, Ordering::Relaxed);
            rt.traffic_fine_syncs.store(demod.symbols.stats.fine_syncs, Ordering::Relaxed);
            if since.elapsed() >= std::time::Duration::from_secs(5) {
                let pct = busy.as_secs_f64() / since.elapsed().as_secs_f64() * 100.0;
                rt.traffic_cpu_centi_pct.store((pct * 100.0) as u64, Ordering::Relaxed);
                busy = std::time::Duration::ZERO;
                since = std::time::Instant::now();
            }
        }
        tracing::warn!("dmr traffic thread exiting (IQ hub closed)");
    });
    if let Err(e) = spawned {
        tracing::error!("dmr traffic thread not started: {e}");
    }
}

/// The call-pipeline side of the executor: which call the lifecycle opened
/// for the followed DMR transmission, and when voice last refreshed it.
#[derive(Default)]
struct LifecycleLink {
    /// The lifecycle's call on lane One for the followed grant (0 = none).
    call_id: u64,
    last_voice_refresh_ms: u64,
}

/// Voice refreshes the lifecycle (keep-alive, end-marker cancel) at most
/// this often.
const VOICE_REFRESH_MS: u64 = 500;

/// Turns a follower action into call-boundary events for the shared call
/// lifecycle (`app::grant_follower`), so DMR calls land in Recent calls,
/// recordings and the history like P25 ones. Lane One: the DMR follower
/// uses traffic chain 1.
fn boundary_events(
    action: &FollowerAction,
    link: &mut LifecycleLink,
    now_ms: u64,
) -> Vec<crate::audio::CallBoundary> {
    use crate::audio::{CallBoundary, CallBoundaryKind};
    use crate::hardware::traffic_lane::Lane;
    let event = |kind: CallBoundaryKind, talkgroup: Option<u32>, lane: Option<Lane>| CallBoundary {
        kind,
        nac: 0,
        talkgroup,
        expected_submit_count: 0,
        lane,
    };
    match *action {
        FollowerAction::Grant { grant, not_followed } => vec![event(
            CallBoundaryKind::CcGrantArrival {
                tg: grant.talkgroup,
                source: grant.source,
                freq_hz: grant.freq_hz,
                channel: grant.lcn,
                encrypted: false,
                not_followed,
            },
            Some(grant.talkgroup),
            if not_followed.is_none() { Some(Lane::One) } else { None },
        )],
        FollowerAction::KeepAlive { talkgroup, freq_hz, lcn } => vec![event(
            CallBoundaryKind::CcGrantUpdate { tg: talkgroup, freq_hz, channel: lcn },
            Some(talkgroup),
            None,
        )],
        FollowerAction::Source { source } => vec![event(
            CallBoundaryKind::TdulcComplete { source: Some(source) },
            None,
            Some(Lane::One),
        )],
        FollowerAction::Voice { talkgroup, .. } => {
            if now_ms.saturating_sub(link.last_voice_refresh_ms) < VOICE_REFRESH_MS {
                return Vec::new();
            }
            link.last_voice_refresh_ms = now_ms;
            vec![
                // Voice on the channel: a pending end close is cancelled.
                event(CallBoundaryKind::TrafficNidObserved { voice: true }, None, Some(Lane::One)),
                // ... and the call is live (the lifecycle's hang timer).
                event(CallBoundaryKind::CcGrantUpdate { tg: talkgroup, freq_hz: None, channel: 0 }, Some(talkgroup), None),
            ]
        }
        FollowerAction::End { reason } if link.call_id != 0 && reason != "timeout" => vec![event(
            CallBoundaryKind::VoiceEnd {
                call_id: link.call_id,
                air_ms: now_ms,
                lc: if reason == "clear" { "network_teardown" } else { "call_termination" },
            },
            None,
            Some(Lane::One),
        )],
        _ => Vec::new(),
    }
}

/// Carries out the follower's actions: retunes traffic chain 1 (NCO only:
/// the P25 chain on it stays off) and tells the call lifecycle about the
/// calls (`boundary_events`); voice frames count as the call's voice time.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub fn spawn_dmr_executor(
    rt: std::sync::Arc<DmrRuntime>,
    ip_core: std::sync::Arc<tokio::sync::Mutex<crate::hardware::fpga::IpCore>>,
    rx_lo: std::sync::Arc<AtomicI64>,
    sample_rate_hz: std::sync::Arc<std::sync::atomic::AtomicU32>,
    lo_shift_hz: std::sync::Arc<AtomicI64>,
    call_boundary_tx: crate::audio::CallBoundaryTx,
    call_tracker_tx: crate::app::grant_follower::CallTrackerEventTx,
    call_counts: std::sync::Arc<crate::app::call_counters::CallCounterBook>,
    voice_tx: std::sync::mpsc::SyncSender<crate::app::dmr_voice::DmrVoiceBatch>,
) {
    use crate::app::grant_follower::CallTrackerEventKind;
    use crate::hardware::traffic_lane::Lane;
    use tokio::sync::broadcast::error::RecvError;
    let (tx, mut actions) = tokio::sync::mpsc::unbounded_channel();
    if rt.actions_tx.set(tx).is_err() {
        tracing::error!("dmr executor already running");
        return;
    }
    let mut tracker = call_tracker_tx.subscribe();
    tokio::spawn(async move {
        let mut link = LifecycleLink::default();
        loop {
            let action = tokio::select! {
                a = actions.recv() => match a {
                    Some(a) => a,
                    None => break,
                },
                e = tracker.recv() => {
                    match e {
                        // The lifecycle's call for our followed grant.
                        Ok(ev) if ev.lane == Some(Lane::One) => match ev.kind {
                            CallTrackerEventKind::CallOpen { not_followed: None, .. } => link.call_id = ev.call_id,
                            CallTrackerEventKind::CallClose { .. } if ev.call_id == link.call_id => link.call_id = 0,
                            _ => {}
                        },
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => break,
                    }
                    continue;
                }
            };
            if rt.follow.load(Ordering::Relaxed) {
                let now = crate::app::now_unix_ms();
                if let FollowerAction::Voice { talkgroup, source, frames } = action {
                    // Three AMBE+2 frames: 60 ms of voice.
                    call_counts.update(link.call_id, |c| {
                        c.imbe_extracted += 3;
                        c.vocoder_pcm_samples += 480;
                    });
                    let batch = crate::app::dmr_voice::DmrVoiceBatch {
                        frames,
                        talkgroup,
                        source,
                        call_id: link.call_id,
                        captured_at_ms: now,
                    };
                    if voice_tx.try_send(batch).is_err() {
                        rt.voice_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                for event in boundary_events(&action, &mut link, now) {
                    let _ = call_boundary_tx.send(event);
                }
            }
            if let FollowerAction::Tune { freq_hz } = action {
                let offset = freq_hz as f64 - rx_lo.load(Ordering::Relaxed) as f64
                    + lo_shift_hz.load(Ordering::Relaxed) as f64;
                let sr = sample_rate_hz.load(Ordering::Relaxed) as f64;
                let result = {
                    let core = ip_core.lock().await;
                    match core.lane(Lane::One) {
                        Some(lane) => lane.set_ddc_frequency(offset, sr),
                        None => Err(anyhow::anyhow!("no traffic chain 1")),
                    }
                };
                match result {
                    Ok(()) => {
                        rt.tunes.fetch_add(1, Ordering::Relaxed);
                        rt.traffic_epoch.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        rt.tune_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("dmr: traffic chain 1 retune to {freq_hz} Hz failed: {e}");
                        if let Ok(mut f) = rt.follower.lock() {
                            f.chain_moved();
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dmr::fec::cach::Cach;
    use crate::protocol::dmr::framer::DmrBurst;
    use crate::protocol::dmr::sync::DmrSyncPattern;

    fn burst(pattern: DmrSyncPattern, timeslot: u8, cach_valid: bool) -> FramerEvent {
        FramerEvent::Burst(DmrBurst {
            pattern,
            timeslot,
            bits: [0; 288],
            cach: Cach { valid: cach_valid, busy: false, timeslot, lcss: 0, payload: [0; 17] },
            dibit_index: 0,
        })
    }

    #[test]
    fn counts_bursts_cach_and_voice() {
        let rt = DmrRuntime::default();
        rt.count(&burst(DmrSyncPattern::BaseStationData, 1, true));
        rt.count(&burst(DmrSyncPattern::BaseStationVoice, 2, true));
        rt.count(&burst(DmrSyncPattern::BsVoiceFrameB, 2, false));
        rt.count(&FramerEvent::SyncLoss { timeslot: 0, bits: 288 });
        let s = rt.snapshot();
        assert_eq!(s["bursts"]["ts1"], 1);
        assert_eq!(s["bursts"]["ts2"], 2);
        assert_eq!(s["voice_bursts"], 2);
        assert_eq!(s["cach"]["ok"], 2);
        assert_eq!(s["cach"]["bad"], 1);
        assert_eq!(s["sync_loss_bits"], 288);
        rt.clear();
        assert_eq!(rt.snapshot()["bursts"]["ts2"], 0);
    }

    #[test]
    fn a_dmr_site_switches_the_receiver_and_follower_on() {
        let rt = DmrRuntime::default();
        let site: crate::services::sites::Site =
            serde_json::from_str(r#"{"name":"d","label":"d","protocol":"dmr","preset_default":"8M",
                "control_freq_hz":454368750,"lcn_map":{"5":454368750,"9":452000000},
                "nac":null,"wacn":null,"system_id":null,"rfss_id":null,"site_id":null,"lra":null}"#)
                .unwrap();
        rt.apply_site(&site);
        assert!(rt.enabled.load(Ordering::Relaxed) && rt.follow.load(Ordering::Relaxed));
        assert_eq!(rt.lcn_map().get(&9), Some(&452_000_000));
        let epoch = rt.config_epoch.load(Ordering::Relaxed);
        let p25: crate::services::sites::Site =
            serde_json::from_str(r#"{"name":"p","label":"p","preset_default":"8M","control_freq_hz":860962500,
                "nac":null,"wacn":null,"system_id":null,"rfss_id":null,"site_id":null,"lra":null}"#)
                .unwrap();
        rt.apply_site(&p25);
        assert!(!rt.enabled.load(Ordering::Relaxed) && !rt.follow.load(Ordering::Relaxed));
        assert!(rt.config_epoch.load(Ordering::Relaxed) > epoch);
        // Back to the built-in map.
        assert_eq!(rt.lcn_map().get(&6), Some(&451_087_500));
    }

    #[test]
    fn follower_actions_become_call_boundaries() {
        use crate::app::dmr_follower::DmrGrant;
        use crate::audio::CallBoundaryKind;
        use crate::hardware::traffic_lane::Lane;
        let grant = DmrGrant {
            talkgroup: 87_925,
            source: Some(81_921),
            private: false,
            lcn: 5,
            timeslot: 2,
            freq_hz: Some(454_368_750),
        };
        let mut link = LifecycleLink::default();
        let e = boundary_events(&FollowerAction::Grant { grant, not_followed: None }, &mut link, 0);
        assert!(matches!(
            e[0].kind,
            CallBoundaryKind::CcGrantArrival { tg: 87_925, channel: 5, not_followed: None, .. }
        ));
        assert_eq!(e[0].lane, Some(Lane::One));
        let e = boundary_events(&FollowerAction::Grant { grant, not_followed: Some("busy") }, &mut link, 0);
        assert_eq!(e[0].lane, None);
        // Voice refreshes at most every VOICE_REFRESH_MS.
        let voice = FollowerAction::Voice { talkgroup: 87_925, source: None, frames: [[0; 9]; 3] };
        assert_eq!(boundary_events(&voice, &mut link, 1000).len(), 2);
        assert!(boundary_events(&voice, &mut link, 1060).is_empty());
        // No call known yet: no end.
        assert!(boundary_events(&FollowerAction::End { reason: "clear" }, &mut link, 2000).is_empty());
        link.call_id = 42;
        let e = boundary_events(&FollowerAction::End { reason: "clear" }, &mut link, 2000);
        assert!(matches!(e[0].kind, CallBoundaryKind::VoiceEnd { call_id: 42, lc: "network_teardown", .. }));
        // A timeout is the lifecycle's own (hang timer).
        assert!(boundary_events(&FollowerAction::End { reason: "timeout" }, &mut link, 2000).is_empty());
    }

    #[test]
    fn carrier_offset_from_balance() {
        let rt = DmrRuntime::default();
        // 0.49 rad/symbol (unit A at 454 MHz before 074c) is about -374 Hz.
        rt.balance_mrad.store(490, Ordering::Relaxed);
        assert_eq!(rt.snapshot()["demod"]["carrier_offset_hz"], -374.0);
    }
}
