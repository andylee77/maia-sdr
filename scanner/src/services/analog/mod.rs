//! The analog module: the unit as a receiver of analog signals (broadcast FM, aircraft, amateur
//! and other two-way radio, weather radio). While it has the radio it is parked on one
//! frequency: the window around it is read whole from the capture ring (`listen`), the channel
//! at the frequency is cut in software and a receiver of the kind asked for (wide FM, narrow FM
//! or AM, `protocol::analog`) makes audio of it, which goes to whoever listens (`/ws/analog`),
//! 20 ms at a time. What it is tuned to is kept across restarts (the radio's state). Nothing is
//! scanned, remembered or recorded yet.

mod listen;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::protocol::analog::{Demod, Level, AUDIO_HZ};
use crate::radio::tuner::{RadioHw, Tuner};
use crate::services::events::EventLog;
use crate::services::mode::Hold;

/// A frame of audio: 20 ms.
pub const FRAME_SAMPLES: usize = AUDIO_HZ as usize / 50;
/// Frames the broadcast holds for a slow listener: five seconds.
const BROADCAST: usize = 256;
/// What the radio can be tuned to (the AD9361's range), and a squelch's range in dB over the
/// floor.
const FREQ_HZ: std::ops::RangeInclusive<u64> = 70_000_000..=6_000_000_000;
const SQUELCH_DB: std::ops::RangeInclusive<f32> = 0.0..=40.0;
/// The view's revision moves with the meter at most this often.
const METER_EVERY_MS: u128 = 250;

/// What the receiver is tuned to.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tuned {
    pub freq_hz: u64,
    pub demod: Demod,
    /// How far over the floor a carrier must be for audio to pass; none: no squelch. Wide FM
    /// has none.
    pub squelch_db: Option<f32>,
}

impl Default for Tuned {
    /// The weather radio channel most used.
    fn default() -> Self {
        Tuned { freq_hz: 162_550_000, demod: Demod::Nfm, squelch_db: Some(8.0) }
    }
}

/// A band the page offers: where it is, the step between its channels and how it is heard.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Band {
    pub id: &'static str,
    pub label: &'static str,
    pub low_hz: u64,
    pub high_hz: u64,
    pub step_hz: u32,
    pub demod: Demod,
}

pub const BANDS: [Band; 6] = [
    Band { id: "fm", label: "FM broadcast", low_hz: 87_900_000, high_hz: 107_900_000, step_hz: 200_000, demod: Demod::Wfm },
    Band { id: "air", label: "Air band", low_hz: 118_000_000, high_hz: 136_975_000, step_hz: 25_000, demod: Demod::Am },
    Band { id: "2m", label: "2 m amateur", low_hz: 144_000_000, high_hz: 148_000_000, step_hz: 5_000, demod: Demod::Nfm },
    Band { id: "marine", label: "Marine VHF", low_hz: 156_000_000, high_hz: 162_025_000, step_hz: 25_000, demod: Demod::Nfm },
    Band { id: "weather", label: "Weather radio", low_hz: 162_400_000, high_hz: 162_550_000, step_hz: 25_000, demod: Demod::Nfm },
    Band { id: "70cm", label: "70 cm amateur", low_hz: 420_000_000, high_hz: 450_000_000, step_hz: 12_500, demod: Demod::Nfm },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Another mode has the radio.
    Off,
    Starting,
    Listening,
    Error,
}

/// The radio's window while it listens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Window {
    pub lo_hz: u64,
    pub sample_rate_hz: u32,
    /// `radio`, or `replay` on a development host.
    pub source: &'static str,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Times samples were lost between the radio and the receiver.
    pub gaps: u64,
    /// Blocks of the window dropped because the receiver was behind.
    pub dropped: u64,
    /// Frames of audio made since the scanner started.
    pub frames: u64,
}

/// `GET /api/v1/analog`.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    /// Moves when anything here changes.
    pub rev: u64,
    pub state: State,
    pub error: Option<String>,
    pub tuned: Tuned,
    pub window: Option<Window>,
    /// What the meter last read; none while it does not listen.
    pub level: Option<Level>,
    pub audio_hz: u32,
    /// Open `/ws/analog` connections.
    pub listeners: usize,
    pub counts: Counts,
    pub bands: &'static [Band],
    pub low_hz: u64,
    pub high_hz: u64,
}

/// The module's line in `status`.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub state: State,
    pub error: Option<String>,
    pub freq_hz: u64,
    pub demod: Demod,
    /// Audio passes; none while it does not listen.
    pub open: Option<bool>,
}

struct Inner {
    state: State,
    error: Option<String>,
    tuned: Tuned,
    window: Option<Window>,
    level: Option<Level>,
    metered: Instant,
    counts: Counts,
    rev: u64,
}

