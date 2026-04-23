//! Auto-PPM calibration: find the crystal-trim error and apply it to
//! the DDC NCO without a reboot or CLI change.
//!
//! Two-stage loop, both reading hardware that already exists:
//!
//!   Stage A — wideband FFT peak-find. Grab one frame from the
//!             spectrometer DMA, look for the strongest bin within
//!             ±10 kHz of the expected control-channel offset
//!             (`control_freq - rx_lo`), parabolic-interpolate for
//!             sub-bin precision, and apply the resulting delta to
//!             the DDC NCO. Gets the error into PLL capture range.
//!
//!   Stage B — PLL residual. Wait for the Costas loop to re-settle,
//!             then sample `pll_dbg` (Q2.13 phase/symbol) at ~10 Hz
//!             for a few seconds. Mean converts to a Hz residual
//!             (`mean * sym_rate / (2π * 2^13)`), added to the NCO
//!             shift. The last ~Hz of crystal trim lives here —
//!             wideband alone can't reach sub-bin.
//!
//! The DDC NCO is programmed to `(control_freq - rx_lo) + lo_shift_hz`.
//! Stage A + B compute `lo_shift_hz`; boot ppm from `--lo-ppm` is a
//! starting point, fully overridden by the calibration result.

#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use anyhow::{anyhow, Result};

#[cfg(target_os = "linux")]
use crate::httpd::AppState;
#[cfg(target_os = "linux")]
use crate::services::spectrum::{wideband_power_db, WIDEBAND_FFT_SIZE};

/// On-disk persistence path. Survives reboots so the board doesn't
/// need to re-learn the crystal trim every time power cycles.
/// `/mnt/jffs2` is the Tezuka persistent JFFS2 flash partition (same
/// place SSL certs live in `S50p25-httpd-certificates`). It is
/// mounted by the init scripts before p25-httpd starts, so this
/// path is reachable by the time the calibration task runs.
/// `/var/lib` on this board is tmpfs and does NOT survive reboots.
#[cfg(target_os = "linux")]
pub const PPM_CAL_FILE: &str = "/mnt/jffs2/p25-ppm-cal.json";

/// JSON schema for `PPM_CAL_FILE`.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PersistedPpm {
    pub lo_shift_hz:       i64,
    pub lo_ppm:            f64,
    pub rx_lo_hz:          i64,
    pub control_freq_hz:   u64,
    pub unix_secs:         i64,
    pub method:            String,
}

/// P25 control-channel symbol rate. The PLL in `LsmPllUpdate` runs
/// on `symbol_strobe`, so its Q2.13 output scales by this rate when
/// converting to Hz.
#[cfg(target_os = "linux")]
const SYM_RATE_HZ: f64 = 4800.0;

/// Search window around the expected control offset, in Hz. Wide
/// enough to handle the ±5 ppm worst-case AD9361 crystal trim at
/// 1 GHz (= ±5 kHz), narrow enough that a nearby traffic channel
/// or broadcast spur can't false-lock the auto-tune.
#[cfg(target_os = "linux")]
const SEARCH_WINDOW_HZ: f64 = 10_000.0;

/// How long to let the PLL settle after a stage-A NCO move before
/// sampling `pll_dbg`. The Costas loop bandwidth is ~30 Hz, so 3 s
/// is ~90 time-constants — more than enough.
#[cfg(target_os = "linux")]
const PLL_SETTLE_SECS: u64 = 3;

/// Number of wideband FFT frames to average in stage A. A single
/// frame has ±1-2 dB bin-level noise which turns into ~100 Hz
/// peak-position jitter through parabolic interpolation. √N
/// averaging drops that by ~2.8× at N=8 — enough to make stage A
/// converge run-to-run.
#[cfg(target_os = "linux")]
const STAGE_A_FRAMES: usize = 8;

/// Number of `pll_dbg` samples to take for the stage-B mean.
#[cfg(target_os = "linux")]
const PLL_SAMPLES: usize = 30;

