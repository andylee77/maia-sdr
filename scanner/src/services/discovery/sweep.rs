//! The scan: it takes the radio from the live site, steps and probes, and gives it back.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::carriers::{find_carriers, power_db, Carrier};
use super::probe::{DmrProbe, Heard, P25Probe, Probes};
use super::{existing_site, plan_steps, ScanRequest, ScanState, SWEEP_PRESET, SWEEP_RATE_HZ};
use crate::hardware::presets::find_preset;
use crate::radio::lease::{Lease, LeaseGuard, RadioLease};
use crate::radio::plan::usable_half_hz;
use crate::radio::streams::StreamSource;
use crate::radio::tuner::{RadioHw, Tuner, TuningPlan};
use crate::services::config::systems::SystemsConfig;
use crate::services::events::EventLog;
use crate::trunking::site::LiveSite;
use crate::util::time::unix_ms;

/// Above the frame's floor by this much, in this share of the frames: a continuous carrier.
const CARRIER_DB: f32 = 12.0;
const CARRIER_PERSIST: f32 = 0.8;
/// After an LO move, before the spectrometer's frames count.
const LO_SETTLE: Duration = Duration::from_millis(200);
/// Control messages that make a carrier a control channel.
const MIN_MESSAGES: u64 = 3;
/// Listening on after the identity, for the band plan and neighbours.
const AFTER_IDENTITY: Duration = Duration::from_secs(3);
/// Probed frequencies this close are one.
const SAME_HZ: u64 = 3_000;
/// How often a probe looks at what it heard (and at a cancel).
const POLL: Duration = Duration::from_millis(250);
/// Blocks of the control channel's IQ queued for the probes (about 5 s).
const QUEUE: usize = 256;

#[derive(Default)]
pub struct Discovery {
    state: Mutex<ScanState>,
}

impl Discovery {
    pub fn state(&self) -> ScanState {
        self.lock().clone()
    }

    pub fn cancel(&self) {
        let mut d = self.lock();
        if d.running() {
            d.cancel = true;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ScanState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, f: impl FnOnce(&mut ScanState)) {
        f(&mut self.lock());
    }

    fn cancelled(&self) -> bool {
        self.lock().cancel
    }

    /// Start a scan; refused while the radio is taken (a switch or another scan).
    pub fn start<H: RadioHw + StreamSource + Send + Sync + 'static>(
        self: &Arc<Self>,
        req: ScanRequest,
        lease: &RadioLease,
        live: Arc<LiveSite<H>>,
        tuner: Arc<Tuner<H>>,
        systems: SystemsConfig,
        log: Arc<EventLog>,
    ) -> Result<u64> {
        let guard = lease.take(Lease::Scan).context("the radio is busy (a site switch or another scan)")?;
        let id = {
            let mut d = self.lock();
            let id = d.id + 1;
            *d = ScanState { id, state: "sweeping", started_unix_ms: unix_ms(), bands: req.bands(), ..Default::default() };
            id
        };
        log.system("scan", format!("scan {id} started over {} bands", req.bands().len()));
        let me = self.clone();
        tokio::spawn(async move { me.run(id, req, guard, live, tuner, systems, log).await });
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run<H: RadioHw + StreamSource + Send + Sync + 'static>(
        self: Arc<Self>,
        id: u64,
        req: ScanRequest,
        guard: LeaseGuard,
        live: Arc<LiveSite<H>>,
        tuner: Arc<Tuner<H>>,
        systems: SystemsConfig,
        log: Arc<EventLog>,
    ) {
        let back_to = live.pause_for_scan().await;
        let result = self.sweep(&req, &tuner, &systems).await;
        self.update(|d| {
            d.state = "restoring";
            d.probing_hz = None;
            d.band = None;
            d.lo_hz = None;
        });
        drop(guard);
        live.resume_after_scan(back_to).await;
        let summary = {
            let mut d = self.lock();
            d.finished_unix_ms = unix_ms();
            match result {
                Ok(()) if d.cancel => d.state = "cancelled",
                Ok(()) => d.state = "done",
                Err(e) => {
                    d.state = "error";
                    d.error = Some(format!("{e:#}"));
                }
            }
            format!(
                "scan {id} {}: {} control channels, {} traffic channels, {} other carriers, {} probed",
                d.state,
                d.sites.len(),
                d.traffic.len(),
                d.other.len(),
                d.probed
            )
        };
        log.system("scan", summary);
    }

