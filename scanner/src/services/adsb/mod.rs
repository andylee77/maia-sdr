//! The aircraft module: ADS-B, what aircraft send of themselves, on its two links: Mode S at
//! 1090 MHz and UAT at 978 MHz. While the mode has the radio it listens to one of them, or to
//! each in turn (`Listen`; the radio hears one at a time): the window is read whole from the
//! capture ring (`listen`), a thread of its own finds the messages in it (`protocol::adsb`),
//! and the aircraft they are from are kept with what each last said (`table`), in memory.
//! What it listens to is kept across restarts (the radio's state); nothing else is. Once the
//! radio has been told where it is (a setting), each aircraft that has said its place has its
//! distance and bearing from there (`range`).

mod listen;
mod range;
mod table;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

pub use table::Aircraft;

use crate::protocol::adsb::uat::message;
use crate::protocol::adsb::{mode_s, squitter, uat};
use crate::radio::tuner::{RadioHw, Tuner};
use crate::services::config::radio::Position;
use crate::services::events::EventLog;
use crate::services::mode::Hold;
use crate::util::time::unix_ms;

/// The seconds the rate of messages is taken over.
const RATE_OVER: usize = 10;

/// One of the two links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Link {
    /// Mode S extended squitters, 1090 MHz.
    #[serde(rename = "1090")]
    Es,
    /// UAT, 978 MHz.
    #[serde(rename = "978")]
    Uat,
}

impl Link {
    /// The link's frequency, MHz.
    pub fn mhz(self) -> u64 {
        match self {
            Link::Es => crate::protocol::adsb::FREQ_HZ / 1_000_000,
            Link::Uat => uat::FREQ_HZ / 1_000_000,
        }
    }

    /// Where the radio's LO is for it: on Mode S, and a megahertz above UAT.
    fn lo_hz(self) -> u64 {
        match self {
            Link::Es => crate::protocol::adsb::FREQ_HZ,
            Link::Uat => uat::LO_HZ,
        }
    }
}

/// What the module listens to while it has the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Listen {
    #[serde(rename = "1090")]
    Es,
    #[serde(rename = "978")]
    Uat,
    /// Each in turn: an aircraft is kept a minute, and sends on its link every second or so,
    /// so both links' aircraft stay listed.
    #[default]
    #[serde(rename = "both")]
    Both,
}

impl Listen {
    fn says(self) -> &'static str {
        match self {
            Listen::Es => "1090 MHz",
            Listen::Uat => "978 MHz",
            Listen::Both => "1090 and 978 MHz in turn",
        }
    }
}

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
    /// Mode S extended squitters whose parity was right.
    pub squitters: u64,
    /// Mode S all-call replies whose parity was right.
    pub all_call: u64,
    /// Mode S preambles found and read.
    pub tried: u64,
    /// UAT messages from aircraft that their code could put right.
    pub uat: u64,
    /// Of them, those that had wrong bytes to put right.
    pub mended: u64,
    /// Places UAT's synchronisation fitted.
    pub fitted: u64,
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
    /// What it is asked to listen to: one link, or both in turn.
    pub link: Listen,
    /// The link it listens to now; none while it does not listen.
    pub on: Option<Link>,
    pub window: Option<Window>,
    /// When it last began to listen.
    pub since_unix_ms: Option<u64>,
    /// Messages a second over the last ten, of whichever link was listened to.
    pub messages_per_s: f32,
    pub counts: Counts,
    /// Where the radio is, once it has been told (the radio's settings): what each aircraft's
    /// distance and bearing are from.
    pub position: Option<Position>,
    /// The aircraft heard twice or more in the last minute, by their addresses.
    pub aircraft: Vec<Aircraft>,
}

