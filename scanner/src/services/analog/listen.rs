//! The analog module listening: the radio parked half a megahertz above the frequency it is
//! tuned to (so the LO's own spur is outside the channel), its window read whole from the
//! capture ring as it fills, and a thread of its own that cuts the channel at the frequency and
//! has the receiver make audio of it. A change of frequency moves the radio and starts the
//! capture afresh; a change of kind or of squelch is the thread's alone.
//!
//! When the reader falls behind or the core loses words the capture is started afresh, and the
//! gap is counted: the audio breaks there.
//!
//! On a development host a raw capture's file stands in for the radio (`--analog-replay`),
//! played at its own rate and from its start again at its end: only what its window holds can
//! be tuned.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver as Queue, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::{Shared, Tuned, Window, FRAME_SAMPLES};
use crate::hardware::ad9361::GainMode;
use crate::hardware::presets::find_preset;
use crate::protocol::analog::Receiver;
use crate::protocol::ism::channel::Channel;
use crate::protocol::ism::named_window;
use crate::radio::tuner::{RadioHw, Tuner, TuningPlan};
use crate::services::discovery::sweep::LO_SETTLE;
use crate::services::events::EventLog;

/// The radio's window: the narrowest preset the AD9361 takes that divides down to the channel's
/// rate, its LO this far above the frequency listened to.
const PRESET: &str = "3M";
const LO_ABOVE_HZ: u64 = 500_000;
/// How often the capture ring is read: a sub-buffer takes 87 ms to fill at 3 MSPS.
const POLL: Duration = Duration::from_millis(20);
/// Blocks of the window waiting for the thread: 1.4 s of it.
const QUEUE: usize = 16;
/// A replayed file is read this much at a time.
const REPLAY_BLOCK: usize = 1 << 20;

/// What the thread is given.
enum Piece {
    /// More of the window, as the capture ring holds it.
    Block(Vec<u8>),
    /// Samples were lost before the next block, or the radio moved: the channel and the
    /// receiver begin again, the channel this far (Hz) from the window's centre, and what
    /// follows is of this frequency.
    Afresh(f64, u64),
}

/// The thread's end of the listening, held by the task that feeds it.
struct Feed {
    tx: Option<SyncSender<Piece>>,
    thread: Option<std::thread::JoinHandle<()>>,
    shared: Arc<Shared>,
    /// Where the channel is in the window (Hz from its centre), and the frequency it is.
    offset_hz: f64,
    freq_hz: u64,
    /// A block was dropped: the next one comes after a gap.
    behind: bool,
}

impl Feed {
    fn start(shared: Arc<Shared>, sample_rate_hz: u32, offset_hz: f64, freq_hz: u64) -> Result<Feed> {
        let channel = Channel::new(sample_rate_hz, offset_hz)?;
        let (tx, rx) = sync_channel(QUEUE);
        let state = shared.clone();
        let thread = std::thread::Builder::new().name("analog-listen".into()).spawn(move || listen(rx, channel, sample_rate_hz, freq_hz, state)).context("the receiver's thread")?;
        Ok(Feed { tx: Some(tx), thread: Some(thread), shared, offset_hz, freq_hz, behind: false })
    }