    async fn sweep<H: RadioHw + StreamSource + Send + Sync + 'static>(&self, req: &ScanRequest, tuner: &Tuner<H>, systems: &SystemsConfig) -> Result<()> {
        let preset = find_preset(SWEEP_PRESET).context("the sweep preset")?;
        let uh = usable_half_hz(SWEEP_RATE_HZ) as f64;
        let bands = req.bands();
        let steps = plan_steps(&bands, uh);
        self.update(|d| d.steps = steps.len());
        let (tx, rx) = sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let mut probes = Probes::start(rx, vec![Box::new(P25Probe::default()), Box::new(DmrProbe::default())])?;
        tuner.hw().control_streams(tx, stop.clone(), Arc::default());
        let result = async {
            let mut probed: Vec<u64> = Vec::new();
            let near = |list: &[u64], f: u64| list.iter().any(|&p| p.abs_diff(f) <= SAME_HZ);
            let in_bands = |f: u64| bands.iter().any(|&(a, b)| f >= a && f <= b);
            let mut budget = req.max_candidates;
            for (k, &lo) in steps.iter().enumerate() {
                if self.cancelled() {
                    return Ok(());
                }
                self.update(|d| {
                    d.step = k + 1;
                    d.state = "sweeping";
                    d.band = bands.iter().copied().find(|&(a, b)| lo >= a && lo <= b);
                    d.lo_hz = Some(lo);
                    d.probing_hz = None;
                });
                tuner.apply(TuningPlan { preset, lo_hz: lo, control_hz: lo }).await?;
                tokio::time::sleep(LO_SETTLE).await;
                let frames = grab(tuner.hw(), req.frames).await;
                let mut carriers = find_carriers(&frames, lo as f64, SWEEP_RATE_HZ as f64, uh, CARRIER_DB, CARRIER_PERSIST);
                carriers.sort_by(|a, b| b.level_db.total_cmp(&a.level_db));
                carriers.retain(|c| !near(&probed, c.freq_hz) && in_bands(c.freq_hz));
                carriers.truncate(budget);
                budget -= carriers.len();
                self.update(|d| {
                    d.carriers += carriers.len();
                    d.to_probe += carriers.len();
                    d.state = "probing";
                });
                for c in carriers {
                    if self.cancelled() {
                        return Ok(());
                    }
                    probed.push(c.freq_hz);
                    self.probe(&mut probes, tuner, req, systems, c, false).await?;
                }
            }
            // Neighbours' control channels in the bands that no spectrum turned up (weak, or on
            // the DC spur).
            let mut neighbours: Vec<u64> =
                self.lock().sites.iter().flat_map(|s| s.neighbours.iter().filter_map(|n| n.freq_hz)).filter(|&f| in_bands(f)).collect();
            neighbours.sort_unstable();
            neighbours.dedup();
            neighbours.retain(|&f| !near(&probed, f));
            self.update(|d| {
                d.to_probe += neighbours.len();
                d.band = None;
            });
            for f in neighbours {
                if self.cancelled() {
                    return Ok(());
                }
                if tuner.tuning().lo_hz.abs_diff(f) as f64 > uh - 100_000.0 {
                    // A window around it, the channel off the DC spur.
                    tuner.apply(TuningPlan { preset, lo_hz: f + 1_000_000, control_hz: f }).await?;
                    tokio::time::sleep(LO_SETTLE).await;
                }
                probed.push(f);
                self.probe(&mut probes, tuner, req, systems, Carrier { freq_hz: f, level_db: 0.0, persistence: 0.0 }, true).await?;
            }
            anyhow::Ok(())
        }
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::task::spawn_blocking(move || probes.stop()).await?;
        result
    }

    /// Listen to one carrier with every protocol's decoder.
    async fn probe<H: RadioHw + StreamSource + Send + Sync + 'static>(
        &self,
        probes: &mut Probes,
        tuner: &Tuner<H>,
        req: &ScanRequest,
        systems: &SystemsConfig,
        c: Carrier,
        via_neighbour: bool,
    ) -> Result<()> {
        self.update(|d| d.probing_hz = Some(c.freq_hz));
        tuner.set_control(c.freq_hz).await?;
        // The last carrier's counts go now; its queued blocks are dropped when this one's first
        // arrives (otherwise its identity lands on this frequency).
        probes.reset();
        let t0 = Instant::now();
        let probed_by = t0 + Duration::from_millis(req.probe_ms);
        while Instant::now() < probed_by && !self.cancelled() {
            tokio::time::sleep(POLL).await;
        }
        if probes.messages(MIN_MESSAGES) {
            // A control channel: listen until its identity (and a little more), or give up.
            let deadline = t0 + Duration::from_millis(req.identity_ms.max(req.probe_ms));
            let mut identified_at: Option<Instant> = None;
            loop {
                if identified_at.is_none() && probes.identified() {
                    identified_at = Some(Instant::now());
                }
                let now = Instant::now();
                if now >= deadline || identified_at.is_some_and(|t| now.duration_since(t) >= AFTER_IDENTITY) || self.cancelled() {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
        }
        let heard = probes.heard(c.freq_hz, c.level_db);
        // A steady carrier of something else, or a call that ended since the spectrum pass.
        let steady = matches!(heard, Heard::Nothing) && !via_neighbour && still_there(tuner, c.freq_hz).await;
        self.update(|d| {
            d.probed += 1;
            match heard {
                Heard::Control(mut site) => {
                    site.existing_site = existing_site(&site, systems);
                    site.via_neighbour = via_neighbour;
                    d.found(*site);
                }
                Heard::Traffic => d.traffic.push(c),
                Heard::Nothing if steady => d.other.push(c),
                Heard::Nothing => {}
            }
        });
        Ok(())
    }
}

/// `n` spectrometer frames as dB per bin, taken after the first two (the LO settling).
async fn grab<H: RadioHw>(hw: &H, n: usize) -> Vec<Vec<f32>> {
    let mut out = Vec::new();
    let mut discard = 2;
    let deadline = Instant::now() + Duration::from_secs(4);
    while out.len() < n && Instant::now() < deadline {
        match hw.spectrum().await {
            Some(_) if discard > 0 => discard -= 1,
            Some(bytes) => {
                let db = power_db(&bytes);
                if !db.is_empty() {
                    out.push(db);
                }
            }
            None => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    out
}

/// Is a carrier still at `freq` (above the floor in 2 of 3 new frames)?
async fn still_there<H: RadioHw>(tuner: &Tuner<H>, freq: u64) -> bool {
    let frames = grab(tuner.hw(), 3).await;
    let lo = tuner.tuning().lo_hz as f64;
    let found = find_carriers(&frames, lo, SWEEP_RATE_HZ as f64, f64::MAX, CARRIER_DB, 0.66);
    found.iter().any(|c| c.freq_hz.abs_diff(freq) <= 8_000)
}
