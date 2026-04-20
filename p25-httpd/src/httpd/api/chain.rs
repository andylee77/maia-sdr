//! HDL chain internals: dibit dumps, IQ captures, LSM control bits, NID ring.
//!
//! Consumer orientation: "what is the FPGA gateware actually doing?"
//! Every endpoint here exposes a raw slice of the HDL pipeline for
//! cross-validation during bring-up, bake verification, or live
//! debugging. Not intended for normal consumer screens — high data
//! volume (dibit rings, IQ captures) and domain-specific
//! interpretation. Diagnostic tools and the dashboard's "Debug" tab
//! are the main consumers today.
//!
//! **Control/traffic symmetry is deliberate.** Every `/api/control_*`
//! endpoint has a `/api/traffic_*` counterpart with identical response
//! shape. Phase 10-prep normalised these. Keep the symmetry when
//! adding new HDL-exposed data — it makes the control and traffic
//! chains one-to-one comparable for symmetric debugging (did retune
//! break the traffic side but not control? etc.).

use std::sync::Arc;

use axum::{
    extract::State,
    response::Response,
    Json,
};

#[allow(unused_imports)]
use p25_json::*;

#[allow(unused_imports)]
use crate::httpd::AppState;
#[allow(unused_imports)]
use crate::protocol::p25::control_channel::{
    ControlChannelDecoder, RUNTIME_SYNC_THRESHOLD, CC_SYNC_THRESHOLD,
};

// ── API-wait cadence + deadline constants ─────────────────────────
//
// Several aligned-capture and IQ-dump endpoints arm a flag, then
// poll for the async HDL reader to populate a buffer. The cadence
// and deadline constants below are shared across those endpoints.

/// Poll cadence for the aligned dibit-capture endpoints. Fast enough
/// that the arm-and-wait loop returns within ~50 ms of the reader
/// populating the snapshot.
const CAPTURE_POLL_INTERVAL_MS: u64 = 50;

/// Deadline for `/api/control_dibit_capture_aligned`. The LSM dibit
/// reader IRQ fires only about every 3.5 s (one buffer per IRQ) and
/// holds the decoder write() lock while draining — 10 s gives us at
/// least 3 cycles of headroom before declaring the chain stalled.
const CONTROL_CAPTURE_TIMEOUT_MS: u64 = 10_000;

/// Deadline for `/api/traffic_dibit_capture_aligned`. The traffic
/// framer only sees sync hits while a real call is active, so 15 s
/// covers arming just before a grant arrives.
const TRAFFIC_CAPTURE_TIMEOUT_MS: u64 = 15_000;

/// Retry delay for `/api/iq_dump` when not enough sub-buffers have
/// arrived yet. Sub-buffers are ~131 ms wide post-DDC, so 60 ms is
/// faster than the arrival rate — tight enough to not miss one, but
/// not so tight it spins on the lock.
const IQ_DRAIN_RETRY_MS: u64 = 60;

/// Returns recent dibits as a hex string + diagnostic counters.
///
/// Each pair of hex chars = 8 dibits. Useful for sanity-checking
/// the demod output from a browser without devmem on the target.
pub async fn get_dibit_dump(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let decoder = state.decoder.read().await;
    Json(dibit_dump_json(&decoder, "C4FM HDL chain (c4fm_dibit_dma)"))
}


/// Phase 6F.2: LSM-side counterpart of `/api/dibit_dump`. Same diagnostic
/// shape but reads from `lsm_decoder` (the software decoder fed by
/// `lsm_dibit_dma`). Lets us compare the LSM dibit stream's histogram /
/// sync correlator / raw_DUID distribution against the C4FM stream side
/// by side without having to grep the on-target log.
pub async fn get_control_lsm_dibit_dump(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let decoder = state.lsm_decoder.read().await;
    Json(dibit_dump_json(&decoder, "PL HDL LSM chain (lsm_dibit_dma)"))
}


