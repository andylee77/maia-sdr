//! Data mode: the unit watching the open-protocol data bands (315, 345, 433.92 and 902 to
//! 928 MHz) for the devices that transmit nearby (`doc/DATA_MODE.md`). While it has the radio it
//! listens on one window, 433.92 MHz (`listen`): each burst heard is listed with its frequency,
//! length and level, the bursts of one frequency are an emitter with how often it sends, and
//! the IQ of the next bursts is kept on the card when asked (`captures`). Nothing is decoded
//! yet, and what was heard is not kept across a restart: the band plan, the scan, the devices
//! and their data log come with the study's later steps.

pub mod captures;
mod listen;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::protocol::ism::bursts::{Burst, Counters};
use crate::protocol::ism::channel::{PASS_HZ, RATE_HZ};
use crate::radio::tuner::{RadioHw, Tuner};
use crate::services::config::radio::{Gain, GainMode};
use crate::services::events::EventLog;
use crate::services::mode::Hold;
use captures::{Captures, Job, Kept};

/// The manual gain the mode listens with when the radio's own setting is an AGC: bursts would
/// find an AGC still settling. Unit A's captures at 433.92 MHz were taken at it, with the
/// strongest device at an eighth of full scale.
const DEFAULT_GAIN_DB: f64 = 55.0;
/// The ADC's full scale, in counts.
const FULL_SCALE: f32 = 2048.0;
/// The newest bursts listed.
const LISTED: usize = 100;
/// The emitters kept; the one not heard for longest goes.
const EMITTERS: usize = 200;
/// Bursts this close are one emitter's: in unit A's captures a device's bursts came within
/// 0.3 kHz of each other, and two devices sat 8.9 kHz apart.
const EMITTER_HZ: f64 = 3_000.0;
/// An emitter's interval, length and level are the medians of its last bursts.
const RECENT: usize = 16;
/// The most bursts a capture can be armed for, and the widest it can listen for them.
const ARM_MOST: u32 = 100;
const ARM_SPAN_HZ: u32 = 20_000;
/// The counts that change all the time are told at most this often.
const TOLD_EVERY: Duration = Duration::from_secs(1);

/// The gain data mode listens with, from the radio's own setting.
pub fn gain_db(gain: &Gain) -> f64 {
    match (gain.mode, gain.manual_db) {
        (GainMode::Manual, Some(db)) => f64::from(db),
        _ => DEFAULT_GAIN_DB,
    }
}

/// What the mode listens to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Window {
    /// The band plan's name for it.
    pub name: &'static str,
    /// The frequency listened around, and the edges of what is heard either side of it.
    pub spot_hz: u64,
    pub low_hz: u64,
    pub high_hz: u64,
    /// The radio's LO and rate for it, and the rate of the channel cut around the spot.
    pub lo_hz: u64,
    pub sample_rate_hz: u32,
    pub channel_rate_hz: u32,
    /// The receiver's manual gain; none for a replayed file.
    pub gain_db: Option<f64>,
    /// `radio`, or `replay`: a file in the radio's place (a development host).
    pub source: &'static str,
}

impl Window {
    fn of(plan: &listen::Plan, lo_hz: u64, sample_rate_hz: u32, gain_db: Option<f64>, source: &'static str) -> Window {
        Window {
            name: plan.name,
            spot_hz: plan.spot_hz,
            low_hz: plan.spot_hz - PASS_HZ as u64,
            high_hz: plan.spot_hz + PASS_HZ as u64,
            lo_hz,
            sample_rate_hz,
            channel_rate_hz: RATE_HZ,
            gain_db,
            source,
        }
    }

    /// "433.92 MHz (a 3 MSPS window at 434.42 MHz)".
    fn describe(&self) -> String {
        format!("{} MHz (a {} MSPS window at {} MHz)", self.spot_hz as f64 / 1e6, f64::from(self.sample_rate_hz) / 1e6, self.lo_hz as f64 / 1e6)
    }
}

