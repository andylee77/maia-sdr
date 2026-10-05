//! Live TV (maia-sdr doc/changes/085): the radio core's 8-VSB block on one RF channel, and the
//! PS's half of the receiver.
//!
//! In ATSC mode live TV takes the radio as a scan does (the two exclude each other) and tunes the
//! channel as 081's naming does: the 10M preset (10 MSPS), the LO on the channel's centre with the
//! crystal shift, the slow-attack AGC. The block gets its matched filter for the true sample rate
//! and its nominal symbol step, and starts. Its packets come through the lane ring to a thread
//! here:
//!
//! - codewords: each corrected where its flag says so and derandomized
//!   (`protocol::atsc::live::transport`), counted, handed to the stream's players and read for
//!   PSIP;
//! - snapshots: gathered and solved on a worker thread for the equalizer's taps, which load at
//!   the block's next field.
//!
//! A snapshot is asked for 10 fields (0.25 s) after the last one's field once the timing loop
//! tracks (083). The block starts afresh when it overruns or its fields stay lost: its front end
//! finds the segment syncs once, at its start.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use crate::hardware::ad9361::GainMode;
use crate::hardware::presets::find_preset;
use crate::hardware::radiocore::VsbStatus;
use crate::protocol::atsc::demod::SYMBOL_RATE;
use crate::protocol::atsc::fec::rs;
use crate::protocol::atsc::live::packets::{self, Gathered, Gatherer, CODEWORDS, FLAG_RERUN, FLAG_TRACKING, PACKET, SNAPSHOT};
use crate::protocol::atsc::live::{matched_filter, step_word, table_words, taps, transport, MF_DSPS, MF_TAP_BITS};
use crate::protocol::atsc::psip::Psip;
use crate::protocol::atsc::ts::Sections;
use crate::protocol::atsc::Channel;
use crate::radio::tuner::{RadioHw, Tuner, TuningPlan};
use crate::services::discovery::sweep::LO_SETTLE;
use crate::services::events::EventLog;
use crate::util::time::unix_ms;

/// The block's rate: 081's naming preset.
pub const PRESET: &str = "10M";
/// Fields between snapshots (0.25 s, 083).
const SNAPSHOT_FIELDS: u32 = 10;
/// A snapshot asked for and not seen by then is asked for again.
const SNAPSHOT_WAIT: Duration = Duration::from_millis(300);
/// The block starts afresh when its fields have been lost this long, or never found.
const LOST_RESTART: Duration = Duration::from_secs(3);
const FIRST_FIELDS: Duration = Duration::from_secs(8);
/// The control loop's pace.
const TICK: Duration = Duration::from_millis(50);
/// Block packets queued for the thread: 2.9 s of codewords.
const QUEUE: usize = 2048;
/// Fields of MER kept for the statistics.
const MER_FIELDS: usize = 40;

/// A player's view of the live stream: whole block packets' transport stream packets.
pub type LiveReceiver = broadcast::Receiver<Arc<Vec<u8>>>;

/// What live TV is doing, for the Viewer.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LiveStats {
    /// The radio core has the 8-VSB block: live TV can play.
    pub available: bool,
    /// idle, starting, searching (the block finding its syncs), playing (its fields counted), or
    /// error.
    pub state: &'static str,
    pub rf: Option<u8>,
    pub started_unix_ms: u64,
    pub error: Option<String>,
    /// The block's registers.
    pub block: Option<VsbStatus>,
    /// The newest codeword packet's field, and the loops at that field's sync: the carrier's
    /// offset from the channel's centre, the timing against the nominal step, the gain word, the
    /// segment sync's correlation (four syncs of 5 levels at 128 a level: 2,560 at best).
    pub field: Option<u32>,
    pub carrier_hz: Option<f64>,
    pub timing_ppm: Option<f64>,
    pub gain_word: Option<u32>,
    pub sync: Option<i32>,
    /// The block's decoder has started afresh since its fields were lost and found again.
    pub rerun: bool,
    /// The latest fields' MER, dB, the newest last.
    pub mer_db: Vec<f32>,
    pub codewords: u64,
    pub corrected: u64,
    pub failed: u64,
    /// The block's packets read, those missing between two, and those the reader dropped.
    pub packets: u64,
    pub missed: u64,
    pub dropped: u64,
    pub snapshots: u64,
    pub solves: u64,
    pub solve_ms: Option<f64>,
    pub restarts: u64,
    /// What the station says of itself.
    pub psip: Psip,
    /// Players on the stream.
    pub viewers: usize,
}

