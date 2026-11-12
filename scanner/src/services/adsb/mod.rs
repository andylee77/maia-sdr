//! The aircraft module: ADS-B, what aircraft send of themselves at 1090 MHz. While the mode has
//! the radio it listens there: the window is read whole from the capture ring (`listen`), a
//! thread of its own finds the Mode S messages in it (`protocol::adsb`), and the aircraft they
//! are from are kept with what each last said (`table`), in memory. Nothing is kept across a
//! restart, and nothing is plotted yet.

mod listen;
mod table;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

pub use table::Aircraft;

use crate::protocol::adsb::mode_s::Frame;
use crate::protocol::adsb::squitter;
use crate::radio::tuner::{RadioHw, Tuner};
use crate::services::events::EventLog;
use crate::services::mode::Hold;
use crate::util::time::unix_ms;

/// The seconds the rate of messages is taken over.
const RATE_OVER: usize = 10;

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
    /// The receiver's manual gain; none for a replayed file.
    pub gain_db: Option<f32>,
    /// `radio`, or `replay` on a development host.
    pub source: &'static str,
}

/// Since the scanner started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Extended squitters whose parity was right.
    pub squitters: u64,
    /// All-call replies whose parity was right.
    pub all_call: u64,
    /// Preambles found and read.
    pub tried: u64,
    /// Times an aircraft came to be listed.
    pub aircraft: u64,
    /// Times samples were lost between the radio and the search.
    pub gaps: u64,
    /// Blocks of the window dropped because the search was behind.
    pub dropped: u64,
}

/// `GET /api/v1/adsb`.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    /// Moves when anything here changes: once a second while it listens.
    pub rev: u64,
    pub state: State,
    pub error: Option<String>,
    pub window: Option<Window>,
    /// When it last began to listen.
    pub since_unix_ms: Option<u64>,
    /// Messages a second over the last ten.
    pub messages_per_s: f32,
    pub counts: Counts,
    /// The aircraft heard twice or more in the last minute, by their addresses.
    pub aircraft: Vec<Aircraft>,
}

/// The module's line in `status`.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub state: State,
    pub error: Option<String>,
    pub aircraft: usize,
    pub messages_per_s: f32,
}

#[derive(Default)]
struct Rate {
    /// The second being counted, and its messages.
    second: u64,
    in_second: u32,
    /// The whole seconds before it, the newest last.
    whole: VecDeque<u32>,
}

impl Rate {
    /// `messages` heard at `now_ms`; true when a second ended.
    fn count(&mut self, messages: u32, now_ms: u64) -> bool {
        let second = now_ms / 1000;
        let turned = second != self.second;
        if turned {
            // The seconds passed since the one counted, the silent ones too.
            let passed = second.saturating_sub(self.second).min(RATE_OVER as u64);
            self.whole.push_back(self.in_second);
            self.whole.extend((1..passed).map(|_| 0));
            while self.whole.len() > RATE_OVER {
                self.whole.pop_front();
            }
            self.second = second;
            self.in_second = 0;
        }
        self.in_second += messages;
        turned
    }

    fn per_second(&self) -> f32 {
        if self.whole.is_empty() {
            return 0.0;
        }
        let rate = self.whole.iter().sum::<u32>() as f32 / self.whole.len() as f32;
        (rate * 10.0).round() / 10.0
    }

    fn begin(&mut self, now_ms: u64) {
        *self = Rate { second: now_ms / 1000, ..Default::default() };
    }
}

struct Inner {
    state: State,
    error: Option<String>,
    window: Option<Window>,
    since_unix_ms: Option<u64>,
    counts: Counts,
    /// Preambles tried in the listenings before this one.
    tried_before: u64,
    table: table::Table,
    rate: Rate,
    rev: u64,
}

impl Inner {
    /// The aircraft silent for a minute at `now_ms` go; the view's revision moves when one that
    /// was listed did. Asked whenever the list is read, since they go whether or not the module
    /// still listens.
    fn gone(&mut self, now_ms: u64) {
        if self.table.tidy(now_ms) {
            self.rev += 1;
        }
    }
}

/// What the service, the task that reads the radio and the search's thread share.
pub(super) struct Shared {
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn starting(&self) {
        let mut i = self.lock();
        i.rev += 1;
        i.state = State::Starting;
        i.error = None;
    }

    fn listening(&self, window: Window, now_ms: u64) {
        let mut i = self.lock();
        i.rev += 1;
        i.state = State::Listening;
        i.error = None;
        i.window = Some(window);
        i.since_unix_ms = Some(now_ms);
        i.rate.begin(now_ms);
    }

    /// The listening ended: with `error`, because it could not go on. The aircraft heard stay
    /// listed until their minute is over (`Inner::gone`).
    fn stopped(&self, error: Option<String>) {
        let mut i = self.lock();
        i.rev += 1;
        i.state = if error.is_some() { State::Error } else { State::Off };
        i.error = error;
        i.window = None;
        i.tried_before = i.counts.tried;
        i.rate = Rate::default();
    }

    fn count(&self, f: impl FnOnce(&mut Counts)) {
        f(&mut self.lock().counts);
    }

    /// The messages a block of the window held, heard at `now_ms`, and the preambles tried in
    /// this listening so far. Once a second the silent aircraft go and the view's revision
    /// moves.
    fn heard(&self, frames: &[Frame], tried: u64, now_ms: u64) {
        let mut i = self.lock();
        for frame in frames {
            if frame.long {
                i.counts.squitters += 1;
            } else {
                i.counts.all_call += 1;
            }
            i.table.heard(&squitter::read(frame), frame.level_dbfs, now_ms);
        }
        i.counts.tried = i.tried_before + tried;
        i.counts.aircraft = i.table.listed();
        if i.rate.count(frames.len() as u32, now_ms) {
            i.table.tidy(now_ms);
            i.rev += 1;
        }
    }
}