/// A burst heard.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Heard {
    /// Its number since the scanner started.
    pub seq: u64,
    /// When it began.
    pub unix_ms: u64,
    pub freq_hz: u64,
    /// How wide it is 10 dB under its strongest bin (the bins are 3.9 kHz).
    pub width_hz: u32,
    /// From its first to its last millisecond on, and the time it was on between them.
    pub length_ms: u32,
    pub on_ms: u32,
    /// Its strongest 3.9 kHz bin over the floor, and its power at its strongest millisecond
    /// against the ADC's full scale.
    pub over_db: f32,
    pub level_dbfs: f32,
    /// The emitter it is of.
    pub emitter: u32,
    /// Its IQ on the card, when it was kept.
    pub capture: Option<String>,
}

/// The bursts of one frequency.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Emitter {
    pub id: u32,
    pub freq_hz: u64,
    pub bursts: u64,
    pub first_unix_ms: u64,
    pub last_unix_ms: u64,
    /// The time from one burst's start to the next: the median of its last ones; none until it
    /// was heard twice.
    pub every_ms: Option<u64>,
    /// The medians of its last bursts.
    pub length_ms: u32,
    pub width_hz: u32,
    pub over_db: f32,
    pub level_dbfs: f32,
    #[serde(skip)]
    recent: VecDeque<(u64, u32, u32, f32, f32)>,
}

fn median<T: Copy + PartialOrd>(values: impl Iterator<Item = T>) -> Option<T> {
    let mut v: Vec<T> = values.collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v.get(v.len() / 2).copied()
}

impl Emitter {
    fn heard(&mut self, h: &Heard) {
        self.bursts += 1;
        self.last_unix_ms = h.unix_ms;
        // The frequency follows the device as it drifts.
        self.freq_hz = (self.freq_hz as f64 + 0.2 * (h.freq_hz as f64 - self.freq_hz as f64)).round() as u64;
        self.recent.push_back((h.unix_ms, h.length_ms, h.width_hz, h.over_db, h.level_dbfs));
        while self.recent.len() > RECENT {
            self.recent.pop_front();
        }
        let times: Vec<u64> = self.recent.iter().map(|r| r.0).collect();
        // With no interval yet (its first burst, or its first since the mode was away) it keeps
        // the one it had.
        self.every_ms = median(times.windows(2).map(|w| w[1].saturating_sub(w[0]))).or(self.every_ms);
        self.length_ms = median(self.recent.iter().map(|r| r.1)).unwrap_or(h.length_ms);
        self.width_hz = median(self.recent.iter().map(|r| r.2)).unwrap_or(h.width_hz);
        self.over_db = median(self.recent.iter().map(|r| r.3)).unwrap_or(h.over_db);
        self.level_dbfs = median(self.recent.iter().map(|r| r.4)).unwrap_or(h.level_dbfs);
    }
}

/// Why nothing was armed: what was asked is not valid, or the mode cannot keep it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArmError {
    Invalid(String),
    Unable(String),
}

/// "Keep the IQ of the next bursts".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Arm {
    /// Bursts still to keep.
    pub left: u32,
    /// Only bursts within `span_hz` / 2 of this; none: any burst.
    pub freq_hz: Option<u64>,
    pub span_hz: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Bursts heard since the scanner started.
    pub bursts: u64,
    /// Milliseconds left out as broadband clicks, and bursts too short or too thin to count,
    /// since the mode last began to listen.
    pub clicks: u64,
    pub slight: u64,
    /// Times samples were lost and the listening began afresh: the capture ring was read too
    /// late or the core lost words (`gaps`), or the listening thread was behind (`dropped`).
    pub gaps: u64,
    pub dropped: u64,
    /// Captures written, and those that could not be.
    pub captures: u64,
    pub capture_errors: u64,
}

