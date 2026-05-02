//! Visual diagnostics: spectrum, deviation, distribution.
//!
//! Consumer orientation: "show me what the signal looks like right
//! now." The endpoints return arrays that an SDR-savvy consumer
//! renders as a 2-D plot:
//!
//!   - `/api/spectrum` — narrowband FFT magnitudes from the post-DDC
//!     IQ ring (software FFT on ARM). Accepts
//!     `?chain=control|traffic` and `?fft=<512..16384>`.
//!   - `/api/spectrum_wide` — wideband FFT unpacked from the HDL
//!     spectrometer (no PS FFT). Spans the full AD9361 sample rate.
//!   - `/api/deviation` — Anritsu-style modulation metrics pulled
//!     from the pre-differential IQ ring (post-rotate + post-AGC but
//!     pre-diff-demod). `sign(I)/sign(Q)` hard decisions, EVM / BER /
//!     cluster-radius.
//!   - `/api/distribution` — symbol-time atan2 histogram scaled to
//!     Hz (±600 inner dibits, ±1800 outer). Feeds the Plots-tab
//!     "distribution" panel.
//!
//! All four endpoints source samples from the same set of rings that
//! `/ws/iq` / the dashboard's live plots use.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};

#[allow(unused_imports)]
use p25_json::*;

// ── IQ drain cadence constants ────────────────────────────────────
//
// /api/spectrum, /api/deviation, and /api/distribution all read
// shared DMA rings. Competing readers (/ws/iq, other endpoints) can
// starve one endpoint's drain for a window; these retry-with-deadline
// constants survive that contention.

/// Deadline for accumulating enough IQ sub-buffers for one FFT /
/// metrics snapshot. Sub-buffers arrive every ~131 ms; 3 s gives ~22
/// opportunities even under 50% reader contention.
const IQ_DRAIN_DEADLINE_MS: u64 = 3000;

/// Retry cadence while waiting for sub-buffers. Faster than the
/// ~131 ms arrival rate so we don't miss one, slow enough to not
/// spin on the `ip_core` lock.
const IQ_DRAIN_RETRY_MS: u64 = 60;

#[allow(unused_imports)]
use crate::httpd::AppState;
#[allow(unused_imports)]
use crate::protocol::p25::control_channel::{
    ControlChannelDecoder, RUNTIME_SYNC_THRESHOLD, CC_SYNC_THRESHOLD,
};

/// `GET /api/spectrum?chain=control|traffic`
///
/// Runs an N-point FFT (default 4096) over the most recent IQ ring
/// samples from the selected chain and returns the magnitude
/// spectrum in dBFS, fftshifted so bin 0 is the most-negative
/// frequency (-sample_rate_hz/2 relative to the chain's DDC center).
///
/// Query params:
///   - `chain`    — `control` (default) or `traffic`
///   - `fft`      — one of {1024, 2048, 4096, 8192, 16384}; default 4096
///   - `averages` — 1..floor(65536/fft); default 1
#[cfg(target_os = "linux")]
pub async fn get_spectrum(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let chain = params
        .get("chain")
        .map(String::as_str)
        .unwrap_or("control");

    let fft_size = crate::services::spectrum::clamp_fft_size(
        params.get("fft").and_then(|s| s.parse::<usize>().ok()),
    );
    let averages = crate::services::spectrum::clamp_averages(
        params.get("averages").and_then(|s| s.parse::<usize>().ok()),
        fft_size,
    );
    let min_samples = fft_size * averages;
    let min_bytes = min_samples * 4;

    let bytes: Vec<u8> = {
        let mut acc: Vec<u8> = Vec::with_capacity(min_bytes);
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(IQ_DRAIN_DEADLINE_MS);
        loop {
            {
                let mut core = state.ip_core.lock().await;
                let bufs: Vec<&[u8]> = match chain {
                    "control" => core.read_iq_buffers(),
                    // M2A 2026-05-02: traffic IQ ring deleted with the old chain.
                    "traffic" => Vec::new(),
                    other => {
                        return Json(serde_json::json!({
                            "ok": false,
                            "error": format!(
                                "unknown chain '{other}'; expected control|traffic"
                            ),
                        }));
                    }
                };
                for b in bufs {
                    acc.extend_from_slice(b);
                }
            }
            if acc.len() >= min_bytes {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(IQ_DRAIN_RETRY_MS)).await;
        }
        acc
    };

    let Some(snap) = crate::services::spectrum::spectrum_from_bytes(&bytes, fft_size, averages) else {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "not enough IQ samples for fft={fft_size} on chain={chain} \
                 ({} samples available, need at least {fft_size})",
                bytes.len() / 4,
            ),
        }));
    };

    let rx_lo = state
        .ad9361
        .get_rx_lo_frequency()
        .await
        .unwrap_or(state.current_rx_lo.load(
            std::sync::atomic::Ordering::Relaxed) as u64) as f64;
    let center_hz: f64 = match chain {
        "control" => state.boot_control_freq as f64,
        "traffic" => {
            let mgr = state.traffic_chain.lock().await;
            rx_lo + mgr.last_offset_hz as f64
        }
        _ => rx_lo,
    };

    Json(serde_json::json!({
        "ok":              true,
        "chain":           chain,
        "center_hz":       center_hz,
        "sample_rate_hz":  snap.sample_rate_hz,
        "fft_size":        snap.fft_size,
        "averages_used":   snap.averages_used,
        "mag_db":          snap.mag_db,
    }))
}


