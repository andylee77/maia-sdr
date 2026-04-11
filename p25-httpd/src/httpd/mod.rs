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

use crate::p25::control_channel::{
    ControlChannelDecoder, RUNTIME_SYNC_THRESHOLD, SYNC_THRESHOLD,
};
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
    /// **Phase 6F.9 IQ-LSM `ControlChannelDecoder`**, fed by the Phase
    /// 6D `LsmPipeline` running on RAW IQ from the iq_dma ring. This
    /// is a third parallel TSBK pipeline that bypasses the HDL slicer
    /// for sync detection -- the LSM IQ task uses the soft-decision
    /// sync correlator on `demod.soft_phases` (the same one SDRTrunk
    /// uses) and dispatches every detected sync into
    /// `process_directed_tsdu`. The hope is to catch the ~9 syncs/sec
    /// the soft correlator finds vs the ~5 syncs/sec the dibit-domain
    /// hard correlator finds. See doc/changes/029.
    pub iq_lsm_decoder: Arc<RwLock<ControlChannelDecoder>>,
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
    /// Phase 7A.1: traffic-channel grant follower. Singleton, driven by
    /// the 50 ms polling task in main.rs that snapshots
    /// `lsm_decoder.grants` and forwards the newest entry. Read by
    /// `/api/traffic` to surface state, current TG/channel/frequency,
    /// NCO offset, and retune counters. Phase 7H will replace the
    /// singleton with a slot allocator over a channelizer.
    pub traffic_manager:
        Arc<tokio::sync::Mutex<crate::p25::traffic_manager::TrafficManager>>,
    /// Phase 7A.1: data-side counters for the traffic dibit DMA path,
    /// updated by the traffic dibit reader task in main.rs. Read by
    /// `/api/traffic` alongside the TrafficManager state.
    pub traffic_stats: Arc<tokio::sync::Mutex<crate::TrafficStats>>,
    /// Phase 7A.1: when false, the grant follower task in main.rs
    /// skips its 50 ms poll iteration entirely (no retunes, no
    /// timeouts). Flipped via `GET /api/traffic?follower=on|off` so
    /// the user can take manual control of the traffic DDC NCO +
    /// demod_enable bits without the polling task immediately
    /// overriding them. Default true; process-lifetime only.
    pub traffic_follower_enabled: Arc<std::sync::atomic::AtomicBool>,
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
        .route("/api/tsbk_opcodes", get(get_tsbk_opcodes))
        .route("/api/recent_tsbks", get(get_recent_tsbks))
        // Phase 6F.7 testing knobs. Both endpoints accept GET with
        // query params so they work from a plain curl / browser bar
        // without -X PUT / -X POST. The PUT/POST aliases are kept for
        // anyone who wants HTTP-method-correct calls.
        .route("/api/sync_tune", get(get_sync_tune).put(put_sync_tune))
        .route(
            "/api/decoder_reset",
            get(get_decoder_reset).post(post_decoder_reset),
        )
        // Phase 6G.2: runtime read/write of the lsm_control register
        // (lsm_enable, lsm_dibit_dma_enable, lsm_dc_block_enable). The
        // dc_block_enable bit is the runtime A/B knob the doc 030 PL
        // port roadmap wanted -- previously had to be poked via
        // ssh + devmem on the board.
        .route("/api/lsm_control", get(get_lsm_control))
        // Phase 7A.1: traffic-channel grant follower state + dibit
        // counters. Read-only diagnostic surface for the singleton
        // voice channel scaffold; will gain monitor-list write
        // operations in Phase 7B.
        .route("/api/traffic", get(get_traffic))
        .route("/api/aliases", get(get_aliases).put(put_aliases))
        .route("/ws/events", get(ws_events))
        .with_state(state)
}

// ── REST Handlers ──────────────────────────────────────────────────────