/// Inter-sample delay for stage B. 30 samples × 100 ms = 3 s,
/// capturing ~1440 symbol periods — well above 1/f noise in the
/// loop.
#[cfg(target_os = "linux")]
const PLL_SAMPLE_INTERVAL_MS: u64 = 100;

/// Outcome of one full calibration pass. Returned as JSON from
/// `POST /api/ppm_calibrate` so operators can see what happened
/// even without dashboard rendering.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct CalibrationResult {
    pub rx_lo_hz:               f64,
    pub control_freq_hz:        f64,
    pub sample_rate_hz:         f64,
    pub expected_offset_hz:     f64,
    pub stage_a_peak_bin:       f64,
    pub stage_a_peak_db:        f32,
    pub stage_a_actual_hz:      f64,
    pub stage_a_delta_hz:       f64,
    pub pll_mean_q213:          f64,
    pub pll_samples:            usize,
    pub stage_b_residual_hz:    f64,
    pub final_lo_shift_hz:      f64,
    pub final_lo_ppm:           f64,
    pub duration_ms:            u64,
}

/// Run one full stage-A + stage-B calibration. Assumes the HDL chain
/// is already tuned (non-zero rx_lo, preset applied). Applies the
/// result to the DDC NCO on success.
#[cfg(target_os = "linux")]
pub async fn run_calibration(
    state: &Arc<AppState>,
) -> Result<CalibrationResult> {
    let start = Instant::now();

    let rx_lo = state.current_rx_lo.load(Ordering::Relaxed) as f64;
    let sample_rate = state.current_sample_rate_hz.load(
        Ordering::Relaxed) as f64;
    let control_freq = state.boot_control_freq as f64;
    if rx_lo <= 0.0 || sample_rate <= 0.0 {
        return Err(anyhow!(
            "radio not tuned yet (rx_lo={rx_lo} sample_rate={sample_rate})"));
    }
    let expected_offset = control_freq - rx_lo;

    // ── Stage A: wideband FFT peak-find ─────────────────────────
    // Average STAGE_A_FRAMES independent frames (power-domain mean,
    // linear sum of mag_db is wrong since dB isn't power; convert
    // back to linear, sum, convert back). This tames the ~100 Hz
    // per-frame peak jitter that single-frame parabolic interp
    // picks up from adjacent-bin dB noise.
    let mut mag_linear: Vec<f64> =
        vec![0.0; crate::services::spectrum::WIDEBAND_FFT_SIZE];
    let mut frames_seen: usize = 0;
    for _ in 0..STAGE_A_FRAMES {
        let bytes = match grab_wideband_frame(
            state, Duration::from_millis(500)).await {
            Ok(b) => b,
            Err(_) => break,
        };
        let db = wideband_power_db(&bytes);
        if db.len() != mag_linear.len() {
            break;
        }
        for (i, &d) in db.iter().enumerate() {
            mag_linear[i] += 10f64.powf(d as f64 / 10.0);
        }
        frames_seen += 1;
    }
    if frames_seen == 0 {
        return Err(anyhow!("no wideband frames captured"));
    }
    let mag_db: Vec<f32> = mag_linear.iter()
        .map(|&v| (10.0 * (v / frames_seen as f64).log10()) as f32)
        .collect();
    let (peak_bin_f, peak_db, actual_offset) =
        find_peak_near(&mag_db, sample_rate, expected_offset,
                       SEARCH_WINDOW_HZ)?;

    // The stage-A delta is the error in Hz between the expected
    // control-channel offset and the interpolated peak.
    let stage_a_delta = actual_offset - expected_offset;
    let new_nco = expected_offset + stage_a_delta;    // = actual_offset
    apply_ddc_frequency(state, new_nco, sample_rate).await?;

    // ── Stage B: PLL residual ──────────────────────────────────
    tokio::time::sleep(Duration::from_secs(PLL_SETTLE_SECS)).await;

    let mut pll_sum: i64 = 0;
    let mut pll_n: usize = 0;
    for _ in 0..PLL_SAMPLES {
        let core = state.ip_core.lock().await;
        let (pll_q213, _) = core.lsm_debug();
        drop(core);
        pll_sum += pll_q213 as i64;
        pll_n += 1;
        tokio::time::sleep(
            Duration::from_millis(PLL_SAMPLE_INTERVAL_MS)).await;
    }
    let pll_mean = if pll_n > 0 {
        pll_sum as f64 / pll_n as f64
    } else { 0.0 };

    // pll_dbg is Q2.13 radians per symbol. freq_residual_hz =
    // pll_mean * (sym_rate / (2π * 2^13)). Sign: positive pll_out
    // means the PLL is rotating samples by +phase per symbol, which
    // implies the RF signal has a NEGATIVE residual. To null it we
    // adjust the NCO by `+pll_residual_hz` (move the downconverter
    // in the same direction the PLL was correcting).
    let hz_per_q213 = SYM_RATE_HZ / (2.0 * std::f64::consts::PI
                                     * 8192.0);
    let pll_residual_hz = pll_mean * hz_per_q213;
    let final_lo_shift = stage_a_delta + pll_residual_hz;
    let final_nco = expected_offset + final_lo_shift;
    apply_ddc_frequency(state, final_nco, sample_rate).await?;

    // lo_ppm that produces final_lo_shift at rx_lo (matching
    // main.rs::nco_lo_shift_hz formula: shift = -ppm * 1e-6 * rx_lo).
    let final_lo_ppm = -final_lo_shift / (rx_lo * 1e-6);

    // Persist to AppState so /api/ppm can read it. A full cal also
    // resets the baseline — this IS the new "known-good" reference
    // that the periodic fine-tune is allowed to wander ±0.2 ppm off.
    let hz_int = final_lo_shift.round() as i64;
    state.current_lo_shift_hz.store(hz_int, Ordering::Relaxed);
    state.baseline_lo_shift_hz.store(hz_int, Ordering::Relaxed);

    // Event-log the calibration so /api/log has an audit trail of
    // shift changes, not just tracing/stdout.
    state.event_log.push(
        crate::services::event_log::LogCategory::System,
        format!("auto-PPM calibration: stage A delta {:+.1} Hz, \
                 stage B residual {:+.1} Hz -> shift {:+} Hz \
                 ({:+.4} ppm, baseline reset)",
                stage_a_delta, pll_residual_hz,
                hz_int, final_lo_ppm),
        serde_json::json!({
            "kind":             "ppm.calibrate",
            "stage_a_delta_hz": stage_a_delta,
            "stage_b_residual_hz": pll_residual_hz,
            "final_lo_shift_hz": hz_int,
            "final_lo_ppm":     final_lo_ppm,
        }),
    );
    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    state.last_ppm_cal_unix_secs.store(unix_secs, Ordering::Relaxed);

    // Persist to disk so the next boot picks up the calibration
    // without re-running it. Best-effort — a permissions or I/O
    // failure here doesn't fail the calibration itself.
    //
    // Guard: skip persistence if we haven't got a real wall clock
    // yet (pre-NTP the kernel reports seconds-since-boot, i.e. a
    // 1970-ish value). The cal value itself is fine; we just don't
    // want to stamp the file with an invalid timestamp and have it
    // look "1h ago" forever.
    const EPOCH_YEAR_2001: i64 = 1_000_000_000;
    if unix_secs >= EPOCH_YEAR_2001 {
        let persisted = PersistedPpm {
            lo_shift_hz:      hz_int,
            lo_ppm:           final_lo_ppm,
            rx_lo_hz:         rx_lo as i64,
            control_freq_hz:  state.boot_control_freq,
            unix_secs,
            method:           "auto_stage_a_b".to_string(),
        };
        if let Err(e) = save_persisted(&persisted) {
            tracing::warn!("auto-PPM: persistence failed: {e:#}");
        }
    } else {
        tracing::info!(
            "auto-PPM: skipping persistence (pre-NTP unix_secs={unix_secs})");
    }

    Ok(CalibrationResult {
        rx_lo_hz:            rx_lo,
        control_freq_hz:     control_freq,
        sample_rate_hz:      sample_rate,
        expected_offset_hz:  expected_offset,
        stage_a_peak_bin:    peak_bin_f,
        stage_a_peak_db:     peak_db,
        stage_a_actual_hz:   actual_offset,
        stage_a_delta_hz:    stage_a_delta,
        pll_mean_q213:       pll_mean,
        pll_samples:         pll_n,
        stage_b_residual_hz: pll_residual_hz,
        final_lo_shift_hz:   final_lo_shift,
        final_lo_ppm,
        duration_ms:         start.elapsed().as_millis() as u64,
    })
}

