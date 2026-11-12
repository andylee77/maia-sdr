//! The aircraft module listening: the radio on one of the two links with a 4 MSPS window and a
//! fixed gain, the window read whole from the capture ring as it fills, and a thread of its own
//! that finds the link's messages in it and has them read.
//!
//! Asked for both links, it takes them in turn: the radio moves from one to the other (a fifth
//! of a second each way) and stays `ES_FOR` on Mode S and `UAT_FOR` on UAT. An aircraft sends
//! on its link every second or so and is kept a minute, so both links' aircraft stay listed;
//! what each link sends while the radio is on the other is not heard.
//!
//! When the reader falls behind or the core loses words the capture is started afresh, and the
//! gap is counted: a message on the air meanwhile is lost.
//!
//! On a development host a raw capture's file stands in for the radio, one a link
//! (`--adsb-replay`, `--uat-replay`), played at its own rate and from its start again at its
//! end and at each turn to its link.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver as Queue, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;

use super::{Link, Listen, Replay, Shared, Window};
use crate::hardware::ad9361::GainMode;
use crate::hardware::presets::find_preset;
use crate::protocol::adsb::{mode_s, uat, RATE_HZ};
use crate::protocol::ism::named_window;
use crate::radio::tuner::{RadioHw, Tuner, TuningPlan};
use crate::services::discovery::sweep::LO_SETTLE;
use crate::services::events::EventLog;
use crate::util::time::unix_ms;

/// The radio's window: the preset of the rate the readers take.
const PRESET: &str = "4M";
/// The receiver's gain, fixed: a message is too short for a gain control to follow, and of
/// the two gains tried on unit A's antenna (55 and 73 dB) the higher heard half as many Mode S
/// aircraft again. The nearest aircraft clip at it, which their messages survive.
const GAIN_DB: f64 = 73.0;
/// How long the radio stays on each link when it takes both in turn. Mode S has by far the
/// more aircraft and sends a callsign only every five seconds, so its stay is over two of
/// those; a UAT aircraft says everything it has within a few seconds.
const ES_FOR: Duration = Duration::from_secs(10);
const UAT_FOR: Duration = Duration::from_secs(5);
/// How often the capture ring is read: a sub-buffer takes 66 ms to fill at 4 MSPS.
const POLL: Duration = Duration::from_millis(20);
/// Blocks of the window waiting for the thread: a second of it.
const QUEUE: usize = 16;
/// A replayed file is read this much at a time.
const REPLAY_BLOCK: usize = 1 << 20;

/// What the thread is given.
enum Piece {
    /// More of the window, as the capture ring holds it.
    Block(Vec<u8>),
    /// Samples were lost before the next block.
    Afresh,
    /// The radio moved: what follows is of this link.
    Tuned(Link),
}

/// The thread's end of the listening, held by the task that feeds it.
struct Feed {
    tx: Option<SyncSender<Piece>>,
    thread: Option<std::thread::JoinHandle<()>>,
    shared: Arc<Shared>,
    /// The link the radio moved to, until the thread has been told.
    moved: Option<Link>,
    /// A block was dropped, or samples were lost: the next one comes after a gap.
    behind: bool,
}

impl Feed {
    fn start(shared: Arc<Shared>) -> Result<Feed> {
        let (tx, rx) = sync_channel(QUEUE);
        let state = shared.clone();
        let thread = std::thread::Builder::new().name("adsb-listen".into()).spawn(move || listen(rx, state)).context("the search's thread")?;
        Ok(Feed { tx: Some(tx), thread: Some(thread), shared, moved: None, behind: false })
    }

    fn block(&mut self, bytes: Vec<u8>) {
        let Some(tx) = &self.tx else { return };
        // The thread is told of the radio's move, or of a gap, before what follows it.
        if let Some(link) = self.moved {
            if tx.try_send(Piece::Tuned(link)).is_err() {
                return;
            }
            (self.moved, self.behind) = (None, false);
        }
        if self.behind && tx.try_send(Piece::Afresh).is_err() {
            return;
        }
        self.behind = matches!(tx.try_send(Piece::Block(bytes)), Err(TrySendError::Full(_)));
        if self.behind {
            self.shared.count(|c| c.dropped += 1);
        }
    }