#[cfg(not(target_os = "linux"))]
pub async fn get_spectrum(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "spectrum only available on the target (linux/arm)",
    }))
}

/// `GET /api/spectrum_wide` — wideband spectrum from the HDL
/// spectrometer.
///
/// Reads one completed integration from `wideband_spec_dma`
/// (BRAM-backed, 4096-bin FFT averaged in hardware). **No PS FFT** —
/// the PS just unpacks the 47-bit mantissa + 3-bit exponent per bin
/// to f32 dB and fft-shifts for display. Span = AD9361 sample rate
/// (preset-dependent, 2-16 MHz).
#[cfg(target_os = "linux")]
pub async fn get_spectrum_wide(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    const DEADLINE_MS: u64 = 500;
    const RETRY_MS: u64 = 50;
    let bytes: Option<Vec<u8>> = {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(DEADLINE_MS);
        loop {
            {
                let mut core = state.ip_core.lock().await;
                if let Some(buf) = core.read_wideband_spec_buffer() {
                    break Some(buf.to_vec());
                }
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            tokio::time::sleep(
                std::time::Duration::from_millis(RETRY_MS)).await;
        }
    };
    let Some(bytes) = bytes else {
        return Json(serde_json::json!({
            "ok": false,
            "error": "no wideband spectrum ready (spectrometer idle?)",
        }));
    };
    let mag_db = crate::services::spectrum::wideband_power_db(&bytes);
    if mag_db.is_empty() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "wideband buffer shorter than 32 KB",
        }));
    }
    let rx_lo = state
        .current_rx_lo
        .load(std::sync::atomic::Ordering::SeqCst) as f64;
    let span_hz = state
        .current_sample_rate_hz
        .load(std::sync::atomic::Ordering::SeqCst) as f64;
    Json(serde_json::json!({
        "ok":             true,
        "center_hz":      rx_lo,
        "span_hz":        span_hz,
        "bins":           mag_db.len(),
        "mag_db":         mag_db,
        "source":         "hdl_spectrometer",
    }))
}

#[cfg(not(target_os = "linux"))]
pub async fn get_spectrum_wide(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "spectrum_wide only available on the target (linux/arm)",
    }))
}

// ── Pre-differential IQ helpers (shared between deviation + distribution) ──