/// Block until the spectrometer publishes a new frame, or fail.
#[cfg(target_os = "linux")]
async fn grab_wideband_frame(
    state: &Arc<AppState>,
    deadline: Duration,
) -> Result<Vec<u8>> {
    let start = Instant::now();
    loop {
        {
            let mut core = state.ip_core.lock().await;
            if let Some(buf) = core.read_wideband_spec_buffer() {
                return Ok(buf.to_vec());
            }
        }
        if start.elapsed() > deadline {
            return Err(anyhow!(
                "no wideband spectrum frame within {deadline:?}"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Apply an NCO offset to the control DDC. Shared entry point so the
/// calibration doesn't drift from the main.rs boot path.
#[cfg(target_os = "linux")]
async fn apply_ddc_frequency(
    state: &Arc<AppState>,
    nco_offset: f64,
    sample_rate: f64,
) -> Result<()> {
    let core = state.ip_core.lock().await;
    core.set_ddc_frequency(nco_offset, sample_rate)?;
    Ok(())
}

/// Parabolic-interpolated peak finder restricted to `[expected ±
/// window]`. Returns `(bin_float, db, actual_offset_hz)` where
/// `actual_offset_hz = (bin_float - N/2) * bin_width`.
#[cfg(target_os = "linux")]
fn find_peak_near(
    mag_db: &[f32],
    sample_rate: f64,
    expected_offset: f64,
    window_hz: f64,
) -> Result<(f64, f32, f64)> {
    let n = WIDEBAND_FFT_SIZE as f64;
    let bw = sample_rate / n;
    let center_bin = n / 2.0;
    let expected_bin = center_bin + expected_offset / bw;
    let window_bins = (window_hz / bw).ceil() as isize;

    let lo = (expected_bin as isize - window_bins).max(0) as usize;
    let hi = ((expected_bin as isize + window_bins) as usize)
        .min(mag_db.len() - 1);
    if hi <= lo {
        return Err(anyhow!(
            "peak-search window empty (lo={lo} hi={hi})"));
    }

    let mut best_bin = lo;
    let mut best_db = mag_db[lo];
    for b in lo..=hi {
        if mag_db[b] > best_db {
            best_db = mag_db[b];
            best_bin = b;
        }
    }
    // Parabolic interpolation using immediate neighbours.
    let delta = if best_bin > 0 && best_bin + 1 < mag_db.len() {
        let y1 = mag_db[best_bin - 1] as f64;
        let y2 = mag_db[best_bin] as f64;
        let y3 = mag_db[best_bin + 1] as f64;
        let denom = y1 - 2.0 * y2 + y3;
        if denom.abs() < 1e-6 { 0.0 }
        else { 0.5 * (y1 - y3) / denom }
    } else { 0.0 };
    let peak_bin_f = best_bin as f64 + delta;
    let actual_offset = (peak_bin_f - center_bin) * bw;
    Ok((peak_bin_f, best_db, actual_offset))
}

/// Load a previous calibration from disk, if present. Returns `None`
/// silently on missing file or any parse error — boot should continue
/// with the CLI `--lo-ppm` value in those cases.
#[cfg(target_os = "linux")]
pub fn load_persisted() -> Option<PersistedPpm> {
    let path = Path::new(PPM_CAL_FILE);
    if !path.exists() { return None; }
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<PersistedPpm>(&bytes).ok()
}

#[cfg(target_os = "linux")]
fn save_persisted(p: &PersistedPpm) -> Result<()> {
    let path = Path::new(PPM_CAL_FILE);
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let body = serde_json::to_vec_pretty(p)?;
    std::fs::write(path, body)?;
    Ok(())
}

/// Spawn a one-shot background task that waits for the PLL to reach
/// acquisition (system_acquired = true on the control-channel
/// decoder), then runs auto-PPM. Called from `main` once at boot so
/// the board self-calibrates after its first lock. Idempotent — if
/// the system never acquires, the task exits after the deadline.
#[cfg(target_os = "linux")]
pub fn spawn_boot_autoppm(state: Arc<AppState>) {
    tokio::spawn(async move {
        // Give the PLL time to converge on the boot NCO before
        // running stage A. 30 s covers the worst-case
        // "cold-boot + find sync" case we see in logs.
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let acquired = {
                let dec = state.lsm_decoder.read().await;
                dec.system.wacn.is_some()
            };
            if acquired { break; }
            if Instant::now() > deadline {
                tracing::info!(
                    "auto-PPM: boot-trigger timed out waiting for system_acquired; \
                     skipping initial calibration");
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        // 15 s cushion once acquired — `system_acquired` fires on
        // the FIRST valid NID, but the PLL integrator keeps drifting
        // for another 10+ s as BCH and IID settle. Running the boot
        // cal too early gave noisy stage-A results (-0.26 ppm one
        // run, -0.71 the next). 15 s lets both stages converge.
        tokio::time::sleep(Duration::from_secs(15)).await;
        match run_calibration(&state).await {
            Ok(r) => tracing::info!(
                "auto-PPM (boot): final_lo_ppm={:.4} shift={:.0} Hz  \
                 stage_a={:.0} stage_b={:.1}  duration={}ms",
                r.final_lo_ppm, r.final_lo_shift_hz,
                r.stage_a_delta_hz, r.stage_b_residual_hz, r.duration_ms),
            Err(e) => tracing::warn!(
                "auto-PPM (boot): calibration failed: {e:#}"),
        }
    });
}

/// Interval between periodic fine-tune checks.
#[cfg(target_os = "linux")]
const FINE_TUNE_INTERVAL_SECS: u64 = 900;    // 15 min

/// Threshold (|PLL residual| in Hz) above which a fine-tune commits
/// a correction. Below this the loop is good enough — committing
/// ~10 Hz tweaks every 15 min would be churn without audible benefit.
#[cfg(target_os = "linux")]
const FINE_TUNE_THRESHOLD_HZ: f64 = 30.0;

/// Maximum correction the fine-tune loop is allowed to apply in a
/// single iteration. Crystal trim physically drifts at <0.1 ppm/°C;
/// over 15 min at most tens of Hz. A reading larger than this is
/// almost always a transient signal disturbance (brief fade, traffic
/// retune glitch, ADC overload) — clamp so the loop can't accumulate
/// catastrophically from bad data. Hit 2026-04-23: running 11+ h
/// without this cap drifted shift to ~5 ppm via 16+ bad iterations.
#[cfg(target_os = "linux")]
const FINE_TUNE_MAX_STEP_HZ: f64 = 150.0;

/// Hard cap on the absolute shift the fine-tune loop can drive to.
/// Matches the boot loader's MAX_PLAUSIBLE_HZ. AD9361 crystals don't
/// land outside ±1 ppm on healthy hardware (800 Hz at 858 MHz LO).
#[cfg(target_os = "linux")]
const FINE_TUNE_MAX_ABS_SHIFT_HZ: f64 = 1000.0;

/// Minimum `agc_product` (gain × mag, ideal = 1.0) required before
/// we trust `pll_dbg` enough to fine-tune on it. Below this, the
/// AGC loop is wobbling and the PLL error reading is noise, not a
/// real frequency offset.
#[cfg(target_os = "linux")]
const FINE_TUNE_MIN_AGC_PRODUCT: f64 = 0.6;

/// How long the control decoder can be deacquired before the
/// fine-tune task resets `current_lo_shift_hz` to 0 as a baseline.
/// Premise: if we've been unable to decode for this long, the
/// persisted shift can't be trusted — better to start fresh and
/// let the boot / manual auto-PPM path reacquire.
#[cfg(target_os = "linux")]
const SYNC_LOST_RESET_SECS: u64 = 300;

/// Maximum ppm fine-tune is allowed to wander off the baseline that
/// the last full calibration / operator override established. A full
/// recal (POST /api/ppm_calibrate) or manual PUT /api/ppm resets
/// this baseline. Fine-tune is a tracking correction, NOT a redefine;
/// if the crystal has drifted more than this we want operator
/// attention, not silent accumulation.
#[cfg(target_os = "linux")]
const FINE_TUNE_MAX_PPM_OFF_BASELINE: f64 = 0.2;

/// Spawn a long-lived task that periodically samples `pll_dbg`,
/// computes the residual in Hz, and applies it to the DDC NCO if it
/// exceeds `FINE_TUNE_THRESHOLD_HZ`. This is stage-B-only — no
/// wideband FFT, no settle wait — because the loop is already
/// tracking and we just need to slide the reference. Runs for the
/// life of the process.
///
/// Crystal drift over temperature is typically <0.1 ppm/°C, so
/// over an hour of ambient-temperature change you might see 10-30 Hz
/// at 1 GHz. This task catches that slowly without interrupting the
/// decode path (no NCO discontinuity large enough to unlock the PLL).
#[cfg(target_os = "linux")]
pub fn spawn_periodic_fine_tune(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut sync_lost_since: Option<Instant> = None;
        loop {
            tokio::time::sleep(
                Duration::from_secs(FINE_TUNE_INTERVAL_SECS)).await;

            let acquired = {
                let dec = state.lsm_decoder.read().await;
                dec.system.wacn.is_some()
            };

            // Sync-loss guard. If we've been unable to acquire for
            // `SYNC_LOST_RESET_SECS`, the current shift is almost
            // certainly wrong (that's probably WHY we're not
            // acquiring). Reset to 0 as baseline so the next
            // reacquisition starts from a known-good state rather
            // than compounding whatever drove us off.
            if !acquired {
                let lost = match sync_lost_since {
                    Some(t) => t,
                    None => {
                        sync_lost_since = Some(Instant::now());
                        continue;
                    }
                };
                if lost.elapsed() >= Duration::from_secs(SYNC_LOST_RESET_SECS) {
                    let old = state.current_lo_shift_hz
                        .load(Ordering::Relaxed);
                    if old != 0 {
                        reset_shift_to_zero(&state).await;
                        tracing::warn!(
                            "auto-PPM (fine-tune): sync lost for {:.0}s, \
                             shift reset to 0 (was {:+} Hz)",
                            lost.elapsed().as_secs_f64(), old);
                        state.event_log.push(
                            crate::services::event_log::LogCategory::System,
                            format!("auto-PPM RESET: sync lost {:.0}s, \
                                     shift 0 (was {:+} Hz)",
                                     lost.elapsed().as_secs_f64(), old),
                            serde_json::json!({
                                "kind":        "ppm.sync_lost_reset",
                                "lost_secs":   lost.elapsed().as_secs_f64(),
                                "old_shift":   old,
                            }),
                        );
                        sync_lost_since = Some(Instant::now());
                    }
                }
                continue;
            }
            sync_lost_since = None;

            // Decode-health + AGC-health gate. If decode is visibly
            // clean (sync_distance=0, nid_valid, low n_errors) AND
            // the AGC loop is tracking (agc_product near 1.0), we
            // trust the PLL and let it measure. If either is soft,
            // we skip — a wobbly PLL reading isn't a real offset.
            //
            // Operator ask 2026-04-23: "if we have 100% decodes we
            // should not adjust". Tight sync + no NID errors = the
            // decode path is healthy. Why push the shift around?
            let (agc_product, health_ok) = {
                let core = state.ip_core.lock().await;
                let (g_q9_7, m_q1_15) = core.lsm_agc_debug();
                let st = core.lsm_status();
                let g = g_q9_7 as f64 / 128.0;
                let m = m_q1_15 as f64 / 32768.0;
                let product = g * m;
                let decode_healthy = st.nid_valid
                    && st.sync_distance == 0
                    && st.n_errors <= 2;
                (product, decode_healthy)
            };
            if agc_product < FINE_TUNE_MIN_AGC_PRODUCT {
                tracing::debug!(
                    "auto-PPM (fine-tune): skip, agc_product {:.3} < \
                     {:.2} (loop not tracking)",
                    agc_product, FINE_TUNE_MIN_AGC_PRODUCT);
                continue;
            }
            if !health_ok {
                tracing::debug!(
                    "auto-PPM (fine-tune): skip, decode not clean \
                     (sync_distance / n_errors / nid_valid check)");
                continue;
            }

            // Sample PLL briefly — this runs while decode is live so
            // we keep it short to minimise lock contention on ip_core.
            const N: usize = 20;
            let mut sum: i64 = 0;
            for _ in 0..N {
                let core = state.ip_core.lock().await;
                let (pll, _) = core.lsm_debug();
                drop(core);
                sum += pll as i64;
                tokio::time::sleep(
                    Duration::from_millis(PLL_SAMPLE_INTERVAL_MS)).await;
            }
            let pll_mean = sum as f64 / N as f64;
            let hz_per_q213 = SYM_RATE_HZ / (2.0 * std::f64::consts::PI
                                             * 8192.0);
            let residual_hz = pll_mean * hz_per_q213;
            if residual_hz.abs() < FINE_TUNE_THRESHOLD_HZ {
                continue;
            }

            // Per-iteration cap. A reading beyond this is almost
            // always a transient; clamp so one bad sample can't
            // accumulate catastrophically over many iterations.
            let clamped_residual = residual_hz.clamp(
                -FINE_TUNE_MAX_STEP_HZ, FINE_TUNE_MAX_STEP_HZ);
            if residual_hz.abs() > FINE_TUNE_MAX_STEP_HZ {
                tracing::warn!(
                    "auto-PPM (fine-tune): residual {:+.1} Hz > cap \
                     ±{:.0}; clamping to {:+.1}",
                    residual_hz, FINE_TUNE_MAX_STEP_HZ,
                    clamped_residual);
            }

            let sample_rate = state.current_sample_rate_hz
                .load(Ordering::Relaxed) as f64;
            let rx_lo = state.current_rx_lo
                .load(Ordering::Relaxed) as f64;
            let control_freq = state.current_control_freq
                .load(Ordering::Relaxed) as f64;
            let old_shift = state.current_lo_shift_hz
                .load(Ordering::Relaxed) as f64;
            let baseline = state.baseline_lo_shift_hz
                .load(Ordering::Relaxed) as f64;
            let new_shift = old_shift + clamped_residual;

            // Absolute-value guard. AD9361 crystal drift doesn't
            // physically reach ±1 ppm on healthy hardware; anything
            // past that limit indicates accumulated error and we
            // refuse to apply it. The user can still manually
            // override via PUT /api/ppm.
            if new_shift.abs() > FINE_TUNE_MAX_ABS_SHIFT_HZ {
                tracing::warn!(
                    "auto-PPM (fine-tune): new_shift {:+.0} Hz would \
                     exceed ±{:.0} Hz envelope; skipping. Check RF.",
                    new_shift, FINE_TUNE_MAX_ABS_SHIFT_HZ);
                continue;
            }

            // Baseline guard (operator-requested 2026-04-23).
            // Fine-tune is a tracking correction, NOT a redefinition
            // of the setpoint. Anything more than ±0.2 ppm off the
            // baseline (which is reset by full recal / manual
            // override) is unusual enough to warrant refusing and
            // surfacing an event for operator review.
            let max_off_baseline =
                FINE_TUNE_MAX_PPM_OFF_BASELINE * rx_lo * 1e-6;
            let off_baseline = (new_shift - baseline).abs();
            if off_baseline > max_off_baseline {
                tracing::warn!(
                    "auto-PPM (fine-tune): would push {:+.0} Hz off \
                     baseline (limit {:.0} Hz = ±{:.1} ppm); skipping",
                    new_shift - baseline, max_off_baseline,
                    FINE_TUNE_MAX_PPM_OFF_BASELINE);
                state.event_log.push(
                    crate::services::event_log::LogCategory::System,
                    format!("auto-PPM fine-tune REFUSED: \
                             {:+.0} Hz off baseline (cap {:.0} Hz = \
                             ±{:.1} ppm)",
                             new_shift - baseline, max_off_baseline,
                             FINE_TUNE_MAX_PPM_OFF_BASELINE),
                    serde_json::json!({
                        "kind":         "ppm.finetune.refused",
                        "residual_hz":  residual_hz,
                        "old_shift":    old_shift,
                        "new_shift":    new_shift,
                        "baseline":     baseline,
                        "off_baseline": new_shift - baseline,
                        "limit_hz":     max_off_baseline,
                    }),
                );
                continue;
            }

            let new_nco = control_freq - rx_lo + new_shift;
            let applied = {
                let core = state.ip_core.lock().await;
                core.set_ddc_frequency(new_nco, sample_rate).is_ok()
            };
            if !applied { continue; }
            state.current_lo_shift_hz.store(
                new_shift.round() as i64, Ordering::Relaxed);
            let unix_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64).unwrap_or(0);
            state.last_ppm_cal_unix_secs.store(
                unix_secs, Ordering::Relaxed);
            let new_ppm = -new_shift / (rx_lo * 1e-6);
            tracing::info!(
                "auto-PPM (fine-tune): residual {:+.1} Hz (raw {:+.1}) \
                 agc_product {:.3} -> shift {:+.0} Hz ({:+.4} ppm)",
                clamped_residual, residual_hz, agc_product, new_shift,
                new_ppm);
            state.event_log.push(
                crate::services::event_log::LogCategory::System,
                format!("auto-PPM fine-tune: residual {:+.1} Hz -> \
                         shift {:+.0} Hz ({:+.4} ppm)",
                         clamped_residual, new_shift, new_ppm),
                serde_json::json!({
                    "kind":             "ppm.finetune",
                    "residual_hz_raw":  residual_hz,
                    "residual_hz_used": clamped_residual,
                    "agc_product":      agc_product,
                    "old_shift":        old_shift,
                    "new_shift":        new_shift,
                    "baseline":         baseline,
                    "new_lo_ppm":       new_ppm,
                }),
            );
            if unix_secs >= 1_000_000_000 {
                let persisted = PersistedPpm {
                    lo_shift_hz:     new_shift.round() as i64,
                    lo_ppm:          -new_shift / (rx_lo * 1e-6),
                    rx_lo_hz:        rx_lo as i64,
                    control_freq_hz: state.boot_control_freq,
                    unix_secs,
                    method:          "fine_tune".to_string(),
                };
                let _ = save_persisted(&persisted);
            }
        }
    });
}

/// Reset `current_lo_shift_hz` to 0 and reprogram the DDC NCO to
/// match. Used when sync is lost long enough that we stop trusting
/// the current shift.
#[cfg(target_os = "linux")]
async fn reset_shift_to_zero(state: &Arc<AppState>) {
    state.current_lo_shift_hz.store(0, Ordering::Relaxed);
    state.baseline_lo_shift_hz.store(0, Ordering::Relaxed);
    let sample_rate = state.current_sample_rate_hz
        .load(Ordering::Relaxed) as f64;
    let rx_lo = state.current_rx_lo
        .load(Ordering::Relaxed) as f64;
    let control_freq = state.current_control_freq
        .load(Ordering::Relaxed) as f64;
    let nco_offset = control_freq - rx_lo;
    let core = state.ip_core.lock().await;
    let _ = core.set_ddc_frequency(nco_offset, sample_rate);
    // Also overwrite the persisted file so a reboot picks up 0,
    // not the stale bad value that got us here.
    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64).unwrap_or(0);
    if unix_secs >= 1_000_000_000 {
        let persisted = PersistedPpm {
            lo_shift_hz:     0,
            lo_ppm:          0.0,
            rx_lo_hz:        rx_lo as i64,
            control_freq_hz: state.boot_control_freq,
            unix_secs,
            method:          "sync_lost_reset".to_string(),
        };
        let _ = save_persisted(&persisted);
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub struct CalibrationResult;