/// Phase 10-prep: traffic-side counterpart of `/api/control_lsm_dibit_dump`.
/// Reads from `traffic_lsm_decoder` so we can diagnose the traffic
/// framer's slicer / sync correlator / raw_DUID distribution without
/// waiting for a grant. Same response shape as the control-side
/// endpoint so dashboard / tooling can treat them symmetrically.
pub async fn get_traffic_lsm_dibit_dump(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let decoder = state.traffic_lsm_decoder.read().await;
    Json(dibit_dump_json(
        &decoder, "PL HDL traffic LSM chain (traffic_lsm_dibit_dma)"))
}


/// Phase 6F.2h diagnostic capture endpoint.
///
/// Returns the LSM decoder's `recent_dibits` rolling buffer (up to 2048
/// raw on-air dibits) as a hex string + the cumulative dibit counter
/// at capture time so a follow-up call can detect overlaps.
///
/// Arming the next-sync alignment capture is a separate endpoint
/// (`/api/control_dibit_capture_aligned`); this one just returns
/// whatever's currently in the rolling buffer with no waiting.
///
/// **Not IQ** — this returns post-demod hard-decision dibits. For raw
/// post-DDC complex IQ samples use `/api/control_iq_dump`.
pub async fn get_control_dibit_capture(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let decoder = state.lsm_decoder.read().await;
    let dibits: Vec<u8> = decoder.recent_dibits.iter().copied().collect();
    let hex: String = dibits.iter().map(|d| format!("{:1X}", d & 0x3)).collect();
    Json(serde_json::json!({
        "captured":     dibits.len(),
        "total_dibits": decoder.total_dibits(),
        "dibits_hex":   hex,
        "note": "One hex digit per dibit, oldest first. Each digit is the \
                 low 2 bits (00..03). 4800 sym/s -> 2048 dibits ~= 426 ms.",
    }))
}


/// Phase 10-prep: traffic-side counterpart of `/api/control_dibit_capture`.
/// Rolling recent-dibits buffer from the traffic LSM decoder.
pub async fn get_traffic_dibit_capture(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let decoder = state.traffic_lsm_decoder.read().await;
    let dibits: Vec<u8> = decoder.recent_dibits.iter().copied().collect();
    let hex: String = dibits.iter().map(|d| format!("{:1X}", d & 0x3)).collect();
    Json(serde_json::json!({
        "chain":        "traffic",
        "captured":     dibits.len(),
        "total_dibits": decoder.total_dibits(),
        "dibits_hex":   hex,
        "note": "Same shape as /api/control_dibit_capture, but reads from the \
                 traffic-chain rolling dibit buffer so post-retune slicer \
                 behaviour can be inspected without a call being active.",
    }))
}


/// Phase 6F.2h diagnostic capture endpoint -- aligned snapshot.
///
/// Arms the LSM decoder to capture the next sync hit and returns the
/// full pipeline trace through that one frame:
///   - 24 sync dibits
///   - 33 raw NID dibits (status dibit at index 11 not yet skipped)
///   - 64-bit nid_bits word fed to BCH (status dibit removed, packed
///     MSB-first)
///   - BCH-corrected NAC + DUID + raw DUID
///   - 122 raw TSDU body dibits
///   - 98 trellis data dibits after status + null removal
///   - 12 trellis-decoded TSBK bytes
///   - CRC validation result (Plain | Xored | None)
///
/// This is the data we use to bisect between "deinterleaver bug" and
/// "trellis bug" if the dashboard counters say PS LSM is still failing
/// CRC after 6F.2g lands.
///
/// **Behaviour:** the endpoint is one-shot per call. It arms the
/// capture flag, then waits up to 2 seconds for the next sync hit. If
/// no sync hits in that window it returns `{"status": "timeout"}`.
/// Otherwise it returns the snapshot and clears the armed state.
///
/// **Not IQ** — this returns post-demod decoded frame data. For raw
/// post-DDC complex IQ samples use `/api/control_iq_dump`.
pub async fn get_control_dibit_capture_aligned(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    use std::time::{Duration, Instant};

    // Arm the capture: clear any previous snapshot, set the armed flag.
    {
        let mut dec = state.lsm_decoder.write().await;
        dec.aligned_capture = None;
        dec.aligned_capture_armed = true;
    }

    // Poll for up to 10 seconds. The LSM dibit reader IRQ fires only
    // about every 3.5 seconds (one buffer per IRQ), and the reader
    // holds the decoder write() lock for the duration of one buffer
    // (~8000 dibits). So the API may have to wait up to 2 IRQ cycles
    // before it can read a populated capture. 10 s gives us at least
    // 3 cycles of headroom -- if no sync hits in that long, the chain
    // is genuinely stalled.
    let deadline = Instant::now() + Duration::from_millis(CONTROL_CAPTURE_TIMEOUT_MS);
    loop {
        {
            let dec = state.lsm_decoder.read().await;
            if let Some(snap) = dec.aligned_capture.as_ref() {
                return Json(snap.to_json());
            }
        }
        if Instant::now() >= deadline {
            // Disarm so we don't capture later than the user expects.
            let mut dec = state.lsm_decoder.write().await;
            dec.aligned_capture_armed = false;
            return Json(serde_json::json!({
                "status": "timeout",
                "note": "No sync hit observed within 2 seconds. Either the \
                         LSM dibit stream is stalled (check IRQ counters) \
                         or the sync correlator is missing every frame \
                         (check best Hamming distance on the LSM dibit dump).",
            }));
        }
        tokio::time::sleep(Duration::from_millis(CAPTURE_POLL_INTERVAL_MS)).await;
    }
}


