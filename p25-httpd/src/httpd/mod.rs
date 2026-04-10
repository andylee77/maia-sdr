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
        .route("/api/dibit_dump", get(get_dibit_dump))
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

    Json(serde_json::json!({
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
        "dibits_hex": hex,
    }))
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
  </div>
  <div>
    <span class="alias-btn" onclick="showAliases()">&#x2699; Aliases</span>
    <button class="theme-btn" onclick="toggleTheme()" id="themeBtn">&#x1f319;</button>
  </div>
</div>

<div class="grid2">
  <div class="card">
    <h2>System Identity</h2>
    <table>
      <tr><th>NAC</th><td class="v" id="nac">--</td></tr>
      <tr><th>WACN</th><td class="v" id="wacn">--</td></tr>
      <tr><th>System</th><td class="v" id="sys">--</td></tr>
      <tr><th>RFSS / Site</th><td class="v" id="rfss">--</td></tr>
      <tr><th>Control CH</th><td class="v" id="cc">--</td></tr>
    </table>
  </div>
  <div class="card">
    <h2>Decode Stats</h2>
    <table>
      <tr><th>Messages</th><td class="v" id="msgs">0</td></tr>
      <tr><th>Active Grants</th><td class="v" id="grants_n">0</td></tr>
      <tr><th>Bands Known</th><td class="v" id="bands_n">0</td></tr>
      <tr><th>Dibit Count</th><td class="v" id="dibits">0</td></tr>
      <tr><th>Overflow</th><td class="v" id="overflow">No</td></tr>
    </table>
  </div>
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

<div class="grid2">
  <div class="card">
    <h2>Dibit Histogram</h2>
    <table>
      <tr><th>Total Dibits</th><td class="v" id="dh_total">0</td></tr>
      <tr><th>Value 0 (+1)</th><td class="v" id="dh_0">--</td></tr>
      <tr><th>Value 1 (+3)</th><td class="v" id="dh_1">--</td></tr>
      <tr><th>Value 2 (-1)</th><td class="v" id="dh_2">--</td></tr>
      <tr><th>Value 3 (-3)</th><td class="v" id="dh_3">--</td></tr>
    </table>
  </div>
  <div class="card">
    <h2>Sync Correlator</h2>
    <table>
      <tr><th>Sync Hits</th><td class="v" id="sy_hits">0</td></tr>
      <tr><th>Near Misses</th><td class="v" id="sy_near">0</td></tr>
      <tr><th>Best Distance</th><td class="v" id="sy_best">--</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.8em;margin-top:6px">
      Hamming distance to P25 frame sync (48 bits). Random&asymp;24, locked&le;4.
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

  const dump = await fetchJson('/api/dibit_dump');
  if (dump) {
    $('dh_total').textContent = dump.total_dibits.toLocaleString();
    const fmt = (n, p) => `${n.toLocaleString()} (${p.toFixed(1)}%)`;
    $('dh_0').textContent = fmt(dump.histogram['0'], dump.histogram['0_pct']);
    $('dh_1').textContent = fmt(dump.histogram['1'], dump.histogram['1_pct']);
    $('dh_2').textContent = fmt(dump.histogram['2'], dump.histogram['2_pct']);
    $('dh_3').textContent = fmt(dump.histogram['3'], dump.histogram['3_pct']);
    $('sy_hits').textContent = dump.sync.hits.toLocaleString();
    $('sy_near').textContent = dump.sync.near_misses.toLocaleString();
    const bd = dump.sync.best_distance;
    $('sy_best').textContent = (bd >= 4294967000) ? '--' : bd;
  }

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
