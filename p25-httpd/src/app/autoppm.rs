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
#[cfg(any(target_os = "linux", test))]
const SYM_RATE_HZ: f64 = 4800.0;

/// `pll_dbg` (Q2.13 rad/symbol) as a frequency in Hz.
#[cfg(any(target_os = "linux", test))]
pub fn pll_q213_to_hz(pll_q213: f64) -> f64 {
    pll_q213 * SYM_RATE_HZ / (2.0 * std::f64::consts::PI * 8192.0)
}

/// Best estimate of the correct `lo_shift_hz` from one `pll_dbg`
/// reading taken while `shift_hz` was applied.
///
/// The residual reads NCO minus signal: positive when the NCO sits
/// above the carrier. Bench-measured 2026-09-26 on a cabled replay
/// (doc/changes/055_autoppm_sign_fix.md): stepping `lo_shift_hz`
/// 370/470/570 gave median residuals −87/+4/+106 Hz, and moving the
/// transmitter +105 Hz moved it −106 Hz. So the shift must move by
/// MINUS the residual. The previous `shift + residual` made the fixed
/// point repelling: each apply doubled the error (the 2026-04-24
/// "positive feedback" walk from −0.535 to +1.11 ppm).
#[cfg(any(target_os = "linux", test))]
pub fn true_shift_estimate_hz(shift_hz: f64, pll_q213: f64) -> f64 {
    shift_hz - pll_q213_to_hz(pll_q213)
}

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
    // Change 070: the control channel tuned now (a site switch moves it).
    let control_freq = state.current_control_freq.load(Ordering::Relaxed) as f64;
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

    // pll_dbg is Q2.13 radians per symbol and reads NCO minus
    // signal: positive means the NCO sits above the carrier, so the
    // shift moves by MINUS the residual (see true_shift_estimate_hz).
    let pll_residual_hz = pll_q213_to_hz(pll_mean);
    let final_lo_shift = true_shift_estimate_hz(stage_a_delta, pll_mean);
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

    // Clear the tracker ring — stale pre-recal estimates would
    // contaminate the trimmed mean and drag shift back toward the
    // old (wrong) value. Bump last_shift_change_ms so the sampler
    // skips for PLL_TRACKER_SETTLE_MS while the Costas loop
    // reconverges on the newly-applied NCO position.
    if let Ok(mut ring) = state.ppm_tracker_ring.lock() {
        ring.clear();
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    state.ppm_last_shift_change_ms.store(now_ms, Ordering::Relaxed);
    // Stamp the recal anchor. The tracker's apply gate won't move
    // shift more than ±auto_ppm_anchor_hz from this until the next
    // forced recal.
    state.last_recal_shift_hz.store(hz_int, Ordering::Relaxed);

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
            control_freq_hz:  state.current_control_freq.load(Ordering::Relaxed),
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
pub fn save_persisted(p: &PersistedPpm) -> Result<()> {
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

// ── Continuous PPM tracker (replaces the old fine-tune task) ─────
// Approach 2026-04-24: instead of a discrete fire-every-15-min
// correction with thresholds and gates that misfire, run a
// continuous low-rate sampler that maintains a trimmed-mean
// estimate of the PLL residual, and apply it on a slow cadence.
// The PPM correction is a single scalar applied to the DDC NCO —
// it does not move the AD9361 LO and does not retune anything —
// so smooth continuous updates are safe.
//
// Why this design:
//   - Old fine-tune fired once per 15 min on a 2 s PLL average.
//     A bursty TSDU control channel has AGC product swinging
//     0.3–1.0 across symbols, so the snapshot was dice-roll —
//     gates would skip even when a real correction was needed
//     (observed 2026-04-24: boot landed −0.27 ppm, true −0.54,
//     fine-tune did not converge).
//   - Trimmed-mean over a long window naturally rejects bursts,
//     spurs, and brief fades without needing per-iteration step
//     caps or off-baseline guards.
//   - Continuous sampling is cheap (one register read per second)
//     and the long average is robust to anything except a real
//     crystal drift, which is exactly what we want to track.

/// How often we sample pll_dbg / agc_product into the ring.
#[cfg(target_os = "linux")]
const PPM_TRACKER_SAMPLE_INTERVAL_MS: u64 = 1_000;

/// Ring length. 300 samples × 1 s = 5 min of history.
#[cfg(target_os = "linux")]
const PPM_TRACKER_WINDOW_SAMPLES: usize = 300;

/// Fraction trimmed off each tail before computing the mean.
/// 0.10 → drop top 10% + bottom 10% = 20% of samples thrown out.
/// Catches RF transients, traffic-retune glitches, momentary
/// ADC clipping. The remaining 80% is the steady-state estimate.
#[cfg(target_os = "linux")]
const PPM_TRACKER_TRIM_FRAC: f64 = 0.10;

/// How often we recompute + apply the trimmed mean.
#[cfg(target_os = "linux")]
const PPM_TRACKER_APPLY_INTERVAL_SECS: u64 = 60;

/// Minimum samples in the ring before the trimmed mean is
/// allowed to fire. 60 = 1 min of clean data minimum.
#[cfg(target_os = "linux")]
const PPM_TRACKER_MIN_SAMPLES: usize = 60;

/// AGC-product floor for "this sample is signal, not noise".
/// 2026-04-24 revision: tightened from 0.3 → 0.6. The old gate
/// admitted idle-between-TSDU samples where the PLL had no signal
/// to track; those contaminated the trimmed mean and dragged shift
/// away from the correct post-recal value. 0.6 requires a clear
/// burst presence (product = gain × mag typically hits 1-3 during
/// a TSDU, drops to 0.1-0.3 between), so only actually-locked
/// samples feed the estimator.
#[cfg(target_os = "linux")]
const PPM_TRACKER_MIN_AGC_PRODUCT: f64 = 0.6;

/// 2026-04-24: seconds to skip sampling after any shift change.
/// The Costas loop takes ~3 s to reconverge after the NCO moves;
/// pll_dbg during that transient is the INTEGRATOR UNWINDING, not
/// true error. Sampling through the transient was the primary
/// cause of the "tracker pulls toward 0 after recalibrate" bug —
/// fresh samples at the new shift with garbage residuals averaged
/// in with old accurate samples and dragged the estimate down.
#[cfg(target_os = "linux")]
const PPM_TRACKER_SETTLE_MS: u64 = 3_000;

/// Hard envelope on the resulting shift, in PPM. Real crystals don't
/// drift past ±1 ppm on healthy hardware. The tracker CLAMPS to this,
/// so no amount of bad data or pll_dbg bias can walk shift past the
/// ceiling. Operator invariant: auto-PPM will never produce a shift
/// whose |ppm| exceeds this value.
#[cfg(target_os = "linux")]
const PPM_TRACKER_ENVELOPE_PPM: f64 = 1.0;

/// Minimum |Δshift| from currently-applied shift before we bother
/// reprogramming the NCO. Avoids continuous 1-Hz nudges that
/// burn JFFS2 wear on the persistence file.
#[cfg(target_os = "linux")]
const PPM_TRACKER_MIN_APPLY_DELTA_HZ: f64 = 2.0;

/// Persistence: only re-save the on-disk file if the shift moved
/// by at least this much from the last save. JFFS2 wear-leveling
/// matters; this caps saves at ≈6/h in steady state.
#[cfg(target_os = "linux")]
const PPM_TRACKER_PERSIST_DELTA_HZ: f64 = 5.0;

/// Spawn the continuous PPM tracker. Replaces the old
/// `spawn_periodic_fine_tune`. Two cooperating tasks:
///
///   1. **Sampler** ticks at 1 Hz, pushes (pll_dbg, agc_product)
///      into a ring buffer. Drops the sample if AGC product is
///      below the noise floor or the system is deacquired.
///   2. **Updater** wakes every 60 s, reads the ring, computes the
///      trimmed mean, converts to Hz, and applies the resulting
///      shift to the DDC NCO. Persists if the shift moved enough.
///
/// No discrete thresholds, no step clamps, no off-baseline guard.
/// The trimmed mean over 5 min handles all of those.
#[cfg(target_os = "linux")]
pub fn spawn_periodic_fine_tune(state: Arc<AppState>) {
    // Ring holds ESTIMATES OF THE TRUE SHIFT in Hz (floats), not raw
    // residuals. 2026-04-24: the old integrator form
    // (new_shift = old_shift + residual) had no fixed point — any
    // systematic bias in pll_dbg walked shift indefinitely until the
    // safety envelope clamped. See
    // doc/diagnostics/2026-04-24/AUDIO_DROPS_ANALYSIS.md §Issue 1.
    //
    // Each sample estimates where the true shift is RIGHT NOW:
    //     estimate_i = shift_at_sample_i - residual_in_hz_at_sample_i
    // (2026-09-26: this was `+`, which made the fixed point repelling;
    // see true_shift_estimate_hz.)
    // Trimmed mean of those estimates is the running best guess of
    // the steady-state truth. Updater ASSIGNS (not adds) the mean to
    // shift, clamped to ±1 ppm.
    //
    // Ring lives on AppState so `run_calibration` can clear it on a
    // forced /api/ppm_calibrate — otherwise pre-recal estimates stay
    // in the trimmed mean and pull shift back toward the old value.
    let ring = state.ppm_tracker_ring.clone();
    let last_shift_change = state.ppm_last_shift_change_ms.clone();

    // Sampler task — 1 Hz, pushes (shift_now + residual_hz_now).
    // Skips the sample if (a) system isn't acquired, (b) AGC product
    // is below burst threshold, or (c) within the PLL-settle window
    // after a shift change (NCO moved → Costas reconverging → garbage
    // pll_dbg transient).
    {
        let state = state.clone();
        let ring = ring.clone();
        let last_shift_change = last_shift_change.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(
                    PPM_TRACKER_SAMPLE_INTERVAL_MS)).await;
                let acquired = {
                    let dec = state.lsm_decoder.read().await;
                    dec.system.wacn.is_some()
                };
                // Change 071: not while a sweep probes other systems'
                // transmitters (their offsets are not this crystal's).
                if !acquired || !state.radio_lease.is_normal() { continue; }
                // PLL settle gate — skip samples within N ms of the
                // most recent shift change (forced recal or tracker
                // apply). During that window pll_dbg reflects the
                // loop integrator unwinding, not a true residual.
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let last_change = last_shift_change
                    .load(Ordering::Relaxed);
                if last_change > 0
                    && now_ms.saturating_sub(last_change)
                        < PPM_TRACKER_SETTLE_MS
                {
                    continue;
                }
                let (pll, agc_product) = {
                    let core = state.ip_core.lock().await;
                    let (pll, _) = core.lsm_debug();
                    let (g_q9_7, m_q1_15) = core.lsm_agc_debug();
                    let g = g_q9_7 as f64 / 128.0;
                    let m = m_q1_15 as f64 / 32768.0;
                    (pll, g * m)
                };
                if agc_product < PPM_TRACKER_MIN_AGC_PRODUCT {
                    continue;
                }
                // Read the shift that was applied when this pll_dbg
                // was measured. Racy by one sample at worst; the
                // trimmed mean drowns out single-sample races.
                let shift_now = state.current_lo_shift_hz
                    .load(Ordering::Relaxed) as f64;
                let estimate_hz = true_shift_estimate_hz(shift_now, pll as f64);
                let mut r = ring.lock().unwrap();
                if r.len() == PPM_TRACKER_WINDOW_SAMPLES {
                    r.pop_front();
                }
                r.push_back(estimate_hz);
            }
        });
    }

    // Updater task — every 60 s, trimmed mean of estimates → NCO.
    tokio::spawn(async move {
        let mut last_persisted_shift: f64 = state.current_lo_shift_hz
            .load(Ordering::Relaxed) as f64;
        loop {
            tokio::time::sleep(Duration::from_secs(
                PPM_TRACKER_APPLY_INTERVAL_SECS)).await;

            let samples: Vec<f64> = {
                let r = ring.lock().unwrap();
                r.iter().copied().collect()
            };
            if samples.len() < PPM_TRACKER_MIN_SAMPLES {
                tracing::debug!(
                    "auto-PPM tracker: only {} samples, need {}; skipping",
                    samples.len(), PPM_TRACKER_MIN_SAMPLES);
                continue;
            }

            // Trimmed mean: sort, drop top + bottom TRIM_FRAC,
            // average what's left.
            let mut sorted = samples.clone();
            sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(
                std::cmp::Ordering::Equal));
            let trim = (sorted.len() as f64
                * PPM_TRACKER_TRIM_FRAC).floor() as usize;
            let kept = &sorted[trim..sorted.len() - trim];
            let mean_estimate_hz = kept.iter().sum::<f64>()
                / kept.len() as f64;

            let rx_lo = state.current_rx_lo
                .load(Ordering::Relaxed) as f64;
            let control_freq = state.current_control_freq
                .load(Ordering::Relaxed) as f64;
            let sample_rate = state.current_sample_rate_hz
                .load(Ordering::Relaxed) as f64;
            let old_shift = state.current_lo_shift_hz
                .load(Ordering::Relaxed) as f64;

            // Hard 1 ppm envelope. Clamp, not skip — if the estimate
            // wanders into nonsense the operator still sees a bounded
            // shift, not the old behaviour where the updater gave up
            // and the shift stayed at yesterday's value.
            let envelope_hz = (rx_lo * 1e-6) * PPM_TRACKER_ENVELOPE_PPM;
            let new_shift = mean_estimate_hz
                .clamp(-envelope_hz, envelope_hz);
            let clamped = (mean_estimate_hz - new_shift).abs() > 0.5;

            if (new_shift - old_shift).abs()
                < PPM_TRACKER_MIN_APPLY_DELTA_HZ {
                continue;
            }

            // Apply gates — check BEFORE touching hardware so the
            // estimate is still surfaced advisory via /api/ppm even
            // when apply is blocked.
            //
            //   1. Auto-PPM toggle off → advisory-only.
            //   2. Anchor window: tracker can only move shift within
            //      ±auto_ppm_anchor_hz of the last forced recal.
            //      Stage A (wideband peak-find) is ground truth for
            //      the carrier position; tracker can fine-tune near
            //      it but not walk away from it. 0 anchor disables
            //      the restriction (legacy unrestricted mode).
            //   3. No recal this session → no anchor → skip apply.
            if !state.auto_ppm_enabled.load(Ordering::Relaxed) {
                tracing::debug!(
                    "auto-PPM tracker: apply suppressed (auto disabled), \
                     estimate {:+.1} Hz vs current {:+.0} Hz",
                    mean_estimate_hz, old_shift);
                continue;
            }
            let anchor = state.auto_ppm_anchor_hz
                .load(Ordering::Relaxed) as i64;
            let last_recal = state.last_recal_shift_hz
                .load(Ordering::Relaxed);
            if anchor > 0 {
                if last_recal == 0 {
                    tracing::debug!(
                        "auto-PPM tracker: apply suppressed (no recal \
                         anchor set), estimate {:+.1} Hz",
                        mean_estimate_hz);
                    continue;
                }
                let delta = (new_shift as i64 - last_recal).abs();
                if delta > anchor {
                    tracing::info!(
                        "auto-PPM tracker: estimate {:+.1} Hz is \
                         {:+} Hz from recal anchor {} (>±{}); \
                         skipping apply",
                        mean_estimate_hz,
                        new_shift as i64 - last_recal,
                        last_recal, anchor);
                    state.event_log.push(
                        crate::services::event_log::LogCategory::System,
                        format!("auto-PPM: estimate {:+.0} Hz outside \
                                 anchor (recal={}, ±{}); not applied",
                                 mean_estimate_hz, last_recal, anchor),
                        serde_json::json!({
                            "kind":        "ppm.tracker_blocked",
                            "estimate_hz": mean_estimate_hz,
                            "recal_hz":    last_recal,
                            "anchor_hz":   anchor,
                            "reason":      "outside_anchor",
                        }),
                    );
                    continue;
                }
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
            // Reset the ring + stamp the settle timer. Any samples
            // taken AT THE OLD SHIFT are stale against the new NCO
            // position — keeping them in the ring biases subsequent
            // means back toward the old value. Combined with the
            // sampler's settle gate this gives the PLL ~3 s to
            // reconverge before the first new sample lands.
            {
                let mut r = ring.lock().unwrap();
                r.clear();
            }
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            last_shift_change.store(now_ms, Ordering::Relaxed);
            let new_ppm = -new_shift / (rx_lo * 1e-6);
            let clamp_note = if clamped { " [CLAMPED]" } else { "" };
            tracing::info!(
                "auto-PPM tracker: estimate {:+.1} Hz (n={} of {}) \
                 -> shift {:+.0} Hz ({:+.4} ppm){}",
                mean_estimate_hz, kept.len(), samples.len(),
                new_shift, new_ppm, clamp_note);
            state.event_log.push(
                crate::services::event_log::LogCategory::System,
                format!("auto-PPM tracker: est {:+.1} Hz -> shift \
                         {:+.0} Hz ({:+.4} ppm, n={}){}",
                         mean_estimate_hz, new_shift, new_ppm,
                         kept.len(), clamp_note),
                serde_json::json!({
                    "kind":            "ppm.tracker",
                    "estimate_hz":     mean_estimate_hz,
                    "old_shift":       old_shift,
                    "new_shift":       new_shift,
                    "new_lo_ppm":      new_ppm,
                    "envelope_hz":     envelope_hz,
                    "clamped":         clamped,
                    "samples_total":   samples.len(),
                    "samples_kept":    kept.len(),
                }),
            );

            // Persist if drifted enough from last persisted value.
            if (new_shift - last_persisted_shift).abs()
                >= PPM_TRACKER_PERSIST_DELTA_HZ
                && unix_secs >= 1_000_000_000
            {
                let persisted = PersistedPpm {
                    lo_shift_hz:     new_shift.round() as i64,
                    lo_ppm:          new_ppm,
                    rx_lo_hz:        rx_lo as i64,
                    control_freq_hz: state.current_control_freq.load(Ordering::Relaxed),
                    unix_secs,
                    method:          "tracker".to_string(),
                };
                if save_persisted(&persisted).is_ok() {
                    last_persisted_shift = new_shift;
                }
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
            control_freq_hz: state.current_control_freq.load(Ordering::Relaxed),
            unix_secs,
            method:          "sync_lost_reset".to_string(),
        };
        let _ = save_persisted(&persisted);
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub struct CalibrationResult;

#[cfg(test)]
mod tests {
    use super::*;

    // Bench 2026-09-26 (cabled replay, unit A): shift 370 → median
    // pll_dbg ≈ −87 Hz, 470 → ≈ 0, 570 → ≈ +106 Hz. Every reading
    // must point at the same true shift, 470.
    #[test]
    fn estimates_converge_on_bench_measured_truth() {
        let q = |hz: f64| hz / pll_q213_to_hz(1.0);
        for (shift, resid_hz) in [(370.0, -100.0), (470.0, 0.0), (570.0, 100.0)] {
            let est = true_shift_estimate_hz(shift, q(resid_hz));
            assert!((est - 470.0).abs() < 1e-9, "{shift} -> {est}");
        }
    }

    // Assigning the estimate must shrink the error, never grow it.
    #[test]
    fn tracker_fixed_point_is_attracting() {
        let truth = 470.0;
        let q_per_hz = 1.0 / pll_q213_to_hz(1.0);
        let mut shift = 300.0;
        for _ in 0..3 {
            let pll = (shift - truth) * q_per_hz; // NCO minus signal
            let next = true_shift_estimate_hz(shift, pll);
            assert!((next - truth).abs() <= (shift - truth).abs());
            shift = next;
        }
        assert!((shift - truth).abs() < 1e-9);
    }

    #[test]
    fn residual_scale_is_q213_rad_per_symbol() {
        // 2π·2^13 Q2.13 units per symbol at 4800 sym/s = 4800 Hz
        let full = 2.0 * std::f64::consts::PI * 8192.0;
        assert!((pll_q213_to_hz(full) - 4800.0).abs() < 1e-9);
    }
}
