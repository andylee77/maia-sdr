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
    let bytes = grab_wideband_frame(state, Duration::from_millis(500))
        .await?;
    let mag_db = wideband_power_db(&bytes);
    if mag_db.is_empty() {
        return Err(anyhow!("wideband buffer too short"));
    }
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

    // Persist to AppState so /api/ppm can read it.
    let hz_int = final_lo_shift.round() as i64;
    state.current_lo_shift_hz.store(hz_int, Ordering::Relaxed);
    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    state.last_ppm_cal_unix_secs.store(unix_secs, Ordering::Relaxed);

    // Persist to disk so the next boot picks up the calibration
    // without re-running it. Best-effort — a permissions or I/O
    // failure here doesn't fail the calibration itself.
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
        // One extra 3 s cushion once acquired — lets the PLL settle
        // at the post-lock operating point before we measure.
        tokio::time::sleep(Duration::from_secs(3)).await;
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

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub struct CalibrationResult;