async fn get_system(State(state): State<Arc<AppState>>) -> Json<SystemInfo> {
    // Phase 6F.11 API-level merge: read BOTH lsm decoders and pick
    // the most-populated value for each field. This gives us the
    // union of state visible to either decoder pipeline. Both
    // decoders are tracking the same radio, so disagreement is
    // either (a) a transient where one is ahead of the other, or
    // (b) one pipeline lost a TSBK that the other caught -- either
    // way the right answer is "show the populated value".
    //
    // Why merge in the API instead of architecturally? Keeps the
    // diagnostic A/B comparison in /api/decoder_compare intact, and
    // we don't lose the regression insurance of having two
    // independent decoder paths. See doc/changes/030 for the
    // discussion.
    let dec_a = state.lsm_decoder.read().await;
    let dec_b = state.iq_lsm_decoder.read().await;
    let sa = &dec_a.system;
    let sb = &dec_b.system;
    fn pick<T: Clone>(a: Option<T>, b: Option<T>) -> Option<T> {
        a.or(b)
    }
    let system_clock_str = pick(sa.last_sync_clock, sb.last_sync_clock).map(
        |(y, mo, d, h, mn, locked)| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02} {}",
                y, mo, d, h, mn,
                if locked { "LOCKED" } else { "UNLOCKED" }
            )
        },
    );
    Json(SystemInfo {
        nac: pick(sa.nac, sb.nac).map(|n| format!("{}", n)),
        wacn: pick(sa.wacn, sb.wacn).map(|w| format!("{:05X}", w)),
        system_id: pick(sa.system_id, sb.system_id).map(|s| format!("{:03X}", s)),
        rfss_id: pick(sa.rfss_id, sb.rfss_id),
        site_id: pick(sa.site_id, sb.site_id),
        lra: pick(sa.lra, sb.lra),
        control_channel: pick(sa.control_channel, sb.control_channel)
            .map(|c| format!("{}", c)),
        secondary_cch_a: pick(sa.secondary_cch_a, sb.secondary_cch_a)
            .map(|c| format!("{}", c)),
        secondary_cch_b: pick(sa.secondary_cch_b, sb.secondary_cch_b)
            .map(|c| format!("{}", c)),
        sndcp_downlink_channel: pick(sa.sndcp_downlink_channel, sb.sndcp_downlink_channel)
            .map(|c| format!("{}", c)),
        sndcp_uplink_channel: pick(sa.sndcp_uplink_channel, sb.sndcp_uplink_channel)
            .map(|c| format!("{}", c)),
        system_clock: system_clock_str,
        build: Some(crate::BUILD_TAG.to_string()),
    })
}

async fn get_grants(State(state): State<Arc<AppState>>) -> Json<Vec<ChannelGrant>> {
    // Phase 6F.11 API-level merge: union grants from both decoders,
    // de-duped by channel. If the same channel appears in both we
    // pick the YOUNGER (smaller age) one since it's more recent.
    //
    // Phase 6G.x: also de-dupe by talkgroup across the union. Each
    // decoder's `grants` map is now talkgroup-clean internally
    // (`purge_other_grants_for_talkgroup` runs before insert), but
    // the cross-decoder union can still hold a stale entry if
    // decoder A latched TG T on channel X while decoder B latched
    // the same TG on a different channel Y just before A caught
    // the update. Treat the YOUNGEST entry per TG as the truth,
    // matching the per-channel rule.
    let dec_a = state.lsm_decoder.read().await;
    let dec_b = state.iq_lsm_decoder.read().await;
    let mut by_channel: std::collections::HashMap<u16, ChannelGrant> =
        std::collections::HashMap::new();
    let push = |dec: &ControlChannelDecoder,
                map: &mut std::collections::HashMap<u16, ChannelGrant>| {
        for g in dec.grants.values() {
            let cg = ChannelGrant {
                channel: format!("{}", g.channel),
                talkgroup: g.talkgroup.0,
                talkgroup_alias: dec.aliases.get(&g.talkgroup.0).cloned(),
                source: g.source.map(|s| s.0),
                frequency_mhz: g.frequency_hz.map(|f| f as f64 / 1_000_000.0),
                age_secs: g.timestamp.elapsed().as_secs(),
            };
            match map.get(&g.channel.0) {
                Some(existing) if existing.age_secs <= cg.age_secs => {}
                _ => {
                    map.insert(g.channel.0, cg);
                }
            }
        }
    };
    push(&dec_a, &mut by_channel);
    push(&dec_b, &mut by_channel);

    // Second pass: collapse by talkgroup, picking the youngest
    // surviving channel entry per TG. TG 0 is excluded from the
    // dedup (matches the decoder-side wildcard sentinel) so we
    // never collapse multiple unrelated "no-talkgroup" entries.
    let mut by_talkgroup: std::collections::HashMap<u16, ChannelGrant> =
        std::collections::HashMap::new();
    let mut tg0_passthrough: Vec<ChannelGrant> = Vec::new();
    for cg in by_channel.into_values() {
        if cg.talkgroup == 0 {
            tg0_passthrough.push(cg);
            continue;
        }
        match by_talkgroup.get(&cg.talkgroup) {
            Some(existing) if existing.age_secs <= cg.age_secs => {}
            _ => {
                by_talkgroup.insert(cg.talkgroup, cg);
            }
        }
    }
    let mut grants: Vec<ChannelGrant> = by_talkgroup.into_values().collect();
    grants.extend(tg0_passthrough);
    grants.sort_by_key(|g| g.age_secs);
    Json(grants)
}

