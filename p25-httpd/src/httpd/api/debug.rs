//! Visual diagnostics: spectrum, constellation.
//!
//! Consumer orientation: "show me what the signal looks like right
//! now." Two endpoints, both returning arrays that an SDR-savvy
//! consumer renders as a 2-D plot:
//!
//!   - `/api/spectrum` — FFT magnitudes from the IQ capture ring.
//!     Hand-rolled Cooley-Tukey under the hood (no external FFT
//!     dep — see `src/spectrum.rs`). Accepts `?chain=control|traffic`
//!     and `?fft=<512|1024|2048|4096>`.
//!   - `/api/constellation` — raw IQ scatter from the LSM slicer
//!     input. Interpretation notes live in project memory
//!     `reference_p25_constellation_interpretation`.
//!
//! The dashboard's Debug tab renders both. A diagnostic session on
//! 2026-04-17 (see `doc/diagnostics/2026-04-17/PERFORMANCE_ANALYSIS.md`)
//! caught a Gardner-TED timing-loop problem purely by watching the
//! constellation X-pattern — this endpoint set is load-bearing for
//! field debugging.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};

#[allow(unused_imports)]
use p25_json::*;

// ── IQ drain cadence constants ────────────────────────────────────
//
// /api/spectrum and /api/constellation both read the shared iq_dma
// ring. Competing readers (/ws/iq, the constellation poller, the
// spectrum poller) can starve one endpoint's drain for a window;
// these retry-with-deadline constants survive that contention.

/// Deadline for accumulating enough IQ sub-buffers for one FFT /
/// constellation snapshot. Sub-buffers arrive every ~131 ms; 3 s
/// gives ~22 opportunities even under 50% reader contention.
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
///   - `chain` — `control` (default) or `traffic`
///   - `fft`   — one of {1024, 2048, 4096, 8192, 16384}; default 4096
///   - `averages` — 1..floor(65536/fft); default 1. Power-averages N
///     non-overlapping FFT segments (noise floor drops by
///     ~10·log10(averages) dB, carriers stay put). Set 1 for live
///     sweep, 4–16 for noise-floor / channel-shape analysis.
///
/// The traffic chain requires the bake #2 bitstream flashed; on
/// older binaries the endpoint returns an error explaining the
/// missing UIO device. The control chain works on any bitstream
/// that has the Phase 6C `iq_dma` ring.
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

    // Pull buffers from the requested ring. read_*_buffers() is a
    // rolling-window reader with a single shared cursor per channel,
    // so every reader (/api/constellation, /ws/iq, another
    // /api/spectrum tab) partitions the arriving sub-buffers. On
    // boards where the constellation + spectrum + live-IQ all tick
    // concurrently, any one reader can be starved for a window.
    // Retry window is sized for 3 s of wall-clock to survive that
    // contention — sub-buffers arrive every ~131 ms, so even 50%
    // contention still gives us ~11 fresh buffers to catch across
    // 3 s, which covers up to averages=16 × fft=16384.
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

    // Center frequency for the display axis. Control chain =
    // boot_control_freq (matches what /api/stats reports as the
    // current tuned control center); traffic chain = RX LO +
    // TrafficManager.last_offset_hz (the follower's per-call NCO).
    // Fall back to RX LO if the traffic chain hasn't been retuned.
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


