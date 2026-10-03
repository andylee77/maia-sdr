//! The crystal: its calibration, and the tracker that follows its drift.
//!
//! The AD9361's reference is a few tenths of a ppm off and drifts with temperature. The tuner
//! corrects it with an LO shift (`tuner::lo_shift_hz`); this service measures the shift on the
//! live control channel.
//!
//! - **Calibration**, once the live site is decoded after start and on request: the
//!   spectrometer's peak within ±10 kHz of the control channel brings the error into the carrier
//!   loop's capture range, then the loop's mean residual gives the rest.
//! - **Tracker:** each second, while a burst is on the channel, an estimate of the right
//!   correction; each minute the trimmed mean of the last five minutes becomes the correction,
//!   when it stays within the anchor of this run's calibration.
//!
//! Both measurements are the control decoder's own carrier offset, signal minus NCO: the P25
//! LSM's carrier loop (while a signal is on the channel) and the DMR equaliser. The shift that
//! cancels an offset is `shift + offset` (the other sign makes the correction run away). A C4FM
//! site is calibrated from the spectrum alone and not tracked. Estimates are kept in ppm, so
//! they survive an LO move.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::time::Instant;

use crate::protocol::events::SiteIdentity;
use crate::radio::lease::RadioLease;
use crate::radio::tuner::{RadioHw, Tuner, Tuning};
use crate::services::config::{self, state, Config, Paths};
use crate::services::discovery::carriers::power_db;
use crate::services::events::EventLog;
use crate::trunking::receivers::ControlStatus;
use crate::util::time::unix_ms;

/// The spectrometer's search around the control channel: ±5 ppm at 2 GHz.
const SEARCH_HZ: f64 = 10_000.0;
const SPECTRUM_FRAMES: usize = 8;
/// After the correction moves, before the loop's residual means anything again.
const SETTLE: Duration = Duration::from_secs(3);
const RESIDUAL_READS: usize = 30;
const RESIDUAL_EVERY: Duration = Duration::from_millis(100);
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// Five minutes of samples.
const WINDOW: usize = 300;
const MIN_SAMPLES: usize = 60;
/// Share dropped from each end before the mean (bursts, fades, retune glitches).
const TRIM: f64 = 0.10;
/// Samples between updates (a minute).
const UPDATE_EVERY: u32 = 60;
/// The tracker never goes beyond this, whatever the estimates say.
const ENVELOPE_PPM: f64 = 1.0;
/// Smaller moves are not worth an LO write.
const MIN_STEP_HZ: f64 = 2.0;
/// Smaller moves are not worth a flash write.
const SAVE_STEP_HZ: f64 = 5.0;
/// From the first decoded messages to the first calibration (the loop is still converging).
const FIRST_CALIBRATION_AFTER: Duration = Duration::from_secs(15);
/// The site counts as decoded while its last message is this recent.
const FRESH_MS: u64 = 2_000;

/// The correction (ppm) that commands `shift_hz` at `lo_hz`: the inverse of `lo_shift_hz`.
pub fn ppm_of(shift_hz: f64, lo_hz: u64) -> f64 {
    -shift_hz / (lo_hz as f64 * 1e-6)
}

fn hz_of(ppm: f64, lo_hz: u64) -> f64 {
    -ppm * lo_hz as f64 * 1e-6
}

/// The mean of `samples` without the top and bottom `trim` share.
pub fn trimmed_mean(samples: &[f64], trim: f64) -> Option<f64> {
    let mut s = samples.to_vec();
    s.sort_by(f64::total_cmp);
    let cut = (s.len() as f64 * trim).floor() as usize;
    let kept = s.get(cut..s.len() - cut).filter(|k| !k.is_empty())?;
    Some(kept.iter().sum::<f64>() / kept.len() as f64)
}

