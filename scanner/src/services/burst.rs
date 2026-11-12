//! The burst timing check (maia-sdr doc/changes/082, bake A's gate): one burst on the air (a key
//! fob, the ESP32-DIV) must start at the same AD9361 sample, to within one transform, in a
//! spectrum frame's header, in the control lane's packets and in the capture ring.
//!
//! It takes the radio as a scan does and tunes the 2 MSPS preset with the burst's frequency
//! `offset_hz` above the LO, the spectrometer at one transform a frame. A first press (`find`)
//! shows where the transmitter really is (a fob may sit tens of kHz from its nominal frequency,
//! outside a lane's channel filter), and the control lane moves there. Then the capture runs, the
//! floors are learned for a second, and the next press, seen in the frames, triggers the capture:
//! it finishes two more sub-buffers and stops, so the ring keeps the burst's lead-in. Each of the
//! three gives the sample index where the burst's power first rises above its floor.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::util::time::unix_ms;

/// The spectrometer's transform, in samples: the resolution the check asks for.
pub const TRANSFORM: u64 = 4096;
/// The preset the check runs at: frames of one transform every 2 ms, which the spectrum's
/// reader keeps up with.
pub const PRESET: &str = "2M";
/// Samples (at the AD9361's rate) a capture point averages.
pub const CAPTURE_BLOCK: usize = 64;
/// Lane samples a lane point averages.
pub const LANE_BLOCK: usize = 4;

