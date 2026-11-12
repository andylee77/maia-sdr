//! Data mode listening: the radio parked on a window of the data bands, its raw IQ read from
//! the capture ring as it fills, and a thread of its own that cuts the channel around the
//! window's spot, finds the bursts in it and hands the armed ones' IQ to be kept.
//!
//! The window is narrow (3 MSPS), so the capture ring is read whole, without a gap, and a
//! burst's IQ comes from the same samples it was found in. When the reader falls behind or the
//! core loses words the capture is started afresh: what was open ends there, and the gap is
//! counted.
//!
//! On a development host a raw capture's file stands in for the radio (`--data-replay`), played
//! at its own rate and from its start again at its end.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rustfft::num_complex::Complex32;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::{Shared, Window};
use crate::hardware::ad9361::GainMode;
use crate::hardware::presets::find_preset;
use crate::protocol::ism::bursts::{Burst, Detector};
use crate::protocol::ism::channel::{self, Channel, RATE_HZ};
use crate::protocol::ism::named_window;
use crate::radio::tuner::{RadioHw, Tuner, TuningPlan};
use crate::services::discovery::sweep::LO_SETTLE;
use crate::services::events::EventLog;
use crate::util::time::unix_ms;

/// A window of the data bands: the spot listened to, and the radio's LO and rate for it. The LO
/// is half a megahertz above the spot, so its DC spur is outside the channel.
pub struct Plan {
    pub name: &'static str,
    pub spot_hz: u64,
    pub lo_hz: u64,
    pub preset: &'static str,
}

/// The one window so far: 433.92 MHz, where unit A's first captures found four devices within
/// 50 kHz (doc/changes/109). 3 MSPS is the narrowest preset the AD9361 takes (without its own
/// filter it samples at 2.083 MSPS or more) that divides down to the channel's rate.
pub const PLAN: Plan = Plan { name: "433", spot_hz: 433_920_000, lo_hz: 434_420_000, preset: "3M" };

/// How often the capture ring is read: a sub-buffer takes 87 ms to fill at 3 MSPS.
const POLL: Duration = Duration::from_millis(20);
/// Blocks of the window waiting for the thread: 1.4 s of it.
const QUEUE: usize = 16;
/// A replayed file is read this much at a time.
const REPLAY_BLOCK: usize = 1 << 20;
/// The channel's last seconds are kept for the captures: a burst of three, and what it waits to
/// end.
const KEPT_SAMPLES: usize = 5 * RATE_HZ as usize;
/// A capture has this much before its burst and after it: 20 ms.
pub const MARGIN_SAMPLES: u64 = RATE_HZ as u64 / 50;
/// The clock's fit of samples to time is begun again this often, to follow the crystal.
const CLOCK_RENEW: Duration = Duration::from_secs(600);

/// What the thread is given.
enum Piece {
    /// More of the window: the AD9361 sample index of its first sample, its bytes, and when it
    /// was read.
    Block { index: u64, bytes: Vec<u8>, read_unix_ms: u64 },
    /// Samples were lost before the next block.
    Gap,
}

/// The thread's end of the listening, held by the task that feeds it.
struct Feed {
    tx: Option<SyncSender<Piece>>,
    thread: Option<std::thread::JoinHandle<()>>,
    shared: Arc<Shared>,
    /// A block was dropped: the next one comes after a gap.
    behind: bool,
}

impl Feed {
    fn start(shared: Arc<Shared>, window: Window, offset_hz: f64) -> Result<Feed> {
        let channel = Channel::new(window.sample_rate_hz, offset_hz)?;
        let (tx, rx) = sync_channel(QUEUE);
        let state = shared.clone();
        let thread = std::thread::Builder::new().name("data-listen".into()).spawn(move || listen(rx, channel, window, state)).context("the listening thread")?;
        Ok(Feed { tx: Some(tx), thread: Some(thread), shared, behind: false })
    }