/// Phase 10-prep: traffic-side counterpart of
/// `/api/control_dibit_capture_aligned`. Same arm + wait protocol but
/// against the traffic LSM decoder. Useful for debug-capturing a
/// post-retune TDU or LDU frame to see where the slicer / framer
/// is landing before the grant follower cancels the retune.
pub async fn get_traffic_dibit_capture_aligned(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    use std::time::{Duration, Instant};

    {
        let mut dec = state.traffic_lsm_decoder.write().await;
        dec.aligned_capture = None;
        dec.aligned_capture_armed = true;
    }

    // Traffic framer only sees sync hits while a real call is active
    // (the noise-gate changes landing in the Phase 10 bake will make
    // this even more true). Give up to 15 seconds of headroom in
    // case the user is arming this just before a grant arrives.
    let deadline = Instant::now() + Duration::from_millis(TRAFFIC_CAPTURE_TIMEOUT_MS);
    loop {
        {
            let dec = state.traffic_lsm_decoder.read().await;
            if let Some(snap) = dec.aligned_capture.as_ref() {
                return Json(snap.to_json());
            }
        }
        if Instant::now() >= deadline {
            let mut dec = state.traffic_lsm_decoder.write().await;
            dec.aligned_capture_armed = false;
            return Json(serde_json::json!({
                "status": "timeout",
                "chain":  "traffic",
                "note": "No sync hit on the traffic chain within 15 s. \
                         Likely no active call during the capture window -- \
                         arm again while a grant is in progress for a \
                         populated snapshot.",
            }));
        }
        tokio::time::sleep(Duration::from_millis(CAPTURE_POLL_INTERVAL_MS)).await;
    }
}


// ── IQ dump endpoints (2026-04-18) ───────────────────────────────
//
// Post-DDC complex IQ capture, served as a WAV file so SDRTrunk and
// similar tools can replay it through their own demod. The ring
// already exists — /api/spectrum and /api/constellation both read
// from it — so the endpoint is a thin "accumulate N seconds of sub-
// buffers and wrap in RIFF". The bytes in each sub-buffer are
// already little-endian i16 interleaved (re, im), which is exactly
// WAV stereo i16 PCM layout, so no sample conversion is needed.
//
// Primary use case: Phase 10.5 item 1a SDRTrunk cross-validation —
// capture during a "loose" cluster_var_mean moment, replay on a PC,
// determine whether the X-pattern is upstream of our demod loop.
// Also used for eye-plot offline analysis.
//
// Sample rate is fixed at post-DDC 62.5 kSPS on both chains; the
// same value is exported by spectrum::SAMPLE_RATE_HZ.

/// Post-DDC sample rate on both chains. Matches spectrum::SAMPLE_RATE_HZ.
const IQ_DUMP_SAMPLE_RATE_HZ: u32 = 62_500;