/// Live TV's shared state: its statistics and the stream's players.
pub struct Live {
    stats: Mutex<LiveStats>,
    hub: broadcast::Sender<Arc<Vec<u8>>>,
    running: tokio::sync::Mutex<Option<Running>>,
}

impl Default for Live {
    fn default() -> Self {
        Live {
            stats: Mutex::new(LiveStats { state: "idle", ..Default::default() }),
            hub: broadcast::channel(256).0,
            running: tokio::sync::Mutex::new(None),
        }
    }
}

/// The receiver while it runs, with its hold on the radio.
struct Running {
    rf: u8,
    stop: Arc<AtomicBool>,
    control: tokio::task::JoinHandle<()>,
    thread: std::thread::JoinHandle<()>,
    _radio: Box<dyn std::any::Any + Send>,
}

impl Live {
    pub fn stats(&self) -> LiveStats {
        let mut s = self.lock().clone();
        s.viewers = self.hub.receiver_count();
        s
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LiveStats> {
        self.stats.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn update(&self, f: impl FnOnce(&mut LiveStats)) {
        f(&mut self.lock());
    }

    /// The channel playing, if any.
    pub async fn rf(&self) -> Option<u8> {
        self.running.lock().await.as_ref().map(|r| r.rf)
    }

    /// Players on the stream.
    pub fn viewers(&self) -> usize {
        self.hub.receiver_count()
    }

    /// A new player on the stream.
    pub fn subscribe(&self) -> LiveReceiver {
        self.hub.subscribe()
    }

    /// A start that failed.
    pub fn failed(&self, rf: u8, e: &anyhow::Error) {
        self.update(|s| {
            s.state = "error";
            s.rf = Some(rf);
            s.error = Some(format!("{e:#}"));
        });
    }

    /// Tune `rf` and start the block; `radio` is ATSC mode's hold on the radio, kept while it
    /// plays.
    pub async fn start<H: RadioHw + 'static>(self: &Arc<Self>, rf: u8, tuner: Arc<Tuner<H>>, log: Arc<EventLog>, radio: Box<dyn std::any::Any + Send>) -> Result<()> {
        let ch = Channel::get(rf).with_context(|| format!("RF channel {rf} is not in the plan"))?;
        anyhow::ensure!(tuner.hw().vsb_present().await, "the radio core has no 8-VSB block (core 2.1 or later)");
        self.update(|s| *s = LiveStats { state: "starting", rf: Some(rf), started_unix_ms: unix_ms(), ..Default::default() });
        let preset = find_preset(PRESET).context("the live TV preset")?;
        tuner.apply(TuningPlan { preset, lo_hz: ch.center_hz(), control_hz: ch.center_hz() }).await?;
        tuner.set_gain(GainMode::SlowAttack, None).await?;
        tokio::time::sleep(LO_SETTLE).await;
        // The sample clock runs off the crystal the LO is corrected for.
        let fs = f64::from(preset.sample_rate_hz) * (1.0 + tuner.tuning().crystal_ppm * 1e-6);
        let (_, n, table) = matched_filter(fs, MF_TAP_BITS);
        let table = table_words(n, &table).context("the matched filter's table does not fit the block")?;
        let step = step_word(fs);
        let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE);
        tuner.hw().vsb_start(table.clone(), step, 0, tx.clone()).await?;
        let stop = Arc::new(AtomicBool::new(false));
        let (snaps_tx, snaps_rx) = mpsc::channel::<Gathered>(4);
        let thread = {
            let (me, stop) = (self.clone(), stop.clone());
            std::thread::Builder::new()
                .name("atsc-live".into())
                .spawn(move || me.packets(rx, snaps_tx, stop, fs))
                .context("the live TV thread")?
        };
        let control = {
            let (me, stop) = (self.clone(), stop.clone());
            let start = Start { table, step, packets: tx };
            tokio::spawn(async move { me.control(tuner, start, snaps_rx, stop).await })
        };
        log.system("atsc", format!("live TV on RF {rf} ({:.3} MHz)", ch.center_hz() as f64 / 1e6));
        *self.running.lock().await = Some(Running { rf, stop, control, thread, _radio: radio });
        Ok(())
    }

    /// Stop the block and the receiver.
    pub async fn stop(&self) {
        let Some(r) = self.running.lock().await.take() else { return };
        r.stop.store(true, Ordering::SeqCst);
        let _ = r.control.await;
        let _ = tokio::task::spawn_blocking(move || r.thread.join()).await;
        self.update(|s| {
            s.state = "idle";
            s.block = None;
        });
    }