fn default_offset() -> i64 {
    300_000
}
fn default_wait() -> u64 {
    60
}
fn default_threshold() -> f64 {
    15.0
}
fn default_find() -> bool {
    true
}
fn default_span() -> u64 {
    150_000
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BurstRequest {
    /// The transmitter's nominal frequency.
    pub freq_hz: u64,
    /// The burst sits this far above the LO (away from the DC bin).
    #[serde(default = "default_offset")]
    pub offset_hz: i64,
    /// How far either side of `freq_hz` the frames look for it.
    #[serde(default = "default_span")]
    pub span_hz: u64,
    /// Seconds to wait for each press.
    #[serde(default = "default_wait")]
    pub wait_s: u64,
    /// A burst rises this far above the floor.
    #[serde(default = "default_threshold")]
    pub threshold_db: f64,
    /// Find the transmitter's frequency with a first press (else the lane sits on `freq_hz`).
    #[serde(default = "default_find")]
    pub find: bool,
}

/// Where one of the three placed the burst's start.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Placed {
    /// AD9361 sample index.
    pub index: u64,
    pub db: f64,
    pub floor_db: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Timing {
    pub burst_hz: f64,
    /// The first 64-sample block of the capture above its floor.
    pub capture: Option<Placed>,
    /// The first frame whose band rose above its floor: `index` is its header's sample index.
    pub frame: Option<Placed>,
    pub frame_sequence: Option<u32>,
    /// The first block of four lane samples above its floor, less the DDC's delay.
    pub lane: Option<Placed>,
    /// The DDC's delay subtracted from the lane's index (AD9361 samples).
    pub lane_delay: f64,
    /// Lane minus capture, and frame minus capture, in samples.
    pub lane_minus_capture: Option<i64>,
    pub frame_minus_capture: Option<i64>,
    /// The capture's start lies within one transform of the frame's and of the lane's.
    pub pass: Option<bool>,
    /// Frames the spectrum's reader missed while the check listened (the frame of the burst's
    /// start may be among them).
    pub frames_missed: u64,
    pub capture_overflow: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BurstState {
    pub id: u64,
    /// idle, tuning, finding, armed, measuring, restoring, done, cancelled, error
    pub state: &'static str,
    /// What to do now.
    pub message: Option<String>,
    pub started_unix_ms: u64,
    pub finished_unix_ms: u64,
    pub timing: Option<Timing>,
    pub error: Option<String>,
    #[serde(skip)]
    pub cancel: bool,
}

impl BurstState {
    pub fn running(&self) -> bool {
        !matches!(self.state, "" | "idle" | "done" | "cancelled" | "error")
    }
}

#[derive(Default)]
pub struct BurstCheck {
    state: Mutex<BurstState>,
}

impl BurstCheck {
    pub fn state(&self) -> BurstState {
        self.lock().clone()
    }

    pub fn cancel(&self) {
        let mut s = self.lock();
        if s.running() {
            s.cancel = true;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BurstState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, f: impl FnOnce(&mut BurstState)) {
        f(&mut self.lock());
    }

    fn cancelled(&self) -> bool {
        self.lock().cancel
    }

    fn begin(&self) -> u64 {
        let mut s = self.lock();
        let id = s.id + 1;
        *s = BurstState { id, state: "tuning", started_unix_ms: unix_ms(), ..Default::default() };
        id
    }

    fn finish(&self, result: anyhow::Result<Timing>) {
        let mut s = self.lock();
        s.finished_unix_ms = unix_ms();
        s.message = None;
        match result {
            Ok(t) => {
                s.state = if s.cancel { "cancelled" } else { "done" };
                s.timing = Some(t);
            }
            Err(e) => {
                s.state = if s.cancel { "cancelled" } else { "error" };
                s.error = Some(format!("{e:#}"));
            }
        }
    }
}

/// A point of a power series: the sample index of its first sample and its mean power.
pub type Point = (u64, f64);

fn db(power: f64) -> f64 {
    10.0 * power.max(1e-30).log10()
}

fn median(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mid = xs.len() / 2;
    xs.select_nth_unstable_by(mid, f64::total_cmp);
    Some(xs[mid])
}

/// Where a series first rises `threshold_db` above its floor (the median of its points before
/// `learn_until`), from `learn_until` on.
pub fn onset(points: &[Point], learn_until: u64, threshold_db: f64) -> Option<Placed> {
    let floor = median(points.iter().filter(|p| p.0 < learn_until).map(|p| p.1).collect())?;
    let above = floor * 10f64.powf(threshold_db / 10.0);
    points
        .iter()
        .find(|p| p.0 >= learn_until && p.1 > above)
        .map(|&(index, power)| Placed { index, db: db(power), floor_db: db(floor) })
}

/// Mean power of each `block` of interleaved i16 samples, the first at `first_index`, a sample
/// every `step` AD9361 samples.
pub fn block_powers(iq: &[i16], first_index: u64, step: u64, block: usize, out: &mut Vec<Point>) {
    for (k, b) in iq.chunks_exact(2 * block).enumerate() {
        let p = b.chunks_exact(2).map(|s| f64::from(s[0]).powi(2) + f64::from(s[1]).powi(2)).sum::<f64>() / block as f64;
        out.push((first_index + (k * block) as u64 * step, p));
    }
}

/// The strongest bin of a frame's band (`bins`), and its power (mantissa × 4^exponent).
pub fn band_peak(frame: &[u8], bins: std::ops::Range<usize>) -> (usize, f64) {
    let mut best = (bins.start, 0.0);
    for bin in bins {
        let w = u64::from_le_bytes(frame[8 * bin..8 * bin + 8].try_into().unwrap());
        let p = (w & ((1 << 47) - 1)) as f64 * 4f64.powi(((w >> 56) & 0x7) as i32);
        if p > best.1 {
            best = (bin, p);
        }
    }
    best
}

/// The three starts compared: the capture's must lie within one transform of the lane's and in
/// the frame's transform or the one before it (where the window hides a burst's start).
pub fn compare(t: &mut Timing) {
    let c = t.capture.map(|p| p.index as i64);
    t.lane_minus_capture = c.zip(t.lane).map(|(c, l)| l.index as i64 - c);
    t.frame_minus_capture = c.zip(t.frame).map(|(c, f)| f.index as i64 - c);
    let n = TRANSFORM as i64;
    t.pass = t.lane_minus_capture.zip(t.frame_minus_capture).map(|(l, f)| l.abs() < n && (-n..n).contains(&f));
}

#[cfg(target_os = "linux")]
pub use live::start;

#[cfg(not(target_os = "linux"))]
pub fn start(
    _: &std::sync::Arc<BurstCheck>,
    _: BurstRequest,
    _: &crate::radio::lease::RadioLease,
    _: std::sync::Arc<crate::trunking::site::LiveSite<crate::radio::hw::Hardware>>,
    _: std::sync::Arc<crate::boot::radio::RadioTuner>,
) -> anyhow::Result<u64> {
    anyhow::bail!("no radio core on this host")
}

#[cfg(target_os = "linux")]
mod live {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{sync_channel, Receiver};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};

    use super::*;
    use crate::boot::radio::RadioTuner;
    use crate::hardware::presets::find_preset;
    use crate::hardware::radiocore::frame::BINS;
    use crate::hardware::radiocore::CAPTURE_SAMPLE_BYTES;
    use crate::radio::hw::Hardware;
    use crate::radio::lease::{Lease, LeaseGuard, RadioLease};
    use crate::radio::streams::{Block, StreamSource};
    use crate::radio::tuner::TuningPlan;
    use crate::trunking::site::LiveSite;

    const POLL: Duration = Duration::from_millis(20);
    const LEARN: Duration = Duration::from_secs(1);
    /// After an LO move, before the frames count.
    const SETTLE: Duration = Duration::from_millis(300);
    /// A burst has ended when the band stays this long under its threshold.
    const QUIET: Duration = Duration::from_millis(1500);
    /// Sub-buffers the capture writes after the one it was writing at the trigger.
    const STOP_AFTER: u8 = 2;
    /// The capture's floor: its first quarter second.
    const CAPTURE_LEARN_S: f64 = 0.25;
    /// The spectrometer's integrations when the check gives the radio back.
    const NORMAL_INTEGRATIONS: u16 = 256;

    /// A frame as the tap saw it: sequence, sample index, the band's strongest bin and power.
    type FramePoint = (u32, u64, usize, f64);

    /// Start the check; refused while the radio is taken.
    pub fn start(
        me: &Arc<BurstCheck>,
        req: BurstRequest,
        lease: &RadioLease,
        live: Arc<LiveSite<Hardware>>,
        tuner: Arc<RadioTuner>,
    ) -> Result<u64> {
        let guard = lease.take(Lease::Diagnostic).context("the radio is busy (a scan, a switch or another mode)")?;
        let id = me.begin();
        let me = me.clone();
        tokio::spawn(async move { me.run(req, guard, live, tuner).await });
        Ok(id)
    }

    impl BurstCheck {
        async fn run(self: Arc<Self>, req: BurstRequest, guard: LeaseGuard, live: Arc<LiveSite<Hardware>>, tuner: Arc<RadioTuner>) {
            let back_to = live.pause_for_scan().await;
            let result = self.measure(&req, &tuner).await;
            self.update(|s| s.state = "restoring");
            let board = tuner.hw();
            board.spectrum.set_tap(None);
            {
                let core = board.core.lock().await;
                core.stop_capture();
                core.set_spectrometer_integrations(NORMAL_INTEGRATIONS);
                core.set_spectrum_mode(false, false);
            }
            drop(guard);
            live.resume(back_to).await;
            self.finish(result);
        }

        async fn wait(&self, d: Duration) -> Result<()> {
            let until = Instant::now() + d;
            while Instant::now() < until {
                if self.cancelled() {
                    bail!("cancelled");
                }
                tokio::time::sleep(POLL).await;
            }
            Ok(())
        }

        async fn measure(&self, req: &BurstRequest, tuner: &RadioTuner) -> Result<Timing> {
            let preset = find_preset(PRESET).context("the 2M preset")?;
            let fs = f64::from(preset.sample_rate_hz);
            if req.offset_hz.unsigned_abs() as f64 + req.span_hz as f64 > 0.45 * fs {
                bail!("offset_hz and span_hz must keep the band within ±{:.0} Hz of the LO", 0.45 * fs);
            }
            let lo = (req.freq_hz as i64 - req.offset_hz) as u64;
            tuner.apply(TuningPlan { preset, lo_hz: lo, control_hz: req.freq_hz }).await?;
            let board = tuner.hw();
            {
                let core = board.core.lock().await;
                core.set_spectrometer_integrations(1);
                core.set_spectrum_mode(false, false);
            }
            let bin_of = |hz: f64| ((hz - lo as f64) / fs * BINS as f64 + (BINS / 2) as f64).round() as usize;
            let band = bin_of(req.freq_hz as f64 - req.span_hz as f64)..bin_of(req.freq_hz as f64 + req.span_hz as f64) + 1;
            let frames: Arc<Mutex<Vec<FramePoint>>> = Arc::default();
            let into = frames.clone();
            let tap_band = band.clone();
            board.spectrum.set_tap(Some(Box::new(move |h, bytes| {
                let (bin, p) = band_peak(bytes, tap_band.clone());
                into.lock().unwrap_or_else(|e| e.into_inner()).push((h.sequence, h.sample_index, bin, p));
            })));
            self.wait(SETTLE).await?;

            let mut burst_hz = req.freq_hz as f64;
            if req.find {
                self.update(|s| {
                    s.state = "finding";
                    s.message = Some(format!("press the transmitter once ({:.3} MHz)", req.freq_hz as f64 / 1e6));
                });
                let (bin, _) = self.next_burst(&frames, req, true).await?;
                burst_hz = lo as f64 + (bin as f64 - (BINS / 2) as f64) * fs / BINS as f64;
                self.update(|s| s.message = Some(format!("found at {:.4} MHz; let go", burst_hz / 1e6)));
                self.until_quiet(&frames, req).await?;
                tuner.set_control(burst_hz.round() as u64).await?;
            }

            // The control lane's blocks, from its tuning on the burst.
            let stop = Arc::new(AtomicBool::new(false));
            let (tx, mut rx) = sync_channel::<Block>(1024);
            board.streams.control_streams(tx, stop.clone(), Arc::default());
            self.update(|s| {
                s.state = "armed";
                s.message = Some("wait".into());
            });
            board.core.lock().await.start_capture();
            frames.lock().unwrap_or_else(|e| e.into_inner()).clear();
            let mut lane: Vec<Point> = Vec::new();
            let measured = async {
                self.wait(SETTLE).await?;
                drain(&mut rx, preset.total_decim() as u64, &mut lane);
                self.wait(LEARN).await?;
                drain(&mut rx, preset.total_decim() as u64, &mut lane);
                let learned = learned_until(&frames);
                self.update(|s| {
                    s.message = Some(format!("press the transmitter again ({:.4} MHz)", burst_hz / 1e6));
                    s.state = "measuring";
                });
                self.next_burst_after(&frames, &mut rx, &mut lane, preset.total_decim() as u64, req, learned).await?;
                board.core.lock().await.trigger_capture(STOP_AFTER);
                self.update(|s| s.message = Some("seen; capturing".into()));
                self.wait(Duration::from_millis(400)).await?;
                drain(&mut rx, preset.total_decim() as u64, &mut lane);
                anyhow::Ok(learned)
            }
            .await;
            stop.store(true, Ordering::Relaxed);
            board.spectrum.set_tap(None);
            let learned = measured?;

            // The capture stops itself two sub-buffers after the trigger.
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let st = board.core.lock().await.capture_status();
                if st.stopped {
                    break;
                }
                if Instant::now() > deadline {
                    bail!("the capture did not stop after the trigger ({} sub-buffers)", st.buffers);
                }
                tokio::time::sleep(POLL).await;
            }
            let mut capture: Vec<Point> = Vec::new();
            let mut first = None;
            let status = {
                let mut core = board.core.lock().await;
                core.read_stopped_capture(|index, bytes| {
                    first.get_or_insert(index);
                    let iq: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
                    debug_assert_eq!(bytes.len() % CAPTURE_SAMPLE_BYTES, 0);
                    block_powers(&iq, index, 1, CAPTURE_BLOCK, &mut capture);
                })
            };
            let first = first.context("the capture holds nothing")?;

            let frames = std::mem::take(&mut *frames.lock().unwrap_or_else(|e| e.into_inner()));
            let mut t = Timing { burst_hz, lane_delay: preset.group_delay(), capture_overflow: status.overflow, ..Default::default() };
            t.frames_missed = frames.windows(2).map(|w| u64::from(w[1].0.wrapping_sub(w[0].0).wrapping_sub(1))).sum();
            t.capture = onset(&capture, first + (CAPTURE_LEARN_S * fs) as u64, req.threshold_db);
            let frame_points: Vec<Point> = frames.iter().map(|f| (f.1, f.3)).collect();
            t.frame = onset(&frame_points, learned, req.threshold_db);
            t.frame_sequence = t.frame.and_then(|p| frames.iter().find(|f| f.1 == p.index)).map(|f| f.0);
            let lane_learned = learned + t.lane_delay as u64;
            t.lane = onset(&lane, lane_learned, req.threshold_db).map(|p| Placed { index: (p.index as f64 - t.lane_delay).round() as u64, ..p });
            compare(&mut t);
            Ok(t)
        }

        /// The next frame whose band rises above the floor learned over the frames before it.
        async fn next_burst(&self, frames: &Mutex<Vec<FramePoint>>, req: &BurstRequest, learn_first: bool) -> Result<(usize, f64)> {
            if learn_first {
                frames.lock().unwrap_or_else(|e| e.into_inner()).clear();
                self.wait(LEARN).await?;
            }
            let floor = median(frames.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|f| f.3).collect()).context("no spectrum frames")?;
            let above = floor * 10f64.powf(req.threshold_db / 10.0);
            let deadline = Instant::now() + Duration::from_secs(req.wait_s);
            loop {
                if let Some(f) = frames.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|f| f.3 > above) {
                    return Ok((f.2, f.3));
                }
                if Instant::now() > deadline {
                    bail!("no burst within {} s", req.wait_s);
                }
                self.wait(POLL).await?;
            }
        }

        /// Wait until the band has been quiet for a while.
        async fn until_quiet(&self, frames: &Mutex<Vec<FramePoint>>, req: &BurstRequest) -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(req.wait_s);
            loop {
                frames.lock().unwrap_or_else(|e| e.into_inner()).clear();
                self.wait(QUIET).await?;
                let recent: Vec<f64> = frames.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|f| f.3).collect();
                let (lo, hi) = recent.iter().fold((f64::MAX, 0f64), |(a, b), &p| (a.min(p), b.max(p)));
                if !recent.is_empty() && db(hi) - db(lo) < req.threshold_db {
                    return Ok(());
                }
                if Instant::now() > deadline {
                    bail!("the transmitter did not stop within {} s", req.wait_s);
                }
            }
        }

        /// Wait for the burst after the learning (frames from sample `learned` on), draining the
        /// lane meanwhile.
        async fn next_burst_after(
            &self,
            frames: &Mutex<Vec<FramePoint>>,
            rx: &mut Receiver<Block>,
            lane: &mut Vec<Point>,
            decimation: u64,
            req: &BurstRequest,
            learned: u64,
        ) -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(req.wait_s);
            loop {
                drain(rx, decimation, lane);
                let seen = {
                    let f = frames.lock().unwrap_or_else(|e| e.into_inner());
                    let points: Vec<Point> = f.iter().map(|f| (f.1, f.3)).collect();
                    onset(&points, learned, req.threshold_db).is_some()
                };
                if seen {
                    return Ok(());
                }
                if Instant::now() > deadline {
                    bail!("no burst within {} s", req.wait_s);
                }
                self.wait(POLL).await?;
            }
        }
    }

    /// The sample index the frames had reached when the learning ended.
    fn learned_until(frames: &Mutex<Vec<FramePoint>>) -> u64 {
        frames.lock().unwrap_or_else(|e| e.into_inner()).last().map_or(0, |f| f.1 + TRANSFORM)
    }

    fn drain(rx: &mut Receiver<Block>, decimation: u64, lane: &mut Vec<Point>) {
        while let Ok(b) = rx.try_recv() {
            block_powers(&b.iq, b.index, decimation, LANE_BLOCK, lane);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rise_above_the_floor_is_placed_at_its_first_point() {
        let mut pts: Vec<Point> = (0..100).map(|i| (i * 10, 1.0 + (i % 3) as f64 * 0.1)).collect();
        pts[70].1 = 200.0;
        pts[71].1 = 300.0;
        let p = onset(&pts, 500, 15.0).unwrap();
        assert_eq!(p.index, 700);
        assert!((p.floor_db - db(1.1)).abs() < 1e-9);
        assert!(onset(&pts, 720, 15.0).is_none(), "nothing after the rise");
        assert!(onset(&pts, 0, 15.0).is_none(), "no floor learned");
    }

    #[test]
    fn block_powers_carry_each_block_s_first_index() {
        let iq: Vec<i16> = (0..16).map(|i| if i < 8 { 1 } else { 10 }).collect();
        let mut out = Vec::new();
        block_powers(&iq, 1000, 40, 4, &mut out);
        assert_eq!(out, vec![(1000, 2.0), (1160, 200.0)]);
    }

    #[test]
    fn the_band_s_peak_reads_mantissa_and_exponent() {
        let mut frame = vec![0u8; 4096 * 8];
        let put = |f: &mut [u8], bin: usize, w: u64| f[8 * bin..8 * bin + 8].copy_from_slice(&w.to_le_bytes());
        put(&mut frame, 100, 1000);
        put(&mut frame, 105, 300 | 2 << 56);
        put(&mut frame, 200, 1 << 40);
        assert_eq!(band_peak(&frame, 90..150), (105, 4800.0));
    }

    #[test]
    fn the_starts_pass_within_one_transform() {
        let at = |index| Some(Placed { index, db: 0.0, floor_db: 0.0 });
        let mut t = Timing { capture: at(100_000), lane: at(100_900), frame: at(98_304), ..Default::default() };
        compare(&mut t);
        assert_eq!((t.lane_minus_capture, t.frame_minus_capture, t.pass), (Some(900), Some(-1696), Some(true)));
        t.frame = at(106_496);
        compare(&mut t);
        assert_eq!(t.pass, Some(false), "the frame two transforms late");
        t.frame = None;
        compare(&mut t);
        assert_eq!(t.pass, None);
    }
}