/// Maximum seconds per /api/*_iq_dump request. 60 s × 62.5 kSPS ×
/// 4 bytes = 14.6 MB; higher values blow HTTP client timeouts and
/// are better served by multiple back-to-back calls.
const IQ_DUMP_MAX_SECONDS: u32 = 60;

/// Default seconds when the client omits `?seconds=`.
const IQ_DUMP_DEFAULT_SECONDS: u32 = 5;

/// Wrap a little-endian i16-stereo-interleaved byte buffer in a PCM
/// WAV container at the given sample rate. Matches SDRTrunk's own
/// "save baseband IQ" format, so files written by this endpoint open
/// natively in SDRTrunk with I→left, Q→right.
fn wrap_as_wav(iq_bytes: &[u8], sample_rate_hz: u32) -> Vec<u8> {
    let channels: u16 = 2;
    let bits_per_sample: u16 = 16;
    let byte_rate: u32 = sample_rate_hz * channels as u32 * bits_per_sample as u32 / 8;
    let block_align: u16 = channels * bits_per_sample / 8;
    let data_size: u32 = iq_bytes.len() as u32;
    let riff_size: u32 = 36 + data_size;

    let mut out = Vec::with_capacity(44 + iq_bytes.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());   // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes());    // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate_hz.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(iq_bytes);
    out
}

/// Parse + clamp the `?seconds=` query param. Invalid / missing →
/// `IQ_DUMP_DEFAULT_SECONDS`; out-of-range → clamp.
fn parse_seconds(params: &std::collections::HashMap<String, String>) -> u32 {
    params
        .get("seconds")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(IQ_DUMP_DEFAULT_SECONDS)
        .clamp(1, IQ_DUMP_MAX_SECONDS)
}

#[cfg(target_os = "linux")]
fn iq_dump_error_response(msg: String) -> Response {
    use axum::http::StatusCode;
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({ "ok": false, "error": msg }).to_string(),
        ))
        .unwrap()
}

#[cfg(target_os = "linux")]
fn wav_response(wav: Vec<u8>, chain: &str, seconds: u32) -> Response {
    use axum::http::StatusCode;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let filename = format!(
        "{chain}_{}_{ts}_{seconds}s.wav",
        IQ_DUMP_SAMPLE_RATE_HZ
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "audio/wav")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{filename}\""),
        )
        .header("x-sample-rate-hz", IQ_DUMP_SAMPLE_RATE_HZ.to_string())
        .header("x-channels", "2")
        .header("x-format", "i16le-iq-stereo")
        .body(axum::body::Body::from(wav))
        .unwrap()
}

/// `GET /api/control_iq_dump?seconds=N`
///
/// Captures N seconds of post-DDC complex IQ from the control chain's
/// `iq_dma` ring (62.5 kSPS) and returns it as a PCM WAV file with
/// I=left, Q=right. `seconds` defaults to 5, clamps to 1..60.
///
/// The ring emits 32 KB sub-buffers (8192 complex samples ≈ 131 ms)
/// via DMA, so the handler holds the `ip_core` lock only in short
/// windows to let the consumer drain without blocking the reader.
/// Total time-to-respond ≈ `seconds` plus ~200 ms of slack.
#[cfg(target_os = "linux")]
pub async fn get_control_iq_dump(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let seconds = parse_seconds(&params);
    match accumulate_iq(&state, "control", seconds).await {
        Ok(bytes) => wav_response(wrap_as_wav(&bytes, IQ_DUMP_SAMPLE_RATE_HZ), "control", seconds),
        Err(msg) => iq_dump_error_response(msg),
    }
}

/// `GET /api/traffic_iq_dump?seconds=N`
///
/// Traffic-chain counterpart of `/api/control_iq_dump`. Reads the
/// `traffic_iq_dma` ring instead; the DDC center is the follower's
/// current NCO offset (RX LO + TrafficManager.last_offset_hz), so the
/// captured IQ is already centered on whatever traffic frequency the
/// grant follower last retuned to. Requires the bake-#2 bitstream.
#[cfg(target_os = "linux")]
pub async fn get_traffic_iq_dump(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let seconds = parse_seconds(&params);
    match accumulate_iq(&state, "traffic", seconds).await {
        Ok(bytes) => wav_response(wrap_as_wav(&bytes, IQ_DUMP_SAMPLE_RATE_HZ), "traffic", seconds),
        Err(msg) => iq_dump_error_response(msg),
    }
}