    /// The block's packets: codewords to the stream and PSIP, snapshots gathered for the solver.
    fn packets(&self, rx: Receiver<Vec<u8>>, snaps: mpsc::Sender<Gathered>, stop: Arc<AtomicBool>, fs: f64) {
        let mut gather = Gatherer::default();
        let mut sections = Sections::default();
        let mut psip = Psip::default();
        let mut sequence: Option<u16> = None;
        let mut psip_changed = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let bytes = match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(b) => b,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let Ok(p) = <&[u8; PACKET]>::try_from(bytes.as_slice()) else { continue };
            match packets::format(p) {
                Some(CODEWORDS) => {
                    let Some((h, cws)) = packets::unpack(p) else { continue };
                    let missed = sequence.map_or(0, |s| u64::from(h.sequence.wrapping_sub(s).wrapping_sub(1)));
                    sequence = Some(h.sequence);
                    let mut chunk = Vec::with_capacity(cws.len() * 188);
                    let (mut corrected, mut failed) = (0, 0);
                    for (_, place, data, flag) in &cws {
                        let t = transport(*place, data, *flag);
                        let mut b = t.bytes;
                        match t.outcome {
                            rs::Outcome::Failed => {
                                failed += 1;
                                // The transport error indicator: players skip it.
                                b[1] |= 0x80;
                            }
                            o => {
                                corrected += u64::from(matches!(o, rs::Outcome::Corrected(_)));
                                for (pid, s) in sections.push(&b, &Psip::pids()) {
                                    psip.section(pid, &s);
                                }
                            }
                        }
                        chunk.extend_from_slice(&b);
                    }
                    let _ = self.hub.send(Arc::new(chunk));
                    let named = psip_changed.elapsed() > Duration::from_secs(1);
                    if named {
                        psip_changed = Instant::now();
                    }
                    self.update(|s| {
                        s.packets += 1;
                        s.missed += missed;
                        s.codewords += cws.len() as u64;
                        s.corrected += corrected;
                        s.failed += failed;
                        if s.field != Some(h.field) {
                            if let Some(db) = h.mer_before_db() {
                                s.mer_db.push(db as f32);
                                if s.mer_db.len() > MER_FIELDS {
                                    s.mer_db.remove(0);
                                }
                            }
                        }
                        s.field = Some(h.field);
                        s.carrier_hz = Some(f64::from(h.freq) / 4_294_967_296.0 * SYMBOL_RATE);
                        let nominal = fs / SYMBOL_RATE;
                        s.timing_ppm = Some(f64::from(h.rate) / 4_294_967_296.0 / nominal * 1e6);
                        s.gain_word = Some(h.gain);
                        s.sync = Some(h.sync);
                        s.rerun = h.flags & FLAG_RERUN != 0;
                        if named {
                            s.psip = psip.clone();
                        }
                    });
                }
                Some(SNAPSHOT) => {
                    let Some((h, values)) = packets::unpack_snapshot(p) else { continue };
                    if let Some(g) = gather.push(&h, &values) {
                        self.update(|s| s.snapshots += 1);
                        let _ = snaps.try_send(g);
                    }
                }
                _ => {}
            }
        }
        self.update(|s| s.psip = psip);
    }

    /// The block's status, its snapshots asked for and solved, its restarts.
    async fn control<H: RadioHw>(self: Arc<Self>, tuner: Arc<Tuner<H>>, start: Start, mut snaps: mpsc::Receiver<Gathered>, stop: Arc<AtomicBool>) {
        let hw = tuner.hw();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut started = Instant::now();
        let mut last_snapshot: Option<u32> = None;
        let mut asked: Option<Instant> = None;
        let mut seen_fields = false;
        let mut lost_since: Option<Instant> = None;
        while !stop.load(Ordering::SeqCst) {
            tokio::select! {
                _ = tick.tick() => {}
                Some(g) = snaps.recv() => {
                    asked = None;
                    if g.flags & FLAG_TRACKING == 0 {
                        // Before the timing loop tracks: ask again.
                        continue;
                    }
                    last_snapshot = Some(g.field);
                    let t0 = Instant::now();
                    if let Ok(Some(t)) = tokio::task::spawn_blocking(move || taps::for_block(&g)).await {
                        let ms = t0.elapsed().as_secs_f64() * 1e3;
                        let mut loaded = hw.vsb_load_taps(t.clone()).await;
                        if !loaded {
                            // A set still armed: it loads within a field.
                            tokio::time::sleep(Duration::from_millis(30)).await;
                            loaded = hw.vsb_load_taps(t).await;
                        }
                        self.update(|s| {
                            s.solves += 1;
                            s.solve_ms = Some(ms);
                        });
                        if !loaded {
                            tracing::warn!("live TV: a tap set stayed armed; the solve's taps were not loaded");
                        }
                    }
                }
            }
            let Some(status) = hw.vsb_status().await else { break };
            let dropped = hw.vsb_dropped();
            let fields = status.fields;
            self.update(|s| {
                s.block = Some(status);
                s.dropped = dropped;
                s.state = if fields { "playing" } else { "searching" };
            });
            // Snapshots, once the timing loop tracks: each 10 fields after the last one's.
            if status.tracking && fields && !status.snapshot_pending && asked.is_none_or(|t| t.elapsed() > SNAPSHOT_WAIT) {
                let due = last_snapshot.is_none_or(|f| status.fields_counted >= f + SNAPSHOT_FIELDS);
                if due {
                    hw.vsb_snapshot().await;
                    asked = Some(Instant::now());
                }
            }
            // Afresh when it overran, its fields stayed lost, or it never found them.
            if fields {
                seen_fields = true;
                lost_since = None;
            } else if seen_fields {
                lost_since.get_or_insert_with(Instant::now);
            }
            let lost = lost_since.is_some_and(|t| t.elapsed() > LOST_RESTART);
            let never = !seen_fields && started.elapsed() > FIRST_FIELDS;
            if status.overrun || lost || never {
                tracing::info!("live TV: the block starts afresh (overrun {}, fields lost {lost}, none found {never})", status.overrun);
                if let Err(e) = hw.vsb_start(start.table.clone(), start.step, 0, start.packets.clone()).await {
                    self.update(|s| {
                        s.state = "error";
                        s.error = Some(format!("{e:#}"));
                    });
                    break;
                }
                started = Instant::now();
                (last_snapshot, asked, seen_fields, lost_since) = (None, None, false, None);
                self.update(|s| s.restarts += 1);
            }
        }
        hw.vsb_stop().await;
    }
}