/// The mode as its page shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct View {
    /// `off` (the radio is another mode's), `starting`, `listening` or `error`.
    pub state: &'static str,
    pub error: Option<String>,
    /// What it listens to, or last did.
    pub window: Option<Window>,
    pub since_unix_ms: Option<u64>,
    /// The noise in what is heard, against the ADC's full scale; none in the first quarter
    /// second.
    pub floor_dbfs: Option<f32>,
    pub counts: Counts,
    /// By frequency.
    pub emitters: Vec<Emitter>,
    /// The newest first.
    pub bursts: Vec<Heard>,
    pub arm: Option<Arm>,
    /// Whether captures can be kept (a card's folder is there for them).
    pub captures: bool,
    /// Goes up with each change of the above.
    pub rev: u64,
}

/// The mode in a line, for every page's header.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub state: &'static str,
    pub error: Option<String>,
    /// The window it listens to, or last did: the band plan's name for it, and its spot.
    pub window: Option<&'static str>,
    pub spot_hz: Option<u64>,
    pub emitters: usize,
    pub bursts: u64,
    pub arm_left: Option<u32>,
}

struct Inner {
    state: &'static str,
    error: Option<String>,
    window: Option<Window>,
    since_unix_ms: Option<u64>,
    floor_dbfs: Option<f32>,
    counts: Counts,
    emitters: Vec<Emitter>,
    next_emitter: u32,
    bursts: VecDeque<Heard>,
    arm: Option<Arm>,
    rev: u64,
    told: Instant,
}

/// A burst whose IQ is to be kept.
pub(self) struct Keep {
    heard: Heard,
    id: String,
}

/// What the listening and the API share.
pub(self) struct Shared {
    inner: Mutex<Inner>,
    captures: Option<Arc<Captures>>,
    writer: Mutex<Option<mpsc::Sender<Job>>>,
}

fn db(ratio: f32) -> f32 {
    10.0 * ratio.max(1e-12).log10()
}

