//! The channel finder's runs. In ATSC mode it holds the radio (the mode's lease), so it tunes as
//! it likes; going back to scanner mode stops a run first and takes the radio from it. A run
//! sweeps the channels' spectrum, then decodes each 8-VSB channel strong enough for its names.
//! A run that finishes is kept with the unit's state (`kept`), and is the last scan again after
//! a restart.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::OwnedMutexGuard;

use super::kept;
use super::live::{Live, LiveReceiver};
use super::{usable_half, AtscRequest, AtscScan, ChannelSpectrum, FoundChannel, Station};
use crate::hardware::ad9361::GainMode;
use crate::hardware::presets::find_preset;
use crate::protocol::atsc::spectrum::{measure, Kind};
use crate::protocol::atsc::receiver::{identify, Identified};
use crate::protocol::atsc::{windows, Channel};
use crate::radio::lease::LeaseGuard;
use crate::radio::tuner::{RadioHw, Tuner, TuningPlan};
use crate::services::discovery::sweep::{grab, LO_SETTLE};
use crate::services::discovery::{SWEEP_PRESET, SWEEP_RATE_HZ};
use crate::services::events::EventLog;
use crate::util::time::unix_ms;

/// The gain at which the spectrum's dB scale reads about dBm (`discovery::carriers`).
const SCALE_GAIN_DB: f64 = 60.0;

/// Shown either side of a channel in its spectrum.
const SPECTRUM_MARGIN_HZ: f64 = 500_000.0;
/// A channel is decoded at this rate, its centre at the LO (the ±3 MHz channel inside ±4.5 MHz).
const IDENTIFY_PRESET: &str = "10M";
/// 0.5 s of it: PSIP sends the virtual channel table at least every 0.4 s.
const CAPTURE_SAMPLES: usize = 5_000_000;
/// 8-VSB needs about 15 dB of carrier to noise; weaker channels are not tried.
const IDENTIFY_MIN_DB: f32 = 15.0;

/// ATSC mode's hold on the radio, and the site scanner mode brings back.
struct Held {
    _lease: LeaseGuard,
    back_to: Option<String>,
}

/// One window of the last scan: its LO, its channels, and its frames averaged (dB per bin, about
/// dBm).
struct WindowSpectrum {
    lo_hz: u64,
    channels: Vec<Channel>,
    db: Vec<f32>,
}

#[derive(Default)]
pub struct Atsc {
    state: Mutex<AtscScan>,
    /// The last scan's windows.
    spectra: Mutex<Vec<WindowSpectrum>>,
    /// Where a finished scan is kept, and whether this binary may write the file (`restore`).
    keep: Mutex<Option<(PathBuf, bool)>>,
    /// The radio while ATSC mode lasts; a run or live TV holds this lock.
    radio: Arc<tokio::sync::Mutex<Option<Held>>>,
    /// Scanner mode is coming back: no new run, and the running one stops.
    leaving: AtomicBool,
    /// Live TV (the radio core's 8-VSB block).
    pub live: Arc<Live>,
}

impl Atsc {
    pub fn state(&self) -> AtscScan {
        self.lock().clone()
    }

    /// At start: the scan kept at `path` is the last scan again (without its spectra), and the
    /// scans that finish from now on are kept there.
    pub fn restore(&self, path: PathBuf) {
        let (scan, writable) = kept::load(&path);
        if let Some(scan) = scan {
            *self.lock() = scan;
        }
        *self.keep.lock().unwrap_or_else(|e| e.into_inner()) = Some((path, writable));
    }