/// What the block is started with, for its restarts.
struct Start {
    table: Vec<[u16; MF_DSPS]>,
    step: u32,
    packets: SyncSender<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::atsc::pl::ps;

    /// Offline: the model's codeword packets of a capture (`ATSC_VECTOR_DIR`, a directory
    /// `pl::vectors` wrote; the gateware's packets are these word for word) through the receiver's
    /// packet thread: its players get the model's transport stream byte for byte, and its
    /// statistics and PSIP are the model's.
    #[test]
    fn the_models_packets_play_as_the_model_reads_them() {
        let Ok(dir) = std::env::var("ATSC_VECTOR_DIR") else {
            eprintln!("ATSC_VECTOR_DIR not set: skipped");
            return;
        };
        let bytes = std::fs::read(std::path::Path::new(&dir).join("packets.bin")).expect("packets.bin");
        let packets: Vec<[u8; PACKET]> = bytes.chunks_exact(PACKET).map(|c| c.try_into().unwrap()).collect();
        let st = ps::station(&packets);
        let want = ps::ts_bytes(&ps::stream(&packets).0);

        let live = Arc::new(Live::default());
        let mut hub = live.subscribe();
        let (tx, rx) = sync_channel::<Vec<u8>>(4);
        let (snaps, _snaps_rx) = mpsc::channel::<Gathered>(4);
        let thread = {
            let live = live.clone();
            std::thread::spawn(move || live.packets(rx, snaps, Arc::new(AtomicBool::new(false)), 10e6))
        };
        let mut got = Vec::new();
        for p in &packets {
            tx.send(p.to_vec()).unwrap();
            got.extend_from_slice(&hub.blocking_recv().unwrap());
        }
        drop(tx);
        thread.join().unwrap();

        let s = live.stats();
        eprintln!("{dir}: {} codewords, {} corrected, {} failed, TSID {:?}, MER {:?}", s.codewords, s.corrected, s.failed, s.psip.tsid, s.mer_db.last());
        assert_eq!(got.len(), want.len());
        assert!(got == want, "the players' stream differs from the model's");
        assert_eq!((s.packets, s.missed), (packets.len() as u64, 0));
        assert_eq!((s.codewords, s.corrected, s.failed), (st.packets as u64, st.corrected as u64, st.failed as u64));
        assert_eq!(s.psip.tsid, st.psip.tsid);
        assert_eq!(s.psip.channels.len(), st.psip.channels.len());
    }
}