/// Drain enough pre-diff IQ from the selected chain to cover
/// `min_bytes`, or hit the deadline. Returns the accumulated raw
/// buffer (interleaved i16 LE pairs, 2 samples per symbol packed).
#[cfg(target_os = "linux")]
async fn drain_pre_diff(
    state: &Arc<AppState>,
    chain: &str,
    min_bytes: usize,
) -> Result<Vec<u8>, String> {
    let mut acc: Vec<u8> = Vec::new();
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(IQ_DRAIN_DEADLINE_MS);
    loop {
        {
            let mut core = state.ip_core.lock().await;
            let bufs: Vec<&[u8]> = match chain {
                "control" => core.read_pre_diff_iq_buffers(),
                // M2A 2026-05-02: traffic pre-diff ring deleted with the old chain.
                "traffic" => Vec::new(),
                other => {
                    return Err(format!(
                        "unknown chain '{other}'; expected control|traffic"
                    ));
                }
            };
            for b in bufs {
                acc.extend_from_slice(b);
            }
        }
        if acc.len() >= min_bytes {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(
            std::time::Duration::from_millis(IQ_DRAIN_RETRY_MS)).await;
    }
    Ok(acc)
}

/// Decode interleaved i16 LE pairs to (re, im) f32 at Q1.15 scale.
/// The HDL pre-diff ring is NATIVE Q1.15 signed 16. `LsmPllRotate`
/// was instantiated with `iq_width=16` for the pre-diff path, so
/// samples come out at the full Q1.15 range; scale = 1/32768.
///
/// Interleave: rotate_mid (even idx) then rotate_cur (odd idx). Only
/// the cur samples are decision-instant; mid is a half-symbol crossing
/// for timing recovery and is dropped here.
#[cfg(target_os = "linux")]
fn pre_diff_sym_points(bytes: &[u8], max_syms: usize) -> Vec<(f32, f32)> {
    const Q15_SCALE: f32 = 1.0 / 32768.0;
    bytes
        .chunks_exact(4)
        .map(|c| {
            let r = i16::from_le_bytes([c[0], c[1]]) as f32;
            let i = i16::from_le_bytes([c[2], c[3]]) as f32;
            (r * Q15_SCALE, i * Q15_SCALE)
        })
        .skip(1)            // rotate_mid first, rotate_cur second
        .step_by(2)         // every other pair is a decision point
        .take(max_syms)
        .collect()
}

/// Apply differential demod to a stream of consecutive symbol-time
/// samples: `d[n] = z[n] * conj(z[n-1])`. P25 LSM/CQPSK encodes the
/// symbol in the phase change between consecutive symbols, not in the
/// absolute phase, so endpoints that score symbols (deviation /
/// distribution / EVM) operate on the differential, not the raw
/// pre-diff samples.
///
/// Output length = input length - 1.
#[cfg(target_os = "linux")]
fn differentiate(syms: &[(f32, f32)]) -> Vec<(f32, f32)> {
    if syms.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(syms.len() - 1);
    for k in 1..syms.len() {
        let (i0, q0) = syms[k - 1];
        let (i1, q1) = syms[k];
        // (i1 + j q1) * (i0 - j q0) = (i1 i0 + q1 q0) + j (q1 i0 - i1 q0)
        out.push((i1 * i0 + q1 * q0, q1 * i0 - i1 * q0));
    }
    out
}

/// `GET /api/deviation?chain=control|traffic&window_syms=N`
///
/// Anritsu-style modulation metrics pulled from the pre-diff post-PLL
/// ring, with the differential applied in software. P25 LSM encodes
/// the symbol in the phase change between consecutive symbols, so we
/// compute `d[n] = z[n] * conj(z[n-1])` and score the four
/// differential rails (±π/4 inner, ±3π/4 outer) by quadrant. Metrics:
///
/// - `evm` (aka mod_fidelity): RMS error / RMS reference on diff.
/// - `ambiguous_frac` (BER proxy): diff samples crossing decision margin.
/// - `cluster_radius`: mean |z| of the raw pre-diff samples; ≈1 when
///   AGC is healthy (a value tied to AGC, not the diff product).
/// - `peak_mag`: raw pre-diff peak magnitude.
/// - `quad_balance`: histogram of the four diff-quadrant hits.
#[cfg(target_os = "linux")]
pub async fn get_deviation(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let chain = params
        .get("chain")
        .map(String::as_str)
        .unwrap_or("control");
    let window_syms: usize = params
        .get("window_syms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(4800)
        .clamp(64, 9600);

    // Two samples per symbol in the pre-diff ring (rotate_mid +
    // rotate_sym), so we need `8 * window_syms` bytes.
    let min_bytes: usize = 8 * window_syms;
    let bytes = match drain_pre_diff(&state, chain, min_bytes).await {
        Ok(b) => b,
        Err(msg) => {
            return Json(serde_json::json!({
                "ok": false,
                "error": msg,
            }));
        }
    };

    if bytes.len() < 8 * 64 {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "not enough pre-diff IQ samples on chain={} ({} bytes)",
                chain, bytes.len()
            ),
        }));
    }

    let symbol_pts = pre_diff_sym_points(&bytes, window_syms);
    if symbol_pts.len() < 2 {
        return Json(serde_json::json!({
            "ok": false,
            "error": "not enough symbol-time points recovered",
        }));
    }

    // `peak` and `cluster_radius` characterise the AGC on the raw
    // pre-diff samples (should be ≈1 when AGC is healthy).
    let peak = symbol_pts
        .iter()
        .map(|&(r, i)| (r * r + i * i).sqrt())
        .fold(0.0f32, f32::max)
        .max(1e-6);
    let mean_mag: f32 = symbol_pts
        .iter()
        .map(|&(r, i)| (r * r + i * i).sqrt())
        .sum::<f32>()
        / symbol_pts.len() as f32;
    let cluster_radius = mean_mag.max(1e-6);

    // Apply the differential and score four rails. `d[n]` lives at
    // approximately unit-magnitude-squared after AGC, so normalise
    // by mean |d| to bring the rail magnitudes back to ≈1.
    let diff_pts = differentiate(&symbol_pts);
    let diff_mean_mag: f32 = diff_pts
        .iter()
        .map(|&(r, i)| (r * r + i * i).sqrt())
        .sum::<f32>()
        / diff_pts.len() as f32;
    let diff_norm = 1.0 / diff_mean_mag.max(1e-6);

    let mut soft_iq: Vec<f32> = Vec::with_capacity(diff_pts.len() * 2);
    let mut hard: Vec<u8> = Vec::with_capacity(diff_pts.len());
    let mut quad_counts = [0usize; 4];
    let mut err_sq: f64 = 0.0;
    let mut evm_ref_sq: f64 = 0.0;
    for &(r, i) in &diff_pts {
        let rn = r * diff_norm;
        let in_ = i * diff_norm;
        let bit_i = (rn > 0.0) as u8;
        let bit_q = (in_ > 0.0) as u8;
        let h = (bit_i << 1) | bit_q;
        quad_counts[h as usize] += 1;
        let ix = if bit_i == 1 { 1.0 } else { -1.0 };
        let iy = if bit_q == 1 { 1.0 } else { -1.0 };
        let dr = (rn - ix) as f64;
        let di = (in_ - iy) as f64;
        err_sq += dr * dr + di * di;
        evm_ref_sq += (ix * ix + iy * iy) as f64;
        soft_iq.push(rn);
        soft_iq.push(in_);
        hard.push(h);
    }
    let n = diff_pts.len() as f64;
    let evm = (err_sq / n).sqrt() / (evm_ref_sq / n).sqrt();

    let ambiguity_margin = 0.25_f32;
    let ambiguous = diff_pts
        .iter()
        .filter(|&&(r, i)| {
            (r * diff_norm).abs() < ambiguity_margin
                || (i * diff_norm).abs() < ambiguity_margin
        })
        .count();
    let ber = ambiguous as f64 / n;

    Json(serde_json::json!({
        "ok":             true,
        "chain":          chain,
        "window_syms":    hard.len(),
        "sample_rate_hz": 4800,
        "modulation":     "LSM",
        "soft_iq":        soft_iq,
        "hard":           hard,
        "metrics": {
            "evm":              evm,
            "ambiguous_frac":   ber,
            "cluster_radius":   cluster_radius,
            "peak_mag":         peak,
            "quad_balance":     quad_counts,
        },
    }))
}

