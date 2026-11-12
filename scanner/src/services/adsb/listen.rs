//! The aircraft module listening: the radio at 1090 MHz with a 4 MSPS window and a fixed gain,
//! the window read whole from the capture ring as it fills, and a thread of its own that finds
//! the messages in it and has them read.
//!
//! When the reader falls behind or the core loses words the capture is started afresh, and the
//! gap is counted: a message on the air meanwhile is lost.
//!
//! On a development host a raw capture's file stands in for the radio (`--adsb-replay`), played
//! at its own rate and from its start again at its end.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver as Queue, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::{Shared, Window};
use crate::hardware::ad9361::GainMode;
use crate::hardware::presets::find_preset;
use crate::protocol::adsb::mode_s::Demod;
use crate::protocol::adsb::{FREQ_HZ, RATE_HZ};
use crate::protocol::ism::named_window;
use crate::radio::tuner::{RadioHw, Tuner, TuningPlan};
use crate::services::discovery::sweep::LO_SETTLE;
use crate::services::events::EventLog;
use crate::util::time::unix_ms;

/// The radio's window: the preset of the rate the search reads.
const PRESET: &str = "4M";
/// The receiver's gain, fixed: the pulses are too short for a gain control to follow, and of
/// the two gains tried on unit A's antenna (55 and 73 dB) the higher heard half as many
/// aircraft again. The nearest aircraft clip at it, which their pulses' places survive.
pub(super) const GAIN_DB: f64 = 73.0;
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
}

/// The thread's end of the listening, held by the task that feeds it.
struct Feed {
    tx: Option<SyncSender<Piece>>,
    thread: Option<std::thread::JoinHandle<()>>,
    shared: Arc<Shared>,
    /// A block was dropped, or samples were lost: the next one comes after a gap.
    behind: bool,
}

impl Feed {
    fn start(shared: Arc<Shared>) -> Result<Feed> {
        let (tx, rx) = sync_channel(QUEUE);
        let state = shared.clone();
        let thread = std::thread::Builder::new().name("adsb-listen".into()).spawn(move || listen(rx, state)).context("the search's thread")?;
        Ok(Feed { tx: Some(tx), thread: Some(thread), shared, behind: false })
    }

    fn block(&mut self, bytes: Vec<u8>) {
        let Some(tx) = &self.tx else { return };
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

    /// End the thread once it has taken what is queued.
    async fn end(mut self) {
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

/// Listen until `stop`: on the radio, or on `replay`'s file in its place.
pub(super) async fn run<H: RadioHw + 'static>(shared: Arc<Shared>, tuner: Arc<Tuner<H>>, replay: Option<PathBuf>, stop: Arc<AtomicBool>, log: Arc<EventLog>) {
    shared.starting();
    let result = match &replay {
        Some(path) => replayed(&shared, path, &stop, &log).await,
        None => radio(&shared, &tuner, &stop, &log).await,
    };
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
    anyhow::ensure!(preset.sample_rate_hz == RATE_HZ, "the {PRESET} preset is not at the rate the search reads");
    tuner.set_gain(GainMode::Manual, Some(GAIN_DB)).await?;
    tuner.apply(TuningPlan { preset, lo_hz: FREQ_HZ, control_hz: FREQ_HZ }).await?;
    tokio::time::sleep(LO_SETTLE).await;
    let hw = tuner.hw();
    hw.capture_begin().await?;
    let mut feed = Feed::start(shared.clone())?;
    log.radio("adsb", format!("the aircraft module listens at {} MHz ({GAIN_DB} dB of gain)", FREQ_HZ / 1_000_000));
    shared.listening(Window { lo_hz: FREQ_HZ, sample_rate_hz: RATE_HZ, gain_db: Some(GAIN_DB as f32), source: "radio" }, unix_ms());
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !stop.load(Ordering::SeqCst) {
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

async fn replayed(shared: &Arc<Shared>, path: &Path, stop: &AtomicBool, log: &EventLog) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    let (lo_hz, rate) = named_window(&name).with_context(|| format!("{name}: a replayed capture's name gives its LO and rate, as adsb_1090M_4000k.cs16"))?;
    anyhow::ensure!(rate == RATE_HZ, "{name} is at {rate} samples a second: the search reads {RATE_HZ}");
    let mut file = tokio::fs::File::open(path).await.with_context(|| format!("{}", path.display()))?;
    let mut feed = Feed::start(shared.clone())?;
    log.radio("adsb", format!("the aircraft module listens at {} MHz, replayed from {name}", lo_hz / 1_000_000));
    shared.listening(Window { lo_hz, sample_rate_hz: rate, gain_db: None, source: "replay" }, unix_ms());
    let started = Instant::now();
    let mut sent: u64 = 0;
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !stop.load(Ordering::SeqCst) {
        tick.tick().await;
        let due = (started.elapsed().as_secs_f64() * f64::from(rate)) as u64;
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
                file.seek(std::io::SeekFrom::Start(0)).await?;
                feed.gap();
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
    let mut demod = Demod::default();
    let mut frames = Vec::new();
    while let Ok(piece) = rx.recv() {
        match piece {
            Piece::Block(bytes) => {
                frames.clear();
                demod.push(&bytes, &mut frames);
                shared.heard(&frames, demod.tried(), unix_ms());
            }
            Piece::Afresh => demod.afresh(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Adsb, State};
    use super::*;
    use crate::protocol::adsb::mode_s::tests::window;

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
        drop(tx);
        drop(feed.tx.take());
        feed.thread.take().unwrap().join().unwrap();
    }

    #[test]
    fn the_window_s_preset_is_at_the_rate_the_search_reads() {
        assert_eq!(find_preset(PRESET).unwrap().sample_rate_hz, RATE_HZ);
    }
}