/// The strongest bin within `window_hz` of `offset_hz` in a DC-centred frame spanning
/// `sample_rate`, interpolated between its neighbours: its offset from the LO (Hz) and level.
pub fn peak_near(db: &[f32], sample_rate: f64, offset_hz: f64, window_hz: f64) -> Option<(f64, f32)> {
    let n = db.len();
    if n < 3 {
        return None;
    }
    let bin_hz = sample_rate / n as f64;
    let centre = (n / 2) as f64;
    let at = centre + offset_hz / bin_hz;
    let reach = (window_hz / bin_hz).ceil();
    let lo = (at - reach).max(1.0) as usize;
    let hi = ((at + reach).max(0.0) as usize).min(n - 2);
    if hi <= lo {
        return None;
    }
    let best = (lo..=hi).max_by(|&a, &b| db[a].total_cmp(&db[b]))?;
    let (y1, y2, y3) = (f64::from(db[best - 1]), f64::from(db[best]), f64::from(db[best + 1]));
    let denom = y1 - 2.0 * y2 + y3;
    let frac = if denom.abs() < 1e-6 { 0.0 } else { 0.5 * (y1 - y3) / denom };
    Some(((best as f64 + frac - centre) * bin_hz, db[best]))
}

/// Where the correction is measured at the live site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The P25 control decoder's LSM carrier loop.
    P25Loop,
    /// The DMR equaliser's carrier offset.
    DmrEqualiser,
    /// The spectrum only (a C4FM site).
    Spectrum,
}

impl Source {
    /// The live site's measurement, while it is decoded.
    pub fn of(control: &ControlStatus) -> Option<Source> {
        let fresh = control.last_message_age_ms.is_some_and(|a| a <= FRESH_MS);
        if !control.running || !fresh {
            return None;
        }
        match control.identity {
            Some(SiteIdentity::P25(_)) if control.modulation == Some("lsm") => Some(Source::P25Loop),
            Some(SiteIdentity::P25(_)) => Some(Source::Spectrum),
            // The equaliser's offset needs no identity: a Connect Plus site never sends the Tier
            // III one.
            _ if control.carrier_offset_hz.is_some() => Some(Source::DmrEqualiser),
            _ => None,
        }
    }
}

/// The tracker's samples: estimates (ppm) of the right correction.
#[derive(Debug, Default)]
pub struct Tracker {
    estimates: VecDeque<f64>,
    /// No samples before this: the loop is reconverging after a move.
    quiet_until: Option<Instant>,
    /// (LO, control channel) the samples were taken on; another site's are not comparable.
    tuned: Option<(u64, u64)>,
}

impl Tracker {
    pub fn sample(&mut self, now: Instant, tuned: (u64, u64), estimate_ppm: f64) {
        if self.tuned != Some(tuned) {
            self.estimates.clear();
            self.tuned = Some(tuned);
        }
        if self.quiet_until.is_some_and(|t| now < t) {
            return;
        }
        if self.estimates.len() == WINDOW {
            self.estimates.pop_front();
        }
        self.estimates.push_back(estimate_ppm);
    }

    /// The correction moved: earlier samples are stale, and so are the next few seconds'.
    pub fn moved(&mut self, now: Instant) {
        self.estimates.clear();
        self.quiet_until = Some(now + SETTLE);
    }

    pub fn len(&self) -> usize {
        self.estimates.len()
    }

    /// The trimmed mean, once there are enough samples.
    pub fn estimate(&self) -> Option<f64> {
        if self.estimates.len() < MIN_SAMPLES {
            return None;
        }
        trimmed_mean(&self.estimates.iter().copied().collect::<Vec<_>>(), TRIM)
    }
}

/// What the tracker does with its estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    Apply(f64),
    Hold(&'static str),
}