/// Non-Linux stubs so the workspace still compiles on Windows for
/// cargo check. Same shape as the other target-gated endpoints.
#[cfg(not(target_os = "linux"))]
pub async fn get_control_iq_dump(
    State(_state): State<Arc<AppState>>,
    axum::extract::Query(_params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    iq_dump_stub_response()
}

#[cfg(not(target_os = "linux"))]
pub async fn get_traffic_iq_dump(
    State(_state): State<Arc<AppState>>,
    axum::extract::Query(_params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    iq_dump_stub_response()
}

#[cfg(not(target_os = "linux"))]
fn iq_dump_stub_response() -> Response {
    use axum::http::StatusCode;
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({
                "ok": false,
                "error": "iq_dump only available on the target (linux/arm)",
            })
            .to_string(),
        ))
        .unwrap()
}

/// Core loop: poll the chain's iq_dma ring for `seconds` wall-clock
/// seconds, accumulating raw bytes. Holds the `ip_core` mutex only
/// while calling `read_*_iq_buffers()` so other HDL readers are not
/// starved. Returns the accumulated little-endian-i16-stereo bytes.
#[cfg(target_os = "linux")]
async fn accumulate_iq(
    state: &Arc<AppState>,
    chain: &str,
    seconds: u32,
) -> Result<Vec<u8>, String> {
    let target_samples: usize = (seconds as usize) * (IQ_DUMP_SAMPLE_RATE_HZ as usize);
    let target_bytes: usize = target_samples * 4;
    let mut acc: Vec<u8> = Vec::with_capacity(target_bytes);

    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis((seconds as u64) * 1000 + 2000);

    while acc.len() < target_bytes && std::time::Instant::now() < deadline {
        {
            let mut core = state.ip_core.lock().await;
            let bufs: Vec<&[u8]> = match chain {
                "control" => core.read_iq_buffers(),
                "traffic" => core.read_traffic_iq_buffers(),
                other => {
                    return Err(format!(
                        "unknown chain '{other}'; expected control|traffic"
                    ));
                }
            };
            for b in bufs {
                acc.extend_from_slice(b);
                if acc.len() >= target_bytes {
                    break;
                }
            }
        }
        if acc.len() < target_bytes {
            tokio::time::sleep(std::time::Duration::from_millis(IQ_DRAIN_RETRY_MS)).await;
        }
    }

    if acc.len() < 4 {
        return Err(format!(
            "no IQ data available on chain={chain} within {seconds}+2 s; \
             is iq_dma enabled in the current bitstream?"
        ));
    }

    acc.truncate(target_bytes.min(acc.len() - (acc.len() % 4)));
    Ok(acc)
}