async fn get_bands(State(state): State<Arc<AppState>>) -> Json<Vec<BandInfo>> {
    // Phase 6F.11 API-level merge: union frequency bands from both
    // decoders, de-duped by identifier. The two pipelines can land
    // different IDEN_UPDATE blocks at different times, so the union
    // gives the dashboard the complete table even if either single
    // pipeline missed a band.
    let dec_a = state.lsm_decoder.read().await;
    let dec_b = state.iq_lsm_decoder.read().await;
    let mut by_id: std::collections::HashMap<u8, BandInfo> =
        std::collections::HashMap::new();
    for dec in [&*dec_a, &*dec_b] {
        for b in dec.bands.values() {
            by_id.entry(b.identifier).or_insert_with(|| BandInfo {
                identifier: b.identifier,
                base_frequency_mhz: b.base_frequency_hz as f64 / 1_000_000.0,
                channel_spacing_khz: b.channel_spacing_hz as f64 / 1_000.0,
                transmit_offset_mhz: b.transmit_offset_hz as f64 / 1_000_000.0,
                bandwidth_khz: b.bandwidth_hz as f64 / 1_000.0,
            });
        }
    }
    let mut bands: Vec<BandInfo> = by_id.into_values().collect();
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

    // Poll for up to 10 seconds. The LSM dibit reader IRQ fires only
    // about every 3.5 seconds (one buffer per IRQ), and the reader
    // holds the decoder write() lock for the duration of one buffer
    // (~8000 dibits). So the API may have to wait up to 2 IRQ cycles
    // before it can read a populated capture. 10 s gives us at least
    // 3 cycles of headroom -- if no sync hits in that long, the chain
    // is genuinely stalled.
    let deadline = Instant::now() + Duration::from_millis(10000);
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

/// Phase 6F.7: read the current runtime sync threshold + a quick
/// histogram-based "tuning hint" so an operator can decide what to
/// set next without rebuilding the dashboard.
///
/// **Dual-mode endpoint:** if called with `?threshold=N`, this also
/// updates the runtime threshold (mirroring the PUT handler) so it
/// works from a browser bar or plain `curl` without `-X PUT`. The
/// response always contains the *current* (post-update) value.
async fn get_sync_tune(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;

    // GET-with-query-param shortcut: if `threshold=N` is present and
    // valid, update the runtime threshold before returning the
    // histogram.
    let mut updated_from = None;
    if let Some(v) = params.get("threshold") {
        if let Ok(n) = v.parse::<u32>() {
            if n <= 24 {
                let prev = RUNTIME_SYNC_THRESHOLD.swap(n, Ordering::Relaxed);
                updated_from = Some(prev);
            }
        }
    }

    let dec = state.lsm_decoder.read().await;
    let cur = RUNTIME_SYNC_THRESHOLD.load(Ordering::Relaxed);
    let hist = dec.sync_distance_hist;

    // Cumulative counts at each prospective threshold (0..=24).
    let mut cumulative = [0u64; 25];
    let mut running = 0u64;
    for (i, &v) in hist.iter().enumerate() {
        running += v;
        cumulative[i] = running;
    }
    let total: u64 = hist.iter().sum();

    Json(serde_json::json!({
        "current_threshold":   cur,
        "default_threshold":   SYNC_THRESHOLD,
        "updated_from":        updated_from,
        "total_observations":  total,
        "cumulative_at_threshold": cumulative,
        "histogram": hist,
        "note": "GET /api/sync_tune?threshold=N updates the runtime sync \
                 threshold in-place (no PUT needed). Range 0..=24. Use \
                 the histogram + cumulative arrays to pick a threshold \
                 that captures the real-sync cluster (clear bump above \
                 the binomial random tail) without flooding the \
                 pipeline with noise. After tuning, GET \
                 /api/decoder_reset clears counters so you can measure \
                 the new threshold against a clean baseline.",
    }))
}

/// Phase 6F.7: clear the per-run decoder counters and histograms
/// without restarting the binary. Lets us measure a new
/// `/api/sync_tune?threshold=N` value against a clean baseline
/// instead of waiting hours for the cumulative counters to wash out.
///
/// What this clears: NID counters, TSDU counters, TSBK CRC counters,
/// per-opcode histograms, per-block-position counters, sync hits,
/// sync near misses, sync distance histogram, recent_messages ring,
/// raw_duid_hist, dibit_hist.
///
/// What this PRESERVES: system identity (NAC/WACN/RFSS/...),
/// frequency band table, active grants, talkgroup aliases. Those are
/// long-lived radio state that shouldn't be wiped just because we
/// want a clean measurement window.
async fn get_decoder_reset(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    {
        let mut dec = state.lsm_decoder.write().await;
        dec.reset_diagnostics();
    }
    {
        // Phase 6F.9: also reset the iq_lsm decoder so the sweep tool
        // and `/api/decoder_compare` start from a clean baseline.
        let mut dec = state.iq_lsm_decoder.write().await;
        dec.reset_diagnostics();
    }
    Json(serde_json::json!({
        "ok": true,
        "note": "Both lsm_decoder + iq_lsm_decoder counters + histograms \
                 cleared. System identity, bands, grants, and aliases \
                 preserved.",
    }))
}

async fn post_decoder_reset(state: State<Arc<AppState>>) -> Json<serde_json::Value> {
    get_decoder_reset(state).await
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
/// curl http://192.168.2.1:8080/api/lsm_control?dc_block=0
/// curl http://192.168.2.1:8080/api/lsm_control?dc_block=1
/// ```
async fn get_lsm_control(
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
                let (_, _, prev) = core.lsm_control_readback();
                core.set_lsm_dc_block_enable(new_bit);
                updated_from = Some(prev);
            }
        }

        let (lsm_en, lsm_dma_en, lsm_dc_block) = core.lsm_control_readback();
        Json(serde_json::json!({
            "lsm_enable":            lsm_en,
            "lsm_dibit_dma_enable":  lsm_dma_en,
            "lsm_dc_block_enable":   lsm_dc_block,
            "updated_from":          updated_from,
            "register_address":      "0x7C4600A0",
            "bit_layout": {
                "lsm_enable":            "[0]",
                "lsm_dibit_dma_enable":  "[1]",
                "lsm_dc_block_enable":   "[2]"
            },
            "note": "GET /api/lsm_control?dc_block=0 disables the LSM \
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

/// Phase 7A.1: GET /api/traffic -- traffic-channel grant follower
/// state + dibit DMA counters, with optional manual control via
/// query parameters.
///
/// **Read-side** (no params): returns a snapshot of the TrafficManager
/// state machine, the TrafficStats counters, and the traffic_dma IRQ
/// count.
///
/// **Write-side** (query params, applied in this order before reading
/// the snapshot below):
///
/// 1. `?reset_stats=1` -- zero out the TrafficStats counters
///    (wakeups, total_*, dibit_hist). Useful for clean A/B
///    comparisons after a config change.
/// 2. `?follower=on|off` -- pause/resume the 50 ms grant-follower
///    polling task in main.rs. When `off`, manual retunes won't be
///    immediately overridden by the next snapshot. Default state is
///    `on`; the override does NOT persist across p25-httpd restarts.
/// 3. `?retune_hz=<i64>` -- manually write the traffic DDC NCO offset
///    in Hz, signed, relative to the AD9361 RX LO. Bypasses the
///    grant follower entirely. Does NOT touch `demod_enable` --
///    explicit by design (see #4).
/// 4. `?demod_enable=0|1` -- manually flip the
///    `traffic_demod_control.demod_enable` register bit. Required
///    after a manual retune to actually start the dibit stream.
///
/// All four params can be combined in one call:
/// `GET /api/traffic?follower=off&reset_stats=1&retune_hz=2862500&demod_enable=1`
/// will pause the follower, zero the counters, retune to RX LO + 2.8625
/// MHz, and turn on the demod -- in that order, so the histogram
/// counts only what arrives after the retune.
///
/// At Phase 7A.1 the traffic chain is C4FM-only and Clay County is
/// LSM, so the dibit *content* is expected garbage on real LSM voice
/// channels. The histogram is included as a sanity check: a dead
/// chain produces all-zero dibits, a live chain produces a roughly
/// even spread across all four dibit values. Phase 7A.2 will add an
/// LSM parallel chain on the traffic side and the histogram will
/// become decode-quality data.
async fn get_traffic(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;

    // Track which write actions actually fired so the JSON response
    // can echo them back -- gives the caller a confirmation that the
    // params were parsed and applied (vs. silently ignored due to a
    // typo).
    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // ── 1. reset_stats ──
    if params.get("reset_stats").map(String::as_str) == Some("1") {
        let mut s = state.traffic_stats.lock().await;
        *s = crate::TrafficStats::default();
        applied.push("reset_stats=1".into());
    }

    // ── 2. follower on/off ──
    if let Some(v) = params.get("follower") {
        match v.as_str() {
            "on" | "1" | "true" => {
                state
                    .traffic_follower_enabled
                    .store(true, Ordering::Relaxed);
                applied.push("follower=on".into());
            }
            "off" | "0" | "false" => {
                state
                    .traffic_follower_enabled
                    .store(false, Ordering::Relaxed);
                applied.push("follower=off".into());
            }
            other => {
                errors.push(format!(
                    "follower={other}: expected on|off|1|0|true|false"
                ));
            }
        }
    }

    // ── 3. retune_hz (manual NCO write) ──
    //    Linux-only because it touches the FPGA registers via the
    //    ip_core lock. The non-Linux build path simply records an
    //    error so host-side cargo test of the routing still works.
    if let Some(v) = params.get("retune_hz") {
        match v.parse::<i64>() {
            Ok(offset_hz) => {
                #[cfg(target_os = "linux")]
                {
                    let core = state.ip_core.lock().await;
                    // Read the AD9361 sample rate from the cached
                    // register (the same value the startup configure
                    // call used). For Phase 7A.1 we hard-code this
                    // from the well-known default; if we ever start
                    // varying sample rate at runtime this needs to
                    // come from a shared config struct instead.
                    let sample_rate_hz = 8_000_000.0_f64;
                    match core.set_traffic_ddc_frequency(
                        offset_hz as f64,
                        sample_rate_hz,
                    ) {
                        Ok(()) => {
                            applied.push(format!(
                                "retune_hz={offset_hz}"
                            ));
                            // Mirror the manager-side bookkeeping so
                            // /api/traffic shows the new offset
                            // immediately even though the follower
                            // didn't drive it.
                            let mut mgr =
                                state.traffic_manager.lock().await;
                            mgr.last_offset_hz = offset_hz;
                            // Recompute the NCO word the same way
                            // the helper does, so the dashboard's
                            // displayed nco_word matches the register.
                            let nco_frac =
                                offset_hz as f64 / sample_rate_hz;
                            mgr.nco_word = (nco_frac
                                * (1u64 << 28) as f64)
                                as i32
                                as u32
                                & 0x0FFF_FFFF;
                        }
                        Err(e) => {
                            errors.push(format!(
                                "retune_hz={offset_hz} rejected: {e}"
                            ));
                        }
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = offset_hz;
                    errors.push(
                        "retune_hz requires hardware (target_os=linux)"
                            .into(),
                    );
                }
            }
            Err(_) => {
                errors.push(format!(
                    "retune_hz={v}: expected signed integer Hz offset"
                ));
            }
        }
    }

    // ── 4. demod_enable ──
    if let Some(v) = params.get("demod_enable") {
        let parsed = match v.as_str() {
            "1" | "on" | "true" => Some(true),
            "0" | "off" | "false" => Some(false),
            _ => None,
        };
        match parsed {
            Some(bit) => {
                #[cfg(target_os = "linux")]
                {
                    let core = state.ip_core.lock().await;
                    core.set_traffic_demod_enable(bit);
                    applied.push(format!("demod_enable={bit}"));
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = bit;
                    errors.push(
                        "demod_enable requires hardware (target_os=linux)"
                            .into(),
                    );
                }
            }
            None => {
                errors.push(format!(
                    "demod_enable={v}: expected 0|1|on|off|true|false"
                ));
            }
        }
    }

    // ── Snapshot read (always, even after a write) ──
    let (
        state_label,
        current_channel,
        current_talkgroup,
        current_frequency_hz,
        nco_word,
        last_offset_hz,
        grants_seen,
        retunes,
        last_retune_at_secs_ago,
    ) = {
        let mgr = state.traffic_manager.lock().await;
        let label = mgr.state_label();
        let ch = mgr.current_channel().map(|c| c.0);
        let tg = mgr.current_talkgroup().map(|t| t.0);
        let freq = mgr.current_frequency();
        let nco = mgr.nco_word;
        let offset = mgr.last_offset_hz;
        let seen = mgr.grants_seen;
        let retunes = mgr.retunes;
        let age = mgr
            .last_retune_at
            .map(|t| t.elapsed().as_secs_f64());
        (label, ch, tg, freq, nco, offset, seen, retunes, age)
    };

    let stats_json = {
        let s = state.traffic_stats.lock().await;
        let total: u64 = s.dibit_hist.iter().sum();
        let pct = |v: u64| -> f64 {
            if total == 0 {
                0.0
            } else {
                100.0 * v as f64 / total as f64
            }
        };
        serde_json::json!({
            "wakeups":        s.wakeups,
            "total_buffers":  s.total_buffers,
            "total_bytes":    s.total_bytes,
            "total_dibits":   s.total_dibits,
            "dibit_hist":     s.dibit_hist,
            "dibit_hist_pct": [
                pct(s.dibit_hist[0]), pct(s.dibit_hist[1]),
                pct(s.dibit_hist[2]), pct(s.dibit_hist[3])
            ],
            "started_secs_ago": s.started_at.map(|t| t.elapsed().as_secs_f64()),
            "last_secs_ago":    s.last_at.map(|t| t.elapsed().as_secs_f64()),
        })
    };

    let irq_json = {
        let s = state.irq_stats.lock().await;
        serde_json::json!({
            "traffic_dma_total": s.traffic,
        })
    };

    let follower_on =
        state.traffic_follower_enabled.load(Ordering::Relaxed);

    Json(serde_json::json!({
        "state":                     state_label,
        "follower_enabled":          follower_on,
        "current_channel":           current_channel,
        "current_talkgroup":         current_talkgroup,
        "current_frequency_hz":      current_frequency_hz,
        "nco_word":                  nco_word,
        "nco_word_hex":              format!("0x{:08X}", nco_word),
        "last_offset_hz":            last_offset_hz,
        "grants_seen":               grants_seen,
        "retunes":                   retunes,
        "last_retune_secs_ago":      last_retune_at_secs_ago,
        "stats":                     stats_json,
        "irq":                       irq_json,
        "applied":                   applied,
        "errors":                    errors,
        "phase":                     "7A.1",
        "modulation":                "C4FM-only (LSM traffic chain coming in 7A.2)",
        "controls": {
            "reset_stats":   "?reset_stats=1            -- zero TrafficStats",
            "follower":      "?follower=on|off          -- pause/resume 50 ms poll",
            "retune_hz":     "?retune_hz=<i64>          -- manual NCO offset (Hz, signed)",
            "demod_enable":  "?demod_enable=0|1         -- manual demod_enable bit"
        },
        "note": "Singleton voice-channel grant follower. Polls \
                 lsm_decoder.grants @ 50ms and retunes the traffic DDC \
                 to the most recent grant. At 7A.1 the traffic demod \
                 is C4FM and the test target (Clay County) is LSM, so \
                 the dibit histogram is the only useful 'is the chain \
                 alive' signal -- the dibit *content* is garbage until \
                 7A.2 ships an LSM traffic chain. Manual retune does \
                 NOT auto-enable demod -- explicit by design.",
    }))
}

/// Phase 6F.7: PUT /api/sync_tune?threshold=N -- update the runtime
/// sync threshold without rebuilding. Validates 0 <= N <= 24.
async fn put_sync_tune(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;
    let new = params
        .get("threshold")
        .and_then(|v| v.parse::<u32>().ok());
    match new {
        Some(n) if n <= 24 => {
            let prev = RUNTIME_SYNC_THRESHOLD.swap(n, Ordering::Relaxed);
            Json(serde_json::json!({
                "ok":            true,
                "previous":      prev,
                "current":       n,
                "default":       SYNC_THRESHOLD,
                "note":          "Threshold updated. Counters keep accumulating; \
                                  use /api/sync_tune to verify the new histogram \
                                  shape after a few seconds of new data.",
            }))
        }
        _ => Json(serde_json::json!({
            "ok":     false,
            "error":  "missing or invalid `threshold` query param (must be 0..=24)",
            "current": RUNTIME_SYNC_THRESHOLD.load(Ordering::Relaxed),
        })),
    }
}

/// Phase 6F.4: per-opcode histogram of CRC-OK and CRC-FAIL TSBK
/// blocks decoded by the LSM software decoder. Lets the dashboard
/// see the on-air opcode distribution and pinpoint missing parsers.
async fn get_tsbk_opcodes(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let dec = state.lsm_decoder.read().await;

    // Map opcode index → SDRTrunk-style label so the dashboard
    // doesn't have to mirror the table. Covers all opcodes that
    // appear in `Opcode.java` for the OSP direction (control-channel
    // outbound). Lowercase here means we don't recognise it as a P25
    // opcode at all (probably trellis-decode garbage).
    fn label(op: u8) -> &'static str {
        match op {
            0x00 => "GRP_V_CH_GRANT",
            0x02 => "GRP_V_CH_GRANT_UPDT",
            0x03 => "GRP_V_CH_GRANT_UPDT_EXP",
            0x04 => "UU_V_CH_GRANT",
            0x05 => "UU_ANS_REQ",
            0x06 => "UU_V_CH_GRANT_UPDT",
            0x08 => "TELE_INT_V_CH_GRANT",
            0x09 => "TELE_INT_V_CH_GRANT_UPDT",
            0x0A => "TELE_INT_ANS_REQ",
            0x14 => "SNDCP_DCH_GRANT",
            0x15 => "SNDCP_DCH_PAG_RQ",
            0x16 => "SNDCP_DCH_ANN_EX",
            0x18 => "STS_UPDT",
            0x1C => "MSG_UPDT",
            0x1F => "CALL_ALERT",
            0x20 => "ACK_RESPONSE_FNE",
            0x21 => "QUEUED_RESP",
            0x22 => "EXT_FNCT_CMD",
            0x24 => "DENY_RESPONSE",
            0x27 => "GRP_AFFIL_RESP",
            0x28 => "SCCB",
            0x29 => "RFSS_STS_BCST_EXP",
            0x2A => "NET_STS_BCST_EXP",
            0x2B => "ADJ_STS_BCST_EXP",
            0x2C => "IDEN_UP_VUHF_EXP",
            0x2D => "DENY_RESPONSE_EXP",
            0x2F => "DE_REGIST_ACK",
            0x30 => "TDMA_SYNC_BCST",
            0x31 => "AUTH_DMD",
            0x32 => "AUTH_FNE_RESULT",
            0x33 => "IDEN_UPDATE_TDMA",
            0x34 => "IDEN_UPDATE_VUHF",
            0x36 => "TIME_DATE",
            0x37 => "ROAM_ADDR_CMD",
            0x38 => "SYS_SRV_BCST",
            0x39 => "SEC_CCH_BROADCST",
            0x3A => "RFSS_STATUS_BCST",
            0x3B => "NET_STATUS_BCAST",
            0x3C => "ADJ_STS_BCAST",
            0x3D => "IDEN_UPDATE",
            0x3E => "PROT_PARAM_BCST",
            0x3F => "PROT_PARAM_UPDT",
            _ => "(unknown)",
        }
    }

    let mut entries = Vec::with_capacity(64);
    let mut total_ok = 0u64;
    let mut total_fail = 0u64;
    for op in 0u8..64 {
        let ok = dec.tsbk_opcode_hist_ok[op as usize];
        let fail = dec.tsbk_opcode_hist_fail[op as usize];
        total_ok += ok;
        total_fail += fail;
        if ok > 0 || fail > 0 {
            let parsed = matches!(
                op,
                // 6F.4 + 6F.5: voice grants, IDEN_UPDATE variants,
                // RFSS / NET / ADJ status broadcasts.
                0x00 | 0x02 | 0x33 | 0x34 | 0x3A | 0x3B | 0x3C | 0x3D
                // 6F.11: 5 new parsers added in this phase.
                | 0x05 | 0x09 | 0x16 | 0x30 | 0x39
            );
            entries.push(serde_json::json!({
                "opcode": format!("0x{:02X}", op),
                "label": label(op),
                "ok": ok,
                "fail": fail,
                "parsed": parsed,
            }));
        }
    }
    // Sort by ok-count descending so the most common live opcodes
    // float to the top of the list.
    entries.sort_by(|a, b| {
        let a_ok = a["ok"].as_u64().unwrap_or(0);
        let b_ok = b["ok"].as_u64().unwrap_or(0);
        b_ok.cmp(&a_ok)
    });

    // Per-block-position rates (TSBK1 / TSBK2 / TSBK3 attempts and
    // CRC successes). If TSBK2 / TSBK3 success rates are massively
    // worse than TSBK1, the multi-block continuation alignment is
    // wrong somewhere upstream.
    let attempts_pos = dec.tsbk_block_attempts_by_pos;
    let crc_ok_pos = dec.tsbk_crc_ok_by_pos;
    let pos_pct = |a: u64, ok: u64| -> f64 {
        if a == 0 { 0.0 } else { 100.0 * ok as f64 / a as f64 }
    };

    Json(serde_json::json!({
        "tsdu_attempts": dec.tsdu_attempts,
        "tsbk_block_attempts_total": dec.tsbk_block_attempts,
        "blocks_per_tsdu": if dec.tsdu_attempts == 0 { 0.0 }
            else { dec.tsbk_block_attempts as f64 / dec.tsdu_attempts as f64 },
        "crc_ok_total": total_ok,
        "crc_fail_total": total_fail,
        "crc_ok_pct": if (total_ok + total_fail) == 0 { 0.0 }
            else { 100.0 * total_ok as f64 / (total_ok + total_fail) as f64 },
        "by_position": {
            "tsbk1": {
                "attempts": attempts_pos[0],
                "crc_ok": crc_ok_pos[0],
                "crc_ok_pct": pos_pct(attempts_pos[0], crc_ok_pos[0]),
            },
            "tsbk2": {
                "attempts": attempts_pos[1],
                "crc_ok": crc_ok_pos[1],
                "crc_ok_pct": pos_pct(attempts_pos[1], crc_ok_pos[1]),
            },
            "tsbk3": {
                "attempts": attempts_pos[2],
                "crc_ok": crc_ok_pos[2],
                "crc_ok_pct": pos_pct(attempts_pos[2], crc_ok_pos[2]),
            },
        },
        "mfid_breakdown": {
            "standard_0x00": dec.tsbk_mfid_hist_ok[0],
            "motorola_0x90": dec.tsbk_mfid_hist_ok[1],
            "harris_0xA4":   dec.tsbk_mfid_hist_ok[2],
            "other":         dec.tsbk_mfid_hist_ok[3],
        },
        "opcodes": entries,
    }))
}

/// Phase 6F.4: dump the most recent TSBK messages with their
/// originating block index (TSBK1/2/3), so the dashboard can show a
/// live activity feed in the same format as SDRTrunk's
/// decoded_messages.log.
async fn get_recent_tsbks(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let dec = state.lsm_decoder.read().await;
    let now = std::time::Instant::now();

    let summarize = |msg: &crate::p25::tsbk::TsbkMessage| -> String {
        use crate::p25::tsbk::TsbkMessage::*;
        match msg {
            NetworkStatus { wacn, system_id, channel } => format!(
                "NET_STATUS_BCAST WACN:{:05X} SYS:{:03X} CH:{}",
                wacn, system_id, channel
            ),
            RfssStatus { lra, rfss_id, site_id, channel } => format!(
                "RFSS_STATUS_BCST LRA:{} RFSS:{} SITE:{} CH:{}",
                lra, rfss_id, site_id, channel
            ),
            AdjacentStatus { lra, rfss_id, site_id, channel, system_id } => format!(
                "ADJ_STS_BCAST LRA:{} SYS:{:03X} RFSS:{} SITE:{} CH:{}",
                lra, system_id, rfss_id, site_id, channel
            ),
            IdentifierUpdate {
                identifier,
                bw,
                transmit_offset,
                channel_spacing,
                base_frequency,
            } => format!(
                "IDEN_UPDATE ID:{} OFFSET:{} SPACING:{} BASE:{} BW:{}",
                identifier, transmit_offset, channel_spacing, base_frequency, bw
            ),
            GroupVoiceChannelGrant { channel, talkgroup, source } => format!(
                "GRP_V_CH_GRANT CH:{} TG:{} SRC:{}",
                channel, talkgroup, source
            ),
            GroupVoiceChannelGrantUpdate {
                channel_a, talkgroup_a, channel_b, talkgroup_b,
            } => format!(
                "GRP_V_CH_GRANT_UPDT CH_A:{} TG_A:{} CH_B:{} TG_B:{}",
                channel_a, talkgroup_a, channel_b, talkgroup_b
            ),
            // Phase 6F.11 new opcodes
            SecondaryControlChannelBroadcast {
                rfss_id, site_id, channel_a, channel_b,
            } => format!(
                "SEC_CCH_BROADCST RFSS:{} SITE:{} A:{} B:{}",
                rfss_id, site_id, channel_a, channel_b
            ),
            SndcpDataChannelAnnouncementExplicit {
                downlink_channel, uplink_channel, autonomous_access,
                requested_access, ..
            } => format!(
                "SNDCP_DCH_ANN_EX DL:{} UL:{} {}{}",
                downlink_channel, uplink_channel,
                if *autonomous_access { "AUTO " } else { "" },
                if *requested_access { "REQ" } else { "" },
            ),
            TdmaSyncBroadcast {
                year, month, day, hours, minutes, time_locked, ..
            } => format!(
                "TDMA_SYNC_BCST {:04}-{:02}-{:02} {:02}:{:02} {}",
                year, month, day, hours, minutes,
                if *time_locked { "LOCKED" } else { "UNLOCKED" }
            ),
            TelephoneInterconnectVoiceChannelGrantUpdate {
                channel, call_timer_secs, unit_id,
            } => format!(
                "TEL_INT_VCH_GRNT_UPDT UNIT:{} CH:{} timer:{}s",
                unit_id, channel, call_timer_secs
            ),
            UnitToUnitAnswerRequest { target, source } => format!(
                "UU_ANS_REQ TGT:{} SRC:{}", target, source
            ),
        }
    };

    // Iterate newest-first.
    let entries: Vec<serde_json::Value> = dec
        .recent_messages
        .iter()
        .rev()
        .take(50)
        .map(|(t, block_idx, msg)| {
            let block_label = match block_idx {
                0 => "TSBK1",
                1 => "TSBK2",
                2 => "TSBK3",
                _ => "TSBK?",
            };
            serde_json::json!({
                "age_secs": now.duration_since(*t).as_secs_f64(),
                "block": block_label,
                "summary": summarize(msg),
            })
        })
        .collect();

    Json(serde_json::json!({
        "count": entries.len(),
        "messages": entries,
    }))
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
            "threshold":     RUNTIME_SYNC_THRESHOLD.load(std::sync::atomic::Ordering::Relaxed),
            "threshold_default": SYNC_THRESHOLD,
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
    let dec_iq_lsm = state.iq_lsm_decoder.read().await;
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
        "ps_iq_lsm": {
            "label":           "PS IQ-LSM (software, raw IQ + soft sync -> TSBK)",
            "system_nac":      fmt_nac(dec_iq_lsm.system.nac),
            "messages":        dec_iq_lsm.recent_messages.len(),
            "active_grants":   dec_iq_lsm.grants.len(),
            "bands_known":     dec_iq_lsm.bands.len(),
            "nid_attempts":          dec_iq_lsm.nid_attempts,
            "nid_decode_failures":   dec_iq_lsm.nid_decode_failures,
            "nid_invalid_duid":      dec_iq_lsm.nid_invalid_duid,
            "nid_decoded_ok":        dec_iq_lsm.nid_decoded_ok,
            "nid_decoded_tsdu":      dec_iq_lsm.nid_decoded_tsdu,
            "tsdu_attempts":         dec_iq_lsm.tsdu_attempts,
            "tsbk_block_attempts":   dec_iq_lsm.tsbk_block_attempts,
            "tsbk_trellis_failures": dec_iq_lsm.tsbk_trellis_failures,
            "tsbk_crc_failures":     dec_iq_lsm.tsbk_crc_failures,
            "tsbk_crc_ok":           dec_iq_lsm.tsbk_crc_ok,
            "tsbk_crc_ok_plain":     dec_iq_lsm.tsbk_crc_ok_plain,
            "tsbk_crc_ok_xored":     dec_iq_lsm.tsbk_crc_ok_xored,
            "tsbk_unknown_opcode":   dec_iq_lsm.tsbk_unknown_opcode,
        },
        "ps_phase6d": {
            "label":           "PS Phase 6D (software, raw IQ-fed -- sync detection only)",
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