/// Whether an `estimate` (ppm) replaces the `current` correction at `lo_hz`: clamped to the
/// envelope, a step worth an LO write, tracking on, and within `anchor_hz` of this run's
/// calibration (`anchor_hz` 0: anywhere).
pub fn decide(estimate: f64, current: f64, anchor: Option<f64>, anchor_hz: u32, lo_hz: u64, tracking: bool) -> Decision {
    let target = estimate.clamp(-ENVELOPE_PPM, ENVELOPE_PPM);
    if (hz_of(target, lo_hz) - hz_of(current, lo_hz)).abs() < MIN_STEP_HZ {
        return Decision::Hold("within 2 Hz");
    }
    if !tracking {
        return Decision::Hold("tracking off");
    }
    let Some(anchor) = anchor else { return Decision::Hold("not calibrated since start") };
    if anchor_hz > 0 && (hz_of(target, lo_hz) - hz_of(anchor, lo_hz)).abs() > f64::from(anchor_hz) {
        return Decision::Hold("outside the anchor");
    }
    Decision::Apply(target)
}

/// One calibration's measurements.
#[derive(Debug, Clone, Serialize)]
pub struct Calibration {
    pub at_unix_ms: u64,
    pub lo_hz: u64,
    pub control_hz: u64,
    pub source: Source,
    /// The control channel's peak against where the correction put it.
    pub spectrum_offset_hz: f64,
    pub spectrum_db: f32,
    /// The loop's or equaliser's residual after the spectrum step.
    pub residual_hz: Option<f64>,
    pub residual_samples: usize,
    pub ppm_before: f64,
    pub ppm: f64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CrystalStatus {
    /// The correction applied now.
    pub ppm: f64,
    pub lo_shift_hz: i64,
    pub tracking: bool,
    pub anchor_hz: u32,
    /// This run's calibration, which the tracker stays near.
    pub anchor_ppm: Option<f64>,
    pub source: Option<Source>,
    /// The tracker's estimate and its samples.
    pub estimate_ppm: Option<f64>,
    pub samples: usize,
    /// What the tracker last did with its estimate.
    pub last_decision: Option<String>,
    pub calibrating: bool,
    pub calibration: Option<Calibration>,
}

#[derive(Default)]
struct Inner {
    tracker: Tracker,
    anchor: Option<f64>,
    source: Option<Source>,
    last_decision: Option<String>,
    calibration: Option<Calibration>,
    /// The correction last written to flash.
    saved_ppm: Option<f64>,
}

/// What the service reads and moves.
pub struct Deps<H> {
    pub tuner: Arc<Tuner<H>>,
    /// The live site's control channel status.
    pub control: Arc<dyn Fn() -> ControlStatus + Send + Sync>,
    pub lease: RadioLease,
    pub config: Arc<tokio::sync::Mutex<Config>>,
    pub paths: Paths,
    pub log: Arc<EventLog>,
}

pub struct Crystal<H> {
    deps: Deps<H>,
    inner: Mutex<Inner>,
    /// One calibration at a time.
    calibrating: tokio::sync::Mutex<()>,
}

impl<H: RadioHw + 'static> Crystal<H> {
    pub fn new(deps: Deps<H>) -> Arc<Self> {
        Arc::new(Crystal { deps, inner: Mutex::default(), calibrating: tokio::sync::Mutex::new(()) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub async fn status(&self) -> CrystalStatus {
        let t = self.deps.tuner.tuning();
        let settings = self.deps.config.lock().await.radio.value.crystal.clone();
        let i = self.lock();
        CrystalStatus {
            ppm: t.crystal_ppm,
            lo_shift_hz: t.lo_shift_hz,
            tracking: settings.tracking,
            anchor_hz: settings.anchor_hz,
            anchor_ppm: i.anchor,
            source: i.source,
            estimate_ppm: i.tracker.estimate(),
            samples: i.tracker.len(),
            last_decision: i.last_decision.clone(),
            calibrating: self.calibrating.try_lock().is_err(),
            calibration: i.calibration.clone(),
        }
    }

    /// Calibrate once the live site is decoded, then track.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move { me.run().await });
    }

    async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(SAMPLE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut decoded_since: Option<Instant> = None;
        let mut calibrated = false;
        let mut ticks: u32 = 0;
        loop {
            tick.tick().await;
            let source = Source::of(&(self.deps.control)());
            self.lock().source = source;
            if !calibrated {
                decoded_since = source.and(decoded_since.or(Some(Instant::now())));
                if decoded_since.is_some_and(|t| t.elapsed() >= FIRST_CALIBRATION_AFTER) && self.deps.lease.is_normal() {
                    calibrated = true;
                    if let Err(e) = self.calibrate().await {
                        self.deps.log.system("crystal", format!("calibration after start failed: {e:#}"));
                    }
                }
                continue;
            }
            self.sample(source).await;
            ticks = ticks.wrapping_add(1);
            if ticks % UPDATE_EVERY == 0 {
                self.update().await;
            }
        }
    }

    /// One tracker sample, when the live site gives one.
    async fn sample(&self, source: Option<Source>) {
        if !self.deps.lease.is_normal() || self.calibrating.try_lock().is_err() {
            return;
        }
        let t = self.deps.tuner.tuning();
        if t.preset.is_none() {
            return;
        }
        let shift = match (source, (self.deps.control)().carrier_offset_hz) {
            (Some(Source::P25Loop | Source::DmrEqualiser), Some(offset)) => t.lo_shift_hz as f64 + offset,
            _ => return,
        };
        self.lock().tracker.sample(Instant::now(), (t.lo_hz, t.control_hz), ppm_of(shift, t.lo_hz));
    }

    /// Apply the tracker's estimate when it should be.
    async fn update(&self) {
        let Some(estimate) = self.lock().tracker.estimate() else { return };
        let settings = self.deps.config.lock().await.radio.value.crystal.clone();
        let t = self.deps.tuner.tuning();
        let anchor = self.lock().anchor;
        let ppm = match decide(estimate, t.crystal_ppm, anchor, settings.anchor_hz, t.lo_hz, settings.tracking) {
            Decision::Apply(ppm) => ppm,
            Decision::Hold(why) => {
                self.lock().last_decision = Some(format!("estimate {estimate:+.4} ppm held: {why}"));
                return;
            }
        };
        if !self.deps.lease.is_normal() {
            return;
        }
        match self.deps.tuner.set_crystal_ppm(ppm).await {
            Ok(now) => {
                let text = format!("crystal tracker: {:+.4} -> {ppm:+.4} ppm (LO shift {:+} Hz)", t.crystal_ppm, now.lo_shift_hz);
                {
                    let mut i = self.lock();
                    i.tracker.moved(Instant::now());
                    i.last_decision = Some(text.clone());
                }
                self.deps.log.system("crystal", text);
                let saved = self.lock().saved_ppm;
                if saved.is_none_or(|s| (hz_of(ppm, t.lo_hz) - hz_of(s, t.lo_hz)).abs() >= SAVE_STEP_HZ) {
                    self.save(ppm, &now, "tracker").await;
                }
            }
            Err(e) => tracing::warn!("crystal tracker: the correction was not applied: {e:#}"),
        }
    }

    /// Measure the correction on the live control channel and apply it; it becomes the
    /// tracker's anchor.
    pub async fn calibrate(&self) -> Result<Calibration> {
        let _one = self.calibrating.try_lock().map_err(|_| anyhow::anyhow!("a calibration is running"))?;
        let started = Instant::now();
        let source = Source::of(&(self.deps.control)()).context("the live site is not being decoded")?;
        if !self.deps.lease.is_normal() {
            bail!("the radio is busy (a site switch or a scan)");
        }
        let t = self.deps.tuner.tuning();
        if t.preset.is_none() {
            bail!("the radio is not tuned");
        }
        let expected = t.control_hz as f64 - t.lo_hz as f64;
        let db = average_db(&spectrum(self.deps.tuner.hw(), SPECTRUM_FRAMES).await).context("no spectrometer frames")?;
        let (found, spectrum_db) =
            peak_near(&db, f64::from(t.sample_rate_hz), expected, SEARCH_HZ).context("no peak near the control channel")?;
        let spectrum_offset_hz = found - expected;
        let stepped = self.deps.tuner.set_crystal_ppm(ppm_of(t.lo_shift_hz as f64 + spectrum_offset_hz, t.lo_hz)).await?;
        tokio::time::sleep(SETTLE).await;
        if !self.deps.lease.is_normal() || self.deps.tuner.tuning().lo_hz != t.lo_hz {
            bail!("the radio moved during the calibration");
        }
        let (residual_hz, residual_samples) = self.residual(source).await;
        let shift = stepped.lo_shift_hz as f64 + residual_hz.unwrap_or(0.0);
        let ppm = ppm_of(shift, t.lo_hz);
        let done = self.deps.tuner.set_crystal_ppm(ppm).await?;
        let cal = Calibration {
            at_unix_ms: unix_ms(),
            lo_hz: t.lo_hz,
            control_hz: t.control_hz,
            source,
            spectrum_offset_hz,
            spectrum_db,
            residual_hz,
            residual_samples,
            ppm_before: t.crystal_ppm,
            ppm,
            duration_ms: started.elapsed().as_millis() as u64,
        };
        {
            let mut i = self.lock();
            i.tracker.moved(Instant::now());
            i.anchor = Some(ppm);
            i.calibration = Some(cal.clone());
            i.last_decision = None;
        }
        self.deps.log.system(
            "crystal",
            format!(
                "crystal calibrated: {:+.4} -> {ppm:+.4} ppm (spectrum {spectrum_offset_hz:+.0} Hz, residual {}; LO shift {:+} Hz)",
                t.crystal_ppm,
                residual_hz.map_or("not measured".to_string(), |r| format!("{r:+.1} Hz")),
                done.lo_shift_hz
            ),
        );
        self.save(ppm, &done, "calibration").await;
        Ok(cal)
    }

    /// The residual after a correction step: the decoder's carrier offset over 3 s, while a
    /// signal was on the channel; `None` when the channel was mostly idle.
    async fn residual(&self, source: Source) -> (Option<f64>, usize) {
        if source == Source::Spectrum {
            return (None, 0);
        }
        let mut sum = 0.0;
        let mut n = 0usize;
        for _ in 0..RESIDUAL_READS {
            let read = (self.deps.control)().carrier_offset_hz;
            if let Some(r) = read {
                sum += r;
                n += 1;
            }
            tokio::time::sleep(RESIDUAL_EVERY).await;
        }
        if n * 3 < RESIDUAL_READS {
            return (None, n);
        }
        (Some(sum / n as f64), n)
    }

    /// Keep the correction in `state/radio.json`.
    async fn save(&self, ppm: f64, t: &Tuning, method: &str) {
        let mut c = self.deps.config.lock().await;
        c.state.value.crystal = Some(state::Crystal {
            ppm,
            measured_at_lo_hz: t.lo_hz,
            lo_shift_hz: t.lo_shift_hz,
            control_freq_hz: Some(t.control_hz),
            method: method.to_string(),
            at_unix_ms: unix_ms(),
        });
        match config::save(&self.deps.paths.radio_state(), &c.state) {
            Ok(()) => self.lock().saved_ppm = Some(ppm),
            Err(e) => tracing::warn!("crystal: not saved: {e:#}"),
        }
    }
}

/// `n` spectrometer frames as dB per bin, after the first two (the LO settling).
async fn spectrum<H: RadioHw>(hw: &H, n: usize) -> Vec<Vec<f32>> {
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

/// The frames' mean power per bin, in dB.
fn average_db(frames: &[Vec<f32>]) -> Option<Vec<f32>> {
    let n = frames.first()?.len();
    if n == 0 || frames.iter().any(|f| f.len() != n) {
        return None;
    }
    let mut linear = vec![0f64; n];
    for f in frames {
        for (acc, &d) in linear.iter_mut().zip(f) {
            *acc += 10f64.powf(f64::from(d) / 10.0);
        }
    }
    Some(linear.iter().map(|&p| (10.0 * (p / frames.len() as f64).log10()) as f32).collect())
}

#[cfg(test)]
#[path = "crystal_tests.rs"]
mod tests;