/// What the service, the task that reads the radio and the receiver's thread share.
pub(super) struct Shared {
    inner: Mutex<Inner>,
    audio: broadcast::Sender<Arc<[i16]>>,
    listeners: AtomicUsize,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn change<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let mut i = self.lock();
        i.rev += 1;
        f(&mut i)
    }

    fn tuned(&self) -> Tuned {
        self.lock().tuned
    }

    fn starting(&self) {
        self.change(|i| {
            i.state = State::Starting;
            i.error = None;
            i.level = None;
        });
    }

    fn listening(&self, window: Window) {
        self.change(|i| {
            i.state = State::Listening;
            i.error = None;
            i.window = Some(window);
        });
    }

    /// The listening ended: with `error`, because it could not go on.
    fn stopped(&self, error: Option<String>) {
        self.change(|i| {
            i.state = if error.is_some() { State::Error } else { State::Off };
            i.error = error;
            i.window = None;
            i.level = None;
        });
    }

    fn count(&self, f: impl FnOnce(&mut Counts)) {
        f(&mut self.lock().counts);
    }

    /// The meter's reading of `of_hz`, dropped when the receiver has been tuned elsewhere
    /// since. The view's revision moves when the squelch opens or shuts, and otherwise a few
    /// times a second.
    fn metered(&self, level: Level, of_hz: u64) {
        let mut i = self.lock();
        if i.tuned.freq_hz != of_hz {
            return;
        }
        let opened = i.level.map(|l| l.open) != Some(level.open);
        i.level = Some(level);
        if opened || i.metered.elapsed().as_millis() >= METER_EVERY_MS {
            i.metered = Instant::now();
            i.rev += 1;
        }
    }

    /// A frame of audio, to whoever listens.
    fn frame(&self, samples: &[i16]) {
        self.lock().counts.frames += 1;
        let _ = self.audio.send(samples.into());
    }
}