    fn block(&mut self, index: u64, bytes: Vec<u8>, read_unix_ms: u64) {
        let Some(tx) = &self.tx else { return };
        if self.behind && tx.try_send(Piece::Gap).is_err() {
            return;
        }
        self.behind = matches!(tx.try_send(Piece::Block { index, bytes, read_unix_ms }), Err(TrySendError::Full(_)));
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
pub(super) async fn run<H: RadioHw + 'static>(shared: Arc<Shared>, tuner: Arc<Tuner<H>>, gain_db: f64, replay: Option<PathBuf>, stop: Arc<AtomicBool>, log: Arc<EventLog>) {
    shared.starting();
    let result = match &replay {
        Some(path) => replayed(&shared, path, &stop, &log).await,
        None => radio(&shared, &tuner, gain_db, &stop, &log).await,
    };
    match result {
        Ok(()) => {
            log.radio("data", "data mode stopped listening".to_string());
            shared.stopped(None);
        }
        Err(e) => {
            log.radio("data", format!("data mode is not listening: {e:#}"));
            shared.stopped(Some(format!("{e:#}")));
        }
    }
}

async fn radio<H: RadioHw>(shared: &Arc<Shared>, tuner: &Tuner<H>, gain_db: f64, stop: &AtomicBool, log: &EventLog) -> Result<()> {
    let preset = find_preset(PLAN.preset).context("the window's preset")?;
    tuner.set_gain(GainMode::Manual, Some(gain_db)).await?;
    tuner.apply(TuningPlan { preset, lo_hz: PLAN.lo_hz, control_hz: PLAN.lo_hz }).await?;
    tokio::time::sleep(LO_SETTLE).await;
    let window = Window::of(&PLAN, PLAN.lo_hz, preset.sample_rate_hz, Some(gain_db), "radio");
    let mut feed = Feed::start(shared.clone(), window.clone(), PLAN.spot_hz as f64 - PLAN.lo_hz as f64)?;
    let hw = tuner.hw();
    hw.capture_begin().await?;
    log.radio("data", format!("data mode listens at {} ({gain_db} dB of gain)", window.describe()));
    shared.listening(window);
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !stop.load(Ordering::SeqCst) {
        tick.tick().await;
        let mut blocks = Vec::new();
        let read = hw.capture_read(&mut |index, bytes| blocks.push((index, bytes.to_vec()))).await;
        let now = unix_ms();
        for (index, bytes) in blocks {
            feed.block(index, bytes, now);
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
    let (lo_hz, rate) = named_window(&name).with_context(|| format!("{name}: a replayed capture's name gives its LO and rate, as look_434.42M_2500k.cs16"))?;
    let mut file = tokio::fs::File::open(path).await.with_context(|| format!("{}", path.display()))?;
    let window = Window::of(&PLAN, lo_hz, rate, None, "replay");
    let mut feed = Feed::start(shared.clone(), window.clone(), PLAN.spot_hz as f64 - lo_hz as f64)?;
    log.radio("data", format!("data mode listens at {}, replayed from {name}", window.describe()));
    shared.listening(window);
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
            feed.block(sent, bytes, unix_ms());
            sent += (REPLAY_BLOCK / 4) as u64;
        }
    }
    feed.end().await;
    Ok(())
}

/// Samples to time: the least delay seen between a block's last sample and its reading is the
/// reading's own, so a block read late does not move the clock.
struct Clock {
    rate: f64,
    /// `unix_ms − index × 1000 / rate`, the least seen.
    origin_ms: Option<f64>,
    since: Instant,
}

impl Clock {
    fn new(rate: u32) -> Clock {
        Clock { rate: f64::from(rate), origin_ms: None, since: Instant::now() }
    }

    fn read(&mut self, end_index: u64, read_unix_ms: u64) {
        let origin = read_unix_ms as f64 - end_index as f64 * 1e3 / self.rate;
        if self.since.elapsed() >= CLOCK_RENEW {
            self.origin_ms = None;
            self.since = Instant::now();
        }
        self.origin_ms = Some(self.origin_ms.map_or(origin, |o| o.min(origin)));
    }

    fn unix_ms(&self, index: u64) -> u64 {
        (self.origin_ms.unwrap_or(0.0) + index as f64 * 1e3 / self.rate).max(0.0) as u64
    }
}

/// The channel's newest samples, as a capture keeps them.
struct Kept {
    /// The channel sample index (since the last gap) of the first one held.
    first: u64,
    samples: VecDeque<[i16; 2]>,
}

impl Kept {
    fn push(&mut self, new: &[Complex32]) {
        self.samples.extend(new.iter().map(|s| channel::to_i16(*s)));
        let over = self.samples.len().saturating_sub(KEPT_SAMPLES);
        self.samples.drain(..over);
        self.first += over as u64;
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.first = 0;
    }