impl Shared {
    fn new(captures: Option<Arc<Captures>>) -> Shared {
        Shared {
            inner: Mutex::new(Inner {
                state: "off",
                error: None,
                window: None,
                since_unix_ms: None,
                floor_dbfs: None,
                counts: Counts::default(),
                emitters: Vec::new(),
                next_emitter: 1,
                bursts: VecDeque::new(),
                arm: None,
                rev: 0,
                told: Instant::now(),
            }),
            captures,
            writer: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn change<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let mut i = self.lock();
        i.rev += 1;
        f(&mut i)
    }

    fn starting(&self) {
        self.change(|i| {
            i.state = "starting";
            i.error = None;
        });
    }

    fn listening(&self, window: Window) {
        self.change(|i| {
            i.state = "listening";
            i.window = Some(window);
            i.since_unix_ms = Some(crate::util::time::unix_ms());
            i.floor_dbfs = None;
            i.counts.clicks = 0;
            i.counts.slight = 0;
            // The time it did not listen is no emitter's interval.
            i.emitters.iter_mut().for_each(|e| e.recent.clear());
        });
    }

    /// The listening ended: by the mode's leaving, or with an error. Nothing stays armed.
    fn stopped(&self, error: Option<String>) {
        self.change(|i| {
            i.state = if error.is_some() { "error" } else { "off" };
            i.error = error;
            i.arm = None;
        });
    }

    fn count(&self, f: impl FnOnce(&mut Counts)) {
        self.change(|i| f(&mut i.counts));
    }

    /// The detector's counts and floor, after each block of the window.
    fn read(&self, counters: Counters, floor_power: Option<f32>) {
        let mut i = self.lock();
        i.counts.clicks = counters.clicks;
        i.counts.slight = counters.slight;
        i.floor_dbfs = floor_power.map(|p| db(p / (FULL_SCALE * FULL_SCALE)));
        if i.told.elapsed() >= TOLD_EVERY {
            i.told = Instant::now();
            i.rev += 1;
        }
    }

    /// A burst ended: it is listed, and its emitter with it. `Some` when its IQ is to be kept.
    fn heard(&self, window: &Window, b: &Burst, unix_ms: u64) -> Option<Keep> {
        let keeping = self.captures.is_some();
        self.change(|i| {
            i.counts.bursts += 1;
            let freq_hz = (window.spot_hz as f64 + f64::from(b.offset_hz)).round() as u64;
            let at = match i.emitters.iter().position(|e| (e.freq_hz as f64 - freq_hz as f64).abs() <= EMITTER_HZ) {
                Some(at) => at,
                None => {
                    if i.emitters.len() >= EMITTERS {
                        if let Some(oldest) = (0..i.emitters.len()).min_by_key(|&k| i.emitters[k].last_unix_ms) {
                            i.emitters.remove(oldest);
                        }
                    }
                    let id = i.next_emitter;
                    i.next_emitter += 1;
                    i.emitters.push(Emitter { id, freq_hz, bursts: 0, first_unix_ms: unix_ms, last_unix_ms: unix_ms, every_ms: None, length_ms: 0, width_hz: 0, over_db: 0.0, level_dbfs: 0.0, recent: VecDeque::new() });
                    i.emitters.len() - 1
                }
            };
            let mut heard = Heard {
                seq: i.counts.bursts,
                unix_ms,
                freq_hz,
                width_hz: b.width_hz.round() as u32,
                length_ms: ((b.end - b.start) * 1000 / u64::from(RATE_HZ)) as u32,
                on_ms: b.on_ms.round() as u32,
                over_db: b.over_db,
                level_dbfs: db(b.power / (FULL_SCALE * FULL_SCALE)),
                emitter: i.emitters[at].id,
                capture: None,
            };
            i.emitters[at].heard(&heard);
            let wanted = keeping && i.arm.is_some_and(|a| a.freq_hz.is_none_or(|f| (f as f64 - freq_hz as f64).abs() <= f64::from(a.span_hz) / 2.0));
            let keep = wanted.then(|| {
                let id = captures::id_of(unix_ms, heard.seq);
                heard.capture = Some(id.clone());
                if let Some(a) = i.arm.as_mut() {
                    a.left -= 1;
                }
                i.arm = i.arm.filter(|a| a.left > 0);
                Keep { heard: heard.clone(), id }
            });
            i.bursts.push_front(heard);
            i.bursts.truncate(LISTED);
            keep
        })
    }

    /// Hand a burst's IQ to the writer: `iq` begins `lead_samples` before the burst, at
    /// `unix_ms`.
    fn keep(&self, keep: Keep, unix_ms: u64, lead_samples: u32, iq: Vec<u8>, window: &Window) {
        let (file, meta) = captures::names(&keep.id, window.spot_hz, window.channel_rate_hz);
        let h = &keep.heard;
        let kept = Kept {
            id: keep.id.clone(),
            unix_ms,
            freq_hz: h.freq_hz,
            width_hz: h.width_hz,
            length_ms: h.length_ms,
            on_ms: h.on_ms,
            over_db: h.over_db,
            level_dbfs: h.level_dbfs,
            centre_hz: window.spot_hz,
            sample_rate_hz: window.channel_rate_hz,
            samples: (iq.len() / 4) as u64,
            lead_samples,
            file,
            bytes: iq.len() as u64,
            meta,
        };
        let radio = match window.gain_db {
            Some(gain) => format!("Fishball scanner, AD9361: {}, {gain} dB of gain", window.describe()),
            None => format!("a replayed capture: {}", window.describe()),
        };
        let sent = self.writer.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|tx| tx.send(Job { kept, iq, radio }).is_ok());
        if !sent {
            self.not_kept(&keep.id);
        }
    }

    /// A capture was not written, or was deleted: its burst no longer names it.
    fn forget(&self, id: &str) {
        self.change(|i| i.bursts.iter_mut().filter(|h| h.capture.as_deref() == Some(id)).for_each(|h| h.capture = None));
    }