/// Phase 6G.2: read-back of the `lsm_control` register, plus an
/// optional GET-with-query-param shortcut for toggling
/// `lsm_dc_block_enable` without ssh + devmem.
///
/// Without query params, returns the current state of all three
/// `lsm_control` bits + a hint about which bit positions they map
/// to. The dashboard can poll this once a second to surface the
/// "is the DC blocker actually on?" question that previously
/// required scraping the startup log.
///
/// With `?dc_block=0` or `?dc_block=1`, ALSO writes the bit before
/// reading back. This is the runtime A/B knob the doc 030 PL port
/// roadmap (and Phase 6G.1 verification plan in doc 031) wanted
/// but had to do via `devmem` previously. Range-checked: only
/// `0` or `1` are accepted, everything else is ignored. The two
/// other lsm_control bits (`lsm_enable`, `lsm_dibit_dma_enable`)
/// are NOT exposed for write here -- those are master enables that
/// shouldn't be flipped at runtime, and there's no debugging story
/// that needs them.
///
/// Returns the same shape whether or not the write happened, so a
/// curl-based A/B test loop can just toggle and re-read in one
/// request:
///
/// ```text
/// curl http://192.168.2.1:8080/api/control_lsm_control?dc_block=0
/// curl http://192.168.2.1:8080/api/control_lsm_control?dc_block=1
/// ```
pub async fn get_control_lsm_control(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let mut updated_from: Option<bool> = None;

    #[cfg(target_os = "linux")]
    {
        // Take the ip_core lock once, do both the optional write and
        // the readback under it so nothing can race in between.
        let core = state.ip_core.lock().await;

        if let Some(v) = params.get("dc_block") {
            let new_val = match v.as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            };
            if let Some(new_bit) = new_val {
                let (_, _, prev, _) = core.lsm_control_readback();
                core.set_lsm_dc_block_enable(new_bit);
                updated_from = Some(prev);
            }
        }

        let (lsm_en, lsm_dma_en, lsm_dc_block, lsm_agc) = core.lsm_control_readback();
        Json(serde_json::json!({
            "lsm_enable":            lsm_en,
            "lsm_dibit_dma_enable":  lsm_dma_en,
            "lsm_dc_block_enable":   lsm_dc_block,
            "lsm_agc_enable":        lsm_agc,
            "updated_from":          updated_from,
            "register_address":      "0x7C4600A0",
            "bit_layout": {
                "lsm_enable":            "[0]",
                "lsm_dibit_dma_enable":  "[1]",
                "lsm_dc_block_enable":   "[2]",
                "lsm_agc_enable":        "[4]"
            },
            "note": "GET /api/control_lsm_control?dc_block=0 disables the LSM \
                     front-end DC blocker; ?dc_block=1 enables it. The \
                     other two bits are not writable from this endpoint \
                     -- toggle them via devmem if you really need to. \
                     See doc/changes/031 + 032 for the rationale.",
        }))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (state, params, &mut updated_from);
        Json(serde_json::json!({
            "ok": false,
            "error": "lsm_control read/write requires hardware (target_os=linux)",
        }))
    }
}


/// Phase 10-prep: traffic-side counterpart of
/// `/api/control_lsm_control`. Reads/writes the `traffic_lsm_control`
/// HDL register:
///   bit 0: traffic_lsm_enable
///   bit 1: traffic_lsm_dibit_dma_enable
///   bit 2: traffic_lsm_dc_block_enable
///   bit 3: traffic_lsm_agc_enable  (Phase 10-prep)
///
/// Currently only `dc_block` and `agc` are writable from this
/// endpoint; the enable + dma_enable bits are managed by
/// `retune_traffic_chain` and shouldn't be flipped out-of-band.
pub async fn get_traffic_lsm_control(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let mut updated_dc: Option<bool> = None;
    let mut updated_agc: Option<bool> = None;

    #[cfg(target_os = "linux")]
    {
        let core = state.ip_core.lock().await;

        if let Some(v) = params.get("dc_block") {
            if let Some(new_bit) = match v.as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            } {
                let (_, _, prev, _) = core.traffic_lsm_control_readback();
                core.set_traffic_lsm_dc_block_enable(new_bit);
                updated_dc = Some(prev);
            }
        }

        if let Some(v) = params.get("agc") {
            if let Some(new_bit) = match v.as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            } {
                let (_, _, _, prev) = core.traffic_lsm_control_readback();
                core.set_traffic_lsm_agc_enable(new_bit);
                updated_agc = Some(prev);
            }
        }

        let (en, dma_en, dc_block, agc) = core.traffic_lsm_control_readback();
        Json(serde_json::json!({
            "chain":                        "traffic",
            "traffic_lsm_enable":           en,
            "traffic_lsm_dibit_dma_enable": dma_en,
            "traffic_lsm_dc_block_enable":  dc_block,
            "traffic_lsm_agc_enable":       agc,
            "updated_dc_block_from":        updated_dc,
            "updated_agc_from":             updated_agc,
            "note": "GET /api/traffic_lsm_control?dc_block=0|1 toggles \
                     the traffic-chain DC blocker; ?agc=0|1 toggles the \
                     per-symbol AGC. The enable + dibit_dma_enable bits \
                     are managed by retune_traffic_chain and are \
                     read-only here.",
        }))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (state, params, &mut updated_dc, &mut updated_agc);
        Json(serde_json::json!({
            "ok": false,
            "error": "traffic_lsm_control requires hardware (target_os=linux)",
        }))
    }
}


