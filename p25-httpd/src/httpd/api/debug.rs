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
                    "traffic" => core.read_traffic_iq_buffers(),
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
            let mgr = state.traffic_manager.lock().await;
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
                "traffic" => core.read_traffic_pre_diff_iq_buffers(),
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

/// Decode interleaved i16 LE pairs to (re, im) f32 at Q1.13 scale.
/// The HDL pre-diff ring interleaves rotate_mid (even idx) with
/// rotate_sym (odd); the odd-indexed samples are the decision points.
#[cfg(target_os = "linux")]
fn pre_diff_sym_points(bytes: &[u8], max_syms: usize) -> Vec<(f32, f32)> {
    const Q13_SCALE: f32 = 1.0 / 8192.0;
    bytes
        .chunks_exact(4)
        .map(|c| {
            let r = i16::from_le_bytes([c[0], c[1]]) as f32;
            let i = i16::from_le_bytes([c[2], c[3]]) as f32;
            (r * Q13_SCALE, i * Q13_SCALE)
        })
        .skip(1)            // rotate_mid first, rotate_sym second
        .step_by(2)         // every other pair is a decision point
        .take(max_syms)
        .collect()
}

/// `GET /api/deviation?chain=control|traffic&window_syms=N`
///
/// Anritsu-style modulation metrics pulled from the pre-differential
/// IQ ring. Samples are carrier-derotated + AGC-scaled but before the
/// diff-demod; the hard decision is `sign(I) / sign(Q)` — one of four
/// quadrant clusters at (±1, ±1). Metrics:
///
/// - `evm` (aka mod_fidelity): RMS error / RMS reference (fraction).
/// - `ambiguous_frac` (BER proxy): samples crossing decision margin.
/// - `cluster_radius`: mean |z|, ≈1 when AGC is healthy.
/// - `peak_mag`: raw Q1.13-scaled peak magnitude.
/// - `quad_balance`: histogram of the four quadrants.
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
    if symbol_pts.is_empty() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "no symbol-time points recovered",
        }));
    }

    // LSM / CQPSK: the ideal constellation sits at (±1, ±1) — four
    // points, one per quadrant. Mean-magnitude normalise so the
    // observed cluster mean |z| maps to ≈1 (more stable than peak
    // alone against a single outlier pulling the scale).
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
    let norm = 1.0 / cluster_radius;

    let mut soft_iq: Vec<f32> = Vec::with_capacity(symbol_pts.len() * 2);
    let mut hard: Vec<u8> = Vec::with_capacity(symbol_pts.len());
    let mut quad_counts = [0usize; 4];
    let mut err_sq: f64 = 0.0;
    let mut evm_ref_sq: f64 = 0.0;
    for &(r, i) in &symbol_pts {
        let rn = r * norm;
        let in_ = i * norm;
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
    let n = symbol_pts.len() as f64;
    let evm = (err_sq / n).sqrt() / (evm_ref_sq / n).sqrt();

    let ambiguity_margin = 0.25_f32;
    let ambiguous = symbol_pts
        .iter()
        .filter(|&&(r, i)| {
            (r * norm).abs() < ambiguity_margin || (i * norm).abs() < ambiguity_margin
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
/// For each decision-time sample in the pre-diff ring, compute
/// `atan2(Q, I) * 600 / (π/4)` — which gives deviation in Hz:
/// ±600 Hz for the inner dibits and ±1800 Hz for the outer dibits.
/// Histogrammed into 240 bins spanning ±2400 Hz for display.
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
    if symbol_pts.is_empty() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "no symbol-time points recovered",
        }));
    }

    // Build uniform bin edges spanning ±RANGE_HZ, then histogram
    // the Hz-scaled atan2. Scale factor: π/4 on the unit circle
    // corresponds to 600 Hz inner-dibit deviation → multiplier is
    // 600 / (π/4) = 600 * 4/π ≈ 763.944.
    let scale_hz = 600.0 / (std::f32::consts::PI / 4.0);
    let mut counts = vec![0u32; BINS];
    let edges_hz: Vec<f32> = (0..=BINS)
        .map(|i| {
            -RANGE_HZ + (2.0 * RANGE_HZ) * (i as f32) / (BINS as f32)
        })
        .collect();
    let bin_width = (2.0 * RANGE_HZ) / BINS as f32;
    for &(r, i) in &symbol_pts {
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
        "window_syms": symbol_pts.len(),
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