/// The listening while the mode lasts.
struct Run {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

pub struct Adsb {
    /// The mode's hold on the radio while it lasts.
    radio: tokio::sync::Mutex<Option<Hold>>,
    run: tokio::sync::Mutex<Option<Run>>,
    shared: Arc<Shared>,
    /// A raw capture's file played in the radio's place (a development host).
    replay: Option<PathBuf>,
}

impl Default for Adsb {
    fn default() -> Self {
        Adsb::new(None)
    }
}

impl Adsb {
    pub fn new(replay: Option<PathBuf>) -> Adsb {
        let inner = Inner {
            state: State::Off,
            error: None,
            window: None,
            since_unix_ms: None,
            counts: Counts::default(),
            tried_before: 0,
            table: Default::default(),
            rate: Rate::default(),
            rev: 0,
        };
        Adsb { radio: Default::default(), run: Default::default(), shared: Arc::new(Shared { inner: Mutex::new(inner) }), replay }
    }

    /// The mode begins: the radio is this service's until `leave`, and it listens at 1090 MHz.
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
        self.view_at(unix_ms())
    }

    pub fn summary(&self) -> Summary {
        self.summary_at(unix_ms())
    }

    fn view_at(&self, now_ms: u64) -> View {
        let mut i = self.shared.lock();
        i.gone(now_ms);
        View {
            rev: i.rev,
            state: i.state,
            error: i.error.clone(),
            window: i.window,
            since_unix_ms: i.since_unix_ms,
            messages_per_s: i.rate.per_second(),
            counts: i.counts,
            aircraft: i.table.list(),
        }
    }

    fn summary_at(&self, now_ms: u64) -> Summary {
        let mut i = self.shared.lock();
        i.gone(now_ms);
        Summary { state: i.state, error: i.error.clone(), aircraft: i.table.count(), messages_per_s: i.rate.per_second() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::adsb::squitter::tests::frame;

    #[test]
    fn the_rate_is_the_whole_seconds_mean_over_the_last_ten() {
        let mut rate = Rate::default();
        rate.begin(100_000);
        assert!(!rate.count(30, 100_200));
        assert_eq!(rate.per_second(), 0.0);
        // The second ends: its 30 are the rate.
        assert!(rate.count(10, 101_000));
        assert_eq!(rate.per_second(), 30.0);
        // Three seconds on, two of them silent: 30, 10, 0, 0 over four seconds.
        assert!(rate.count(5, 104_100));
        assert_eq!(rate.per_second(), 10.0);
        // A long silence is ten seconds of nothing.
        assert!(rate.count(1, 200_000));
        assert_eq!((rate.per_second(), rate.whole.len()), (0.5, RATE_OVER));
    }

    #[test]
    fn messages_are_counted_by_kind_and_the_view_moves_once_a_second() {
        let adsb = Adsb::default();
        let shared = &adsb.shared;
        assert_eq!((adsb.view_at(50_000).state, adsb.summary_at(50_000).aircraft), (State::Off, 0));
        shared.starting();
        shared.listening(Window { lo_hz: 1_090_000_000, sample_rate_hz: 4_000_000, gain_db: Some(73.0), source: "radio" }, 50_000);
        let rev = adsb.view_at(50_000).rev;
        let (even, odd, all_call) = (frame("8D40621D58C382D690C8AC2863A7"), frame("8D40621D58C386435CC412692AD6"), frame("5DA1BAC2892941"));
        shared.heard(&[even.clone(), all_call.clone()], 40, 50_300);
        shared.heard(&[odd], 90, 50_600);
        let v = adsb.view_at(50_700);
        assert_eq!((v.rev, v.counts), (rev, Counts { squitters: 2, all_call: 1, tried: 90, aircraft: 1, gaps: 0, dropped: 0 }));
        assert_eq!((v.aircraft.len(), v.aircraft[0].icao.as_str(), v.aircraft[0].lat), (1, "40621D", Some(52.26578)));
        // The second turns: the view moves, and the rate is that second's.
        shared.heard(&[all_call], 120, 51_050);
        let v = adsb.view_at(51_100);
        assert_eq!((v.rev, v.messages_per_s, v.aircraft.len(), adsb.summary_at(51_100).aircraft), (rev + 1, 3.0, 2, 2));
        // It stops: the aircraft stay until a minute after each was last heard, listening or
        // not, and the view moves as they go.
        shared.stopped(None);
        let v = adsb.view_at(52_000);
        assert_eq!((v.rev, v.state, v.window, v.aircraft.len(), v.messages_per_s), (rev + 2, State::Off, None, 2, 0.0));
        let v = adsb.view_at(110_700);
        assert_eq!((v.rev, v.aircraft.iter().map(|a| a.icao.as_str()).collect::<Vec<_>>()), (rev + 3, vec!["A1BAC2"]));
        assert_eq!((adsb.summary_at(111_050).aircraft, adsb.view_at(111_050).rev), (0, rev + 4));
        // The next listening's tries are added to the one before's.
        shared.listening(Window { lo_hz: 1_090_000_000, sample_rate_hz: 4_000_000, gain_db: None, source: "replay" }, 200_000);
        shared.heard(&[], 7, 200_100);
        assert_eq!(adsb.view_at(200_100).counts.tried, 127);
        shared.heard(&[], 9, 201_000);
        assert_eq!(adsb.view_at(201_000).counts.tried, 129);
        shared.count(|c| c.gaps += 1);
        assert_eq!(adsb.view_at(201_000).counts.gaps, 1);
    }
}