    /// Keep the scan that just finished, where one is kept.
    async fn keep(&self, scan: AtscScan, log: &EventLog) {
        let Some((path, writable)) = self.keep.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
        if !writable {
            log.tv("atsc", format!("TV scan {} not kept: {} belongs to a newer version", scan.id, path.display()));
            return;
        }
        let id = scan.id;
        let saved = tokio::task::spawn_blocking(move || kept::save(&path, &scan)).await;
        if let Err(e) = saved.map_err(anyhow::Error::from).and_then(|r| r) {
            log.tv("atsc", format!("TV scan {id} not kept: {e:#}"));
        }
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

    fn spectra(&self) -> std::sync::MutexGuard<'_, Vec<WindowSpectrum>> {
        self.spectra.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// RF channel `number` as the last scan read it.
    pub fn channel_spectrum(&self, number: u8) -> Option<ChannelSpectrum> {
        let spectra = self.spectra();
        let (w, ch) = spectra.iter().find_map(|w| w.channels.iter().find(|c| c.number == number).map(|c| (w, *c)))?;
        let n = w.db.len();
        if n == 0 {
            return None;
        }
        let bin_hz = f64::from(SWEEP_RATE_HZ) / n as f64;
        let bin = |f: f64| ((f - w.lo_hz as f64) / bin_hz + (n / 2) as f64).round().clamp(0.0, (n - 1) as f64) as usize;
        let (first, last) = (bin(ch.low_hz as f64 - SPECTRUM_MARGIN_HZ), bin(ch.high_hz() as f64 + SPECTRUM_MARGIN_HZ));
        Some(ChannelSpectrum {
            number,
            low_hz: ch.low_hz,
            high_hz: ch.high_hz(),
            pilot_hz: ch.pilot_hz().round() as u64,
            lo_hz: w.lo_hz,
            start_hz: w.lo_hz as f64 + (first as f64 - (n / 2) as f64) * bin_hz,
            bin_hz,
            db: w.db[first..=last].to_vec(),
        })
    }

    /// ATSC mode begins: the radio is this service's until `leave`.
    pub async fn enter(&self, lease: LeaseGuard, back_to: Option<String>) {
        *self.radio.lock().await = Some(Held { _lease: lease, back_to });
        self.leaving.store(false, Ordering::SeqCst);
    }

    /// ATSC mode ends: a run and live TV stop, the radio is handed back. Returns the site to bring
    /// back.
    pub async fn leave(&self) -> Option<String> {
        self.leaving.store(true, Ordering::SeqCst);
        self.cancel();
        self.live.stop().await;
        self.radio.lock().await.take().and_then(|h| h.back_to)
    }

    /// Live TV on RF `rf`: what played stops, and the radio goes to the block, its carrier loop
    /// started at the pilot offset the last TV scan measured on the channel. Refused outside ATSC
    /// mode or while a scan runs.
    pub async fn live_start<H: RadioHw + 'static>(self: &Arc<Self>, rf: u8, tuner: Arc<Tuner<H>>, log: Arc<EventLog>) -> Result<()> {
        self.live.stop().await;
        let radio = self.radio.clone().try_lock_owned().map_err(|_| anyhow!("a TV scan is running"))?;
        if radio.is_none() || self.leaving.load(Ordering::SeqCst) {
            bail!("the unit is not in ATSC mode");
        }
        let offset_hz = self.lock().found.iter().find(|c| c.number == rf).and_then(|c| c.pilot_offset_hz).unwrap_or(0);
        let result = self.live.start(rf, offset_hz as f64, tuner, log, Box::new(radio)).await;
        if let Err(e) = &result {
            self.live.failed(rf, e);
        }
        result
    }