#[cfg(not(target_os = "linux"))]
pub async fn get_deviation(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "deviation only available on the target (linux/arm)",
    }))
}

/// `GET /api/distribution?chain=control|traffic`
///
/// Returns a symbol-time phase histogram scaled to deviation in Hz.
/// For each decision-time sample, apply the differential
/// `d[n] = z[n] * conj(z[n-1])`, then compute `atan2(Q, I) * 600/(π/4)`
/// on the diff. That gives deviation in Hz: ±600 for the inner dibits
/// and ±1800 for the outer dibits — clean sharp peaks on a healthy
/// signal. Histogrammed into 240 bins spanning ±2400 Hz for display.
///
/// Response shape:
///   `{ok, chain, bins, edges_hz: [f32; N+1], counts: [u32; N], peak_index}`
#[cfg(target_os = "linux")]
pub async fn get_distribution(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    const BINS: usize = 240;
    const RANGE_HZ: f32 = 2400.0;
    const WINDOW_SYMS: usize = 2400;

    let chain = params
        .get("chain")
        .map(String::as_str)
        .unwrap_or("control");

    let min_bytes: usize = 8 * WINDOW_SYMS;
    let bytes = match drain_pre_diff(&state, chain, min_bytes).await {
        Ok(b) => b,
        Err(msg) => {
            return Json(serde_json::json!({
                "ok": false,
                "error": msg,
            }));
        }
    };

    if bytes.len() < 8 * 64 {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "not enough pre-diff IQ samples on chain={} ({} bytes)",
                chain, bytes.len()
            ),
        }));
    }

    let symbol_pts = pre_diff_sym_points(&bytes, WINDOW_SYMS);
    let diff_pts = differentiate(&symbol_pts);
    if diff_pts.is_empty() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "not enough symbol-time points recovered",
        }));
    }

    // Build uniform bin edges spanning ±RANGE_HZ, then histogram
    // atan2 of the diff. π/4 on the diff-unit-circle corresponds to
    // 600 Hz inner-dibit deviation → multiplier = 600 * 4/π ≈ 763.944.
    let scale_hz = 600.0 / (std::f32::consts::PI / 4.0);
    let mut counts = vec![0u32; BINS];
    let edges_hz: Vec<f32> = (0..=BINS)
        .map(|i| -RANGE_HZ + (2.0 * RANGE_HZ) * (i as f32) / (BINS as f32))
        .collect();
    let bin_width = (2.0 * RANGE_HZ) / BINS as f32;
    for &(r, i) in &diff_pts {
        if r == 0.0 && i == 0.0 {
            continue;
        }
        let dev_hz = i.atan2(r) * scale_hz;
        if !dev_hz.is_finite() {
            continue;
        }
        let mut idx = ((dev_hz + RANGE_HZ) / bin_width).floor() as isize;
        if idx < 0 {
            idx = 0;
        }
        if idx >= BINS as isize {
            idx = (BINS - 1) as isize;
        }
        counts[idx as usize] += 1;
    }

    let peak_index = counts
        .iter()
        .enumerate()
        .max_by_key(|(_, &c)| c)
        .map(|(i, _)| i)
        .unwrap_or(0);

    Json(serde_json::json!({
        "ok":          true,
        "chain":       chain,
        "bins":        BINS,
        "edges_hz":    edges_hz,
        "counts":      counts,
        "peak_index":  peak_index,
        "window_syms": diff_pts.len(),
    }))
}