/// `GET /api/constellation?chain=control|traffic`
///
/// Runs the retired Phase 6D software LSM demod pipeline
/// (`lsm::demod::demod_lsm`) over a fresh IQ buffer and returns the
/// post-PLL soft-symbol (I, Q) points. The dashboard renders these
/// as a 4-quadrant scatter plot so the user can visually inspect
/// phase noise, radial compression, and quadrant bias on either
/// chain. Particularly useful on the traffic chain for diagnosing
/// robotic-audio / marginal-decode symptoms from scatter cloud
/// shape.
#[cfg(target_os = "linux")]
pub async fn get_constellation(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let chain = params
        .get("chain")
        .map(String::as_str)
        .unwrap_or("traffic");

    // Same ring-drain-race mitigation as /api/spectrum: the iq_dma
    // cursor is shared across all readers, so /ws/iq and /api/spectrum
    // can drain sub-buffers faster than the constellation poller sees
    // them. Retry with a 3 s deadline and 60 ms cadence — sub-buffers
    // arrive every ~131 ms, so 3 s gives ~22 opportunities even under
    // 50% reader contention. The constellation needs at least 2048
    // samples (8 KB) = ~33 ms of fresh IQ, well inside the budget.
    let min_bytes: usize = 4 * 2048;
    let bytes: Vec<u8> = {
        let mut acc: Vec<u8> = Vec::new();
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
            if acc.len() >= min_bytes { break; }
            if std::time::Instant::now() >= deadline { break; }
            tokio::time::sleep(std::time::Duration::from_millis(IQ_DRAIN_RETRY_MS)).await;
        }
        acc
    };

    if bytes.len() < 4 * 512 {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "not enough IQ samples for constellation on chain={} \
                 ({} samples available)",
                chain,
                bytes.len() / 4,
            ),
        }));
    }

    // Decode interleaved-IQ bytes → Complex32 vec. 2048 samples ≈
    // 33 ms at 62.5 kSPS, which produces ~157 symbols at 4800 sym/s.
    // Enough for a dense scatter without over-spending on the JSON
    // payload size.
    let n_samples = std::cmp::min(bytes.len() / 4, 2048);
    let start = bytes.len() - n_samples * 4;
    let mut iq: Vec<crate::lsm::Complex32> = Vec::with_capacity(n_samples);
    for chunk in bytes[start..].chunks_exact(4) {
        let r = i16::from_le_bytes([chunk[0], chunk[1]]) as f32;
        let i = i16::from_le_bytes([chunk[2], chunk[3]]) as f32;
        iq.push(crate::lsm::Complex32::new(r, i));
    }

    // Run the software LSM demod pipeline. `demod_lsm` handles the
    // /2 decimator + LPF + RRC + timing recovery + PLL rotate +
    // differential demod steps internally; soft_symbols are the
    // post-PLL decision-time (I, Q) points we plot. sample_rate is
    // the post-DDC 62.5 kSPS.
    let result = crate::lsm::demod::demod_lsm(
        &iq,
        crate::services::spectrum::SAMPLE_RATE_HZ,
    );

    // Keep the payload small: return up to 512 most recent points
    // as packed arrays (two parallel f32 vecs + a clip count).
    let take = std::cmp::min(result.soft_symbols.len(), 512);
    let start_sym = result.soft_symbols.len() - take;
    let (i_arr, q_arr): (Vec<f32>, Vec<f32>) = result
        .soft_symbols[start_sym..]
        .iter()
        .map(|s| (s.re, s.im))
        .unzip();

    Json(serde_json::json!({
        "ok":         true,
        "chain":      chain,
        "count":      take,
        "i":          i_arr,
        "q":          q_arr,
        "pll_final":  result.pll_trace.last().copied().unwrap_or(0.0),
        "timing_final": result.timing_trace.last().copied().unwrap_or(0.0),
        "note": "Post-PLL soft-symbol constellation from the retired Phase 6D \
                 software LSM demod pipeline. 4 clusters expected for a clean LSM \
                 signal (±1, ±j quadrants).",
    }))
}


#[cfg(not(target_os = "linux"))]
pub async fn get_constellation(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "constellation only available on the target (linux/arm)",
    }))
}