    /// The live stream of RF `rf`: the channel the block plays, or, in ATSC mode with nobody
    /// watching another channel, `rf` tuned now. `None` without the block or outside ATSC mode.
    pub async fn live_stream<H: RadioHw + 'static>(self: &Arc<Self>, rf: u8, tuner: Arc<Tuner<H>>, log: Arc<EventLog>) -> Result<Option<LiveReceiver>> {
        if self.live.rf().await == Some(rf) {
            return Ok(Some(self.live.subscribe()));
        }
        if !tuner.hw().vsb_present().await || self.radio.try_lock().is_ok_and(|r| r.is_none()) || self.leaving.load(Ordering::SeqCst) {
            return Ok(None);
        }
        if let Some(other) = self.live.rf().await.filter(|_| self.live.viewers() > 0) {
            bail!("live TV is playing RF {other} for another viewer");
        }
        self.live_start(rf, tuner, log).await?;
        Ok(Some(self.live.subscribe()))
    }

    /// Start a run; refused outside ATSC mode, while one runs or while live TV plays.
    pub fn start<H: RadioHw + Send + Sync + 'static>(
        self: &Arc<Self>,
        req: AtscRequest,
        tuner: Arc<Tuner<H>>,
        log: Arc<EventLog>,
    ) -> Result<u64> {
        let radio = self.radio.clone().try_lock_owned().map_err(|_| anyhow!("the radio is busy: a TV scan runs, or live TV plays (stop it first)"))?;
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
        self.spectra().clear();
        let gain = req.gain_db.map_or("the AGC".to_string(), |g| format!("{g} dB of gain"));
        log.tv("atsc", format!("TV scan {id} started over {} channels at {gain}", channels.len()));
        let me = self.clone();
        tokio::spawn(async move { me.run(id, req, radio, tuner, log).await });
        Ok(id)
    }

    async fn run<H: RadioHw>(self: Arc<Self>, id: u64, req: AtscRequest, radio: OwnedMutexGuard<Option<Held>>, tuner: Arc<Tuner<H>>, log: Arc<EventLog>) {
        let result = self.sweep(&req, &tuner, &log).await;
        drop(radio);
        let (summary, finished) = {
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
            let summary = format!(
                "TV scan {id} {} on {} channels: {} 8-VSB, {} without an 8-VSB pilot (ATSC 3.0 or other), {} vacant",
                d.state,
                d.found.len(),
                d.count(Kind::Vsb),
                d.count(Kind::NoPilot),
                d.count(Kind::Vacant)
            );
            // A scan cut short or failed is not kept: the last one that finished stays.
            (summary, (d.state == "done").then(|| d.clone()))
        };
        log.tv("atsc", summary);
        if let Some(scan) = finished {
            self.keep(scan, &log).await;
        }
    }

    async fn sweep<H: RadioHw>(&self, req: &AtscRequest, tuner: &Tuner<H>, log: &EventLog) -> Result<()> {
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
            let clips_ppm = match (before.adc_clips, after.adc_clips, before.sample_count, after.sample_count) {
                (Some(a), Some(b), Some(s0), Some(s1)) if s1 > s0 => Some(f64::from(b.wrapping_sub(a)) * 1e6 / (s1 - s0) as f64),
                _ => None,
            };
            let gain = after.gain_db;
            let to_dbm = gain.map_or(0.0, |g| (g - SCALE_GAIN_DB) as f32);
            self.spectra().push(WindowSpectrum {
                lo_hz: w.lo_hz,
                channels: w.channels.clone(),
                db: power.iter().map(|&p| (10.0 * p.max(1e-20).log10()) as f32 - to_dbm).collect(),
            });
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
                        clips_ppm,
                        station: None,
                    })
                })
                .collect();
            self.update(|d| d.found.extend(found));
        }
        if req.identify {
            self.identify(tuner, log).await?;
        }
        Ok(())
    }

    /// Each 8-VSB channel strong enough, tuned on its own with its centre at the LO: a short
    /// capture, decoded on a worker thread.
    async fn identify<H: RadioHw>(&self, tuner: &Tuner<H>, log: &EventLog) -> Result<()> {
        let preset = find_preset(IDENTIFY_PRESET).context("the capture preset")?;
        let targets: Vec<u8> =
            self.lock().found.iter().filter(|c| c.kind == Kind::Vsb && c.level_db >= IDENTIFY_MIN_DB).map(|c| c.number).collect();
        self.update(|d| d.to_identify = targets.len());
        for n in targets {
            if self.stopping() {
                return Ok(());
            }
            let Some(ch) = Channel::get(n) else { continue };
            self.update(|d| {
                d.identifying = Some(n);
                d.lo_hz = Some(ch.center_hz());
            });
            tuner.apply(TuningPlan { preset, lo_hz: ch.center_hz(), control_hz: ch.center_hz() }).await?;
            tokio::time::sleep(LO_SETTLE).await;
            let station = match tuner.hw().capture(CAPTURE_SAMPLES).await {
                Ok(capture) => {
                    if capture.overflow {
                        tracing::warn!("RF {n}: the capture lost samples");
                    }
                    // The sample clock runs off the crystal the LO is corrected for.
                    let fs = f64::from(preset.sample_rate_hz) * (1.0 + tuner.tuning().crystal_ppm * 1e-6);
                    station_of(tokio::task::spawn_blocking(move || identify(&capture.samples, fs, 0.0)).await?)
                }
                Err(e) => Station { error: Some(format!("{e:#}")), ..Default::default() },
            };
            log.tv("atsc", describe(n, &station));
            self.update(|d| {
                if let Some(c) = d.found.iter_mut().find(|c| c.number == n) {
                    c.station = Some(station);
                }
                d.identified += 1;
            });
        }
        self.update(|d| d.identifying = None);
        Ok(())
    }
}

fn station_of(id: Option<Identified>) -> Station {
    match id {
        Some(i) => Station {
            tsid: i.psip.tsid,
            channels: i.psip.channels,
            time_unix: i.psip.time_unix,
            mer_db: Some(i.mer_db),
            packets: i.packets,
            corrected: i.corrected,
            failed: i.failed,
            programs: i.multiplex.programs,
            null_bps: i.multiplex.null_bps,
            error: None,
        },
        None => Station { error: Some("no segment or field syncs".into()), ..Default::default() },
    }
}

/// "RF 20: TSID 601, 4.1 WJXT-HD, 17.1 WCWJ-HD (MER 18.7 dB)", for the event log.
fn describe(n: u8, s: &Station) -> String {
    if let Some(e) = &s.error {
        return format!("RF {n}: not decoded ({e})");
    }
    let names: Vec<String> = s.channels.iter().map(|c| format!("{}.{} {}", c.major, c.minor, c.short_name)).collect();
    let mer = s.mer_db.map_or(String::new(), |m| format!(", MER {m:.1} dB"));
    match s.tsid {
        Some(t) if !names.is_empty() => format!("RF {n}: TSID {t}, {}{mer}", names.join(", ")),
        Some(t) => format!("RF {n}: TSID {t}, no channel table heard{mer}"),
        None => format!("RF {n}: no packets decoded ({} of {} failed{mer})", s.failed, s.packets),
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