    fn block(&mut self, bytes: Vec<u8>) {
        let Some(tx) = &self.tx else { return };
        if self.behind && tx.try_send(Piece::Afresh(self.offset_hz, self.freq_hz)).is_err() {
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

    /// The radio moved, or the channel is elsewhere in the window: what the thread holds is of
    /// before, and what follows is of `freq_hz`.
    fn moved(&mut self, offset_hz: f64, freq_hz: u64) {
        self.offset_hz = offset_hz;
        self.freq_hz = freq_hz;
        self.behind = true;
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
            log.radio("analog", "the analog receiver stopped listening".to_string());
            shared.stopped(None);
        }
        Err(e) => {
            log.radio("analog", format!("the analog receiver is not listening: {e:#}"));
            shared.stopped(Some(format!("{e:#}")));
        }
    }
}

fn describe(tuned: &Tuned) -> String {
    let kind = match tuned.demod {
        crate::protocol::analog::Demod::Wfm => "wide FM",
        crate::protocol::analog::Demod::Nfm => "narrow FM",
        crate::protocol::analog::Demod::Am => "AM",
    };
    format!("{:.4} MHz, {kind}", tuned.freq_hz as f64 / 1e6)
}

async fn radio<H: RadioHw>(shared: &Arc<Shared>, tuner: &Tuner<H>, stop: &AtomicBool, log: &EventLog) -> Result<()> {
    let preset = find_preset(PRESET).context("the window's preset")?;
    // The radio's own gain control: a broadcast station and a distant aircraft are 60 dB apart.
    tuner.set_gain(GainMode::SlowAttack, None).await?;
    let mut feed = Feed::start(shared.clone(), preset.sample_rate_hz, -(LO_ABOVE_HZ as f64), shared.tuned().freq_hz)?;
    let hw = tuner.hw();
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut at: Option<u64> = None;
    while !stop.load(Ordering::SeqCst) {
        let tuned = shared.tuned();
        if at != Some(tuned.freq_hz) {
            // The radio moves: its window half a megahertz up, the capture afresh.
            let lo_hz = tuned.freq_hz + LO_ABOVE_HZ;
            if at.is_some() {
                hw.capture_end().await;
            }
            tuner.apply(TuningPlan { preset, lo_hz, control_hz: lo_hz }).await?;
            tokio::time::sleep(LO_SETTLE).await;
            hw.capture_begin().await?;
            feed.moved(-(LO_ABOVE_HZ as f64), tuned.freq_hz);
            // The log has where it began: a turn of the dial is not an event.
            if at.is_none() {
                log.radio("analog", format!("the analog receiver listens at {}", describe(&tuned)));
            }
            at = Some(tuned.freq_hz);
            shared.listening(Window { lo_hz, sample_rate_hz: preset.sample_rate_hz, source: "radio" });
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

async fn replayed(shared: &Arc<Shared>, path: &Path, stop: &AtomicBool, log: &EventLog) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    let (lo_hz, rate) = named_window(&name).with_context(|| format!("{name}: a replayed capture's name gives its LO and rate, as wx_163.05M_3000k.cs16"))?;
    let mut file = tokio::fs::File::open(path).await.with_context(|| format!("{}", path.display()))?;
    let offset = |tuned: &Tuned| tuned.freq_hz as f64 - lo_hz as f64;
    let mut tuned = shared.tuned();
    // A frequency the file's window does not hold, or one the channel cannot be cut at: the
    // listening ends with that said.
    let mut feed = Feed::start(shared.clone(), rate, offset(&tuned), tuned.freq_hz).with_context(|| format!("{} is not in {name}", describe(&tuned)))?;
    log.radio("analog", format!("the analog receiver listens at {}, replayed from {name}", describe(&tuned)));
    shared.listening(Window { lo_hz, sample_rate_hz: rate, source: "replay" });
    let started = Instant::now();
    let mut sent: u64 = 0;
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !stop.load(Ordering::SeqCst) {
        tick.tick().await;
        let now = shared.tuned();
        if now.freq_hz != tuned.freq_hz {
            Channel::new(rate, offset(&now)).with_context(|| format!("{} is not in {name}", describe(&now)))?;
            tuned = now;
            feed.moved(offset(&tuned), tuned.freq_hz);
        }
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

/// The thread: blocks of the window in, frames of audio out. `at_hz` is the frequency the
/// blocks are of.
fn listen(rx: Queue<Piece>, mut channel: Channel, sample_rate_hz: u32, mut at_hz: u64, shared: Arc<Shared>) {
    let mut tuned = shared.tuned();
    let mut receiver = Receiver::new(tuned.demod, tuned.squelch_db);
    let mut samples = Vec::new();
    let mut audio: Vec<i16> = Vec::new();
    while let Ok(piece) = rx.recv() {
        let bytes = match piece {
            Piece::Block(bytes) => bytes,
            Piece::Afresh(offset_hz, freq_hz) => {
                // The channel elsewhere in the window, or the same one after a break: nothing
                // of before is carried over.
                if let Ok(moved) = Channel::new(sample_rate_hz, offset_hz) {
                    channel = moved;
                }
                at_hz = freq_hz;
                tuned = shared.tuned();
                receiver = Receiver::new(tuned.demod, tuned.squelch_db);
                audio.clear();
                continue;
            }
        };
        let now = shared.tuned();
        if now.freq_hz != at_hz {
            // Tuned elsewhere, and the radio is not there yet: what it still hears is not
            // played as the new frequency's, nor read by its meter.
            audio.clear();
            continue;
        }
        if now.demod != tuned.demod {
            receiver = Receiver::new(now.demod, now.squelch_db);
            audio.clear();
        } else if now.squelch_db != tuned.squelch_db {
            receiver.set_squelch(now.squelch_db);
        }
        tuned = now;
        samples.clear();
        channel.push(&bytes, &mut samples);
        receiver.push(&samples, &mut audio);
        let whole = audio.len() / FRAME_SAMPLES * FRAME_SAMPLES;
        for frame in audio[..whole].chunks_exact(FRAME_SAMPLES) {
            shared.frame(frame);
        }
        audio.drain(..whole);
        let level = receiver.level();
        if !level.open {
            // What is left of a transmission is not played before the next.
            audio.clear();
        }
        shared.metered(level, at_hz);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Analog, State};
    use super::*;
    use crate::protocol::analog::tests::{signal, Sent};
    use crate::protocol::analog::{Demod, AUDIO_HZ};
    use crate::protocol::ism::channel::RATE_HZ;

    /// A window's bytes at twice the channel's rate, the narrowest a channel is cut from: a
    /// signal made at its centre, each of its samples twice.
    fn window(sent: Sent, amplitude: f32, seconds: f32) -> Vec<u8> {
        signal(sent, amplitude, 0.0, seconds, 8.0).iter().flat_map(|s| [s, s]).flat_map(|s| [(s.re as i16).to_le_bytes(), (s.im as i16).to_le_bytes()]).flatten().collect()
    }

    #[test]
    fn the_thread_makes_frames_of_audio_and_follows_the_kind_and_the_squelch() {
        let analog = Analog::new(Tuned { freq_hz: 146_520_000, demod: Demod::Nfm, squelch_db: Some(8.0) }, None);
        let (mut audio, _listener) = analog.listen();
        let mut feed = Feed::start(analog.shared.clone(), 2 * RATE_HZ, 0.0, 146_520_000).unwrap();
        // The window given to the thread whole, however slow it is, and the frames it made of
        // it. The thread takes a piece only once it is done with the one before, so when one
        // more empty block than the queue holds has gone in, it is done with the window.
        let tx = feed.tx.clone().unwrap();
        let mut given = |bytes: Vec<u8>| {
            for block in bytes.chunks(80_000) {
                tx.send(Piece::Block(block.to_vec())).unwrap();
            }
            for _ in 0..=QUEUE {
                tx.send(Piece::Block(Vec::new())).unwrap();
            }
            let mut frames = 0;
            while let Ok(frame) = audio.try_recv() {
                assert_eq!(frame.len(), FRAME_SAMPLES);
                frames += 1;
            }
            frames
        };
        // A second of a narrow FM signal: about fifty frames, and the meter says the squelch is
        // open.
        let frames = given(window(Sent::Fm(1000.0, 3000.0), 300.0, 1.0));
        assert!((45..=50).contains(&frames), "{frames}");
        assert_eq!(analog.view().counts.frames, frames);
        assert!(analog.view().level.unwrap().open && analog.view().level.unwrap().over_db.unwrap() > 20.0, "{:?}", analog.view().level);
        // Noise alone: the squelch shuts a third of a second on, and no more frames come.
        let after = given(window(Sent::Carrier, 0.0, 1.0));
        assert!(!analog.view().level.unwrap().open);
        assert!(after <= 20, "{after}");
        // The squelch taken off: the noise is heard. Then AM asked for: the receiver is AM's.
        analog.tune(Tuned { freq_hz: 146_520_000, demod: Demod::Nfm, squelch_db: None }).unwrap();
        let noise = given(window(Sent::Carrier, 0.0, 0.5));
        assert!(noise >= 20, "{noise}");
        analog.tune(Tuned { freq_hz: 146_520_000, demod: Demod::Am, squelch_db: Some(8.0) }).unwrap();
        let am = given(window(Sent::Am(1000.0, 0.5), 300.0, 1.0));
        assert!(am >= 40 && analog.view().level.unwrap().open, "{am} {:?}", analog.view().level);
        // Tuned elsewhere: what the radio still hears is not played as the new frequency's.
        // Once the radio has moved, what it hears there is.
        analog.tune(Tuned { freq_hz: 146_940_000, demod: Demod::Nfm, squelch_db: None }).unwrap();
        assert_eq!(given(window(Sent::Carrier, 0.0, 0.3)), 0);
        feed.moved(0.0, 146_940_000);
        feed.block(Vec::new());
        let moved = given(window(Sent::Carrier, 0.0, 0.3));
        assert!(moved >= 10, "{moved}");
        drop(tx);
        assert_eq!(AUDIO_HZ as usize / 50, FRAME_SAMPLES);
        // A gap is counted, and the thread ends with its queue.
        feed.gap();
        feed.block(window(Sent::Carrier, 100.0, 0.1));
        assert_eq!((analog.view().counts.gaps, analog.view().state), (1, State::Off));
        drop(feed.tx.take());
        feed.thread.take().unwrap().join().unwrap();
    }

    #[test]
    fn the_window_s_preset_is_one_the_radio_takes_and_the_channel_divides() {
        let preset = find_preset(PRESET).unwrap();
        assert!(preset.sample_rate_hz >= 2_083_334 && preset.sample_rate_hz % RATE_HZ == 0);
        // The channel is cut half a megahertz under the window's centre, at any frequency.
        assert!(Channel::new(preset.sample_rate_hz, -(LO_ABOVE_HZ as f64)).is_ok());
        assert_eq!(describe(&Tuned { freq_hz: 102_900_000, demod: Demod::Wfm, squelch_db: None }), "102.9000 MHz, wide FM");
    }
}