    fn not_kept(&self, id: &str) {
        self.forget(id);
        self.count(|c| c.capture_errors += 1);
    }

    fn view(&self) -> View {
        let i = self.lock();
        let mut emitters = i.emitters.clone();
        emitters.sort_by_key(|e| e.freq_hz);
        View {
            state: i.state,
            error: i.error.clone(),
            window: i.window.clone(),
            since_unix_ms: i.since_unix_ms,
            floor_dbfs: i.floor_dbfs,
            counts: i.counts,
            emitters,
            bursts: i.bursts.iter().cloned().collect(),
            arm: i.arm,
            captures: self.captures.is_some(),
            rev: i.rev,
        }
    }
}

/// Whether the card can take a capture now, or why not.
pub type CardCheck = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// The listening while the mode lasts.
struct Run {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

pub struct Data {
    /// The mode's hold on the radio while it lasts.
    radio: tokio::sync::Mutex<Option<Hold>>,
    run: tokio::sync::Mutex<Option<Run>>,
    shared: Arc<Shared>,
    /// A raw capture's file played in the radio's place (a development host).
    replay: Option<PathBuf>,
}

/// A unit without a card's folder for captures, and no replay: it listens and keeps nothing.
impl Default for Data {
    fn default() -> Self {
        Data { radio: Default::default(), run: Default::default(), shared: Arc::new(Shared::new(None)), replay: None }
    }
}

impl Data {
    /// The mode with its captures under `sd`, written by a thread of their own when `card` says
    /// the card can take them.
    pub fn new(sd: &std::path::Path, replay: Option<PathBuf>, card: CardCheck, log: Arc<EventLog>) -> Data {
        let captures = Arc::new(Captures::open(sd));
        let shared = Arc::new(Shared::new(Some(captures.clone())));
        let (tx, rx) = mpsc::channel::<Job>();
        let state = shared.clone();
        let spawned = std::thread::Builder::new().name("data-captures".into()).spawn(move || {
            for job in rx {
                let id = job.kept.id.clone();
                match card().map_err(std::io::Error::other).and_then(|()| captures.keep(job)) {
                    Ok(_) => state.count(|c| c.captures += 1),
                    Err(e) => {
                        log.radio("data", format!("capture {id} was not kept: {e}"));
                        state.not_kept(&id);
                    }
                }
            }
        });
        match spawned {
            Ok(_) => *shared.writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx),
            Err(e) => tracing::error!("the captures' thread did not start: {e}"),
        }
        Data { radio: Default::default(), run: Default::default(), shared, replay }
    }