#[cfg(not(target_os = "linux"))]
pub async fn get_distribution(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "distribution only available on the target (linux/arm)",
    }))
}

// ── Wideband raw IQ capture (2026-05-03) ─────────────────────────────
//
// `GET /api/wideband_iq_capture` returns the snapshot (active capture
// progress + last completed path).
// `POST /api/wideband_iq_capture?seconds=N` (or no body) opens a fresh
// .cs16 capture file in /tmp/p25_iq_captures/ and the wideband IQ
// reader task tees the next N seconds of 8 MSPS samples into it.
//
// File format: raw interleaved i16 little-endian I/Q (8000000 sample
// pairs per second). GNU Radio: `iio_readdev`-equivalent — read with
// `dtype=int16` then reshape to (-1, 2).

#[cfg(target_os = "linux")]
pub async fn get_wideband_iq_capture(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let snap = state.wideband_iq_capture.snapshot().await;
    Json(serde_json::json!({
        "ok": true,
        "capture": snap,
    }))
}

#[cfg(target_os = "linux")]
pub async fn post_wideband_iq_capture(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let seconds: f64 = params
        .get("seconds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2.0);

    match state.wideband_iq_capture.start(seconds).await {
        Ok(path) => {
            // 2026-05-03 dual-DDC pivot: the wideband_iq DMA is off
            // by default. Capture is a primary consumer — flip the
            // DMA on so the next IRQ wakes the reader. The operator
            // is responsible for turning it back off afterward via
            // `POST /api/sw_demod?enabled=0` (or another capture
            // start), since auto-off would race with the drain
            // tail of the previous capture.
            {
                let core = state.ip_core.lock().await;
                core.set_wideband_iq_dma_enable(true);
            }
            Json(serde_json::json!({
                "ok": true,
                "path": path,
                "seconds": seconds,
                "approx_bytes": (seconds * 8_000_000.0 * 4.0) as u64,
                "wideband_iq_dma": true,
                "note": "wideband_iq DMA was enabled for the capture; \
                         disable via POST /api/sw_demod?enabled=0 once \
                         the capture file finishes writing.",
            })).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("{e}"),
            })),
        ).into_response(),
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn get_wideband_iq_capture(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "wideband IQ capture only available on target (linux/arm)",
    }))
}

