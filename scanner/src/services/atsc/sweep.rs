//! The channel finder's runs. In ATSC mode it holds the radio (the mode's lease), so it tunes as
//! it likes; going back to scanner mode stops a run first and takes the radio from it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::OwnedMutexGuard;

use super::{usable_half, AtscRequest, AtscScan, FoundChannel};
use crate::hardware::ad9361::GainMode;
use crate::hardware::presets::find_preset;
use crate::protocol::atsc::spectrum::{measure, Kind};
use crate::protocol::atsc::windows;
use crate::radio::lease::LeaseGuard;
use crate::radio::tuner::{RadioHw, Tuner, TuningPlan};
use crate::services::discovery::sweep::{grab, LO_SETTLE};
use crate::services::discovery::{SWEEP_PRESET, SWEEP_RATE_HZ};
use crate::services::events::EventLog;
use crate::util::time::unix_ms;

/// The gain at which the spectrum's dB scale reads about dBm (`discovery::carriers`).
const SCALE_GAIN_DB: f64 = 60.0;

/// ATSC mode's hold on the radio, and the site scanner mode brings back.
struct Held {
    _lease: LeaseGuard,
    back_to: Option<String>,
}

#[derive(Default)]
pub struct Atsc {
    state: Mutex<AtscScan>,
    /// The radio while ATSC mode lasts; a run holds this lock.
    radio: Arc<tokio::sync::Mutex<Option<Held>>>,
    /// Scanner mode is coming back: no new run, and the running one stops.
    leaving: AtomicBool,
}

impl Atsc {
    pub fn state(&self) -> AtscScan {
        self.lock().clone()
    }

    pub fn cancel(&self) {
        let mut d = self.lock();
        if d.running() {
            d.cancel = true;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AtscScan> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, f: impl FnOnce(&mut AtscScan)) {
        f(&mut self.lock());
    }

    fn stopping(&self) -> bool {
        self.lock().cancel || self.leaving.load(Ordering::SeqCst)
    }

    /// ATSC mode begins: the radio is this service's until `leave`.
    pub async fn enter(&self, lease: LeaseGuard, back_to: Option<String>) {
        *self.radio.lock().await = Some(Held { _lease: lease, back_to });
        self.leaving.store(false, Ordering::SeqCst);
    }

    /// ATSC mode ends: a run stops, the radio is handed back. Returns the site to bring back.
    pub async fn leave(&self) -> Option<String> {
        self.leaving.store(true, Ordering::SeqCst);
        self.cancel();
        self.radio.lock().await.take().and_then(|h| h.back_to)
    }

    /// Start a run; refused outside ATSC mode or while one runs.
    pub fn start<H: RadioHw + Send + Sync + 'static>(
        self: &Arc<Self>,
        req: AtscRequest,
        tuner: Arc<Tuner<H>>,
        log: Arc<EventLog>,
    ) -> Result<u64> {
        let radio = self.radio.clone().try_lock_owned().map_err(|_| anyhow!("a TV scan is already running"))?;
        if radio.is_none() || self.leaving.load(Ordering::SeqCst) {
            bail!("the unit is not in ATSC mode");
        }
        let channels = req.channels();
        let id = {
            let mut d = self.lock();
            let id = d.id + 1;
            *d = AtscScan { id, state: "sweeping", started_unix_ms: unix_ms(), channels: channels.clone(), gain_db: req.gain_db, ..Default::default() };
            id
        };
        let gain = req.gain_db.map_or("the AGC".to_string(), |g| format!("{g} dB of gain"));
        log.system("atsc", format!("TV scan {id} started over {} channels at {gain}", channels.len()));
        let me = self.clone();
        tokio::spawn(async move { me.run(id, req, radio, tuner, log).await });
        Ok(id)
    }

    async fn run<H: RadioHw>(self: Arc<Self>, id: u64, req: AtscRequest, radio: OwnedMutexGuard<Option<Held>>, tuner: Arc<Tuner<H>>, log: Arc<EventLog>) {
        let result = self.sweep(&req, &tuner).await;
        drop(radio);
        let summary = {
            let mut d = self.lock();
            d.finished_unix_ms = unix_ms();
            d.lo_hz = None;
            match result {
                Ok(()) if d.cancel || self.leaving.load(Ordering::SeqCst) => d.state = "cancelled",
                Ok(()) => d.state = "done",
                Err(e) => {
                    d.state = "error";
                    d.error = Some(format!("{e:#}"));
                }
            }
            format!(
                "TV scan {id} {} on {} channels: {} 8-VSB, {} without an 8-VSB pilot (ATSC 3.0 or other), {} vacant",
                d.state,
                d.found.len(),
                d.count(Kind::Vsb),
                d.count(Kind::NoPilot),
                d.count(Kind::Vacant)
            )
        };
        log.system("atsc", summary);
    }

    async fn sweep<H: RadioHw>(&self, req: &AtscRequest, tuner: &Tuner<H>) -> Result<()> {
        let preset = find_preset(SWEEP_PRESET).context("the sweep preset")?;
        match req.gain_db {
            Some(db) => tuner.set_gain(GainMode::Manual, Some(f64::from(db))).await?,
            None => tuner.set_gain(GainMode::SlowAttack, None).await?,
        };
        let windows = windows(&req.channels(), usable_half());
        self.update(|d| d.steps = windows.len());
        for (k, w) in windows.iter().enumerate() {
            if self.stopping() {
                return Ok(());
            }
            self.update(|d| {
                d.step = k + 1;
                d.lo_hz = Some(w.lo_hz);
            });
            tuner.apply(TuningPlan { preset, lo_hz: w.lo_hz, control_hz: w.lo_hz }).await?;
            tokio::time::sleep(LO_SETTLE).await;
            let before = tuner.readback().await;
            let frames = grab(tuner.hw(), req.frames).await;
            let after = tuner.readback().await;
            if frames.is_empty() {
                bail!("no spectrometer frames at {:.1} MHz", w.lo_hz as f64 / 1e6);
            }
            let power = average(&frames);
            let clipped = matches!((before.adc_clips, after.adc_clips), (Some(a), Some(b)) if b != a);
            let gain = after.gain_db;
            let found: Vec<FoundChannel> = w
                .channels
                .iter()
                .filter_map(|c| {
                    let m = measure(&power, w.lo_hz as f64, f64::from(SWEEP_RATE_HZ), c)?;
                    Some(FoundChannel {
                        number: c.number,
                        band: c.band,
                        center_hz: c.center_hz(),
                        kind: m.kind,
                        pilot_hz: m.pilot_hz.map(|f| f.round() as u64),
                        pilot_offset_hz: m.pilot_hz.map(|f| (f - c.pilot_hz()).round() as i64),
                        pilot_db: m.pilot_db,
                        level_db: m.level_db,
                        power_dbm: gain.map(|g| m.power_db - (g - SCALE_GAIN_DB) as f32),
                        gain_db: gain,
                        clipped,
                    })
                })
                .collect();
            self.update(|d| d.found.extend(found));
        }
        Ok(())
    }
}

/// Frames of dB per bin, averaged in power.
fn average(frames: &[Vec<f32>]) -> Vec<f64> {
    let n = frames.iter().map(Vec::len).min().unwrap_or(0);
    let mut out = vec![0.0; n];
    for f in frames {
        for (o, &db) in out.iter_mut().zip(f) {
            *o += 10f64.powf(f64::from(db) / 10.0);
        }
    }
    out.iter_mut().for_each(|o| *o /= frames.len() as f64);
    out
}