    /// Data mode begins: the radio is this service's until `leave`, and it listens.
    pub async fn enter<H: RadioHw + 'static>(&self, hold: Hold, tuner: Arc<Tuner<H>>, gain_db: f64, log: Arc<EventLog>) {
        *self.radio.lock().await = Some(hold);
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(listen::run(self.shared.clone(), tuner, gain_db, self.replay.clone(), stop.clone(), log));
        *self.run.lock().await = Some(Run { stop, task });
    }

    /// Data mode ends: the listening stops, and the hold on the radio comes back, for the next
    /// mode or the scanner.
    pub async fn leave(&self) -> Option<Hold> {
        if let Some(run) = self.run.lock().await.take() {
            run.stop.store(true, Ordering::SeqCst);
            let _ = run.task.await;
        }
        self.radio.lock().await.take()
    }

    pub fn view(&self) -> View {
        self.shared.view()
    }

    pub fn summary(&self) -> Summary {
        let i = self.shared.lock();
        Summary { state: i.state, error: i.error.clone(), window: i.window.as_ref().map(|w| w.name), spot_hz: i.window.as_ref().map(|w| w.spot_hz), emitters: i.emitters.len(), bursts: i.counts.bursts, arm_left: i.arm.map(|a| a.left) }
    }

    /// Keep the IQ of the next `count` bursts: any, or only those within `span_hz` / 2 of
    /// `freq_hz`. Refused while the mode does not listen.
    pub fn arm(&self, count: u32, freq_hz: Option<u64>, span_hz: Option<u32>) -> Result<Arm, ArmError> {
        if self.shared.captures.is_none() {
            return Err(ArmError::Unable("this unit has nowhere to keep captures".into()));
        }
        if !(1..=ARM_MOST).contains(&count) {
            return Err(ArmError::Invalid(format!("count: 1 to {ARM_MOST}")));
        }
        let span_hz = span_hz.unwrap_or(ARM_SPAN_HZ);
        if !(1_000..=2 * PASS_HZ as u32).contains(&span_hz) {
            return Err(ArmError::Invalid(format!("span_hz: 1,000 to {}", 2 * PASS_HZ as u32)));
        }
        self.shared.change(|i| {
            let Some(w) = i.window.as_ref().filter(|_| i.state == "listening") else { return Err(ArmError::Unable("data mode is not listening".into())) };
            if let Some(f) = freq_hz.filter(|f| !(w.low_hz..=w.high_hz).contains(f)) {
                return Err(ArmError::Invalid(format!("{f} Hz is outside what is heard, {} to {} Hz", w.low_hz, w.high_hz)));
            }
            let arm = Arm { left: count, freq_hz, span_hz };
            i.arm = Some(arm);
            Ok(arm)
        })
    }

    pub fn disarm(&self) {
        self.shared.change(|i| i.arm = None);
    }

    /// The captures on the card; none on a unit without the folder.
    pub fn captures(&self) -> Option<&Arc<Captures>> {
        self.shared.captures.as_ref()
    }

    /// Delete a capture; false when there is none of that id.
    pub fn delete_capture(&self, id: &str) -> bool {
        let gone = self.captures().is_some_and(|c| c.delete(id));
        if gone {
            self.shared.forget(id);
        }
        gone
    }

    /// Delete every capture (a factory reset); how many there were.
    pub fn clear_captures(&self) -> usize {
        self.captures().map_or(0, |c| c.clear())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> Window {
        Window::of(&listen::PLAN, 434_420_000, 3_000_000, Some(55.0), "radio")
    }

    fn burst(offset_hz: f32, length_ms: u64) -> Burst {
        Burst { start: 250_000, end: 250_000 + length_ms * 250, offset_hz, width_hz: 11_719.0, on_ms: length_ms as f32 / 2.0, over_db: 31.0, power: 4096.0 }
    }

    fn listening(sd: Option<&std::path::Path>) -> Data {
        let data = match sd {
            Some(sd) => Data::new(sd, None, Arc::new(|| Ok(())), Arc::new(EventLog::default())),
            None => Data::default(),
        };
        data.shared.listening(window());
        data
    }

    #[test]
    fn the_window_is_the_spot_and_what_the_channel_passes_either_side() {
        let w = window();
        assert_eq!((w.low_hz, w.spot_hz, w.high_hz, w.channel_rate_hz), (433_830_000, 433_920_000, 434_010_000, 250_000));
        assert_eq!(w.describe(), "433.92 MHz (a 3 MSPS window at 434.42 MHz)");
    }

    #[test]
    fn the_window_s_preset_is_one_the_radio_takes_and_divides_down_to_the_channel() {
        let plan = &listen::PLAN;
        let preset = crate::hardware::presets::find_preset(plan.preset).expect("the window's preset");
        // Without its own filter the AD9361 samples at 25 MSPS / 12 or more: it refuses 2 MSPS.
        assert!(preset.sample_rate_hz >= 2_083_334, "{} SPS", preset.sample_rate_hz);
        let offset = plan.spot_hz as f64 - plan.lo_hz as f64;
        assert!(crate::protocol::ism::channel::Channel::new(preset.sample_rate_hz, offset).is_ok());
        // The spot is clear of the LO's spur, and inside what the window passes.
        assert!(offset.abs() > f64::from(RATE_HZ) / 2.0 && offset.abs() + PASS_HZ < 0.45 * f64::from(preset.sample_rate_hz));
    }

    #[test]
    fn the_mode_listens_with_a_manual_gain() {
        let manual = Gain { mode: GainMode::Manual, manual_db: Some(48) };
        assert_eq!(gain_db(&manual), 48.0);
        assert_eq!(gain_db(&Gain { mode: GainMode::SlowAttack, manual_db: Some(48) }), DEFAULT_GAIN_DB);
        assert_eq!(gain_db(&Gain { mode: GainMode::Manual, manual_db: None }), DEFAULT_GAIN_DB);
    }

    #[test]
    fn bursts_of_one_frequency_are_an_emitter_with_its_interval() {
        let data = listening(None);
        let w = window();
        // The Oregon Scientific sensor: 39.85 kHz under the spot, every 30 s; and another device
        // 8.9 kHz below it, once.
        for k in 0..4u64 {
            data.shared.heard(&w, &burst(-39_850.0 + 100.0 * k as f32, 392), 1_000_000 + 30_010 * k);
        }
        data.shared.heard(&w, &burst(-48_800.0, 780), 1_050_000);
        let v = data.view();
        assert_eq!(v.state, "listening");
        assert_eq!(v.counts.bursts, 5);
        assert_eq!(v.emitters.len(), 2, "{:?}", v.emitters);
        let (other, oregon) = (&v.emitters[0], &v.emitters[1]);
        assert_eq!((other.freq_hz, other.bursts, other.every_ms), (433_871_200, 1, None));
        assert_eq!((oregon.bursts, oregon.every_ms, oregon.length_ms, oregon.width_hz), (4, Some(30_010), 392, 11_719));
        assert!((oregon.freq_hz as i64 - 433_880_250).abs() < 150, "{}", oregon.freq_hz);
        assert_eq!((oregon.first_unix_ms, oregon.last_unix_ms), (1_000_000, 1_090_030));
        // 4096 counts² against a full scale of 2048: 30 dB under it.
        assert!((oregon.level_dbfs + 30.1).abs() < 0.1 && oregon.over_db == 31.0);
        // The newest first, each with its emitter.
        assert_eq!(v.bursts.iter().map(|h| (h.seq, h.emitter)).collect::<Vec<_>>(), vec![(5, other.id), (4, oregon.id), (3, oregon.id), (2, oregon.id), (1, oregon.id)]);
        assert_eq!((v.bursts[1].freq_hz, v.bursts[1].length_ms, v.bursts[1].on_ms, v.bursts[1].width_hz), (433_880_450, 392, 196, 11_719));
        let s = data.summary();
        assert_eq!((s.state, s.window, s.spot_hz, s.emitters, s.bursts), ("listening", Some("433"), Some(433_920_000), 2, 5));
        // After a time away the first burst gives no interval: the emitter keeps the one it had.
        data.shared.stopped(None);
        data.shared.listening(w.clone());
        data.shared.heard(&w, &burst(-39_850.0, 392), 2_000_000);
        let again = data.view().emitters[1].clone();
        assert_eq!((again.bursts, again.every_ms, again.last_unix_ms), (5, Some(30_010), 2_000_000));
        data.shared.heard(&w, &burst(-39_850.0, 392), 2_030_000);
        assert_eq!(data.view().emitters[1].every_ms, Some(30_000));
    }

    #[test]
    fn arming_keeps_the_next_bursts_that_match_and_then_ends() {
        let sd = tempfile::tempdir().unwrap();
        let data = listening(Some(sd.path()));
        let w = window();
        assert!(data.arm(0, None, None).is_err() && data.arm(101, None, None).is_err());
        assert!(matches!(data.arm(1, Some(435_000_000), None), Err(ArmError::Invalid(_))), "outside what is heard");
        assert!(data.arm(1, None, Some(500)).is_err());
        assert_eq!(data.arm(2, Some(433_880_000), None).unwrap(), Arm { left: 2, freq_hz: Some(433_880_000), span_hz: 20_000 });
        // Another device's burst is not kept; the sensor's next two are.
        assert!(data.shared.heard(&w, &burst(-3_800.0, 747), 1_000_000).is_none());
        let first = data.shared.heard(&w, &burst(-39_850.0, 392), 1_794_450_354_000).expect("the first");
        assert_eq!(first.id, "20261112T022554Z_0002");
        assert_eq!(data.view().arm, Some(Arm { left: 1, freq_hz: Some(433_880_000), span_hz: 20_000 }));
        assert!(data.shared.heard(&w, &burst(-39_850.0, 392), 1_794_450_384_000).is_some());
        assert_eq!(data.view().arm, None, "both kept");
        assert!(data.shared.heard(&w, &burst(-39_850.0, 392), 1_794_450_414_000).is_none());
        // The file is written by the captures' thread.
        data.shared.keep(first, 1_794_450_353_980, 5_000, vec![1; 4 * 108_000], &w);
        let waited = Instant::now();
        while data.view().counts.captures == 0 && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let listing = data.captures().unwrap().listing();
        assert_eq!(listing.count, 1);
        let k = &listing.items[0];
        assert_eq!((k.id.as_str(), k.freq_hz, k.centre_hz, k.samples, k.lead_samples, k.bytes), ("20261112T022554Z_0002", 433_880_150, 433_920_000, 108_000, 5_000, 432_000));
        assert_eq!(data.view().bursts[2].capture.as_deref(), Some("20261112T022554Z_0002"));
        // Deleted, its burst no longer names it.
        assert!(data.delete_capture("20261112T022554Z_0002") && !data.delete_capture("20261112T022554Z_0002"));
        assert_eq!(data.view().bursts[2].capture, None);
    }

    #[test]
    fn nothing_is_armed_unless_the_mode_listens_and_can_keep() {
        let data = Data::default();
        assert_eq!(data.arm(1, None, None).unwrap_err(), ArmError::Unable("this unit has nowhere to keep captures".into()));
        let sd = tempfile::tempdir().unwrap();
        let data = Data::new(sd.path(), None, Arc::new(|| Ok(())), Arc::new(EventLog::default()));
        assert_eq!(data.arm(1, None, None).unwrap_err(), ArmError::Unable("data mode is not listening".into()));
        data.shared.listening(window());
        data.arm(3, None, None).unwrap();
        // The listening's end disarms.
        data.shared.stopped(None);
        assert_eq!((data.view().state, data.view().arm), ("off", None));
        data.shared.stopped(Some("the capture ring stopped".into()));
        assert_eq!((data.view().state, data.view().error.as_deref()), ("error", Some("the capture ring stopped")));
    }

    #[test]
    fn a_capture_the_card_cannot_take_is_told_and_not_listed() {
        let sd = tempfile::tempdir().unwrap();
        let log = Arc::new(EventLog::default());
        let data = Data::new(sd.path(), None, Arc::new(|| Err("the card is absent".into())), log.clone());
        data.shared.listening(window());
        data.arm(1, None, None).unwrap();
        let keep = data.shared.heard(&window(), &burst(0.0, 100), 1_794_450_354_000).unwrap();
        data.shared.keep(keep, 1_794_450_354_000, 0, vec![0; 400], &window());
        let waited = Instant::now();
        while data.view().counts.capture_errors == 0 && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let v = data.view();
        assert_eq!((v.counts.captures, v.counts.capture_errors, v.bursts[0].capture.clone()), (0, 1, None));
        assert_eq!(data.captures().unwrap().listing().count, 0);
        assert!(log.since(0, 10, false, None).iter().any(|e| e.text == "capture 20261112T022554Z_0001 was not kept: the card is absent"));
    }
}