pub fn dibit_dump_json(
    decoder: &ControlChannelDecoder,
    source_label: &str,
) -> serde_json::Value {
    let dibits: Vec<u8> = decoder.recent_dibits.iter().copied().collect();

    // Pack 4 dibits per byte (LSB first), MSB-first byte ordering
    let mut packed = Vec::with_capacity(dibits.len().div_ceil(4));
    for chunk in dibits.chunks(4) {
        let mut b = 0u8;
        for (i, d) in chunk.iter().enumerate() {
            b |= (d & 0x03) << (i * 2);
        }
        packed.push(b);
    }
    let hex: String = packed.iter().map(|b| format!("{:02X}", b)).collect();

    // Histogram + sync stats
    let hist = decoder.dibit_histogram();
    let total = decoder.total_dibits();
    let pct = |v: u64| -> f64 {
        if total == 0 { 0.0 } else { 100.0 * v as f64 / total as f64 }
    };

    // Inner-vs-outer ratio is the canonical health indicator for the
    // symbol-rate slicer: random P25 data should give ~50/50, and a
    // residual DC bias on sym_diff_re skews it. Today (2026-04-09) we're
    // running at ~70/30 inner/outer because of post-DDC DC pedestal,
    // which is why CC_SYNC_THRESHOLD is currently 10 instead of 4.
    let inner = hist[0] + hist[2]; // values 0 (+1) and 2 (-1)
    let outer = hist[1] + hist[3]; // values 1 (+3) and 3 (-3)

    // Raw on-air DUID histogram. With the BCH(64,16) NID FEC currently
    // stubbed (see fec::GolayDecoder::decode_nid), this tells us how
    // often each 4-bit DUID value lands in the NID field after sync.
    // A healthy control channel + working FEC would be ~100% in bucket 7
    // (TSDU). Today, with no FEC, this is a near-uniform spray due to
    // the ~12 bit errors per NID induced by the slicer DC bias.
    let raw_duid_hist = decoder.raw_duid_histogram();
    let raw_duid_total: u64 = raw_duid_hist.iter().sum();
    let raw_duid_pct = |v: u64| -> f64 {
        if raw_duid_total == 0 { 0.0 } else { 100.0 * v as f64 / raw_duid_total as f64 }
    };

    serde_json::json!({
        "source": source_label,
        "total_dibits": total,
        "captured": dibits.len(),
        "histogram": {
            "0":     hist[0],
            "1":     hist[1],
            "2":     hist[2],
            "3":     hist[3],
            "0_pct": pct(hist[0]),
            "1_pct": pct(hist[1]),
            "2_pct": pct(hist[2]),
            "3_pct": pct(hist[3]),
            "inner_pct": pct(inner),
            "outer_pct": pct(outer),
        },
        "sync": {
            "hits":          decoder.sync_hits(),
            "near_misses":   decoder.sync_near_misses(),
            "best_distance": decoder.best_sync_distance(),
            "threshold":     RUNTIME_SYNC_THRESHOLD.load(std::sync::atomic::Ordering::Relaxed),
            "threshold_default": CC_SYNC_THRESHOLD,
            // Phase 6F.6 distance histogram. Bucket i = count of dibit
            // shifts where the sync_register matched at exactly Hamming
            // distance i. Bucket 24 collects everything ≥ 24. The cluster
            // shape tells us whether the slicer is the bottleneck or
            // whether widening the threshold further would help.
            "distance_hist": decoder.sync_distance_hist,
        },
        "raw_duid": {
            "total": raw_duid_total,
            "counts": raw_duid_hist,
            "pct_7_tsdu": raw_duid_pct(raw_duid_hist[7]),
            "pct_5_ldu1": raw_duid_pct(raw_duid_hist[5]),
            "pct_0_hdu":  raw_duid_pct(raw_duid_hist[0]),
            "pct_a_ldu2": raw_duid_pct(raw_duid_hist[0xA]),
            "note": "Healthy control channel + BCH FEC = ~100% in bucket 7 (TSDU). \
                     Anything else means the NID has uncorrected bit errors.",
        },
        "pipeline": {
            "nid_attempts":          decoder.nid_attempts,
            "nid_decode_failures":   decoder.nid_decode_failures,
            "nid_invalid_duid":      decoder.nid_invalid_duid,
            "nid_decoded_ok":        decoder.nid_decoded_ok,
            "nid_decoded_tsdu":      decoder.nid_decoded_tsdu,
            "tsdu_attempts":         decoder.tsdu_attempts,
            "tsbk_block_attempts":   decoder.tsbk_block_attempts,
            "tsbk_trellis_failures": decoder.tsbk_trellis_failures,
            "tsbk_crc_failures":     decoder.tsbk_crc_failures,
            "tsbk_crc_ok":           decoder.tsbk_crc_ok,
            "tsbk_unknown_opcode":   decoder.tsbk_unknown_opcode,
        },
        "dibits_hex": hex,
    })
}