    /// The samples `from..to` that are still held, as the file's bytes, and the first one's
    /// index.
    fn bytes(&self, from: u64, to: u64) -> (u64, Vec<u8>) {
        let from = from.max(self.first);
        let to = to.min(self.first + self.samples.len() as u64).max(from);
        let bytes = self.samples.range((from - self.first) as usize..(to - self.first) as usize).flat_map(|s| [s[0].to_le_bytes(), s[1].to_le_bytes()].concat()).collect();
        (from, bytes)
    }
}

/// The thread: blocks of the window in, bursts out.
fn listen(rx: Receiver<Piece>, mut channel: Channel, window: Window, shared: Arc<Shared>) {
    let decimation = channel.decimation() as u64;
    let delay = channel.delay() as u64;
    let mut detector = Detector::default();
    let mut clock = Clock::new(window.sample_rate_hz);
    let mut kept = Kept { first: 0, samples: VecDeque::with_capacity(KEPT_SAMPLES) };
    // The AD9361 sample index of the channel's first input since the last gap, and the next
    // one expected.
    let mut base: Option<u64> = None;
    let mut next = 0;
    let mut samples = Vec::new();
    let mut bursts = Vec::new();
    while let Ok(piece) = rx.recv() {
        let block = match piece {
            Piece::Block { index, bytes, read_unix_ms } => Some((index, bytes, read_unix_ms)),
            Piece::Gap => None,
        };
        // A gap, or a block that does not follow the last: what was open ends where the
        // samples do.
        if let Some(was) = base.filter(|_| block.as_ref().is_none_or(|b| b.0 != next)) {
            detector.end(&mut bursts);
            heard(&shared, &window, &clock, &kept, &mut bursts, |sample| was + sample * decimation + delay);
            channel.reset();
            kept.clear();
            base = None;
        }
        let Some((index, bytes, read_unix_ms)) = block else { continue };
        let first = *base.get_or_insert(index);
        next = index + (bytes.len() / 4) as u64;
        clock.read(next, read_unix_ms);
        samples.clear();
        channel.push(&bytes, &mut samples);
        kept.push(&samples);
        detector.push(&samples, &mut bursts);
        heard(&shared, &window, &clock, &kept, &mut bursts, |sample| first + sample * decimation + delay);
        let counters = detector.counters();
        shared.read(counters, detector.floor_power());
    }
}

/// Tell of the bursts that ended, and hand on the IQ of those to keep. `at` gives a channel
/// sample's AD9361 sample index.
fn heard(shared: &Shared, window: &Window, clock: &Clock, kept: &Kept, bursts: &mut Vec<Burst>, at: impl Fn(u64) -> u64) {
    for b in bursts.drain(..) {
        let unix_ms = clock.unix_ms(at(b.start));
        let Some(keep) = shared.heard(window, &b, unix_ms) else { continue };
        let from = b.start.saturating_sub(MARGIN_SAMPLES);
        let (first, iq) = kept.bytes(from, b.end + MARGIN_SAMPLES);
        shared.keep(keep, clock.unix_ms(at(first)), (b.start - first) as u32, iq, window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_takes_the_least_delay_and_a_late_block_does_not_move_it() {
        let mut clock = Clock::new(3_000_000);
        // A block of 262,144 samples (87 ms) read 30 ms after its last sample, then one read
        // 12 ms after.
        clock.read(262_144, 1_000_000 + 87 + 30);
        clock.read(524_288, 1_000_000 + 175 + 12);
        assert_eq!(clock.unix_ms(0), 1_000_012);
        clock.read(786_432, 1_000_000 + 262 + 90);
        assert_eq!(clock.unix_ms(3_000_000), 1_001_012);
    }

    #[test]
    fn a_capture_is_cut_from_the_samples_still_held() {
        let mut kept = Kept { first: 0, samples: VecDeque::new() };
        kept.push(&(0..10).map(|n| Complex32::new(n as f32, -(n as f32))).collect::<Vec<_>>());
        let (first, bytes) = kept.bytes(2, 4);
        assert_eq!(first, 2);
        assert_eq!(bytes, [32i16, -32, 48, -48].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>());
        // Asked past what is held: what there is.
        assert_eq!(kept.bytes(8, 20).1.len(), 8);
        // The oldest leave when more than five seconds are held.
        kept.push(&vec![Complex32::default(); KEPT_SAMPLES]);
        assert_eq!((kept.first, kept.samples.len()), (10, KEPT_SAMPLES));
        assert_eq!(kept.bytes(0, 12), (10, vec![0; 8]));
        kept.clear();
        assert_eq!((kept.first, kept.bytes(0, 5).1.len()), (0, 0));
    }
}