    fn gap(&mut self) {
        self.behind = true;
        self.shared.count(|c| c.gaps += 1);
    }

    fn tuned(&mut self, link: Link) {
        self.moved = Some(link);
    }

    /// End the thread once it has taken what is queued.
    async fn end(mut self) {
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

/// The link to be on: the one asked for, or of both the one whose turn it is.
fn turn(asked: Listen, on: Option<(Link, Instant)>) -> Link {
    match (asked, on) {
        (Listen::Es, _) | (Listen::Both, None) => Link::Es,
        (Listen::Uat, _) => Link::Uat,
        (Listen::Both, Some((Link::Es, since))) => if since.elapsed() >= ES_FOR { Link::Uat } else { Link::Es },
        (Listen::Both, Some((Link::Uat, since))) => if since.elapsed() >= UAT_FOR { Link::Es } else { Link::Uat },
    }
}

/// Listen until `stop`: on the radio, or on `replay`'s files in its place.
pub(super) async fn run<H: RadioHw + 'static>(shared: Arc<Shared>, tuner: Arc<Tuner<H>>, replay: Replay, stop: Arc<AtomicBool>, log: Arc<EventLog>) {
    shared.starting();
    let result = if replay.es.is_some() || replay.uat.is_some() { replayed(&shared, &replay, &stop, &log).await } else { radio(&shared, &tuner, &stop, &log).await };
    match result {
        Ok(()) => {
            log.radio("adsb", "the aircraft module stopped listening".to_string());
            shared.stopped(None);
        }
        Err(e) => {
            log.radio("adsb", format!("the aircraft module is not listening: {e:#}"));
            shared.stopped(Some(format!("{e:#}")));
        }
    }
}

async fn radio<H: RadioHw>(shared: &Arc<Shared>, tuner: &Tuner<H>, stop: &AtomicBool, log: &EventLog) -> Result<()> {
    let preset = find_preset(PRESET).context("the window's preset")?;
    anyhow::ensure!(preset.sample_rate_hz == RATE_HZ, "the {PRESET} preset is not at the rate the readers take");
    tuner.set_gain(GainMode::Manual, Some(GAIN_DB)).await?;
    let hw = tuner.hw();
    let mut feed = Feed::start(shared.clone())?;
    log.radio("adsb", format!("the aircraft module listens at {} ({GAIN_DB} dB of gain)", shared.listen().says()));
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut on: Option<(Link, Instant)> = None;
    while !stop.load(Ordering::SeqCst) {
        let link = turn(shared.listen(), on);
        if on.map(|(l, _)| l) != Some(link) {
            // The radio moves to the link, and the capture begins afresh there.
            if on.is_some() {
                hw.capture_end().await;
            }
            tuner.apply(TuningPlan { preset, lo_hz: link.lo_hz(), control_hz: link.lo_hz() }).await?;
            tokio::time::sleep(LO_SETTLE).await;
            hw.capture_begin().await?;
            feed.tuned(link);
            on = Some((link, Instant::now()));
            shared.tuned(link, Window { lo_hz: link.lo_hz(), sample_rate_hz: RATE_HZ, gain_db: Some(GAIN_DB as f32), source: "radio" }, unix_ms());
        }
        tick.tick().await;
        let mut blocks = Vec::new();
        let read = hw.capture_read(&mut |_, bytes| blocks.push(bytes.to_vec())).await;
        for bytes in blocks {
            feed.block(bytes);
        }
        if read.lapped > 0 || read.overflow {
            feed.gap();
            hw.capture_begin().await?;
        }
    }
    hw.capture_end().await;
    feed.end().await;
    Ok(())
}

/// A link's capture, opened at its start: its name gives its LO and its rate, which is to be
/// the readers'.
async fn capture(replay: &Replay, link: Link) -> Result<(tokio::fs::File, u64, String)> {
    let path: &Path = match link {
        Link::Es => replay.es.as_deref(),
        Link::Uat => replay.uat.as_deref(),
    }
    .with_context(|| format!("no capture is given to replay for {} MHz", link.mhz()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    let (lo_hz, rate) = named_window(&name).with_context(|| format!("{name}: a replayed capture's name gives its LO and rate, as adsb_1090M_4000k.cs16"))?;
    anyhow::ensure!(rate == RATE_HZ, "{name} is at {rate} samples a second: the readers take {RATE_HZ}");
    anyhow::ensure!(lo_hz == link.lo_hz(), "{name} has its LO at {lo_hz} Hz: {} MHz is read with it at {}", link.mhz(), link.lo_hz());
    let file = tokio::fs::File::open(path).await.with_context(|| format!("{}", path.display()))?;
    Ok((file, lo_hz, name))
}

async fn replayed(shared: &Arc<Shared>, replay: &Replay, stop: &AtomicBool, log: &EventLog) -> Result<()> {
    let mut feed = Feed::start(shared.clone())?;
    log.radio("adsb", format!("the aircraft module listens at {}, replayed from captures", shared.listen().says()));
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut on: Option<(Link, Instant)> = None;
    let mut playing: Option<(tokio::fs::File, String)> = None;
    let mut sent: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        let link = turn(shared.listen(), on);
        if on.map(|(l, _)| l) != Some(link) {
            let (file, lo_hz, name) = capture(replay, link).await?;
            playing = Some((file, name));
            sent = 0;
            feed.tuned(link);
            on = Some((link, Instant::now()));
            shared.tuned(link, Window { lo_hz, sample_rate_hz: RATE_HZ, gain_db: None, source: "replay" }, unix_ms());
        }
        tick.tick().await;
        let (Some((_, since)), Some((file, name))) = (on, playing.as_mut()) else { continue };
        let due = (since.elapsed().as_secs_f64() * f64::from(RATE_HZ)) as u64;
        while sent + (REPLAY_BLOCK / 4) as u64 <= due {
            let mut bytes = vec![0u8; REPLAY_BLOCK];
            let mut filled = 0;
            while filled < REPLAY_BLOCK {
                match file.read(&mut bytes[filled..]).await? {
                    0 => break,
                    n => filled += n,
                }
            }
            if filled < REPLAY_BLOCK {
                // The file's end (its last part block is left out): from its start again,
                // which is a break in the samples.
                anyhow::ensure!(sent > 0, "{name} holds less than a megabyte");
                *file = tokio::fs::File::open(match link {
                    Link::Es => replay.es.as_deref(),
                    Link::Uat => replay.uat.as_deref(),
                }
                .context("the capture")?)
                .await?;
                feed.gap();
                // Its length is counted as played, so the pace holds.
                sent += (REPLAY_BLOCK / 4) as u64;
                continue;
            }
            feed.block(bytes);
            sent += (REPLAY_BLOCK / 4) as u64;
        }
    }
    feed.end().await;
    Ok(())
}

/// The thread: blocks of the window in, the messages they hold to the aircraft's table.
fn listen(rx: Queue<Piece>, shared: Arc<Shared>) {
    let (mut es, mut uat) = (mode_s::Demod::default(), uat::Demod::default());
    let (mut squitters, mut said) = (Vec::new(), Vec::new());
    let mut on = Link::Es;
    while let Ok(piece) = rx.recv() {
        match (piece, on) {
            (Piece::Block(bytes), Link::Es) => {
                squitters.clear();
                es.push(&bytes, &mut squitters);
                shared.heard(&squitters, es.tried(), unix_ms());
            }
            (Piece::Block(bytes), Link::Uat) => {
                said.clear();
                uat.push(&bytes, &mut said);
                shared.heard_uat(&said, uat.fitted(), unix_ms());
            }
            (Piece::Afresh, Link::Es) => es.afresh(),
            (Piece::Afresh, Link::Uat) => uat.afresh(),
            (Piece::Tuned(link), _) => {
                // What either reader held was of before the radio moved.
                es.afresh();
                uat.afresh();
                on = link;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Adsb, State};
    use super::*;
    use crate::protocol::adsb::mode_s::tests::window;
    use crate::protocol::adsb::uat::message::tests::{LONG_CALLSIGN, SHORT_HEARD};
    use crate::protocol::adsb::uat::tests::window as uat_window;

    #[test]
    fn the_thread_reads_the_window_it_is_given_into_the_aircraft_s_table() {
        let adsb = Adsb::default();
        let mut feed = Feed::start(adsb.shared.clone()).unwrap();
        // The window given to the thread whole, however slow it is: the thread takes a piece
        // only once it is done with the one before, so when one more empty block than the
        // queue holds has gone in, it is done with the window.
        let tx = feed.tx.clone().unwrap();
        let given = |bytes: Vec<u8>| {
            for block in bytes.chunks(40_000) {
                tx.send(Piece::Block(block.to_vec())).unwrap();
            }
            for _ in 0..=QUEUE {
                tx.send(Piece::Block(Vec::new())).unwrap();
            }
        };
        // An aircraft's two halves of a place and another's all-call reply, twice each.
        let (even, odd, all_call) = ("8D40621D58C382D690C8AC2863A7", "8D40621D58C386435CC412692AD6", "5DA1BAC2892941");
        given(window(60_000, &[(2_000, even, 300), (9_990, all_call, 80), (20_000, odd, 300), (41_000, all_call, 80)]));
        let v = adsb.view();
        assert_eq!((v.counts.squitters, v.counts.all_call, v.counts.aircraft, v.counts.dropped), (2, 2, 2, 0));
        assert!(v.counts.tried >= 4);
        assert_eq!(v.aircraft.iter().map(|a| (a.icao.as_str(), a.messages, a.altitude_ft)).collect::<Vec<_>>(), [("40621D", 2, Some(38_000)), ("A1BAC2", 2, None)]);
        // Its pulses were 300 against a full scale of 2048.
        assert!(v.aircraft[0].lat.is_some() && (v.aircraft[0].level_dbfs + 16.7).abs() < 0.5, "{:?}", v.aircraft[0]);
        // A gap is counted, and the search goes on after it.
        feed.gap();
        feed.block(window(4_000, &[(100, even, 300)]));
        given(Vec::new());
        assert_eq!((adsb.view().counts.gaps, adsb.view().counts.squitters, adsb.view().state), (1, 3, State::Off));
        // The radio moves to the other link: what follows is read as that link's, and a Mode S
        // message in it is nothing.
        feed.tuned(Link::Uat);
        feed.block(uat_window(20_000, &[(1_000, SHORT_HEARD, 300.0), (6_000, LONG_CALLSIGN, 300.0)]));
        given(window(8_000, &[(500, even, 300)]));
        let v = adsb.view();
        assert_eq!((v.counts.uat, v.counts.mended, v.counts.squitters, v.counts.aircraft), (2, 0, 3, 3));
        assert!(v.counts.fitted >= 2);
        let small = v.aircraft.iter().find(|a| a.icao == "A82090").unwrap();
        assert_eq!((small.link, small.callsign.as_deref(), small.altitude_ft), (Link::Uat, Some("N6224A"), Some(4_400)));
        // And back.
        feed.tuned(Link::Es);
        feed.block(window(4_000, &[(100, odd, 300)]));
        given(Vec::new());
        assert_eq!(adsb.view().counts.squitters, 4);
        drop(tx);
        drop(feed.tx.take());
        feed.thread.take().unwrap().join().unwrap();
    }

    #[test]
    fn of_both_links_each_has_its_turn() {
        let long_ago = |d: Duration| Instant::now() - d - Duration::from_millis(50);
        assert_eq!((turn(Listen::Es, None), turn(Listen::Uat, None), turn(Listen::Both, None)), (Link::Es, Link::Uat, Link::Es));
        // One link asked for is the link to be on, whatever it is on.
        assert_eq!((turn(Listen::Uat, Some((Link::Es, Instant::now()))), turn(Listen::Es, Some((Link::Uat, Instant::now())))), (Link::Uat, Link::Es));
        // Of both: Mode S for its stay, then UAT for its, then Mode S again.
        assert_eq!(turn(Listen::Both, Some((Link::Es, Instant::now()))), Link::Es);
        assert_eq!(turn(Listen::Both, Some((Link::Es, long_ago(ES_FOR)))), Link::Uat);
        assert_eq!(turn(Listen::Both, Some((Link::Uat, Instant::now()))), Link::Uat);
        assert_eq!(turn(Listen::Both, Some((Link::Uat, long_ago(UAT_FOR)))), Link::Es);
        assert!(ES_FOR > UAT_FOR);
    }

    #[test]
    fn the_window_s_preset_is_at_the_rate_the_readers_take() {
        assert_eq!(find_preset(PRESET).unwrap().sample_rate_hz, RATE_HZ);
    }
}