/// The listening while the mode lasts.
struct Run {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

pub struct Analog {
    /// The mode's hold on the radio while it lasts.
    radio: tokio::sync::Mutex<Option<Hold>>,
    run: tokio::sync::Mutex<Option<Run>>,
    shared: Arc<Shared>,
    /// A raw capture's file played in the radio's place (a development host).
    replay: Option<PathBuf>,
}

impl Default for Analog {
    fn default() -> Self {
        Analog::new(Tuned::default(), None)
    }
}

/// An open `/ws/analog` connection, counted while it lasts.
pub struct Listener(Arc<Shared>);

impl Drop for Listener {
    fn drop(&mut self) {
        self.0.listeners.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Analog {
    /// The receiver as it was last tuned.
    pub fn new(tuned: Tuned, replay: Option<PathBuf>) -> Analog {
        let inner = Inner { state: State::Off, error: None, tuned, window: None, level: None, metered: Instant::now(), counts: Counts::default(), rev: 0 };
        let shared = Shared { inner: Mutex::new(inner), audio: broadcast::channel(BROADCAST).0, listeners: AtomicUsize::new(0) };
        Analog { radio: Default::default(), run: Default::default(), shared: Arc::new(shared), replay }
    }

    /// The mode begins: the radio is this service's until `leave`, and it listens where it is
    /// tuned.
    pub async fn enter<H: RadioHw + 'static>(&self, hold: Hold, tuner: Arc<Tuner<H>>, log: Arc<EventLog>) {
        *self.radio.lock().await = Some(hold);
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(listen::run(self.shared.clone(), tuner, self.replay.clone(), stop.clone(), log));
        *self.run.lock().await = Some(Run { stop, task });
    }

    /// The mode ends: the listening stops, and the hold on the radio comes back, for the next
    /// mode or the scanner.
    pub async fn leave(&self) -> Option<Hold> {
        if let Some(run) = self.run.lock().await.take() {
            run.stop.store(true, Ordering::SeqCst);
            let _ = run.task.await;
        }
        self.radio.lock().await.take()
    }

    pub fn view(&self) -> View {
        let i = self.shared.lock();
        View {
            rev: i.rev,
            state: i.state,
            error: i.error.clone(),
            tuned: i.tuned,
            window: i.window,
            level: i.level,
            audio_hz: AUDIO_HZ,
            listeners: self.shared.listeners.load(Ordering::Relaxed),
            counts: i.counts,
            bands: &BANDS,
            low_hz: *FREQ_HZ.start(),
            high_hz: *FREQ_HZ.end(),
        }
    }

    pub fn summary(&self) -> Summary {
        let i = self.shared.lock();
        Summary { state: i.state, error: i.error.clone(), freq_hz: i.tuned.freq_hz, demod: i.tuned.demod, open: i.level.map(|l| l.open) }
    }

    /// Tune the receiver: at once while it listens, and where it listens next otherwise. Wide
    /// FM takes no squelch. Refused when the frequency or the squelch is out of range.
    pub fn tune(&self, mut to: Tuned) -> Result<Tuned, String> {
        if !FREQ_HZ.contains(&to.freq_hz) {
            return Err(format!("the radio tunes {} to {} MHz", FREQ_HZ.start() / 1_000_000, FREQ_HZ.end() / 1_000_000));
        }
        if let Some(db) = to.squelch_db {
            if !db.is_finite() || !SQUELCH_DB.contains(&db) {
                return Err(format!("a squelch is {} to {} dB over the floor", SQUELCH_DB.start(), SQUELCH_DB.end()));
            }
        }
        if !to.demod.has_squelch() {
            to.squelch_db = None;
        }
        self.shared.change(|i| {
            if i.tuned.freq_hz != to.freq_hz {
                // The meter's last reading was of the frequency before.
                i.level = None;
            }
            i.tuned = to;
        });
        Ok(to)
    }

    /// The audio as it is made, and the connection's place among the listeners.
    pub fn listen(&self) -> (broadcast::Receiver<Arc<[i16]>>, Listener) {
        self.shared.listeners.fetch_add(1, Ordering::Relaxed);
        (self.shared.audio.subscribe(), Listener(self.shared.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_tuned_within_the_radio_s_range_and_wide_fm_takes_no_squelch() {
        let analog = Analog::default();
        assert_eq!(analog.view().tuned, Tuned { freq_hz: 162_550_000, demod: Demod::Nfm, squelch_db: Some(8.0) });
        let rev = analog.view().rev;
        // A broadcast station: the squelch asked for is dropped.
        let to = analog.tune(Tuned { freq_hz: 102_900_000, demod: Demod::Wfm, squelch_db: Some(10.0) }).unwrap();
        assert_eq!(to, Tuned { freq_hz: 102_900_000, demod: Demod::Wfm, squelch_db: None });
        assert!(analog.view().tuned == to && analog.view().rev > rev);
        assert_eq!((analog.summary().freq_hz, analog.summary().demod, analog.summary().open), (102_900_000, Demod::Wfm, None));
        // Out of the radio's range, or a squelch that is none: refused, and it stays where it was.
        assert!(analog.tune(Tuned { freq_hz: 50_000_000, demod: Demod::Nfm, squelch_db: None }).is_err());
        assert!(analog.tune(Tuned { freq_hz: 146_520_000, demod: Demod::Nfm, squelch_db: Some(60.0) }).is_err());
        assert!(analog.tune(Tuned { freq_hz: 146_520_000, demod: Demod::Nfm, squelch_db: Some(f32::NAN) }).is_err());
        assert_eq!(analog.view().tuned, to);
        // An aircraft channel with the squelch off.
        assert_eq!(analog.tune(Tuned { freq_hz: 133_300_000, demod: Demod::Am, squelch_db: None }).unwrap().squelch_db, None);
    }

    #[test]
    fn the_bands_are_in_order_within_the_range_each_on_its_step() {
        for band in &BANDS {
            assert!(FREQ_HZ.contains(&band.low_hz) && band.low_hz < band.high_hz && (band.high_hz - band.low_hz) % u64::from(band.step_hz) == 0, "{}", band.id);
        }
        assert!(BANDS.windows(2).all(|w| w[0].low_hz < w[1].low_hz));
        // A frame is 20 ms of the audio's rate.
        assert_eq!((AUDIO_HZ, FRAME_SAMPLES), (31_250, 625));
    }

    #[test]
    fn the_meter_moves_the_view_when_the_squelch_changes_and_a_few_times_a_second() {
        let analog = Analog::default();
        let level = |open| Level { level_dbfs: -40.0, over_db: Some(12.0), open };
        let at = analog.view().tuned.freq_hz;
        analog.shared.metered(level(false), at);
        let rev = analog.view().rev;
        // The same again at once: the reading is kept, the revision is not moved.
        analog.shared.metered(Level { level_dbfs: -41.0, ..level(false) }, at);
        assert_eq!((analog.view().rev, analog.view().level.unwrap().level_dbfs), (rev, -41.0));
        // The squelch opens: told at once.
        analog.shared.metered(level(true), at);
        assert_eq!(analog.view().rev, rev + 1);
        assert_eq!(analog.summary().open, Some(true));
        // Tuned elsewhere: the reading was the other frequency's and goes, and one of there
        // that comes late is not taken. A change of kind or squelch keeps it.
        let there = Tuned { freq_hz: at - 25_000, ..analog.view().tuned };
        analog.tune(there).unwrap();
        assert_eq!(analog.view().level, None);
        analog.shared.metered(level(true), at);
        assert_eq!(analog.view().level, None);
        analog.shared.metered(level(true), there.freq_hz);
        analog.tune(Tuned { squelch_db: Some(12.0), ..there }).unwrap();
        assert_eq!(analog.view().level, Some(level(true)));
        // A listener is counted while it lasts, and gets the frames made.
        let (mut audio, listener) = analog.listen();
        assert_eq!(analog.view().listeners, 1);
        analog.shared.frame(&[7; FRAME_SAMPLES]);
        assert_eq!((audio.try_recv().unwrap().len(), analog.view().counts.frames), (FRAME_SAMPLES, 1));
        drop(listener);
        assert_eq!(analog.view().listeners, 0);
    }
}