/// Phase 6D: snapshot of the LSM pipeline runtime stats.
///
// Phase 9 retirement: `get_lsm()` (Phase 6D software pipeline stats
// for the dashboard "LSM Pipeline" card) was removed here along with
// the `LsmStats` struct it read. The PL-side equivalent is
// `/api/hdl_lsm` (below), which taps the HDL register bank directly
// via the `HdlLsmRuntime` heartbeat task. That's the single source of
// truth for "is the LSM chain alive / how many valid NIDs / what
// NACs" now.


/// GET /api/nid_capture -- batch NID capture tail for offline BCH
/// analysis.
///
/// Query params:
///   side      = "control" (default) | "traffic"
///   arm       = 1 to arm the ring, 0 to disarm
///   limit     = ring size when arming (default 256, capped at 1024)
///   clear     = 1 to drain the ring and disarm (one-shot readout)
///
/// Read flow:
///   1. `?arm=1&limit=256`  — arm the ring, return {armed:true, limit:256}
///   2. Wait 10-30 s for real traffic to populate the ring
///   3. `?clear=1`          — drain + disarm, returns the full batch
///
/// Offline analysis tool: tools/p25_nid_analyze.py.
pub async fn get_nid_capture(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    let side = params
        .get("side")
        .cloned()
        .unwrap_or_else(|| "control".to_string());
    let arm = params.get("arm").map(String::as_str) == Some("1");
    let clear = params.get("clear").map(String::as_str) == Some("1");
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(256)
        .min(1024);

    // Pick which decoder's ring to hit. `lsm_decoder` for control side
    // (the NAC-healthy one), `traffic_lsm_decoder` for the DUID-broken
    // side that actually matters for audio.
    let decoder_lock = match side.as_str() {
        "traffic" => state.traffic_lsm_decoder.clone(),
        _         => state.lsm_decoder.clone(),
    };

    let mut dec = decoder_lock.write().await;

    let mut action = Vec::new();
    if clear {
        let entries: Vec<_> = dec.drain_capture_ring()
            .into_iter()
            .map(|e| e.to_json())
            .collect();
        action.push("cleared".to_string());
        return Json(serde_json::json!({
            "side":    side,
            "action":  action,
            "count":   entries.len(),
            "entries": entries,
        }));
    }
    if arm {
        dec.arm_capture_ring(limit);
        action.push(format!("armed limit={}", limit));
    }
    let count = dec.capture_ring.len();
    let armed = dec.capture_ring_armed;
    let cap_limit = dec.capture_ring_limit;
    let bch_t = dec.bch_t_override;
    // Snapshot without draining so the client can poll mid-run.
    let entries: Vec<_> = dec.snapshot_capture_ring()
        .into_iter()
        .map(|e| e.to_json())
        .collect();
    Json(serde_json::json!({
        "side":    side,
        "action":  action,
        "armed":   armed,
        "limit":   cap_limit,
        "count":   count,
        "bch_t_override": bch_t,
        "entries": entries,
    }))
}