/// The module's line in `status`.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub state: State,
    pub error: Option<String>,
    pub link: Listen,
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
    listen: Listen,
    /// Where the radio is, once it has been told.
    position: Option<Position>,
    on: Option<Link>,
    window: Option<Window>,
    since_unix_ms: Option<u64>,
    counts: Counts,
    /// Preambles tried and places fitted in the listenings before this one.
    tried_before: u64,
    fitted_before: u64,
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

    /// `messages` more at `now_ms`: once a second the silent aircraft go and the view's
    /// revision moves.
    fn counted(&mut self, messages: usize, now_ms: u64) {
        self.counts.aircraft = self.table.listed();
        if self.rate.count(messages as u32, now_ms) {
            self.table.tidy(now_ms);
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

    fn listen(&self) -> Listen {
        self.lock().listen
    }

    fn starting(&self) {
        let mut i = self.lock();
        i.rev += 1;
        i.state = State::Starting;
        i.error = None;
    }

    /// The radio is on `link` and its window read: at the first of a listening, that it began.
    fn tuned(&self, link: Link, window: Window, now_ms: u64) {
        let mut i = self.lock();
        i.rev += 1;
        if i.state != State::Listening {
            i.state = State::Listening;
            i.error = None;
            i.since_unix_ms = Some(now_ms);
            i.rate.begin(now_ms);
        }
        i.on = Some(link);
        i.window = Some(window);
    }

    /// The listening ended: with `error`, because it could not go on. The aircraft heard stay
    /// listed until their minute is over (`Inner::gone`).
    fn stopped(&self, error: Option<String>) {
        let mut i = self.lock();
        i.rev += 1;
        i.state = if error.is_some() { State::Error } else { State::Off };
        i.error = error;
        i.on = None;
        i.window = None;
        i.tried_before = i.counts.tried;
        i.fitted_before = i.counts.fitted;
        i.rate = Rate::default();
    }

    fn count(&self, f: impl FnOnce(&mut Counts)) {
        f(&mut self.lock().counts);
    }

    /// The Mode S messages a block of the window held, heard at `now_ms`, and the preambles
    /// tried in this listening so far.
    fn heard(&self, frames: &[mode_s::Frame], tried: u64, now_ms: u64) {
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
        i.counted(frames.len(), now_ms);
    }

    /// The UAT messages a block of the window held, heard at `now_ms`, and the places its
    /// synchronisation fitted in this listening so far. A message a ground station sent of
    /// other traffic is counted and says nothing here.
    fn heard_uat(&self, frames: &[uat::Frame], fitted: u64, now_ms: u64) {
        let mut i = self.lock();
        for frame in frames {
            i.counts.uat += 1;
            i.counts.mended += u64::from(frame.mended > 0);
            if let Some(said) = message::read(frame.bytes()) {
                i.table.said(&said, frame.level_dbfs, now_ms);
            }
        }
        i.counts.fitted = i.fitted_before + fitted;
        i.counted(frames.len(), now_ms);
    }
}

/// The listening while the mode lasts.
struct Run {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

/// The captures played in the radio's place on a development host, one a link.
#[derive(Debug, Clone, Default)]
pub struct Replay {
    pub es: Option<PathBuf>,
    pub uat: Option<PathBuf>,
}

pub struct Adsb {
    /// The mode's hold on the radio while it lasts.
    radio: tokio::sync::Mutex<Option<Hold>>,
    run: tokio::sync::Mutex<Option<Run>>,
    shared: Arc<Shared>,
    replay: Replay,
}

impl Default for Adsb {
    fn default() -> Self {
        Adsb::new(Listen::default(), None, Replay::default())
    }
}

impl Adsb {
    /// The module as it was last asked to listen, with the radio's place if it has one.
    pub fn new(listen: Listen, position: Option<Position>, replay: Replay) -> Adsb {
        let inner = Inner {
            state: State::Off,
            error: None,
            listen,
            position,
            on: None,
            window: None,
            since_unix_ms: None,
            counts: Counts::default(),
            tried_before: 0,
            fitted_before: 0,
            table: Default::default(),
            rate: Rate::default(),
            rev: 0,
        };
        Adsb { radio: Default::default(), run: Default::default(), shared: Arc::new(Shared { inner: Mutex::new(inner) }), replay }
    }

    /// The mode begins: the radio is this service's until `leave`, and it listens.
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

    /// Listen to one link, or to both in turn: at once while it listens, and when it next does
    /// otherwise.
    pub fn set_link(&self, to: Listen) {
        let mut i = self.shared.lock();
        if i.listen != to {
            i.listen = to;
            i.rev += 1;
        }
    }

    /// Where the radio is, or that it is not known any more: the distances follow at once.
    pub fn set_position(&self, to: Option<Position>) {
        let mut i = self.shared.lock();
        if i.position != to {
            i.position = to;
            i.rev += 1;
        }
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
            link: i.listen,
            on: i.on,
            window: i.window,
            since_unix_ms: i.since_unix_ms,
            messages_per_s: i.rate.per_second(),
            counts: i.counts,
            position: i.position,
            aircraft: i.table.list(i.position),
        }
    }

    fn summary_at(&self, now_ms: u64) -> Summary {
        let mut i = self.shared.lock();
        i.gone(now_ms);
        Summary { state: i.state, error: i.error.clone(), link: i.listen, aircraft: i.table.count(), messages_per_s: i.rate.per_second() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::adsb::squitter::tests::frame;
    use crate::protocol::adsb::uat::message::tests::{bytes, LONG_CALLSIGN, SHORT_HEARD};

    const WINDOW: Window = Window { lo_hz: 1_090_000_000, sample_rate_hz: 4_000_000, gain_db: Some(73.0), source: "radio" };

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
        assert_eq!((adsb.view_at(50_000).state, adsb.summary_at(50_000).aircraft, adsb.view_at(50_000).link), (State::Off, 0, Listen::Both));
        shared.starting();
        shared.tuned(Link::Es, WINDOW, 50_000);
        let rev = adsb.view_at(50_000).rev;
        let (even, odd, all_call) = (frame("8D40621D58C382D690C8AC2863A7"), frame("8D40621D58C386435CC412692AD6"), frame("5DA1BAC2892941"));
        shared.heard(&[even.clone(), all_call.clone()], 40, 50_300);
        shared.heard(&[odd], 90, 50_600);
        let v = adsb.view_at(50_700);
        assert_eq!((v.rev, v.counts), (rev, Counts { squitters: 2, all_call: 1, tried: 90, aircraft: 1, ..Default::default() }));
        assert_eq!((v.aircraft.len(), v.aircraft[0].icao.as_str(), v.aircraft[0].lat, v.on, v.state), (1, "40621D", Some(52.26578), Some(Link::Es), State::Listening));
        // The second turns: the view moves, and the rate is that second's.
        shared.heard(&[all_call], 120, 51_050);
        let v = adsb.view_at(51_100);
        assert_eq!((v.rev, v.messages_per_s, v.aircraft.len(), adsb.summary_at(51_100).aircraft), (rev + 1, 3.0, 2, 2));
        // It stops: the aircraft stay until a minute after each was last heard, listening or
        // not, and the view moves as they go.
        shared.stopped(None);
        let v = adsb.view_at(52_000);
        assert_eq!((v.rev, v.state, v.on, v.window, v.aircraft.len(), v.messages_per_s), (rev + 2, State::Off, None, None, 2, 0.0));
        let v = adsb.view_at(110_700);
        assert_eq!((v.rev, v.aircraft.iter().map(|a| a.icao.as_str()).collect::<Vec<_>>()), (rev + 3, vec!["A1BAC2"]));
        assert_eq!((adsb.summary_at(111_050).aircraft, adsb.view_at(111_050).rev), (0, rev + 4));
        // The next listening's tries are added to the one before's.
        shared.tuned(Link::Es, WINDOW, 200_000);
        shared.heard(&[], 7, 200_100);
        assert_eq!(adsb.view_at(200_100).counts.tried, 127);
        shared.heard(&[], 9, 201_000);
        assert_eq!(adsb.view_at(201_000).counts.tried, 129);
        shared.count(|c| c.gaps += 1);
        assert_eq!(adsb.view_at(201_000).counts.gaps, 1);
    }

    #[test]
    fn the_other_link_s_messages_are_counted_and_its_aircraft_listed_with_the_first_s() {
        let adsb = Adsb::new(Listen::Uat, None, Replay::default());
        let shared = &adsb.shared;
        shared.tuned(Link::Uat, Window { lo_hz: 979_000_000, ..WINDOW }, 50_000);
        let since = adsb.view_at(50_000).since_unix_ms;
        let message = |hex: &str, mended: u8| {
            let mut payload = [0u8; 34];
            let b = bytes(hex);
            payload[..b.len()].copy_from_slice(&b);
            uat::Frame { at: 0, payload, long: b.len() == 34, mended, level_dbfs: -30.0 }
        };
        // A short message that was received whole, a long one with three bytes put right, and
        // one a ground station sent of other traffic, which says nothing here.
        let mut relayed = message(SHORT_HEARD, 0);
        relayed.payload[0] |= 2;
        shared.heard_uat(&[message(SHORT_HEARD, 0), message(LONG_CALLSIGN, 3), relayed], 12, 50_400);
        let v = adsb.view_at(50_500);
        assert_eq!(v.counts, Counts { uat: 3, mended: 1, fitted: 12, aircraft: 1, ..Default::default() });
        assert_eq!((v.link, v.on, v.aircraft.len()), (Listen::Uat, Some(Link::Uat), 1));
        assert_eq!((v.aircraft[0].icao.as_str(), v.aircraft[0].link, v.aircraft[0].callsign.as_deref()), ("A82090", Link::Uat, Some("N6224A")));
        // The radio turns to the other link: the listening goes on, and both links' aircraft
        // are listed.
        shared.tuned(Link::Es, WINDOW, 55_000);
        let all_call = frame("5DA1BAC2892941");
        shared.heard(&[all_call.clone(), all_call], 5, 55_200);
        let v = adsb.view_at(55_300);
        assert_eq!((v.on, v.since_unix_ms, v.state), (Some(Link::Es), since, State::Listening));
        assert_eq!(v.aircraft.iter().map(|a| (a.icao.as_str(), a.link)).collect::<Vec<_>>(), [("A1BAC2", Link::Es), ("A82090", Link::Uat)]);
        // Asked for one link, or the same again.
        let rev = v.rev;
        adsb.set_link(Listen::Es);
        adsb.set_link(Listen::Es);
        assert_eq!((adsb.view_at(55_300).link, adsb.view_at(55_300).rev, adsb.summary_at(55_300).link), (Listen::Es, rev + 1, Listen::Es));
        // The radio is told where it is: the aircraft that said its place is that far from it,
        // at once, and the other has no distance. Told the same again, nothing moves; forgotten,
        // the distances go.
        assert_eq!((v.position, v.aircraft[1].distance_nm), (None, None));
        let radio = Position { lat: 29.31514, lon: -81.81991 };
        adsb.set_position(Some(radio));
        adsb.set_position(Some(radio));
        let v = adsb.view_at(55_300);
        assert_eq!((v.rev, v.position), (rev + 2, Some(radio)));
        assert_eq!(v.aircraft.iter().map(|a| (a.distance_nm, a.bearing_deg)).collect::<Vec<_>>(), [(None, None), (Some(60.0), Some(0.0))]);
        adsb.set_position(None);
        let v = adsb.view_at(55_300);
        assert_eq!((v.rev, v.position, v.aircraft[1].distance_nm), (rev + 3, None, None));
        assert_eq!(serde_json::to_string(&(Link::Es, Link::Uat, Listen::Both)).unwrap(), r#"["1090","978","both"]"#);
    }
}