#[cfg(not(target_os = "linux"))]
pub async fn post_wideband_iq_capture(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "wideband IQ capture only available on target (linux/arm)",
    }))
}

// ── Live software demod (Stage 2B 2026-05-03) ────────────────────────
//
// `GET  /api/sw_demod`             — runtime stats + current enable
// `POST /api/sw_demod?enabled=0|1` — flip live software demod on/off

#[cfg(target_os = "linux")]
pub async fn get_sw_demod(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;
    let s = &state.sw_demod_stats;
    let elapsed = s.started_at.lock().ok()
        .and_then(|g| *g)
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    let last_chunk_secs_ago = s.last_chunk_at.lock().ok()
        .and_then(|g| *g)
        .map(|t| t.elapsed().as_secs_f64());
    Json(serde_json::json!({
        "ok": true,
        "enabled": state.sw_demod_enabled.load(Ordering::Relaxed),
        "uptime_secs": elapsed,
        "chunks_in": s.chunks_in.load(Ordering::Relaxed),
        "samples_in": s.samples_in.load(Ordering::Relaxed),
        "samples_out_62k5": s.samples_out_62k5.load(Ordering::Relaxed),
        "dibits_emitted": s.dibits_emitted.load(Ordering::Relaxed),
        "framer_dispatches": s.framer_dispatches.load(Ordering::Relaxed),
        "retunes": s.retunes.load(Ordering::Relaxed),
        "nco_offset_hz": s.nco_offset_hz.load(Ordering::Relaxed),
        "last_chunk_secs_ago": last_chunk_secs_ago,
    }))
}

#[cfg(target_os = "linux")]
pub async fn post_sw_demod(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use std::sync::atomic::Ordering;

    let want = match params
        .get("enabled")
        .map(|v| v.as_str())
    {
        Some("1") | Some("true") | Some("on") | Some("yes") => true,
        Some("0") | Some("false") | Some("off") | Some("no") => false,
        Some(other) => return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("bad enabled value: {other:?} (expected 0/1/true/false)"),
            })),
        ).into_response(),
        None => return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "missing enabled=0|1 query param",
            })),
        ).into_response(),
    };
    state.sw_demod_enabled.store(want, Ordering::Relaxed);
    // 2026-05-03 dual-DDC pivot: the wideband_iq DMA is the only
    // upstream data source for the sw_demod task, and it's the
    // dominant CPU cost (32 MB/s drain + i16→f32 + LsmPipeline).
    // Drive the DMA enable from the same toggle so flipping
    // sw_demod off actually frees the cores.
    {
        let core = state.ip_core.lock().await;
        core.set_wideband_iq_dma_enable(want);
    }
    Json(serde_json::json!({
        "ok": true,
        "enabled": want,
        "wideband_iq_dma": want,
    })).into_response()
}

#[cfg(not(target_os = "linux"))]
pub async fn get_sw_demod(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "sw_demod only available on target (linux/arm)",
    }))
}

#[cfg(not(target_os = "linux"))]
pub async fn post_sw_demod(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "sw_demod only available on target (linux/arm)",
    }))
}
