//! HTTP server and REST API
//!
//! Endpoints:
//! - GET  /              -> Dashboard SPA (embedded HTML)
//! - GET  /api/system    -> System identity
//! - GET  /api/grants    -> Active voice grants
//! - GET  /api/bands     -> Frequency band table
//! - GET  /api/stats     -> Decoder statistics
//! - GET  /api/lsm       -> LSM pipeline runtime stats (Phase 6D)
//! - GET  /api/aliases   -> Talkgroup alias map
//! - PUT  /api/aliases   -> Update alias map
//! - WS   /ws/events     -> Real-time TSBK event stream

use std::sync::Arc;

use axum::{
    extract::{ws::WebSocket, State, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use tokio::sync::{broadcast, RwLock};

use crate::p25::control_channel::{ControlChannelDecoder, SYNC_THRESHOLD};
use p25_json::*;

/// Shared application state
pub struct AppState {
    /// Original Phase 2A C4FM `ControlChannelDecoder`, fed by the C4FM HDL
    /// chain via `dibit_dma`. Retained for diagnostics and as a fallback,
    /// but no longer the primary source for the dashboard's identity /
    /// stats / grants panels (those now read `lsm_decoder` -- see below).
    pub decoder: Arc<RwLock<ControlChannelDecoder>>,
    /// Phase 6E.10 LSM `ControlChannelDecoder`, fed by the HDL LSM chain
    /// via `lsm_dibit_dma`. This is now the source of truth for the
    /// dashboard's System Identity, Decode Stats, Active Grants, and
    /// Frequency Bands panels because the test target (Clay County NAC
    /// 0x8A1) is an LSM simulcast control channel that the C4FM decoder
    /// only ever sees as garbage. Phase 6F.1 dashboard migration --
    /// see doc/changes/024 follow-up notes.
    pub lsm_decoder: Arc<RwLock<ControlChannelDecoder>>,
    pub event_tx: broadcast::Sender<String>,
    #[cfg(target_os = "linux")]
    pub ip_core: Arc<tokio::sync::Mutex<crate::fpga::IpCore>>,
    /// AD9361 IIO handle for live AGC gain / RSSI readback in /api/stats.
    /// Stateless wrapper around sysfs paths -- safe to share without a lock.
    #[cfg(target_os = "linux")]
    pub ad9361: Arc<crate::iio::Ad9361>,
    /// Phase 6D: LSM pipeline runtime stats. Populated by the LSM tokio
    /// task on every iq_dma wake; read by the `/api/lsm` handler to
    /// surface the parallel LSM decoder on the dashboard alongside the
    /// existing C4FM dibit pipeline panels.
    pub lsm_stats: Arc<tokio::sync::Mutex<crate::lsm::LsmStats>>,
    /// Phase 6F.2: PL HDL LSM chain runtime stats, populated by the
    /// HDL LSM heartbeat task. Read by `/api/hdl_lsm`. Single source
    /// of truth for everything the heartbeat task observes about the
    /// FPGA-side LSM chain (registers, NID counts, NAC histogram,
    /// last 32 NID ring buffer).
    pub hdl_lsm: Arc<tokio::sync::Mutex<crate::HdlLsmRuntime>>,
    /// Phase 6F.2: per-source IRQ counters from the InterruptHandler
    /// task. Read by `/api/irq_stats`.
    pub irq_stats: Arc<tokio::sync::Mutex<crate::IrqStats>>,
}

/// Build the HTTP router
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/api/system", get(get_system))
        .route("/api/grants", get(get_grants))
        .route("/api/bands", get(get_bands))
        .route("/api/stats", get(get_stats))
        .route("/api/lsm", get(get_lsm))
        .route("/api/hdl_lsm", get(get_hdl_lsm))
        .route("/api/irq_stats", get(get_irq_stats))
        .route("/api/decoder_compare", get(get_decoder_compare))
        .route("/api/dibit_dump", get(get_dibit_dump))
        .route("/api/lsm_dibit_dump", get(get_lsm_dibit_dump))
        .route("/api/lsm_capture", get(get_lsm_capture))
        .route("/api/lsm_capture_aligned", get(get_lsm_capture_aligned))
        .route("/api/aliases", get(get_aliases).put(put_aliases))
        .route("/ws/events", get(ws_events))
        .with_state(state)
}

// ── REST Handlers ──────────────────────────────────────────────────────

async fn get_system(State(state): State<Arc<AppState>>) -> Json<SystemInfo> {
    let decoder = state.lsm_decoder.read().await;
    let sys = &decoder.system;
    Json(SystemInfo {
        nac: sys.nac.map(|n| format!("{}", n)),
        wacn: sys.wacn.map(|w| format!("{:05X}", w)),
        system_id: sys.system_id.map(|s| format!("{:03X}", s)),
        rfss_id: sys.rfss_id,
        site_id: sys.site_id,
        lra: sys.lra,
        control_channel: sys.control_channel.map(|c| format!("{}", c)),
        build: Some(crate::BUILD_TAG.to_string()),
    })
}

async fn get_grants(State(state): State<Arc<AppState>>) -> Json<Vec<ChannelGrant>> {
    let decoder = state.lsm_decoder.read().await;
    let grants: Vec<ChannelGrant> = decoder
        .grants
        .values()
        .map(|g| ChannelGrant {
            channel: format!("{}", g.channel),
            talkgroup: g.talkgroup.0,
            talkgroup_alias: decoder.aliases.get(&g.talkgroup.0).cloned(),
            source: g.source.map(|s| s.0),
            frequency_mhz: g.frequency_hz.map(|f| f as f64 / 1_000_000.0),
            age_secs: g.timestamp.elapsed().as_secs(),
        })
        .collect();
    Json(grants)
}

async fn get_bands(State(state): State<Arc<AppState>>) -> Json<Vec<BandInfo>> {
    let decoder = state.lsm_decoder.read().await;
    let mut bands: Vec<BandInfo> = decoder
        .bands
        .values()
        .map(|b| BandInfo {
            identifier: b.identifier,
            base_frequency_mhz: b.base_frequency_hz as f64 / 1_000_000.0,
            channel_spacing_khz: b.channel_spacing_hz as f64 / 1_000.0,
            transmit_offset_mhz: b.transmit_offset_hz as f64 / 1_000_000.0,
            bandwidth_khz: b.bandwidth_hz as f64 / 1_000.0,
        })
        .collect();
    bands.sort_by_key(|b| b.identifier);
    Json(bands)
}

async fn get_stats(State(state): State<Arc<AppState>>) -> Json<DecoderStats> {
    let decoder = state.lsm_decoder.read().await;

    #[cfg(target_os = "linux")]
    let (dibit_count, overflow, dma_next_address) = {
        let core = state.ip_core.lock().await;
        (
            core.dibit_count() as u32,
            core.demod_overflow(),
            core.dibit_next_address(),
        )
    };
    #[cfg(not(target_os = "linux"))]
    let (dibit_count, overflow, dma_next_address) = (0u32, false, 0u32);

    // AD9361 health: AGC gain (high = AGC searching for weak signal) and
    // RSSI (relative dB scale; for this band, ~100-110 dB is normal P25
    // reception, lower = quieter). Surfacing these via /api/stats so we
    // never have to ssh in and devmem just to find out the radio is alive.
    #[cfg(target_os = "linux")]
    let (rx_gain_db, rx_rssi_db) = {
        let g = state.ad9361.get_rx_gain().await.ok();
        let r = state.ad9361.get_rx_rssi().await.ok();
        (g, r)
    };
    #[cfg(not(target_os = "linux"))]
    let (rx_gain_db, rx_rssi_db): (Option<f64>, Option<f64>) = (None, None);

    Json(DecoderStats {
        recent_messages: decoder.recent_messages.len(),
        active_grants: decoder.grants.len(),
        bands_known: decoder.bands.len(),
        system_acquired: decoder.system.wacn.is_some(),
        dibit_count,
        overflow,
        dma_next_address,
        rx_gain_db,
        rx_rssi_db,
    })
}

/// Returns recent dibits as a hex string + diagnostic counters.
///
/// Each pair of hex chars = 8 dibits. Useful for sanity-checking
/// the demod output from a browser without devmem on the target.
async fn get_dibit_dump(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let decoder = state.decoder.read().await;
    Json(dibit_dump_json(&decoder, "C4FM HDL chain (c4fm_dibit_dma)"))
}

/// Phase 6F.2: LSM-side counterpart of `/api/dibit_dump`. Same diagnostic
/// shape but reads from `lsm_decoder` (the software decoder fed by
/// `lsm_dibit_dma`). Lets us compare the LSM dibit stream's histogram /
/// sync correlator / raw_DUID distribution against the C4FM stream side
/// by side without having to grep the on-target log.
async fn get_lsm_dibit_dump(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let decoder = state.lsm_decoder.read().await;
    Json(dibit_dump_json(&decoder, "PL HDL LSM chain (lsm_dibit_dma)"))
}

/// Phase 6F.2h diagnostic capture endpoint.
///
/// Returns the LSM decoder's `recent_dibits` rolling buffer (up to 2048
/// raw on-air dibits) as a base64-encoded byte array, one byte per
/// dibit (only the low 2 bits used). Also returns a hex-string view
/// for human readability and the cumulative dibit counter at capture
/// time so a follow-up call can detect overlaps.
///
/// Arming the next-sync alignment capture is a separate endpoint
/// (`/api/lsm_capture_aligned`); this one just returns whatever's
/// currently in the rolling buffer with no waiting.
async fn get_lsm_capture(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
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
async fn get_lsm_capture_aligned(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    use std::time::{Duration, Instant};

    // Arm the capture: clear any previous snapshot, set the armed flag.
    {
        let mut dec = state.lsm_decoder.write().await;
        dec.aligned_capture = None;
        dec.aligned_capture_armed = true;
    }

    // Poll for up to 2 seconds (~10 NIDs at the on-air rate, plenty
    // of headroom) for the snapshot to populate.
    let deadline = Instant::now() + Duration::from_millis(2000);
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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn dibit_dump_json(
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
    // which is why SYNC_THRESHOLD is currently 10 instead of 4.
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
            "threshold":     SYNC_THRESHOLD,
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
/// Returns everything the dashboard's "LSM Decoder" card needs in one
/// round trip: task liveness, cumulative counters, top-10 NAC histogram,
/// and the most recent sync event. The overflow counter is returned
/// with a note flagging it as a known false positive in the current
/// Phase 6C gateware (see doc 014 follow-ups).
///
/// All times are derived on the server side from `Instant`s inside the
/// stats struct; the client only sees seconds/milliseconds so there is
/// no clock skew issue vs the Fishball's wall clock (which runs from
/// 1970 anyway until NTP lands).
async fn get_lsm(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use std::time::Instant;
    let stats = state.lsm_stats.lock().await;
    let now = Instant::now();

    let uptime_secs = stats
        .started_at
        .map(|t| now.saturating_duration_since(t).as_secs())
        .unwrap_or(0);
    let last_wake_ms_ago = stats
        .last_wake_at
        .map(|t| now.saturating_duration_since(t).as_millis() as u64);

    // Sort top-10 by count desc (LsmStats::top_nacs handles the ordering).
    let total_nac_hits: u64 = stats.nac_hist.values().sum();
    let top_nacs: Vec<serde_json::Value> = stats
        .top_nacs(10)
        .into_iter()
        .map(|(nac, count)| {
            let pct = if total_nac_hits == 0 {
                0.0
            } else {
                100.0 * (count as f64) / (total_nac_hits as f64)
            };
            serde_json::json!({
                "nac":   format!("0x{:03X}", nac),
                "count": count,
                "pct":   pct,
            })
        })
        .collect();

    let last_sync = stats.last_sync.map(|ls| {
        let age_ms = now.saturating_duration_since(ls.at).as_millis() as u64;
        serde_json::json!({
            "nac":           format!("0x{:03X}", ls.nac),
            "duid":          format!("0x{:X}",   ls.duid),
            "fec_corrected": ls.fec_corrected,
            "distance":      ls.distance,
            "score":         ls.score,
            "age_ms":        age_ms,
        })
    });

    // Steady-state rates (avoid divide-by-zero before the first wake).
    let iq_rate_sps = if uptime_secs > 0 {
        stats.iq_samples as f64 / uptime_secs as f64
    } else {
        0.0
    };
    let dibit_rate_sps = if uptime_secs > 0 {
        stats.dibits as f64 / uptime_secs as f64
    } else {
        0.0
    };

    Json(serde_json::json!({
        "running":             stats.started_at.is_some(),
        "uptime_secs":         uptime_secs,
        "last_wake_ms_ago":    last_wake_ms_ago,
        "wakeups":             stats.wakeups,
        "iq_samples":          stats.iq_samples,
        "iq_samples_per_sec":  iq_rate_sps,
        "dibits":              stats.dibits,
        "dibits_per_sec":      dibit_rate_sps,
        "hard_events":         stats.hard_events,
        "soft_events":         stats.soft_events,
        "overflow_resets":     stats.overflow_resets,
        "overflow_note":
            "Phase 6C gateware fires the iq_dma overflow latch spuriously \
             on every sub-buffer; Rust sample math proves no actual data \
             loss. Tracked in doc 014 follow-ups.",
        "top_nacs":            top_nacs,
        "last_sync":           last_sync,
    }))
}

/// Phase 6F.2: PL HDL LSM chain runtime snapshot.
///
/// Reads the shared `HdlLsmRuntime` populated by the heartbeat task.
/// Includes: live register snapshot, cumulative NID counts, NAC
/// histogram, and the last 32 NID events from the ring buffer.
async fn get_hdl_lsm(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use std::time::Instant;
    let rt = state.hdl_lsm.lock().await;
    let now = Instant::now();
    let uptime_secs = rt
        .started_at
        .map(|t| now.saturating_duration_since(t).as_secs())
        .unwrap_or(0);
    let last_tick_ms_ago = rt
        .last_tick_at
        .map(|t| now.saturating_duration_since(t).as_millis() as u64);
    let last_nid_ms_ago = rt
        .last_nid_at
        .map(|t| now.saturating_duration_since(t).as_millis() as u64);

    let total_nac_hits: u64 = rt.nac_hist.values().sum();
    let top_nacs: Vec<serde_json::Value> = rt
        .top_nacs(10)
        .into_iter()
        .map(|(nac, count)| {
            let pct = if total_nac_hits == 0 {
                0.0
            } else {
                100.0 * (count as f64) / (total_nac_hits as f64)
            };
            serde_json::json!({
                "nac":   format!("0x{:03X}", nac),
                "count": count,
                "pct":   pct,
            })
        })
        .collect();

    Json(serde_json::json!({
        "running":              rt.started_at.is_some(),
        "uptime_secs":          uptime_secs,
        "last_tick_ms_ago":     last_tick_ms_ago,
        "last_nid_ms_ago":      last_nid_ms_ago,

        "live": {
            "pll_dbg":               rt.pll_dbg,
            "sp_dbg":                rt.sp_dbg,
            "sync_distance":         rt.sync_distance,
            "bch_busy":              rt.bch_busy,
            "in_nid_window":         rt.in_nid_window,
            "dibit_overflow_latch":  rt.dibit_overflow_latched,
            "iq_overflow_latch":     rt.iq_overflow_latched,
            "last_nac":              format!("0x{:03X}", rt.last_nac),
            "last_duid":             rt.last_duid,
            "last_drop_count":       rt.last_drop_count,
            "last_nid_valid":        rt.last_nid_valid,
            "last_nid_n_errors":     rt.last_nid_n_errors,
        },

        "cumulative": {
            "total_nid_events":      rt.total_nid_events,
            "valid_nid_events":      rt.valid_nid_events,
            "valid_pct":             if rt.total_nid_events == 0 {
                0.0
            } else {
                100.0 * (rt.valid_nid_events as f64) / (rt.total_nid_events as f64)
            },
            "dibit_overflow_ticks":  rt.dibit_overflow_ticks,
            "iq_overflow_ticks":     rt.iq_overflow_ticks,
        },

        "last_window": {
            "pll_min":               rt.hb_pll_min,
            "pll_max":               rt.hb_pll_max,
            "sp_min":                rt.hb_sp_min,
            "sp_max":                rt.hb_sp_max,
            "sync_dist_best":        rt.hb_sync_dist_best,
            "bch_busy_ticks":        rt.hb_bch_busy_ticks,
            "in_window_ticks":       rt.hb_in_window_ticks,
            "nid_event_ticks":       rt.hb_nid_event_ticks,
            "dibit_overflow_ticks":  rt.hb_dibit_overflow_ticks,
            "iq_overflow_ticks":     rt.hb_iq_overflow_ticks,
            "iq_kbps":               rt.hb_iq_kbps,
            "iq_buf_rolls":          rt.hb_iq_buf_rolls,
            "valid_count":           rt.hb_window_valid_count,
            "event_count":           rt.hb_window_event_count,
        },

        "top_nacs":  top_nacs,
        "nid_ring":  rt.nid_ring.clone(),
    }))
}

/// Phase 6F.2: per-source IRQ counters from the InterruptHandler task.
async fn get_irq_stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use std::time::Instant;
    let s = state.irq_stats.lock().await;
    let now = Instant::now();
    let uptime_secs = s
        .started_at
        .map(|t| now.saturating_duration_since(t).as_secs())
        .unwrap_or(0);
    let last_at_ms_ago = s
        .last_at
        .map(|t| now.saturating_duration_since(t).as_millis() as u64);
    let rate = |n: u64| -> f64 {
        if uptime_secs == 0 { 0.0 } else { (n as f64) / (uptime_secs as f64) }
    };
    Json(serde_json::json!({
        "running":         s.started_at.is_some(),
        "uptime_secs":     uptime_secs,
        "last_at_ms_ago":  last_at_ms_ago,
        "total":           s.total,
        "dibit":           s.dibit,
        "traffic":         s.traffic,
        "iq":              s.iq,
        "lsm_dibit":       s.lsm_dibit,
        "rate_per_sec": {
            "total":     rate(s.total),
            "dibit":     rate(s.dibit),
            "traffic":   rate(s.traffic),
            "iq":        rate(s.iq),
            "lsm_dibit": rate(s.lsm_dibit),
        },
    }))
}

/// Phase 6F.2: side-by-side comparison matrix of all decoder sources.
///
/// Returns the same set of metrics for each of:
///   - PS C4FM software decoder (`state.decoder`)
///   - PS LSM software decoder (`state.lsm_decoder`)
///   - PS Phase 6D iq-fed pipeline (`state.lsm_stats`)
///   - PL HDL LSM chain (`state.hdl_lsm`)
async fn get_decoder_compare(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let dec_c4fm = state.decoder.read().await;
    let dec_lsm = state.lsm_decoder.read().await;
    let lsm_stats = state.lsm_stats.lock().await;
    let hdl_rt = state.hdl_lsm.lock().await;

    fn fmt_nac(n: Option<crate::p25::types::Nac>) -> serde_json::Value {
        match n {
            Some(v) => serde_json::Value::String(format!("{}", v)),
            None => serde_json::Value::Null,
        }
    }
    fn fmt_nac_u16(n: u16) -> String { format!("0x{:03X}", n) }

    let lsm_winner_nac = lsm_stats
        .top_nacs(1)
        .first()
        .map(|(n, _)| fmt_nac_u16(*n))
        .unwrap_or_else(|| "--".to_string());
    let hdl_winner_nac = hdl_rt
        .top_nacs(1)
        .first()
        .map(|(n, _)| fmt_nac_u16(*n))
        .unwrap_or_else(|| "--".to_string());

    Json(serde_json::json!({
        "ps_c4fm": {
            "label":           "PS C4FM (software, HDL c4fm dibit-fed)",
            "system_nac":      fmt_nac(dec_c4fm.system.nac),
            "messages":        dec_c4fm.recent_messages.len(),
            "active_grants":   dec_c4fm.grants.len(),
            "bands_known":     dec_c4fm.bands.len(),
            "sync_hits":       dec_c4fm.sync_hits(),
            "sync_near":       dec_c4fm.sync_near_misses(),
            "sync_best_dist":  if dec_c4fm.best_sync_distance() == u32::MAX {
                serde_json::Value::Null
            } else {
                serde_json::Value::from(dec_c4fm.best_sync_distance())
            },
            "total_dibits":    dec_c4fm.total_dibits(),
            "nid_attempts":          dec_c4fm.nid_attempts,
            "nid_decode_failures":   dec_c4fm.nid_decode_failures,
            "nid_invalid_duid":      dec_c4fm.nid_invalid_duid,
            "nid_decoded_ok":        dec_c4fm.nid_decoded_ok,
            "nid_decoded_tsdu":      dec_c4fm.nid_decoded_tsdu,
            "tsdu_attempts":         dec_c4fm.tsdu_attempts,
            "tsbk_block_attempts":   dec_c4fm.tsbk_block_attempts,
            "tsbk_trellis_failures": dec_c4fm.tsbk_trellis_failures,
            "tsbk_crc_failures":     dec_c4fm.tsbk_crc_failures,
            "tsbk_crc_ok":           dec_c4fm.tsbk_crc_ok,
            "tsbk_crc_ok_plain":     dec_c4fm.tsbk_crc_ok_plain,
            "tsbk_crc_ok_xored":     dec_c4fm.tsbk_crc_ok_xored,
            "tsbk_unknown_opcode":   dec_c4fm.tsbk_unknown_opcode,
        },
        "ps_lsm": {
            "label":           "PS LSM (software, HDL lsm dibit-fed)",
            "system_nac":      fmt_nac(dec_lsm.system.nac),
            "messages":        dec_lsm.recent_messages.len(),
            "active_grants":   dec_lsm.grants.len(),
            "bands_known":     dec_lsm.bands.len(),
            "sync_hits":       dec_lsm.sync_hits(),
            "sync_near":       dec_lsm.sync_near_misses(),
            "sync_best_dist":  if dec_lsm.best_sync_distance() == u32::MAX {
                serde_json::Value::Null
            } else {
                serde_json::Value::from(dec_lsm.best_sync_distance())
            },
            "total_dibits":    dec_lsm.total_dibits(),
            "nid_attempts":          dec_lsm.nid_attempts,
            "nid_decode_failures":   dec_lsm.nid_decode_failures,
            "nid_invalid_duid":      dec_lsm.nid_invalid_duid,
            "nid_decoded_ok":        dec_lsm.nid_decoded_ok,
            "nid_decoded_tsdu":      dec_lsm.nid_decoded_tsdu,
            "tsdu_attempts":         dec_lsm.tsdu_attempts,
            "tsbk_block_attempts":   dec_lsm.tsbk_block_attempts,
            "tsbk_trellis_failures": dec_lsm.tsbk_trellis_failures,
            "tsbk_crc_failures":     dec_lsm.tsbk_crc_failures,
            "tsbk_crc_ok":           dec_lsm.tsbk_crc_ok,
            "tsbk_crc_ok_plain":     dec_lsm.tsbk_crc_ok_plain,
            "tsbk_crc_ok_xored":     dec_lsm.tsbk_crc_ok_xored,
            "tsbk_unknown_opcode":   dec_lsm.tsbk_unknown_opcode,
        },
        "ps_phase6d": {
            "label":           "PS Phase 6D (software, raw IQ-fed)",
            "winner_nac":      lsm_winner_nac,
            "wakeups":         lsm_stats.wakeups,
            "iq_samples":      lsm_stats.iq_samples,
            "dibits":          lsm_stats.dibits,
            "hard_events":     lsm_stats.hard_events,
            "soft_events":     lsm_stats.soft_events,
            "overflow_resets": lsm_stats.overflow_resets,
        },
        "pl_hdl": {
            "label":           "PL HDL LSM chain (FPGA gateware)",
            "winner_nac":      hdl_winner_nac,
            "total_nids":      hdl_rt.total_nid_events,
            "valid_nids":      hdl_rt.valid_nid_events,
            "valid_pct":       if hdl_rt.total_nid_events == 0 {
                0.0
            } else {
                100.0 * (hdl_rt.valid_nid_events as f64)
                    / (hdl_rt.total_nid_events as f64)
            },
            "drop_count":      hdl_rt.last_drop_count,
            "pll_dbg":         hdl_rt.pll_dbg,
            "sp_dbg":          hdl_rt.sp_dbg,
            "sync_distance":   hdl_rt.sync_distance,
            "dibit_overflow_ticks": hdl_rt.dibit_overflow_ticks,
            "iq_overflow_ticks":    hdl_rt.iq_overflow_ticks,
        },
    }))
}

async fn get_aliases(State(state): State<Arc<AppState>>) -> Json<AliasMap> {
    let decoder = state.decoder.read().await;
    Json(decoder.aliases.clone())
}

async fn put_aliases(
    State(state): State<Arc<AppState>>,
    Json(aliases): Json<AliasMap>,
) -> impl IntoResponse {
    let mut decoder = state.decoder.write().await;
    decoder.aliases = aliases;
    axum::http::StatusCode::OK
}

// ── WebSocket ──────────────────────────────────────────────────────────

async fn ws_events(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.event_tx.subscribe();
    while let Ok(msg) = rx.recv().await {
        if socket
            .send(axum::extract::ws::Message::Text(msg.into()))
            .await
            .is_err()
        {
            break;
        }
    }
}

// ── Dashboard HTML ─────────────────────────────────────────────────────

async fn index_html() -> impl IntoResponse {
    axum::response::Html(DASHBOARD_HTML)
}

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Fishball P25</title>
<style>
:root {
  --bg: #0a0a0f;
  --card-bg: #12122a;
  --card-border: #2a2a4a;
  --text: #d0d0e0;
  --text-dim: #707090;
  --accent: #4fc3f7;
  --green: #66bb6a;
  --orange: #ffb74d;
  --red: #ef5350;
  --purple: #ab47bc;
  --mono: 'Cascadia Code', 'Fira Code', 'JetBrains Mono', monospace;
}
[data-theme="light"] {
  --bg: #f5f5f5;
  --card-bg: #ffffff;
  --card-border: #ddd;
  --text: #222;
  --text-dim: #888;
  --accent: #0277bd;
  --green: #2e7d32;
  --orange: #e65100;
  --red: #c62828;
  --purple: #7b1fa2;
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
       background: var(--bg); color: var(--text); padding: 12px; font-size: 14px; }
h1 { color: var(--accent); font-size: 1.3em; }
h2 { color: var(--green); margin: 12px 0 6px; font-size: 1em; font-weight: 600; }
.header { display: flex; align-items: center; justify-content: space-between; margin-bottom: 12px; }
.header-left { display: flex; align-items: center; gap: 12px; }
.status { display: inline-flex; align-items: center; gap: 6px; font-size: 0.85em; }
.dot { width: 8px; height: 8px; border-radius: 50%; background: var(--red); }
.dot.active { background: var(--green); }
.theme-btn { background: none; border: 1px solid var(--card-border); color: var(--text);
              padding: 4px 10px; border-radius: 4px; cursor: pointer; font-size: 0.85em; }
.grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }
.card { background: var(--card-bg); border: 1px solid var(--card-border);
        border-radius: 6px; padding: 10px; }
table { width: 100%; border-collapse: collapse; font-size: 0.85em; }
th { text-align: left; color: var(--text-dim); padding: 3px 6px; border-bottom: 1px solid var(--card-border); font-weight: 500; }
td { padding: 3px 6px; border-bottom: 1px solid rgba(128,128,128,0.1); }
.v { color: var(--accent); font-family: var(--mono); font-size: 0.9em; }
.freq { color: var(--orange); }
.tg { color: var(--green); }
.alias { color: var(--purple); font-size: 0.8em; }

/* Live activity feed */
#activity { max-height: 320px; overflow-y: auto; font-family: var(--mono); font-size: 0.8em; }
.evt { padding: 3px 6px; border-bottom: 1px solid rgba(128,128,128,0.08); display: flex; gap: 8px; }
.evt-time { color: var(--text-dim); min-width: 80px; }
.evt-type { min-width: 80px; font-weight: 600; }
.evt-type.GRP_GRANT { color: var(--green); }
.evt-type.GRANT_UPD { color: var(--accent); }
.evt-type.NET_STS, .evt-type.RFSS_STS { color: var(--orange); }
.evt-type.IDEN_UP { color: var(--purple); }
.evt-type.ADJ_STS { color: var(--text-dim); }
.evt-detail { flex: 1; }

/* Frequency map */
.freq-map { position: relative; height: 60px; background: var(--card-bg);
            border: 1px solid var(--card-border); border-radius: 6px;
            margin: 8px 0; overflow: hidden; }
.freq-marker { position: absolute; bottom: 0; width: 2px; height: 100%;
               background: var(--text-dim); opacity: 0.4; }
.freq-marker.cc { background: var(--accent); opacity: 0.8; width: 3px; }
.freq-marker.active { background: var(--green); opacity: 0.9; width: 3px; }
.freq-label { position: absolute; top: 2px; font-size: 9px; color: var(--text-dim);
              font-family: var(--mono); transform: translateX(-50%); white-space: nowrap; }
.freq-lcn { position: absolute; bottom: 2px; font-size: 9px; color: var(--accent);
            font-family: var(--mono); transform: translateX(-50%); }

/* Aliases modal */
.modal-overlay { display: none; position: fixed; top: 0; left: 0; width: 100%; height: 100%;
                 background: rgba(0,0,0,0.6); z-index: 100; }
.modal-overlay.show { display: flex; align-items: center; justify-content: center; }
.modal { background: var(--card-bg); border: 1px solid var(--card-border);
         border-radius: 8px; padding: 16px; width: 500px; max-width: 90vw; }
.modal h2 { margin-top: 0; }
.modal textarea { width: 100%; height: 200px; background: var(--bg); color: var(--text);
                  border: 1px solid var(--card-border); border-radius: 4px; padding: 8px;
                  font-family: var(--mono); font-size: 0.85em; resize: vertical; }
.modal-btns { display: flex; gap: 8px; margin-top: 8px; justify-content: flex-end; }
.btn { padding: 6px 14px; border: 1px solid var(--card-border); border-radius: 4px;
       cursor: pointer; font-size: 0.85em; background: var(--card-bg); color: var(--text); }
.btn-primary { background: var(--accent); color: #000; border-color: var(--accent); }
.alias-btn { font-size: 0.8em; color: var(--text-dim); cursor: pointer; margin-left: 8px; }
@media (max-width: 768px) { .grid2 { grid-template-columns: 1fr; } }
</style>
</head>
<body>

<div class="header">
  <div class="header-left">
    <h1>&#x1f4e1; Fishball P25</h1>
    <span class="status"><span class="dot" id="dot"></span><span id="status">Offline</span></span>
    <span style="font-size:0.75em;color:var(--text-dim);font-family:var(--mono)" id="build_tag">--</span>
  </div>
  <div>
    <span class="alias-btn" onclick="showAliases()">&#x2699; Aliases</span>
    <button class="theme-btn" onclick="toggleTheme()" id="themeBtn">&#x1f319;</button>
  </div>
</div>

<!-- ── Phase 6F.2: Decoder Comparison Matrix (PS vs PL) ── -->
<h2>Decoder Comparison (PS vs PL)</h2>
<div class="card">
  <table id="cmp_t" style="font-size:0.85em">
    <thead>
      <tr>
        <th style="width:32%">Metric</th>
        <th>PS C4FM<br><span style="color:var(--text-dim);font-weight:400">software, HDL c4fm dibits</span></th>
        <th>PS LSM<br><span style="color:var(--text-dim);font-weight:400">software, HDL lsm dibits</span></th>
        <th>PS Phase 6D<br><span style="color:var(--text-dim);font-weight:400">software, raw IQ</span></th>
        <th>PL HDL LSM<br><span style="color:var(--text-dim);font-weight:400">FPGA gateware</span></th>
      </tr>
    </thead>
    <tbody id="cmp_body">
      <tr><td colspan="5" style="color:var(--text-dim)">Loading...</td></tr>
    </tbody>
  </table>
  <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
    Side-by-side: same metric across all four decoder paths. PS = Processing
    System (ARM software), PL = Programmable Logic (FPGA). Winner NAC for
    Phase 6D / PL HDL is the top of their NAC histogram.
  </p>
</div>

<div class="grid2">
  <div class="card">
    <h2>System Identity <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">PS &middot; LSM software decoder &middot; HDL dibit-fed</span></h2>
    <table>
      <tr><th>NAC</th><td class="v" id="nac">--</td></tr>
      <tr><th>WACN</th><td class="v" id="wacn">--</td></tr>
      <tr><th>System</th><td class="v" id="sys">--</td></tr>
      <tr><th>RFSS / Site</th><td class="v" id="rfss">--</td></tr>
      <tr><th>Control CH</th><td class="v" id="cc">--</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Software decoder fed by HDL LSM dibit DMA. Empty until a TSBK validates.
    </p>
  </div>
  <div class="card">
    <h2>Decode Stats <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">PS &middot; LSM software decoder &middot; HDL dibit-fed</span></h2>
    <table>
      <tr><th>Messages</th><td class="v" id="msgs">0</td></tr>
      <tr><th>Active Grants</th><td class="v" id="grants_n">0</td></tr>
      <tr><th>Bands Known</th><td class="v" id="bands_n">0</td></tr>
      <tr><th>Dibit Count <span style="color:var(--text-dim);font-size:0.85em">(C4FM HDL)</span></th><td class="v" id="dibits">0</td></tr>
      <tr><th>Overflow <span style="color:var(--text-dim);font-size:0.85em">(C4FM HDL)</span></th><td class="v" id="overflow">No</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Decoder counters from PS LSM software decoder. Dibit Count + Overflow are
      from the C4FM HDL chain DMA ring (not migrated).
    </p>
  </div>
</div>

<!-- ── Phase 6F.2: PL HDL LSM Chain Detail + IRQ Counters ── -->
<div class="grid2">
  <div class="card">
    <h2>HDL LSM Chain (PL) <span id="hdl_status" style="font-size:0.75em;color:var(--text-dim);margin-left:6px">--</span></h2>
    <table style="font-size:0.85em">
      <tr><th>Cumulative NIDs (valid/total)</th><td class="v"><span id="hdl_nid_valid">0</span> / <span id="hdl_nid_total">0</span> (<span id="hdl_nid_pct">0%</span>)</td></tr>
      <tr><th>Last NAC / DUID</th><td class="v"><span id="hdl_last_nac">--</span> / <span id="hdl_last_duid">--</span></td></tr>
      <tr><th>Drop count (sync hit while BCH busy)</th><td class="v" id="hdl_drop">0</td></tr>
      <tr><th>PLL register (now)</th><td class="v" id="hdl_pll">--</td></tr>
      <tr><th>Sample point register (now)</th><td class="v" id="hdl_sp">--</td></tr>
      <tr><th>Sync distance (now / window best)</th><td class="v"><span id="hdl_sd_now">--</span> / <span id="hdl_sd_best">--</span></td></tr>
      <tr><th>BCH busy / in-NID-window flags</th><td class="v"><span id="hdl_bch">--</span> / <span id="hdl_inwin">--</span></td></tr>
      <tr><th>Last 1s window: pll min/max</th><td class="v"><span id="hdl_w_pll">--</span></td></tr>
      <tr><th>Last 1s window: sp min/max</th><td class="v"><span id="hdl_w_sp">--</span></td></tr>
      <tr><th>Last 1s window: NIDs (valid/total)</th><td class="v"><span id="hdl_w_nids">--</span></td></tr>
      <tr><th>Last 1s window: iq KB/s, buf rolls</th><td class="v"><span id="hdl_w_iq">--</span></td></tr>
      <tr><th>Cumulative dibit / iq overflow ticks</th><td class="v"><span id="hdl_ovf">0 / 0</span></td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Live FPGA register reads from the heartbeat task (16 ms cadence).
      pll_dbg / sp_dbg are signed 16-bit. Healthy lock for our test target:
      pll near 0, sp ~3000-6000, sync_dist 0.
    </p>
  </div>
  <div class="card">
    <h2>IRQ Source Counters</h2>
    <table style="font-size:0.85em">
      <tr><th>Total IRQs</th><td class="v"><span id="irq_total">0</span> (<span id="irq_total_rate">0/s</span>)</td></tr>
      <tr><th>C4FM dibit DMA done</th><td class="v"><span id="irq_dibit">0</span> (<span id="irq_dibit_rate">0/s</span>)</td></tr>
      <tr><th>Traffic dibit DMA done</th><td class="v"><span id="irq_traffic">0</span> (<span id="irq_traffic_rate">0/s</span>)</td></tr>
      <tr><th>IQ DMA done</th><td class="v"><span id="irq_iq">0</span> (<span id="irq_iq_rate">0/s</span>)</td></tr>
      <tr><th>LSM dibit DMA done</th><td class="v"><span id="irq_lsm">0</span> (<span id="irq_lsm_rate">0/s</span>)</td></tr>
      <tr><th>Last IRQ (ms ago)</th><td class="v" id="irq_last">--</td></tr>
      <tr><th>Uptime</th><td class="v" id="irq_uptime">--</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Per-source IRQ counters from the InterruptHandler task. lsm_dibit
      should be ~17/min (one buffer every ~3.5 s) on a healthy 4800 sym/s
      LSM dibit stream. iq should be ~60/s (one sub-buffer every 16 ms).
    </p>
  </div>
</div>

<!-- ── Phase 6F.2: Last 32 NIDs from PL HDL ring buffer ── -->
<h2>HDL LSM NID Ring (last 32, PL)</h2>
<div class="card">
  <table style="font-size:0.78em">
    <thead>
      <tr>
        <th>#</th><th>t (ms)</th><th>NAC</th><th>DUID</th>
        <th>valid</th><th>n_err</th><th>sync_d</th>
        <th>drop</th><th>pll</th><th>sp</th>
      </tr>
    </thead>
    <tbody id="nid_ring_body">
      <tr><td colspan="10" style="color:var(--text-dim)">No NID events yet</td></tr>
    </tbody>
  </table>
  <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
    32-deep ring buffer of the most recent NID events as observed by the
    HDL LSM chain. Same data the heartbeat task dumps to the log on
    crash transition.
  </p>
</div>

<div class="grid2">
  <div class="card">
    <h2>LSM Decoder (Phase 6D) <span id="lsm_status" style="font-size:0.75em;color:var(--text-dim);margin-left:6px">--</span></h2>
    <table>
      <tr><th>Uptime</th><td class="v" id="lsm_uptime">--</td></tr>
      <tr><th>Wakeups</th><td class="v" id="lsm_wakes">0</td></tr>
      <tr><th>IQ Samples</th><td class="v" id="lsm_iq">0</td></tr>
      <tr><th>Dibits</th><td class="v" id="lsm_dibits">0</td></tr>
      <tr><th>Hard / Soft Syncs</th><td class="v" id="lsm_syncs">0 / 0</td></tr>
      <tr><th>Overflow Resets</th><td class="v" id="lsm_overflows">0</td></tr>
      <tr><th>Last Sync</th><td class="v" id="lsm_last">--</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px" id="lsm_note">
      Parallel LSM pipeline output; independent of the C4FM dibit panels above.
    </p>
  </div>
  <div class="card">
    <h2>Top NACs (LSM)</h2>
    <table>
      <thead><tr><th>NAC</th><th>Count</th><th>%</th></tr></thead>
      <tbody id="lsm_nacs_body"><tr><td colspan="3" style="color:var(--text-dim)">No sync events yet</td></tr></tbody>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Combined hard + soft sync NAC histogram. Winner = locked on-air site ID.
    </p>
  </div>
</div>

<h2>Dibit Stream Diagnostics (PS C4FM vs PS LSM, side by side)</h2>
<div class="grid2">
  <div class="card">
    <h2>PS C4FM Dibit Stream <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">c4fm_dibit_dma</span></h2>
    <table>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Histogram</th></tr>
      <tr><th>Total Dibits</th><td class="v" id="dh_total">0</td></tr>
      <tr><th>Value 0 (+1)</th><td class="v" id="dh_0">--</td></tr>
      <tr><th>Value 1 (+3)</th><td class="v" id="dh_1">--</td></tr>
      <tr><th>Value 2 (-1)</th><td class="v" id="dh_2">--</td></tr>
      <tr><th>Value 3 (-3)</th><td class="v" id="dh_3">--</td></tr>
      <tr><th>Inner / Outer ratio</th><td class="v" id="dh_io">--</td></tr>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Sync correlator</th></tr>
      <tr><th>Sync hits</th><td class="v" id="sy_hits">0</td></tr>
      <tr><th>Near misses</th><td class="v" id="sy_near">0</td></tr>
      <tr><th>Best Hamming distance</th><td class="v" id="sy_best">--</td></tr>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Raw on-air DUID histogram</th></tr>
      <tr><th>Total NIDs</th><td class="v" id="rd_total">0</td></tr>
      <tr><th>Bucket 7 TSDU %</th><td class="v" id="rd_7">--</td></tr>
      <tr><th>Bucket 5 LDU1 %</th><td class="v" id="rd_5">--</td></tr>
      <tr><th>Bucket A LDU2 %</th><td class="v" id="rd_a">--</td></tr>
      <tr><th>Bucket 0 HDU %</th><td class="v" id="rd_0">--</td></tr>
    </table>
  </div>
  <div class="card">
    <h2>PS LSM Dibit Stream <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">lsm_dibit_dma</span></h2>
    <table>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Histogram</th></tr>
      <tr><th>Total Dibits</th><td class="v" id="ldh_total">0</td></tr>
      <tr><th>Value 0 (+1)</th><td class="v" id="ldh_0">--</td></tr>
      <tr><th>Value 1 (+3)</th><td class="v" id="ldh_1">--</td></tr>
      <tr><th>Value 2 (-1)</th><td class="v" id="ldh_2">--</td></tr>
      <tr><th>Value 3 (-3)</th><td class="v" id="ldh_3">--</td></tr>
      <tr><th>Inner / Outer ratio</th><td class="v" id="ldh_io">--</td></tr>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Sync correlator</th></tr>
      <tr><th>Sync hits</th><td class="v" id="lsy_hits">0</td></tr>
      <tr><th>Near misses</th><td class="v" id="lsy_near">0</td></tr>
      <tr><th>Best Hamming distance</th><td class="v" id="lsy_best">--</td></tr>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Raw on-air DUID histogram</th></tr>
      <tr><th>Total NIDs</th><td class="v" id="lrd_total">0</td></tr>
      <tr><th>Bucket 7 TSDU %</th><td class="v" id="lrd_7">--</td></tr>
      <tr><th>Bucket 5 LDU1 %</th><td class="v" id="lrd_5">--</td></tr>
      <tr><th>Bucket A LDU2 %</th><td class="v" id="lrd_a">--</td></tr>
      <tr><th>Bucket 0 HDU %</th><td class="v" id="lrd_0">--</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Side-by-side: identical metrics on the C4FM HDL dibit stream vs the
      LSM HDL dibit stream. If histograms differ, the slicers see different
      signal statistics. If sync best distance differs, frame alignment
      between the two streams is diverging. If TSDU bucket % is &lt;90% on
      either, NID payload bits are being corrupted upstream.
    </p>
  </div>
</div>

<h2>Live Activity</h2>
<div class="card">
  <div id="activity"></div>
</div>

<h2>Frequency Map</h2>
<div class="freq-map" id="freqmap"></div>

<div class="grid2">
  <div class="card">
    <h2>Active Grants</h2>
    <table>
      <thead><tr><th>Channel</th><th>Talkgroup</th><th>Source</th><th>Frequency</th><th>Age</th></tr></thead>
      <tbody id="grants_t"></tbody>
    </table>
  </div>
  <div class="card">
    <h2>Frequency Bands</h2>
    <table>
      <thead><tr><th>Band</th><th>Base (MHz)</th><th>Spacing</th><th>TX Offset</th><th>BW</th></tr></thead>
      <tbody id="bands_t"></tbody>
    </table>
  </div>
</div>

<!-- Aliases Modal -->
<div class="modal-overlay" id="aliasModal">
  <div class="modal">
    <h2>Talkgroup Aliases</h2>
    <p style="color:var(--text-dim);font-size:0.85em;margin:6px 0">JSON map: talkgroup ID (number) &rarr; display name</p>
    <textarea id="aliasText"></textarea>
    <div class="modal-btns">
      <button class="btn" onclick="closeAliases()">Cancel</button>
      <button class="btn btn-primary" onclick="saveAliases()">Save</button>
    </div>
  </div>
</div>

<script>
const $ = id => document.getElementById(id);
let aliases = {};

async function fetchJson(url) {
  try { return await (await fetch(url)).json(); } catch { return null; }
}

async function refresh() {
  const sys = await fetchJson('/api/system');
  if (sys) {
    $('nac').textContent = sys.nac || '--';
    $('wacn').textContent = sys.wacn || '--';
    $('sys').textContent = sys.system_id || '--';
    $('rfss').textContent = (sys.rfss_id != null ? `${sys.rfss_id} / ${sys.site_id}` : '--');
    $('cc').textContent = sys.control_channel || '--';
    if (sys.build) $('build_tag').textContent = 'build: ' + sys.build;
  }

  // ── Phase 6F.2: Decoder Comparison Matrix ──
  const cmp = await fetchJson('/api/decoder_compare');
  if (cmp) {
    const fmtN = v => (v == null) ? '--' : (typeof v === 'number' ? v.toLocaleString() : v);
    const fmtPct = v => (v == null) ? '--' : v.toFixed(1) + '%';
    const rows = [
      ['NAC (winner)', cmp.ps_c4fm.system_nac, cmp.ps_lsm.system_nac, cmp.ps_phase6d.winner_nac, cmp.pl_hdl.winner_nac],
      ['Messages decoded', fmtN(cmp.ps_c4fm.messages), fmtN(cmp.ps_lsm.messages), '--', '--'],
      ['Total NIDs (any source)', '--', '--', fmtN(cmp.ps_phase6d.hard_events + cmp.ps_phase6d.soft_events), fmtN(cmp.pl_hdl.total_nids)],
      ['Valid NIDs', '--', '--', '--', fmtN(cmp.pl_hdl.valid_nids) + ' (' + fmtPct(cmp.pl_hdl.valid_pct) + ')'],
      ['Sync hits (frame sync correlator)', fmtN(cmp.ps_c4fm.sync_hits), fmtN(cmp.ps_lsm.sync_hits), '--', '--'],
      ['Sync near-misses', fmtN(cmp.ps_c4fm.sync_near), fmtN(cmp.ps_lsm.sync_near), '--', '--'],
      ['Sync best Hamming distance', fmtN(cmp.ps_c4fm.sync_best_dist), fmtN(cmp.ps_lsm.sync_best_dist), '--', fmtN(cmp.pl_hdl.sync_distance)],
      ['Total dibits processed', fmtN(cmp.ps_c4fm.total_dibits), fmtN(cmp.ps_lsm.total_dibits), fmtN(cmp.ps_phase6d.dibits), '--'],
      ['Active grants', fmtN(cmp.ps_c4fm.active_grants), fmtN(cmp.ps_lsm.active_grants), '--', '--'],
      ['Frequency bands known', fmtN(cmp.ps_c4fm.bands_known), fmtN(cmp.ps_lsm.bands_known), '--', '--'],
      ['Hard sync events', '--', '--', fmtN(cmp.ps_phase6d.hard_events), '--'],
      ['Soft sync events', '--', '--', fmtN(cmp.ps_phase6d.soft_events), '--'],
      ['IQ samples processed', '--', '--', fmtN(cmp.ps_phase6d.iq_samples), '--'],
      ['Drop count (PL only)', '--', '--', '--', fmtN(cmp.pl_hdl.drop_count)],
      ['Live PLL register', '--', '--', '--', fmtN(cmp.pl_hdl.pll_dbg)],
      ['Live sample-point register', '--', '--', '--', fmtN(cmp.pl_hdl.sp_dbg)],
      ['Overflow events', '--', '--', fmtN(cmp.ps_phase6d.overflow_resets), 'dibit:' + fmtN(cmp.pl_hdl.dibit_overflow_ticks) + ' iq:' + fmtN(cmp.pl_hdl.iq_overflow_ticks)],
      ['── pipeline ──', '', '', '', ''],
      ['NID attempts (sync hit)', fmtN(cmp.ps_c4fm.nid_attempts), fmtN(cmp.ps_lsm.nid_attempts), '--', '--'],
      ['NID BCH decode failures', fmtN(cmp.ps_c4fm.nid_decode_failures), fmtN(cmp.ps_lsm.nid_decode_failures), '--', '--'],
      ['NID invalid DUID after BCH', fmtN(cmp.ps_c4fm.nid_invalid_duid), fmtN(cmp.ps_c4fm.nid_invalid_duid), '--', '--'],
      ['NID decoded OK (any DUID)', fmtN(cmp.ps_c4fm.nid_decoded_ok), fmtN(cmp.ps_lsm.nid_decoded_ok), '--', '--'],
      ['NID decoded OK (TSDU only)', fmtN(cmp.ps_c4fm.nid_decoded_tsdu), fmtN(cmp.ps_lsm.nid_decoded_tsdu), '--', '--'],
      ['TSDU attempts', fmtN(cmp.ps_c4fm.tsdu_attempts), fmtN(cmp.ps_lsm.tsdu_attempts), '--', '--'],
      ['TSBK block attempts', fmtN(cmp.ps_c4fm.tsbk_block_attempts), fmtN(cmp.ps_lsm.tsbk_block_attempts), '--', '--'],
      ['TSBK trellis failures', fmtN(cmp.ps_c4fm.tsbk_trellis_failures), fmtN(cmp.ps_lsm.tsbk_trellis_failures), '--', '--'],
      ['TSBK CRC failures', fmtN(cmp.ps_c4fm.tsbk_crc_failures), fmtN(cmp.ps_lsm.tsbk_crc_failures), '--', '--'],
      ['TSBK CRC OK', fmtN(cmp.ps_c4fm.tsbk_crc_ok), fmtN(cmp.ps_lsm.tsbk_crc_ok), '--', '--'],
      ['  - via plain CRC convention', fmtN(cmp.ps_c4fm.tsbk_crc_ok_plain), fmtN(cmp.ps_lsm.tsbk_crc_ok_plain), '--', '--'],
      ['  - via xored 0xFFFF convention', fmtN(cmp.ps_c4fm.tsbk_crc_ok_xored), fmtN(cmp.ps_lsm.tsbk_crc_ok_xored), '--', '--'],
      ['TSBK unknown opcode', fmtN(cmp.ps_c4fm.tsbk_unknown_opcode), fmtN(cmp.ps_lsm.tsbk_unknown_opcode), '--', '--'],
    ];
    $('cmp_body').innerHTML = rows.map(r =>
      '<tr><th>' + r[0] + '</th>' +
      '<td class="v">' + r[1] + '</td>' +
      '<td class="v">' + r[2] + '</td>' +
      '<td class="v">' + r[3] + '</td>' +
      '<td class="v">' + r[4] + '</td></tr>'
    ).join('');
  }

  // ── Phase 6F.2: PL HDL LSM Chain Detail ──
  const hdl = await fetchJson('/api/hdl_lsm');
  if (hdl) {
    const alive = hdl.running && hdl.last_tick_ms_ago != null && hdl.last_tick_ms_ago < 1000;
    if (!hdl.running) {
      $('hdl_status').textContent = 'NOT STARTED';
      $('hdl_status').style.color = 'var(--red)';
    } else if (alive) {
      $('hdl_status').textContent = 'ALIVE (' + hdl.uptime_secs + 's)';
      $('hdl_status').style.color = 'var(--green)';
    } else {
      $('hdl_status').textContent = 'STALLED';
      $('hdl_status').style.color = 'var(--red)';
    }
    $('hdl_nid_valid').textContent = hdl.cumulative.valid_nid_events.toLocaleString();
    $('hdl_nid_total').textContent = hdl.cumulative.total_nid_events.toLocaleString();
    $('hdl_nid_pct').textContent = hdl.cumulative.valid_pct.toFixed(1) + '%';
    $('hdl_last_nac').textContent = hdl.live.last_nac;
    $('hdl_last_duid').textContent = '0x' + hdl.live.last_duid.toString(16).toUpperCase();
    $('hdl_drop').textContent = hdl.live.last_drop_count;
    $('hdl_pll').textContent = hdl.live.pll_dbg;
    $('hdl_sp').textContent = hdl.live.sp_dbg;
    $('hdl_sd_now').textContent = hdl.live.sync_distance;
    $('hdl_sd_best').textContent = hdl.last_window.sync_dist_best === 99 ? '--' : hdl.last_window.sync_dist_best;
    $('hdl_bch').textContent = hdl.live.bch_busy ? 'YES' : 'no';
    $('hdl_inwin').textContent = hdl.live.in_nid_window ? 'YES' : 'no';
    $('hdl_w_pll').textContent = hdl.last_window.pll_min + ' / ' + hdl.last_window.pll_max;
    $('hdl_w_sp').textContent = hdl.last_window.sp_min + ' / ' + hdl.last_window.sp_max;
    $('hdl_w_nids').textContent = hdl.last_window.valid_count + ' / ' + hdl.last_window.event_count;
    $('hdl_w_iq').textContent = hdl.last_window.iq_kbps + ' KB/s, ' + hdl.last_window.iq_buf_rolls + ' rolls';
    $('hdl_ovf').textContent = hdl.cumulative.dibit_overflow_ticks + ' / ' + hdl.cumulative.iq_overflow_ticks;

    if (hdl.nid_ring && hdl.nid_ring.length) {
      // Reverse so newest is on top.
      const ring = hdl.nid_ring.slice().reverse();
      $('nid_ring_body').innerHTML = ring.map(e =>
        '<tr>' +
        '<td class="v">' + e.seq + '</td>' +
        '<td class="v">' + e.t_ms_since_boot + '</td>' +
        '<td class="v">0x' + e.nac.toString(16).toUpperCase().padStart(3, '0') + '</td>' +
        '<td class="v">' + e.duid + '</td>' +
        '<td class="v" style="color:' + (e.valid ? 'var(--green)' : 'var(--red)') + '">' + (e.valid ? '\u2713' : '\u2717') + '</td>' +
        '<td class="v">' + e.n_errors + '</td>' +
        '<td class="v">' + e.sync_distance + '</td>' +
        '<td class="v">' + e.drop_count + '</td>' +
        '<td class="v">' + e.pll_dbg + '</td>' +
        '<td class="v">' + e.sp_dbg + '</td>' +
        '</tr>'
      ).join('');
    }
  }

  // ── Phase 6F.2: IRQ Source Counters ──
  const irq = await fetchJson('/api/irq_stats');
  if (irq) {
    const fmtRate = r => (r < 1 ? r.toFixed(2) : Math.round(r).toLocaleString()) + '/s';
    $('irq_total').textContent = irq.total.toLocaleString();
    $('irq_total_rate').textContent = fmtRate(irq.rate_per_sec.total);
    $('irq_dibit').textContent = irq.dibit.toLocaleString();
    $('irq_dibit_rate').textContent = fmtRate(irq.rate_per_sec.dibit);
    $('irq_traffic').textContent = irq.traffic.toLocaleString();
    $('irq_traffic_rate').textContent = fmtRate(irq.rate_per_sec.traffic);
    $('irq_iq').textContent = irq.iq.toLocaleString();
    $('irq_iq_rate').textContent = fmtRate(irq.rate_per_sec.iq);
    $('irq_lsm').textContent = irq.lsm_dibit.toLocaleString();
    $('irq_lsm_rate').textContent = fmtRate(irq.rate_per_sec.lsm_dibit);
    $('irq_last').textContent = irq.last_at_ms_ago != null ? irq.last_at_ms_ago : '--';
    $('irq_uptime').textContent = irq.uptime_secs + 's';
  }

  const stats = await fetchJson('/api/stats');
  if (stats) {
    $('msgs').textContent = stats.recent_messages.toLocaleString();
    $('grants_n').textContent = stats.active_grants;
    $('bands_n').textContent = stats.bands_known;
    $('dibits').textContent = stats.dibit_count.toLocaleString();
    $('overflow').textContent = stats.overflow ? 'YES' : 'No';
    $('overflow').style.color = stats.overflow ? 'var(--red)' : '';
    const d = $('dot'), s = $('status');
    if (stats.system_acquired) { d.classList.add('active'); s.textContent = 'Tracking'; }
    else { d.classList.remove('active'); s.textContent = 'Searching'; }
  }

  const lsm = await fetchJson('/api/lsm');
  if (lsm) {
    const alive = lsm.running && lsm.last_wake_ms_ago != null && lsm.last_wake_ms_ago < 3000;
    if (!lsm.running) {
      $('lsm_status').textContent = 'NOT STARTED';
      $('lsm_status').style.color = 'var(--red)';
    } else if (alive) {
      $('lsm_status').textContent = 'ALIVE';
      $('lsm_status').style.color = 'var(--green)';
    } else {
      $('lsm_status').textContent = 'STALLED';
      $('lsm_status').style.color = 'var(--red)';
    }
    $('lsm_uptime').textContent = lsm.uptime_secs + 's';
    $('lsm_wakes').textContent = lsm.wakeups.toLocaleString();
    const iqk = Math.round(lsm.iq_samples_per_sec / 1000);
    $('lsm_iq').textContent = lsm.iq_samples.toLocaleString() + ' (' + iqk + 'k/s)';
    const dps = Math.round(lsm.dibits_per_sec);
    $('lsm_dibits').textContent = lsm.dibits.toLocaleString() + ' (' + dps + '/s)';
    $('lsm_syncs').textContent = lsm.hard_events.toLocaleString() + ' / ' +
      lsm.soft_events.toLocaleString();
    $('lsm_overflows').textContent = lsm.overflow_resets.toLocaleString();
    if (lsm.last_sync) {
      const fec = lsm.last_sync.fec_corrected ? '\u2713' : '\u2717';
      const age = Math.round(lsm.last_sync.age_ms / 1000);
      $('lsm_last').textContent =
        lsm.last_sync.nac + ' DUID' + lsm.last_sync.duid + ' FEC' + fec +
        ' (' + age + 's ago)';
    } else {
      $('lsm_last').textContent = '--';
    }
    if (lsm.top_nacs && lsm.top_nacs.length) {
      $('lsm_nacs_body').innerHTML = lsm.top_nacs.map(n =>
        '<tr><td class="v">' + n.nac + '</td>' +
        '<td>' + n.count.toLocaleString() + '</td>' +
        '<td>' + n.pct.toFixed(1) + '%</td></tr>'
      ).join('');
    } else {
      $('lsm_nacs_body').innerHTML =
        '<tr><td colspan="3" style="color:var(--text-dim)">No sync events yet</td></tr>';
    }
  }

  // Helper to populate one of the dibit-stream cards (C4FM or LSM).
  const renderDibitDump = (dump, ids) => {
    if (!dump) return;
    const fmt = (n, p) => `${n.toLocaleString()} (${p.toFixed(1)}%)`;
    $(ids.total).textContent = dump.total_dibits.toLocaleString();
    $(ids.v0).textContent = fmt(dump.histogram['0'], dump.histogram['0_pct']);
    $(ids.v1).textContent = fmt(dump.histogram['1'], dump.histogram['1_pct']);
    $(ids.v2).textContent = fmt(dump.histogram['2'], dump.histogram['2_pct']);
    $(ids.v3).textContent = fmt(dump.histogram['3'], dump.histogram['3_pct']);
    $(ids.io).textContent = dump.histogram.inner_pct.toFixed(1) + '% / ' + dump.histogram.outer_pct.toFixed(1) + '%';
    $(ids.hits).textContent = dump.sync.hits.toLocaleString();
    $(ids.near).textContent = dump.sync.near_misses.toLocaleString();
    const bd = dump.sync.best_distance;
    $(ids.best).textContent = (bd >= 4294967000) ? '--' : bd;
    $(ids.rd_total).textContent = dump.raw_duid.total.toLocaleString();
    if (dump.raw_duid.total > 0) {
      $(ids.rd_7).textContent = dump.raw_duid.pct_7_tsdu.toFixed(1) + '%';
      $(ids.rd_5).textContent = dump.raw_duid.pct_5_ldu1.toFixed(1) + '%';
      $(ids.rd_a).textContent = dump.raw_duid.pct_a_ldu2.toFixed(1) + '%';
      $(ids.rd_0).textContent = dump.raw_duid.pct_0_hdu.toFixed(1) + '%';
    }
  };

  const c4fmIds = {total:'dh_total', v0:'dh_0', v1:'dh_1', v2:'dh_2', v3:'dh_3', io:'dh_io',
    hits:'sy_hits', near:'sy_near', best:'sy_best',
    rd_total:'rd_total', rd_7:'rd_7', rd_5:'rd_5', rd_a:'rd_a', rd_0:'rd_0'};
  const lsmIds = {total:'ldh_total', v0:'ldh_0', v1:'ldh_1', v2:'ldh_2', v3:'ldh_3', io:'ldh_io',
    hits:'lsy_hits', near:'lsy_near', best:'lsy_best',
    rd_total:'lrd_total', rd_7:'lrd_7', rd_5:'lrd_5', rd_a:'lrd_a', rd_0:'lrd_0'};
  renderDibitDump(await fetchJson('/api/dibit_dump'), c4fmIds);
  renderDibitDump(await fetchJson('/api/lsm_dibit_dump'), lsmIds);

  const grants = await fetchJson('/api/grants');
  if (grants) {
    $('grants_t').innerHTML = grants.map(g =>
      `<tr><td>${g.channel}</td>` +
      `<td class="tg">${g.talkgroup}${g.talkgroup_alias ? ' <span class="alias">' + g.talkgroup_alias + '</span>' : ''}</td>` +
      `<td>${g.source ?? ''}</td>` +
      `<td class="freq">${g.frequency_mhz ? g.frequency_mhz.toFixed(4) : ''}</td>` +
      `<td>${g.age_secs}s</td></tr>`
    ).join('') || '<tr><td colspan="5" style="color:var(--text-dim)">None</td></tr>';
    renderFreqMap(grants);
  }

  const bands = await fetchJson('/api/bands');
  if (bands) {
    $('bands_t').innerHTML = bands.map(b =>
      `<tr><td>${b.identifier}</td>` +
      `<td class="freq">${b.base_frequency_mhz.toFixed(5)}</td>` +
      `<td>${b.channel_spacing_khz} kHz</td>` +
      `<td>${b.transmit_offset_mhz} MHz</td>` +
      `<td>${b.bandwidth_khz} kHz</td></tr>`
    ).join('') || '<tr><td colspan="5" style="color:var(--text-dim)">None</td></tr>';
  }
}

// Frequency map rendering
const LCN_DATA = [
  {lcn:1,freq:855.2375},{lcn:2,freq:856.4375},{lcn:3,freq:857.2125},
  {lcn:4,freq:857.4375},{lcn:5,freq:857.9875},{lcn:6,freq:858.4375},
  {lcn:7,freq:858.4625},{lcn:8,freq:858.9875},{lcn:9,freq:859.4375},
  {lcn:10,freq:860.4375},{lcn:11,freq:860.9625}
];

function renderFreqMap(grants) {
  const map = $('freqmap');
  if (!LCN_DATA.length) return;
  const minF = LCN_DATA[0].freq - 0.5;
  const maxF = LCN_DATA[LCN_DATA.length-1].freq + 0.5;
  const range = maxF - minF;
  const activeFreqs = new Set((grants||[]).map(g => g.frequency_mhz ? g.frequency_mhz.toFixed(4) : null).filter(Boolean));

  let html = '';
  for (const lcn of LCN_DATA) {
    const pct = ((lcn.freq - minF) / range * 100).toFixed(1);
    const isCC = lcn.lcn === 11;
    const isActive = activeFreqs.has(lcn.freq.toFixed(4));
    const cls = isCC ? 'cc' : (isActive ? 'active' : '');
    html += `<div class="freq-marker ${cls}" style="left:${pct}%"></div>`;
    html += `<div class="freq-label" style="left:${pct}%">${lcn.freq.toFixed(2)}</div>`;
    html += `<div class="freq-lcn" style="left:${pct}%">${lcn.lcn}${isCC?' CC':''}${isActive?' ★':''}</div>`;
  }
  map.innerHTML = html;
}

// WebSocket
function connectWs() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${proto}//${location.host}/ws/events`);
  ws.onmessage = e => {
    try {
      const evt = JSON.parse(e.data);
      const el = document.createElement('div');
      el.className = 'evt';
      const alias = evt.talkgroup_alias ? ` <span class="alias">${evt.talkgroup_alias}</span>` : '';
      el.innerHTML =
        `<span class="evt-time">${evt.timestamp}</span>` +
        `<span class="evt-type ${evt.event_type}">${evt.event_type}</span>` +
        `<span class="evt-detail">${evt.summary}${alias}</span>`;
      const act = $('activity');
      act.prepend(el);
      while (act.children.length > 200) act.lastChild.remove();
    } catch {}
    refresh();
  };
  ws.onclose = () => setTimeout(connectWs, 3000);
  ws.onerror = () => ws.close();
}

// Theme
function toggleTheme() {
  const body = document.body;
  const isLight = body.getAttribute('data-theme') === 'light';
  body.setAttribute('data-theme', isLight ? '' : 'light');
  $('themeBtn').textContent = isLight ? '\u{1f319}' : '\u2600\ufe0f';
  localStorage.setItem('theme', isLight ? 'dark' : 'light');
}
(function() {
  if (localStorage.getItem('theme') === 'light') {
    document.body.setAttribute('data-theme', 'light');
    $('themeBtn').textContent = '\u2600\ufe0f';
  }
})();

// Aliases
async function loadAliases() {
  aliases = await fetchJson('/api/aliases') || {};
}
function showAliases() {
  $('aliasText').value = JSON.stringify(aliases, null, 2);
  $('aliasModal').classList.add('show');
}
function closeAliases() { $('aliasModal').classList.remove('show'); }
async function saveAliases() {
  try {
    const parsed = JSON.parse($('aliasText').value);
    await fetch('/api/aliases', {
      method: 'PUT',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify(parsed)
    });
    aliases = parsed;
    closeAliases();
    refresh();
  } catch (e) { alert('Invalid JSON: ' + e.message); }
}

loadAliases();
refresh();
setInterval(refresh, 2000);
connectWs();
</script>
</body>
</html>"##;