/// `GET /api/spectrum_wide` — Phase 10.7 wideband spectrum.
///
/// Reads one completed integration from the HDL wideband spectrometer
/// (`wideband_spec_dma` ring, BRAM-backed DMA, 4096-bin FFT averaged in
/// hardware at 5-10 Hz). **No PS FFT** — the PS just unpacks the 47-bit
/// mantissa + 3-bit exponent per bin to `f32` dB and fft-shifts for
/// display. Span = AD9361 sample rate (preset-dependent, 2-16 MHz).
///
/// Query params: none for v1 (num_integrations etc. are set once in
/// boot).
#[cfg(target_os = "linux")]
pub async fn get_spectrum_wide(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // Grab the latest integration. If none is ready since the last
    // call, wait briefly — at default 256 integrations / 8 MSPS ≈ 8 Hz
    // the cadence is ~125 ms; we give up to 500 ms to catch one.
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
    // Span metadata: AD9361 sample rate from the active tuning preset.
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

/// `GET /api/deviation?chain=control|traffic&window_syms=N`
///
/// Anritsu-style modulation metrics from the HDL post-PLL IQ ring.
/// Samples are already carrier-derotated + AGC-scaled, so the PS work
/// is just: project each sample onto the I axis (soft symbol), slice
/// to the nearest {-3,-1,+1,+3}, compute error stats over a 1-second
/// window.
///
/// Returns:
/// - `soft`: [f32; N]     — post-PLL I values at symbol time
/// - `hard`: [i8; N]      — decided symbols {-3,-1,+1,+3}
/// - `metrics`:
///     - `symbol_dev_hz`  — median |hard|, scaled to the P25 ideal
///                          C4FM ±1800 Hz deviation
///     - `mod_fidelity`   — RMS(error) / RMS(hard)  (fraction; ×100
///                          for percent)
///     - `ber`            — fraction of samples whose soft-to-hard
///                          slice error exceeded the slicer margin
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

    // Two samples per symbol in the post-PLL ring (mid + sym), so we
    // need `2 * window_syms` complex samples = `8 * window_syms` bytes.
    let min_bytes: usize = 8 * window_syms;
    let bytes: Vec<u8> = {
        let mut acc: Vec<u8> = Vec::new();
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(IQ_DRAIN_DEADLINE_MS);
        loop {
            {
                let mut core = state.ip_core.lock().await;
                let bufs: Vec<&[u8]> = match chain {
                    "control" => core.read_post_pll_iq_buffers(),
                    "traffic" => core.read_traffic_post_pll_iq_buffers(),
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
            tokio::time::sleep(
                std::time::Duration::from_millis(IQ_DRAIN_RETRY_MS)).await;
        }
        acc
    };

    if bytes.len() < 8 * 64 {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "not enough post-PLL IQ samples on chain={} ({} bytes)",
                chain, bytes.len()
            ),
        }));
    }

    // Decode: each 4-byte pair is (re, im) i16 LE. Interleave of
    // rotate_mid / rotate_sym means every *other* sample is the one
    // sliced by the demod (rotate_sym). We take the odd-indexed
    // samples as symbol-time points.
    let samples: Vec<(f32, f32)> = bytes
        .chunks_exact(4)
        .map(|c| {
            let r = i16::from_le_bytes([c[0], c[1]]) as f32;
            let i = i16::from_le_bytes([c[2], c[3]]) as f32;
            (r, i)
        })
        .collect();
    // Q1.13 scaling: divide by 2^13 so the ideal constellation sits
    // at ~±1 on each axis (with AGC magnitude ≈ 1).
    const Q13_SCALE: f32 = 1.0 / 8192.0;
    // Every 2nd sample starting from index 1 = rotate_sym output.
    let symbol_pts: Vec<(f32, f32)> = samples
        .iter()
        .skip(1)
        .step_by(2)
        .take(window_syms)
        .map(|&(r, i)| (r * Q13_SCALE, i * Q13_SCALE))
        .collect();
    if symbol_pts.is_empty() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "no symbol-time points recovered",
        }));
    }

    // Soft = projection onto I axis (post-rotate, should be real).
    // Hard = nearest P25 C4FM decision level {-3,-1,+1,+3}.
    // Since the AGC targets a unit-circle magnitude, we pick the
    // "±3" and "±1" thresholds relative to the observed peak.
    let abs_i: Vec<f32> = symbol_pts.iter().map(|&(r, _)| r.abs()).collect();
    let peak = abs_i
        .iter()
        .copied()
        .fold(0.0f32, |a, b| a.max(b))
        .max(1e-6);
    // Outer-rail threshold = midpoint between ±1 and ±3 in normalised
    // units = 2/3 of peak. Inner-rail threshold = 0.
    let outer_thresh = peak * (2.0 / 3.0);
    let mut soft: Vec<f32> = Vec::with_capacity(symbol_pts.len());
    let mut hard: Vec<i8> = Vec::with_capacity(symbol_pts.len());
    let mut err_sq = 0.0f64;
    let mut hard_sq = 0.0f64;
    let mut slice_errors: usize = 0;
    for &(r, _) in &symbol_pts {
        let h: i8 = if r > outer_thresh {
            3
        } else if r > 0.0 {
            1
        } else if r > -outer_thresh {
            -1
        } else {
            -3
        };
        // Normalise soft to the same ±3 scale as hard.
        let soft_norm = r / (peak / 3.0);
        let e = soft_norm - h as f32;
        // Slice "error" indicator: soft on the wrong side of the
        // decision boundary for its hard symbol.
        let boundary = match h {
            3 => outer_thresh,
            1 => 0.0,
            -1 => -outer_thresh,
            _ => f32::NEG_INFINITY,
        };
        if (h > 0 && r <= boundary) || (h < 0 && r >= boundary) {
            slice_errors += 1;
        }
        err_sq += (e as f64).powi(2);
        hard_sq += (h as f64).powi(2);
        soft.push(soft_norm);
        hard.push(h);
    }
    let n = symbol_pts.len() as f64;
    let mod_fidelity = (err_sq / n).sqrt() / (hard_sq / n).sqrt();
    let ber = slice_errors as f64 / n;

    // Symbol deviation: P25 C4FM spec is ±1800 Hz for the ±3 symbols.
    // Our "3" in normalised units corresponds to the outer rail ≈ peak;
    // scale so median-outer-peak maps to 1800 Hz.
    let symbol_dev_hz = 1800.0;

    Json(serde_json::json!({
        "ok":             true,
        "chain":          chain,
        "window_syms":    soft.len(),
        "sample_rate_hz": 4800,
        "soft":           soft,
        "hard":           hard,
        "metrics": {
            "symbol_dev_hz":  symbol_dev_hz,
            "mod_fidelity":   mod_fidelity,
            "ber":            ber,
            "peak_i":         peak,
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


