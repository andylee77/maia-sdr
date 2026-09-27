//! System identity + health + endpoint self-describe.
//!
//! Consumer orientation: "what radio am I talking to, and is it
//! healthy right now?" Primary landing page for any new consumer —
//! a well-behaved client hits `/api/system` on startup (for the
//! build tag and P25 identity) and may poll `/api/sys_health` at
//! 1 Hz for CPU/memory health.
//!
//! Holds the `ENDPOINT_CATALOGUE` that `/api/endpoints` serves. This
//! is the runtime spec — Android, diagnostic tools, and any other
//! consumer MUST use it to discover the live endpoint set rather
//! than hardcoding against a stale copy of `doc/P25_API.md`. See
//! [`doc/API_CONSUMERS.md`](../../../../doc/API_CONSUMERS.md) for
//! the contract.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
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

/// `POST /api/set_time?unix_ms=<i64>`
///
/// Sets the board's wall clock to the given Unix epoch (in ms).
/// Designed for isolated networks — on an RNDIS-over-USB link or
/// any setup without routable internet, the standard NTP-on-boot
/// path fails, and the board sits at 1970-01-01 forever. The
/// dashboard calls this with `Date.now()` every time it loads, so
/// the board ends up with whatever time the browser knows. Not as
/// precise as real NTP (limited to ~HTTP round-trip-jitter) but
/// good enough for event-log ordering + wall-clock display.
///
/// POST (not PUT) because it mutates system state outside /api/.
#[cfg(target_os = "linux")]
pub async fn post_set_time(
    State(_state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let Some(ms_str) = params.get("unix_ms") else {
        return Json(serde_json::json!({
            "ok": false,
            "error": "missing ?unix_ms=<epoch_ms>",
        }));
    };
    let ms: i64 = match ms_str.parse() {
        Ok(v) => v,
        Err(_) => {
            return Json(serde_json::json!({
                "ok": false,
                "error": format!("bad unix_ms '{ms_str}' (expected integer)"),
            }));
        }
    };
    // Sanity: 2020-01-01 to 2070-01-01 in milliseconds. Guards
    // against a misbehaving browser clock / bogus query.
    if !(1_577_836_800_000..3_155_760_000_000).contains(&ms) {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!("unix_ms {ms} out of sane range (2020..2070)"),
        }));
    }
    let tv = libc::timeval {
        tv_sec: (ms / 1000) as libc::time_t,
        tv_usec: ((ms % 1000) * 1000) as libc::suseconds_t,
    };
    let rc = unsafe { libc::settimeofday(&tv, std::ptr::null()) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Json(serde_json::json!({
            "ok": false,
            "error": format!("settimeofday failed: {err}"),
        }));
    }
    Json(serde_json::json!({
        "ok": true,
        "set_unix_ms": ms,
        "note": "Wall clock updated. Use this on boards with no NTP reachability (RNDIS, air-gapped).",
    }))
}


#[cfg(not(target_os = "linux"))]
pub async fn post_set_time(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "set_time only available on the target (linux/arm)",
    }))
}


/// Hand-maintained catalogue of /api/* endpoints. New routes MUST
/// add an entry here in the same commit that adds them to
/// `router()`; the dashboard's API tab renders from this list.
///
/// Order: routes in alphabetical-by-path order so the table is
/// deterministic. Method is GET unless noted; mixed-method routes
/// (GET+PUT/POST) list both.
pub struct EndpointDoc {
    method: &'static str,
    path: &'static str,
    params: &'static str,
    description: &'static str,
}


pub const ENDPOINT_CATALOGUE: &[EndpointDoc] = &[
    EndpointDoc {
        method: "GET",
        path: "/api/aliases",
        params: "",
        description: "Return the talkgroup-alias map (TG number → display name).",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/aliases",
        params: "body=JSON {tg: name, ...}",
        description: "Replace the alias map. Body is a JSON object keyed by TG number.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/audio",
        params: "?format=wav",
        description: "Stream live vocoder PCM as an open-ended WAV (8 kHz 16-bit mono).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/audio_test",
        params: "",
        description: "One-shot ring dump of the vocoder's internal test tone buffer; diagnostic.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/bands",
        params: "",
        description: "List known P25 identifier_update frequency bands (base, spacing, offset, BW).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/bch_t",
        params: "?side=control|traffic",
        description: "Read the runtime BCH(63,16,t) error-correction cap for each decoder.",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/bch_t",
        params: "?side=control|traffic&t=<0..11>",
        description: "Override the BCH-t cap at runtime without rebuilding. Reset with t=reset.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/decoder_compare",
        params: "",
        description: "3-column matrix: PS C4FM vs PS LSM framer vs PL HDL LSM runtime stats.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/decoder_reset",
        params: "?side=control|traffic",
        description: "Reset the framer state of one of the decoders (keeps cumulative counters).",
    },
    EndpointDoc {
        method: "POST",
        path: "/api/decoder_reset",
        params: "?side=control|traffic",
        description: "Same as GET /api/decoder_reset; HTTP-method-correct variant.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/dibit_delivery",
        params: "",
        description: "Dibit ring delivery (054): mode, dibit age at delivery (p50/p90/p99/max + histogram), production-clock uncertainty, resyncs, traffic air-time epoch cuts/splits/discards + recent cuts.",
    },
    EndpointDoc {
        method: "POST",
        path: "/api/dibit_delivery",
        params: "?mode=airtime|poll|legacy&ring=control|traffic&poll_ms=N&settle_dibits=N&reset=1",
        description: "Runtime switch of the dibit readers (054) for bench A/B: airtime (default), poll (low latency, live gating), legacy (pre-054 3.41 s sub-buffers on IRQ).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/dibit_dump",
        params: "",
        description: "Sample the C4FM dibit ring and return histogram + raw DUID hits for inspection.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/encrypted_tgs",
        params: "",
        description: "Read the sticky encrypted-TG history set (TGs ever seen encrypted).",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/encrypted_tgs",
        params: "body=JSON [tg, tg, ...]",
        description: "Overwrite the encrypted-TG blocklist. Useful for manual curation.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/endpoints",
        params: "",
        description: "This catalogue. Self-describing list of every /api/* route.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/grants",
        params: "",
        description: "Active voice-channel grants (one entry per TG currently on a traffic channel).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/hdl_lsm",
        params: "",
        description: "PL HDL LSM chain runtime snapshot: NID events, NAC histogram, PLL/sync debug taps.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/imbe_dump",
        params: "",
        description: "Dump recent IMBE frame batches for offline vocoder cross-check.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/irq_stats",
        params: "",
        description: "Per-source IRQ counters (dibit / iq / lsm_dibit / traffic DMAs).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/sites",
        params: "",
        description: "List known site baselines (Clay / Duval / ...) + active flag.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/sites/{name}",
        params: "",
        description: "Full site detail (NAC, WACN, IDEN bands, CC, alt CCs, traffic_freqs_hz, cc_position).",
    },
    EndpointDoc {
        method: "POST",
        path: "/api/site",
        params: "?name=<site>&no_apply=<bool>",
        description: "Switch active site. Updates AppState + persists boot default. Caller follows up with /api/preset to apply LO snap.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/log",
        params: "?since=<seq>&limit=<n>&category=<name>",
        description: "Event log ring. Monotonic seq for incremental tail reads.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/control_dibit_capture",
        params: "",
        description: "Rolling snapshot of the control LSM decoder's recent dibits (post-demod, up to 2048).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/control_dibit_capture_aligned",
        params: "",
        description: "Sync-aligned one-shot dibit + decoded-frame trace for offline pipeline cross-check.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/control_iq_dump",
        params: "?seconds=N (1..60, default 5)",
        description: "Post-DDC complex IQ from the control chain as a WAV file (stereo i16 @ 62.5 kSPS, I=L / Q=R). SDRTrunk-ingestible.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/control_lsm_control",
        params: "?lsm_enable=0|1&dma_enable=0|1&dc_block=0|1",
        description: "Runtime read/write of the control-chain lsm_control register bits.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/control_lsm_dibit_dump",
        params: "",
        description: "Histogram of the control-chain LSM demod dibit ring (not C4FM).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic_dibit_capture",
        params: "",
        description: "Traffic-chain twin of /api/control_dibit_capture. Rolling dibit snapshot.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic_dibit_capture_aligned",
        params: "",
        description: "Traffic-chain twin of /api/control_dibit_capture_aligned. Arms next sync hit.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic_iq_dump",
        params: "?seconds=N (1..60, default 5)",
        description: "Traffic-chain twin of /api/control_iq_dump. Post-DDC IQ WAV, centered on the follower's current NCO offset.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic_lsm_control",
        params: "?dc_block=0|1&agc=0|1",
        description: "Runtime read/write of the traffic-chain lsm_control register bits.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic_lsm_dibit_dump",
        params: "",
        description: "Histogram of the traffic-chain LSM demod dibit ring.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/rx_gain",
        params: "?db=<i32>&mode=manual|slow_attack|fast_attack|hybrid",
        description: "Read or set AD9361 RX hardwaregain (range -3..76) and AGC mode. mode applied before db if both present.",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/rx_gain",
        params: "?db=<i32>&mode=manual|slow_attack|fast_attack|hybrid",
        description: "PUT twin for /api/rx_gain — same semantics as the GET form.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/grant_map",
        params: "",
        description: "Accumulated grant-frequency map (tg, freq) with counts + last-seen.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/modulation",
        params: "",
        description: "Read current P25 modulation (C4FM/LSM/Auto) + per-decoder NID-valid rates.",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/modulation",
        params: "?set=c4fm|lsm|auto",
        description: "Override or release modulation selection. Auto picks whichever decoder has more valid NIDs.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/monitor",
        params: "",
        description: "Read the TG monitor list (when non-empty, only listed TGs get followed).",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/monitor",
        params: "?add=<tg>&remove=<tg>",
        description: "Add/remove a TG from the monitor list. Empty list = newest-grant-wins.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/nid_capture",
        params: "",
        description: "Batched NID capture for t-sweep analysis by tools/p25_nid_analyze.py.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/recent_tsbks",
        params: "",
        description: "Last ~50 decoded TSBKs with summaries for the activity feed.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/recordings",
        params: "",
        description: "Ring of recent call recordings: id, TG, started_unix_ms, duration_ms, size_bytes.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/recordings/{id}",
        params: "path id, trailing .wav optional",
        description: "Download a recording as WAV (8 kHz 16-bit mono).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/recordings/{id}/sync_trace",
        params: "path id",
        description: "Per-NID PLL/AGC/sync register samples captured during the call. Diagnostic for mid-call sync loss.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/freq_health",
        params: "",
        description: "Per-channel quality aggregator over grant_decode_stats + recordings rings: drop rate, silent rate, first_imbe_ms quantiles, speakers seen.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/presets",
        params: "",
        description: "List every DDC preset (name, sample rate, RF BW, NCO window) and the current live preset.",
    },
    EndpointDoc {
        method: "POST",
        path: "/api/preset",
        params: "JSON: {preset, center_freq_hz?, gain_mode?, gain_db?}",
        description: "Apply a DDC preset. Slow path: AD9361 resettle + FIR coefficient reload.",
    },
    EndpointDoc {
        method: "POST",
        path: "/api/tune",
        params: "JSON: {radio_freq_hz, center_mode?}",
        description: "Scanner-style retune. Auto moves LO only when window exceeded; Lock refuses LO moves (409).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/constellation",
        params: "?chain=control|traffic",
        description: "Post-PLL soft-symbol (I, Q) scatter via the software LSM demod pipeline.",
    },
    EndpointDoc {
        method: "POST",
        path: "/api/set_time",
        params: "?unix_ms=<epoch_ms>",
        description: "Force the wall clock from a browser-pushed value. Fallback for boards without NTP reachability.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/spectrum",
        params: "?chain=control|traffic&fft=<1024|2048|4096|8192|16384>&averages=<N>",
        description: "FFT over post-DDC IQ ring, mag_db array fftshifted. Averages N non-overlapping segments for noise-floor suppression.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/stats",
        params: "",
        description: "Decoder stats + AD9361 readback (gain/RSSI/rx_lo/rf_bandwidth/ddc offset/uptime).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/sync_tune",
        params: "?side=control|traffic",
        description: "Read the per-decoder runtime sync threshold.",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/sync_tune",
        params: "?side=control|traffic&threshold=<0..24>|reset",
        description: "Override the sync-detector Hamming-distance threshold at runtime.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/sys_health",
        params: "",
        description: "Process + kernel health: loadavg, RSS, thread count, free memory. Cheap to poll.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/ps_cores",
        params: "?interval_ms=<50..2000>&top_n=<1..256>",
        description: "Per-core CPU% + per-thread CPU% over a short interval (default 250 ms). Top threads by busy%.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/system",
        params: "",
        description: "System identity: WACN/NAC/RFSS/site, build tag, control channel.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic",
        params: "?follower=on|off&reset_stats=1&retune_hz=<i64>&demod_enable=0|1",
        description: "Traffic-follower state + manual debug knobs (retune_hz routes through full chain).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/tsbk_opcodes",
        params: "",
        description: "Histogram of TSBK opcodes observed. Labels match SDRTrunk OSP opcode names.",
    },
    EndpointDoc {
        method: "GET",
        path: "/ws/audio",
        params: "",
        description: "WebSocket binary stream of AudioChunk payloads (used by dashboard player).",
    },
    EndpointDoc {
        method: "GET",
        path: "/ws/events",
        params: "",
        description: "WebSocket text stream of decoder + traffic events as JSON lines.",
    },
    EndpointDoc {
        method: "GET",
        path: "/ws/iq",
        params: "?chain=control|traffic&source=post_ddc|post_lsm",
        description: "WebSocket binary stream of IQ sub-buffers (32 KB each, i16 LE interleaved). source=post_ddc (default, 62.5 kSPS unfiltered) or post_lsm (31.25 kSPS, matched-filter output — the eye-plot source). Hello text frame on connect.",
    },
];


pub async fn get_endpoints(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let items: Vec<_> = ENDPOINT_CATALOGUE
        .iter()
        .map(|e| {
            serde_json::json!({
                "method":      e.method,
                "path":        e.path,
                "params":      e.params,
                "description": e.description,
            })
        })
        .collect();
    Json(serde_json::json!({
        "count": items.len(),
        "items": items,
    }))
}

// ── REST Handlers ──────────────────────────────────────────────────────


pub async fn get_system(State(state): State<Arc<AppState>>) -> Json<SystemInfo> {
    // 2026-04-16 modulation selector: picks LSM (simulcast) or
    // C4FM (FDMA) based on AppState.active_modulation, auto-detected
    // by the background task in main.rs that watches nid_decoded_ok
    // delta across both decoders.
    let dec = state.active_control_decoder().read().await;
    let s = &dec.system;
    let system_clock_str = s.last_sync_clock.map(
        |(y, mo, d, h, mn, locked)| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02} {}",
                y, mo, d, h, mn,
                if locked { "LOCKED" } else { "UNLOCKED" }
            )
        },
    );
    Json(SystemInfo {
        nac: s.nac.map(|n| format!("{}", n)),
        wacn: s.wacn.map(|w| format!("{:05X}", w)),
        system_id: s.system_id.map(|v| format!("{:03X}", v)),
        rfss_id: s.rfss_id,
        site_id: s.site_id,
        lra: s.lra,
        control_channel: s.control_channel.map(|c| format!("{}", c)),
        secondary_cch_a: s.secondary_cch_a.map(|c| format!("{}", c)),
        secondary_cch_b: s.secondary_cch_b.map(|c| format!("{}", c)),
        sndcp_downlink_channel: s.sndcp_downlink_channel.map(|c| format!("{}", c)),
        sndcp_uplink_channel: s.sndcp_uplink_channel.map(|c| format!("{}", c)),
        system_clock: system_clock_str,
        build: Some(crate::BUILD_TAG.to_string()),
        phase: Some(
            if s.has_tdma_band { "P25 P1+P2" } else { "P25 P1" }.into(),
        ),
    })
}

/// `GET /api/sys_health` — process + kernel health snapshot.
///
/// Returns load averages, daemon RSS, thread count, and a rough
/// free-memory figure. Cheap to poll (all values come from
/// `/proc/*` reads in the same request, no IIO or FPGA I/O). The
/// goal is to let an Android-app monitor (or any headless consumer)
/// distinguish "board is alive but starved for CPU" from "board is
/// alive and healthy" without needing SSH access.
///
/// Recommended by `PERFORMANCE_ANALYSIS_2026_04_17 §5.3 / §6.4`.
#[cfg(target_os = "linux")]
pub async fn get_sys_health(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let load = read_loadavg();
    let (rss_kib, threads) = read_self_status();
    let (mem_total_kib, mem_available_kib) = read_meminfo();
    let uptime_secs = state.boot_instant.elapsed().as_secs();

    // Disk usage for the two writable filesystems wideband / forensics
    // captures land on. /tmp is tmpfs (~492 MB on this Buildroot); SD
    // is the wideband + forensics destination after the 2026-05-03
    // redirect. Captures abort with ENOSPC if either fills, so the
    // dashboard / Android monitor needs visibility before that point.
    let tmp = fs_usage("/tmp");
    let sd  = fs_usage("/mnt/sd");

    Json(serde_json::json!({
        "uptime_secs":        uptime_secs,
        "loadavg_1":          load.0,
        "loadavg_5":          load.1,
        "loadavg_15":         load.2,
        "daemon_rss_kib":     rss_kib,
        "daemon_threads":     threads,
        "mem_total_kib":      mem_total_kib,
        "mem_available_kib":  mem_available_kib,
        "mem_available_pct":  mem_total_kib.map(|t| {
            mem_available_kib
                .map(|a| (a as f64 / t as f64) * 100.0)
                .unwrap_or(0.0)
        }),
        "disk_tmp_total_bytes":  tmp.map(|(t, _)| t),
        "disk_tmp_avail_bytes":  tmp.map(|(_, a)| a),
        "disk_tmp_avail_pct":    tmp.map(|(t, a)| {
            if t == 0 { 0.0 } else { (a as f64 / t as f64) * 100.0 }
        }),
        "disk_sd_total_bytes":   sd.map(|(t, _)| t),
        "disk_sd_avail_bytes":   sd.map(|(_, a)| a),
        "disk_sd_avail_pct":     sd.map(|(t, a)| {
            if t == 0 { 0.0 } else { (a as f64 / t as f64) * 100.0 }
        }),
        "note": "Sampled from /proc/loadavg + /proc/self/status + \
                 /proc/meminfo + statvfs(/tmp, /mnt/sd). Cheap enough \
                 to poll at 1 Hz from a mobile client; any value of \
                 null means the /proc read or statvfs call failed.",
    }))
}

/// statvfs(2) wrapper. Returns (total_bytes, available_bytes) for the
/// filesystem that contains `path`, or None on error (path missing,
/// permission denied, etc.). Bypasses /proc/mounts to avoid parsing
/// edge cases (cgroups, autofs); a single syscall per call is cheap.
#[cfg(target_os = "linux")]
fn fs_usage(path: &str) -> Option<(u64, u64)> {
    let cpath = std::ffi::CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let frsize = s.f_frsize as u64;
    Some((s.f_blocks as u64 * frsize, s.f_bavail as u64 * frsize))
}

#[cfg(not(target_os = "linux"))]
pub async fn get_sys_health(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "error": "sys_health requires /proc (target_os=linux)",
    }))
}

/// `GET /api/pipeline`
///
/// Stage-by-stage snapshot of every queue, buffer, and health gauge
/// from RF front-end to audio broadcast, organized by pipeline stage
/// so an operator can scan one endpoint and see whether ANY stage is
/// unhealthy. Atomic reads only — cheap enough to poll at 1 Hz.
///
/// Distinct from `/api/traffic` (oriented around traffic-chain
/// state) and `/api/stats` (oriented around RF config) — this one
/// is for "show me the whole chain at once".
#[cfg(target_os = "linux")]
pub async fn get_pipeline(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;
    let f = &state.imbe_forwarder;

    // RF front-end — live reads via the AD9361 driver, same path
    // /api/stats uses. All return Option so a transient iiod failure
    // shows up as null rather than cratering the whole endpoint.
    let rf_json = {
        let rx_lo = state.ad9361.get_rx_lo_frequency().await.ok();
        let bw    = state.ad9361.get_rx_rf_bandwidth().await.ok();
        let sr    = state.ad9361.get_sampling_frequency().await.ok();
        let rssi  = state.ad9361.get_rx_rssi().await.ok();
        let gain  = state.ad9361.get_rx_gain().await.ok();
        serde_json::json!({
            "rx_lo_hz":           rx_lo,
            "rf_bandwidth_hz":    bw,
            "sample_rate_hz":     sr,
            "rx_rssi_db":         rssi,
            "rx_gain_db":         gain,
            "radio_freq_hz":      state.current_control_freq
                                     .load(Ordering::Relaxed),
        })
    };

    // PPM state.
    let ppm_json = {
        let shift = state.current_lo_shift_hz.load(Ordering::Relaxed);
        let rx_lo = state.current_rx_lo.load(Ordering::Relaxed) as f64;
        let ppm = if rx_lo > 0.0 {
            -(shift as f64) / (rx_lo * 1e-6)
        } else { 0.0 };
        serde_json::json!({
            "lo_shift_hz":        shift,
            "lo_ppm":             ppm,
            "last_cal_unix_secs": state.last_ppm_cal_unix_secs
                .load(Ordering::Relaxed),
        })
    };

    // HDL chains — reads the live registers once, same source the
    // dedicated /api/hdl_lsm + /api/traffic endpoints use.
    let hdl_json = {
        let core = state.ip_core.lock().await;
        let cs = core.lsm_status();
        let c_drop = core.lsm_drop_count();
        let (c_pll, c_sp) = core.lsm_debug();
        let (c_g, c_m) = core.lsm_agc_debug();
        let ts = core.traffic_lsm_status();
        let t_drop = core.traffic_lsm_drop_count();
        let (t_pll, t_sp) = core.traffic_lsm_debug();
        let (t_g, t_m) = core.traffic_lsm_agc_debug();
        let c_agc_prod = (c_g as f64 / 128.0) * (c_m as f64 / 32768.0);
        let t_agc_prod = (t_g as f64 / 128.0) * (t_m as f64 / 32768.0);
        serde_json::json!({
            "control": {
                "sync_distance":   cs.sync_distance,
                "nid_valid":       cs.nid_valid,
                "dibit_overflow":  cs.dibit_overflow,
                "drop_count":      c_drop,
                "pll_dbg":         c_pll,
                "sample_point":    c_sp,
                "agc_product":     c_agc_prod,
            },
            "traffic": {
                "sync_distance":   ts.sync_distance,
                "nid_valid":       ts.nid_valid,
                "dibit_overflow":  ts.dibit_overflow,
                "drop_count":      t_drop,
                "pll_dbg":         t_pll,
                "sample_point":    t_sp,
                "agc_product":     t_agc_prod,
            },
        })
    };

    // Framers — control + traffic decoder stats.
    let framers_json = {
        let c_dec = state.lsm_decoder.read().await;
        let t_dec = state.traffic_lsm_decoder.read().await;
        serde_json::json!({
            "control": {
                "nid_attempts":          c_dec.nid_attempts,
                "nid_decoded_ok":        c_dec.nid_decoded_ok,
                "nid_decode_failures":   c_dec.nid_decode_failures,
                "nid_entropy_rejected":  c_dec.nid_entropy_rejected,
                "nid_nac_mismatch":      c_dec.nid_nac_mismatch,
                "sync_hits":             c_dec.sync_hits(),
                "sync_near_misses":      c_dec.sync_near_misses(),
                "tsbk_crc_ok":           c_dec.tsbk_crc_ok,
                "tsbk_crc_failures":     c_dec.tsbk_crc_failures,
            },
            "traffic": {
                "nid_attempts":          t_dec.nid_attempts,
                "nid_decoded_ok":        t_dec.nid_decoded_ok,
                "nid_decode_failures":   t_dec.nid_decode_failures,
                "nid_entropy_rejected":  t_dec.nid_entropy_rejected,
                "nid_nac_mismatch":      t_dec.nid_nac_mismatch,
                "sync_hits":             t_dec.sync_hits(),
                "sync_near_misses":      t_dec.sync_near_misses(),
            },
        })
    };

    // IMBE mpsc queue — live depth, capacity, high-water, throughput.
    let submitted = f.frames_submitted.load(Ordering::Relaxed);
    let consumed = f.frames_consumed.load(Ordering::Relaxed);
    let dropped = f.imbe_frames_dropped.load(Ordering::Relaxed);
    let imbe_queue_json = serde_json::json!({
        "depth":             f.queue_depth_now(),
        "capacity":          f.queue_capacity_max(),
        "high_water":        f.queue_high_water.load(Ordering::Relaxed),
        "frames_submitted":  submitted,
        "frames_consumed":   consumed,
        "frames_dropped":    dropped,
        "drop_events":       dropped / 9,
        "last_batch_tg":     f.last_batch_tg.load(Ordering::Relaxed),
    });

    let vocoder_json = serde_json::json!({
        "frames_extracted":  f.imbe_frames_extracted.load(Ordering::Relaxed),
        "pcm_samples":       f.vocoder_pcm_produced.load(Ordering::Relaxed),
        "errors_over_4bit":  f.vocoder_errors.load(Ordering::Relaxed),
        "silent_observed":   f.vocoder_frames_silent_observed
                              .load(Ordering::Relaxed),
        "encrypted_skipped": f.vocoder_frames_encrypted.load(Ordering::Relaxed),
    });

    let audio_bcast_json = serde_json::json!({
        "subscribers":       state.audio_tx.receiver_count(),
        "lag_total":         state.audio_ws_lag_total.load(Ordering::Relaxed),
    });

    let recorder_json = {
        let d = &state.recorder_diag;
        let store = state.recordings.lock().await;
        serde_json::json!({
            "store_count":        store.len(),
            "store_max":          crate::audio::recorder::MAX_RECORDINGS,
            "boundaries_hdu":     d.boundaries_hdu.load(Ordering::Relaxed),
            "src_stamps_applied": d.source_stamps_applied.load(Ordering::Relaxed),
            "src_stamps_lost":    d.source_stamps_lost_no_active
                                   .load(Ordering::Relaxed),
            "boundary_lag":       d.boundary_lag_events.load(Ordering::Relaxed),
        })
    };

    let event_log_json = serde_json::json!({
        "depth":     state.event_log.len(),
        "capacity":  state.event_log.capacity(),
        "last_seq":  state.event_log.last_seq(),
    });

    let grant_stats_json = {
        let r = state.grant_decode_stats.lock()
            .map(|r| r.len()).unwrap_or(0);
        serde_json::json!({
            "completed_grants_buffered": r,
            "capacity":                  20,
        })
    };

    // 2026-05-03 seeding bake: surface the current ConvergedSeeds so
    // the dashboard can show seed status without subscribing to a
    // separate stream. `armed = false` means the heartbeat is still
    // warming up; `armed = true` means subsequent retunes will write
    // these seeds before pulsing reset.
    let seeds_json = {
        let slot = state.converged_seeds.read().await;
        match slot.as_ref() {
            Some(s) => serde_json::json!({
                "armed":                true,
                "agc_seed_q9_11":       s.agc_seed,
                "pll_seed_q2_13":       s.pll_seed,
                "timing_seed_q5_12":    s.timing_seed,
                "samples_in_window":    s.samples_in_window,
                "total_clean_samples":  s.total_clean_samples,
                "age_ms":
                    s.last_updated_at.elapsed().as_millis() as u64,
            }),
            None => serde_json::json!({
                "armed":                false,
                "reason":               "heartbeat warmup",
            }),
        }
    };

    Json(serde_json::json!({
        "build_tag":         crate::BUILD_TAG,
        "uptime_secs":       state.boot_instant.elapsed().as_secs(),
        "rf":                rf_json,
        "ppm":               ppm_json,
        "hdl_chains":        hdl_json,
        "framers":           framers_json,
        "imbe_queue":        imbe_queue_json,
        "vocoder":           vocoder_json,
        "audio_broadcast":   audio_bcast_json,
        "recorder":          recorder_json,
        "event_log":         event_log_json,
        "grant_stats":       grant_stats_json,
        "converged_seeds":   seeds_json,
    }))
}

#[cfg(not(target_os = "linux"))]
pub async fn get_pipeline(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "error": "pipeline requires hardware access (target_os=linux)",
    }))
}

/// `GET /api/freq_health`
///
/// Per-channel quality aggregator. Walks the grant_decode_stats ring
/// (master) + recordings ring (supplemental), groups by
/// (channel, freq_hz), reports per-channel call count, IMBE drop
/// rate, silent-frame rate, first_imbe_ms quantiles, and
/// recency. Designed to answer the operator's question: "are some
/// traffic channels working better than others?"
///
/// No new server-side state — both rings already carry per-call
/// freq + channel (added 2026-04-24-per-call-freq-tag). Entries
/// without a channel/freq are aggregated under a synthetic
/// `(unknown)` bucket so pre-tag data still shows up.
pub async fn get_freq_health(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    use std::collections::BTreeMap;

    // Per-bucket accumulator. Bucket key is the channel string when
    // present, otherwise the freq_hz formatted, otherwise "(unknown)".
    #[derive(Default)]
    struct Acc {
        channel: Option<String>,
        freq_hz: Option<u64>,
        calls: u64,
        ldu_total: u64,
        imbe_extracted: u64,
        imbe_dropped: u64,
        vocoder_pcm_samples: u64,
        vocoder_silent: u64,
        vocoder_errors: u64,
        encrypted_calls: u64,
        first_imbe_ms: Vec<u64>,
        duration_ms: Vec<u64>,
        sources: std::collections::HashSet<u32>,
        last_call_unix_ms: u64,
    }

    let mut buckets: BTreeMap<String, Acc> = BTreeMap::new();
    let bucket_key = |ch: &Option<String>, fh: Option<u64>| -> String {
        if let Some(c) = ch.as_ref() { return c.clone(); }
        if let Some(f) = fh { return format!("freq:{}", f); }
        "(unknown)".to_string()
    };

    // Pass 1: grants — the master. Each entry = one completed grant
    // with per-call deltas already computed.
    if let Ok(ring) = state.grant_decode_stats.lock() {
        for g in ring.iter() {
            let key = bucket_key(&g.channel, g.freq_hz);
            let acc = buckets.entry(key).or_default();
            if acc.channel.is_none() && g.channel.is_some() {
                acc.channel = g.channel.clone();
            }
            if acc.freq_hz.is_none() && g.freq_hz.is_some() {
                acc.freq_hz = g.freq_hz;
            }
            acc.calls += 1;
            acc.ldu_total += g.ldu1_count + g.ldu2_count;
            acc.imbe_extracted += g.imbe_extracted;
            acc.imbe_dropped += g.imbe_dropped;
            acc.vocoder_pcm_samples += g.vocoder_pcm_samples;
            acc.vocoder_silent += g.vocoder_silent;
            acc.vocoder_errors += g.vocoder_errors;
            if g.encrypted { acc.encrypted_calls += 1; }
            if let Some(f) = g.first_imbe_ms { acc.first_imbe_ms.push(f); }
            if g.duration_ms > 0 { acc.duration_ms.push(g.duration_ms); }
            if let Some(s) = g.source { acc.sources.insert(s); }
            if g.ended_unix_ms > acc.last_call_unix_ms {
                acc.last_call_unix_ms = g.ended_unix_ms;
            }
        }
    }

    // Quantile helper. Returns (min, median, max) over a slice.
    fn quantiles(v: &mut Vec<u64>) -> Option<(u64, u64, u64)> {
        if v.is_empty() { return None; }
        v.sort_unstable();
        let n = v.len();
        Some((v[0], v[n/2], v[n-1]))
    }

    // Vocoder-frame count. PCM samples / 160 = vocoder frames
    // (160 samples per IMBE frame at 8 kHz).
    let voc_frames = |pcm_samples: u64| pcm_samples / 160;

    // Build per-channel JSON.
    let mut channels: Vec<serde_json::Value> = Vec::new();
    for (key, mut acc) in buckets {
        let drop_rate = if acc.imbe_extracted + acc.imbe_dropped > 0 {
            acc.imbe_dropped as f64
                / (acc.imbe_extracted + acc.imbe_dropped) as f64
        } else { 0.0 };
        let voc_total = voc_frames(acc.vocoder_pcm_samples);
        let silent_rate = if voc_total > 0 {
            acc.vocoder_silent as f64 / voc_total as f64
        } else { 0.0 };
        let first_q = quantiles(&mut acc.first_imbe_ms);
        let dur_sum: u64 = acc.duration_ms.iter().sum();
        let dur_q = quantiles(&mut acc.duration_ms);
        let freq_mhz = acc.freq_hz.map(|f| (f as f64) / 1.0e6);

        channels.push(serde_json::json!({
            "key":               key,
            "channel":           acc.channel,
            "freq_hz":           acc.freq_hz,
            "freq_mhz":          freq_mhz,
            "calls":             acc.calls,
            "ldu_total":         acc.ldu_total,
            "imbe_extracted":    acc.imbe_extracted,
            "imbe_dropped":      acc.imbe_dropped,
            "imbe_drop_rate":    drop_rate,
            "vocoder_silent":    acc.vocoder_silent,
            "vocoder_silent_rate": silent_rate,
            "vocoder_errors":    acc.vocoder_errors,
            "encrypted_calls":   acc.encrypted_calls,
            "first_imbe_ms":     first_q.map(|(min,med,max)|
                serde_json::json!({"min": min, "median": med, "max": max})),
            "duration_ms":       dur_q.map(|(_min,med,_max)|
                serde_json::json!({"sum": dur_sum, "median": med})),
            "speakers_seen":     acc.sources.len() as u64,
            "last_call_unix_ms": if acc.last_call_unix_ms > 0 {
                Some(acc.last_call_unix_ms) } else { None },
        }));
    }

    // Quality score for ranking: 1.0 - drop_rate - silent_rate.
    // High score = clean channel; low score = drops or silent.
    // Skip channels with <2 calls (insufficient sample size).
    let scored: Vec<(String, f64)> = channels.iter()
        .filter_map(|c| {
            let calls = c["calls"].as_u64().unwrap_or(0);
            if calls < 2 { return None; }
            let dr = c["imbe_drop_rate"].as_f64().unwrap_or(0.0);
            let sr = c["vocoder_silent_rate"].as_f64().unwrap_or(0.0);
            let key = c["key"].as_str().unwrap_or("?").to_string();
            Some((key, 1.0 - dr - sr))
        }).collect();
    let best = scored.iter()
        .max_by(|a,b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let worst = scored.iter()
        .min_by(|a,b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let calls_total: u64 = channels.iter()
        .map(|c| c["calls"].as_u64().unwrap_or(0)).sum();

    // Newest-first by last_call_unix_ms; channels with no recency
    // sink to the bottom.
    channels.sort_by(|a, b| {
        let ta = a["last_call_unix_ms"].as_u64().unwrap_or(0);
        let tb = b["last_call_unix_ms"].as_u64().unwrap_or(0);
        tb.cmp(&ta)
    });

    Json(serde_json::json!({
        "channels": channels,
        "summary": {
            "channels_seen": scored.len() as u64,
            "calls_total":   calls_total,
            "best_channel":  best.map(|(k,s)|
                serde_json::json!({"channel": k, "score": s})),
            "worst_channel": worst.map(|(k,s)|
                serde_json::json!({"channel": k, "score": s})),
        }
    }))
}

/// Parse `/proc/loadavg`. Returns (1-min, 5-min, 15-min) averages, or
/// zeros if the read or parse fails.
#[cfg(target_os = "linux")]
fn read_loadavg() -> (f64, f64, f64) {
    let Ok(s) = std::fs::read_to_string("/proc/loadavg") else {
        return (0.0, 0.0, 0.0);
    };
    let mut parts = s.split_ascii_whitespace();
    let a = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let b = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let c = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    (a, b, c)
}

/// Parse `/proc/self/status` for `VmRSS` and `Threads`.
#[cfg(target_os = "linux")]
fn read_self_status() -> (Option<u64>, Option<u64>) {
    let Ok(s) = std::fs::read_to_string("/proc/self/status") else {
        return (None, None);
    };
    let mut rss = None;
    let mut threads = None;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Format: "VmRSS:\t   12345 kB"
            rss = rest.split_ascii_whitespace().next().and_then(|v| v.parse().ok());
        } else if let Some(rest) = line.strip_prefix("Threads:") {
            threads = rest.split_ascii_whitespace().next().and_then(|v| v.parse().ok());
        }
    }
    (rss, threads)
}

/// `GET /api/ps_cores` — live per-core CPU + per-thread CPU usage.
///
/// Takes two snapshots of `/proc/stat` and `/proc/self/task/*/stat`
/// 250 ms apart and returns deltas as percentages. No state plumbing,
/// each request is self-contained. Cost: ~250 ms wall clock + two
/// /proc walks (sub-ms on a Z7020).
///
/// Per-core percentages sum to 100 (user + system + idle + iowait +
/// irq + softirq + steal). Per-thread `cpu_pct` is "% of one core" —
/// a thread pinning a core reads 100, a thread split across both
/// cores at 50 % each reads 100 (sum). Top threads by CPU% are
/// returned, capped at `top_n` (default 16).
///
/// Designed for the dashboard System Health panel and for diagnosing
/// realtime audio jitter / starvation against the FFT, vocoder, and
/// WS broadcast tasks.
#[cfg(target_os = "linux")]
pub async fn get_ps_cores(
    State(_state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let interval_ms: u64 = params
        .get("interval_ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(250)
        .clamp(50, 2000);
    let top_n: usize = params
        .get("top_n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(16)
        .min(256);

    let snap_a = read_ps_snapshot();
    tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
    let snap_b = read_ps_snapshot();

    let elapsed_s = (snap_b.wall_ms - snap_a.wall_ms) as f64 / 1000.0;
    let user_hz = snap_a.user_hz.max(1) as f64;

    // 10-second rolling window. The 250 ms two-shot above produces a
    // binomial 0/100 % reading on bursty threads (e.g. p25-vocoder
    // releases its 5 ms work batch on LDU boundaries — caught at
    // ~2 % when sampled, missed entirely otherwise). The cumulative
    // value is a steady measure of the thread's actual budget share
    // and is what the dashboard wants for `# of one core` columns.
    let cpu_pct_10s_map = update_and_compute_10s_history(&snap_b, user_hz);

    let cpus: Vec<serde_json::Value> = snap_a
        .cpus
        .iter()
        .zip(snap_b.cpus.iter())
        .map(|(a, b)| {
            let total = (b.total() - a.total()).max(1) as f64;
            let user = (b.user.saturating_sub(a.user)) as f64 / total * 100.0;
            let nice = (b.nice.saturating_sub(a.nice)) as f64 / total * 100.0;
            let system = (b.system.saturating_sub(a.system)) as f64 / total * 100.0;
            let idle = (b.idle.saturating_sub(a.idle)) as f64 / total * 100.0;
            let iowait = (b.iowait.saturating_sub(a.iowait)) as f64 / total * 100.0;
            let irq = (b.irq.saturating_sub(a.irq)) as f64 / total * 100.0;
            let softirq = (b.softirq.saturating_sub(a.softirq)) as f64 / total * 100.0;
            let steal = (b.steal.saturating_sub(a.steal)) as f64 / total * 100.0;
            let busy = 100.0 - idle - iowait;
            serde_json::json!({
                "id":          b.id,
                "user_pct":    round1(user + nice),
                "system_pct":  round1(system),
                "idle_pct":    round1(idle),
                "iowait_pct":  round1(iowait),
                "irq_pct":     round1(irq + softirq),
                "steal_pct":   round1(steal),
                "busy_pct":    round1(busy),
            })
        })
        .collect();

    // Per-thread deltas. Build map from snap_a TIDs and look up in snap_b
    // so threads that vanished during the sample drop out cleanly.
    let mut threads: Vec<serde_json::Value> = Vec::with_capacity(snap_a.threads.len());
    for ta in &snap_a.threads {
        let Some(tb) = snap_b.threads.iter().find(|t| t.tid == ta.tid) else {
            continue;
        };
        let utime_d = tb.utime.saturating_sub(ta.utime);
        let stime_d = tb.stime.saturating_sub(ta.stime);
        let jiffies = (utime_d + stime_d) as f64;
        // Wall-clock-normalised: 100% = one core fully busy for `elapsed_s`.
        let cpu_pct = if elapsed_s > 0.0 {
            (jiffies / (elapsed_s * user_hz)) * 100.0
        } else {
            0.0
        };
        let cpu_pct_10s = cpu_pct_10s_map
            .get(&tb.tid)
            .copied()
            // No 10 s history yet (first-call cold start): fall back
            // to the 250 ms reading so the column is never blank.
            .unwrap_or(cpu_pct);
        threads.push(serde_json::json!({
            "tid":         tb.tid,
            "name":        tb.comm,
            "cpu_pct":     round1(cpu_pct),
            "cpu_pct_10s": round1(cpu_pct_10s),
            "state":       tb.state,
            "vol_ctxsw_delta":   tb.vol_ctxsw.saturating_sub(ta.vol_ctxsw),
            "invol_ctxsw_delta": tb.invol_ctxsw.saturating_sub(ta.invol_ctxsw),
        }));
    }
    // Sort descending by max(cpu_pct, cpu_pct_10s) so bursty threads
    // whose 250 ms sample landed in an idle gap (e.g. p25-vocoder
    // releasing on LDU boundaries) still bubble to the top of the
    // panel based on their steady-state share.
    threads.sort_by(|a, b| {
        let am = a["cpu_pct"].as_f64().unwrap_or(0.0)
            .max(a["cpu_pct_10s"].as_f64().unwrap_or(0.0));
        let bm = b["cpu_pct"].as_f64().unwrap_or(0.0)
            .max(b["cpu_pct_10s"].as_f64().unwrap_or(0.0));
        bm.partial_cmp(&am).unwrap_or(std::cmp::Ordering::Equal)
    });
    let total_threads = threads.len();
    threads.truncate(top_n);

    Json(serde_json::json!({
        "interval_ms":       (snap_b.wall_ms - snap_a.wall_ms) as u64,
        "user_hz":           snap_a.user_hz,
        "num_cpus":          cpus.len(),
        "cpus":              cpus,
        "threads":           threads,
        "total_threads":     total_threads,
        "loadavg_1":         read_loadavg().0,
        "note":              "Two /proc reads `interval_ms` apart, deltas \
                              normalised to wall-clock. cpu_pct is %-of-one-core. \
                              cpu_pct_10s is the same metric but averaged over \
                              the last ~10 s of history (steady-state reading; \
                              cpu_pct itself is a 250 ms binomial). \
                              busy_pct = 100 - idle - iowait. Truncated to \
                              top_n (default 16) by cpu_pct desc.",
    }))
}

#[cfg(not(target_os = "linux"))]
pub async fn get_ps_cores(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "error": "ps_cores requires /proc (target_os=linux)",
    }))
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct PsCoreSnap {
    wall_ms: u128,
    user_hz: u64,
    cpus: Vec<CpuStat>,
    threads: Vec<ThreadStat>,
}

#[cfg(target_os = "linux")]
#[derive(Default, Clone, Copy)]
struct CpuStat {
    id: u32,
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

#[cfg(target_os = "linux")]
impl CpuStat {
    fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }
}

#[cfg(target_os = "linux")]
#[derive(Default, Clone)]
struct ThreadStat {
    tid: u32,
    comm: String,
    state: String,
    utime: u64,
    stime: u64,
    vol_ctxsw: u64,
    invol_ctxsw: u64,
}

#[cfg(target_os = "linux")]
fn read_ps_snapshot() -> PsCoreSnap {
    let wall_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let user_hz = read_user_hz();
    let cpus = read_proc_stat_cpus();
    let threads = read_self_task_stats();
    PsCoreSnap { wall_ms, user_hz, cpus, threads }
}

#[cfg(target_os = "linux")]
fn read_user_hz() -> u64 {
    // sysconf(_SC_CLK_TCK). Almost always 100 on Linux/ARM. We avoid
    // libc here and just hardcode 100 — if a kernel ever ships with a
    // different value the resulting cpu_pct will be off by a constant
    // factor that's still useful for relative comparison.
    100
}

#[cfg(target_os = "linux")]
fn read_proc_stat_cpus() -> Vec<CpuStat> {
    let Ok(s) = std::fs::read_to_string("/proc/stat") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in s.lines() {
        // Skip the aggregate "cpu " line (starts with "cpu " not "cpuN ").
        if !line.starts_with("cpu") {
            break;
        }
        let mut parts = line.split_ascii_whitespace();
        let head = parts.next().unwrap_or("");
        if head == "cpu" || !head.starts_with("cpu") {
            continue;
        }
        let Some(id) = head[3..].parse::<u32>().ok() else {
            continue;
        };
        let f: Vec<u64> = parts.filter_map(|v| v.parse().ok()).collect();
        if f.len() < 8 {
            continue;
        }
        out.push(CpuStat {
            id,
            user: f[0],
            nice: f[1],
            system: f[2],
            idle: f[3],
            iowait: f[4],
            irq: f[5],
            softirq: f[6],
            steal: *f.get(7).unwrap_or(&0),
        });
    }
    out
}

#[cfg(target_os = "linux")]
fn read_self_task_stats() -> Vec<ThreadStat> {
    let Ok(rd) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        let Ok(tid) = name.parse::<u32>() else { continue };
        let stat_path = entry.path().join("stat");
        let Ok(stat) = std::fs::read_to_string(&stat_path) else {
            continue;
        };
        // /proc/<pid>/stat fields after comm: state utime stime ...
        // comm may contain spaces and parens, so split on the LAST ')'.
        let Some(rp) = stat.rfind(')') else { continue };
        let comm_start = stat.find('(').map(|p| p + 1).unwrap_or(0);
        let comm = stat[comm_start..rp].to_string();
        let rest = &stat[rp + 1..];
        let parts: Vec<&str> = rest.split_ascii_whitespace().collect();
        if parts.len() < 13 {
            continue;
        }
        // After ')' the field indices are 0=state, 1=ppid, ..., 11=utime,
        // 12=stime. Reference: man 5 proc, /proc/[pid]/stat — field
        // numbers there are 1-based and counted from PID, so utime is
        // field 14 of the file but index 11 of `rest` after splitting
        // off "PID (comm) ".
        let state = parts[0].to_string();
        let utime: u64 = parts[11].parse().unwrap_or(0);
        let stime: u64 = parts[12].parse().unwrap_or(0);
        // Context-switch counters live in `status`, not `stat`. Cheap
        // enough on a 2-core ARM with ~10 daemon threads.
        let mut vol = 0u64;
        let mut invol = 0u64;
        if let Ok(status) = std::fs::read_to_string(entry.path().join("status")) {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("voluntary_ctxt_switches:") {
                    vol = rest
                        .split_ascii_whitespace()
                        .next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                } else if let Some(rest) =
                    line.strip_prefix("nonvoluntary_ctxt_switches:")
                {
                    invol = rest
                        .split_ascii_whitespace()
                        .next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                }
            }
        }
        out.push(ThreadStat {
            tid,
            comm,
            state,
            utime,
            stime,
            vol_ctxsw: vol,
            invol_ctxsw: invol,
        });
    }
    out
}

#[cfg(target_os = "linux")]
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

// ── 10 s rolling history for /api/ps_cores ─────────────────────────────
//
// Stored as a small in-process ring of past `read_ps_snapshot` results.
// On each call we:
//   1. Trim entries older than ~15 s (a little slack past the 10 s
//      window so a slow consumer can still find a usable anchor).
//   2. Push the fresh snapshot.
//   3. Pick the oldest entry whose age is ≥ ~5 s and ≤ ~12 s as the
//      anchor (target ~10 s back). On the first call there's no
//      anchor; the caller falls back to the 250 ms reading.
//   4. For each TID in the fresh snapshot, compute
//      (utime+stime - anchor.utime+stime) / (Δwall × USER_HZ) × 100.
//
// Cap the ring at 32 entries — at 1 sample per ~2 s panel cadence
// that's a minute of headroom. Each entry stores HashMap<u32, u64>;
// memory is small (a few kB).
#[cfg(target_os = "linux")]
struct PsHistEntry {
    wall_ms: u128,
    jiffies: std::collections::HashMap<u32, u64>,
}

#[cfg(target_os = "linux")]
fn ps_history()
    -> &'static std::sync::Mutex<std::collections::VecDeque<PsHistEntry>>
{
    static H: std::sync::OnceLock<
        std::sync::Mutex<std::collections::VecDeque<PsHistEntry>>,
    > = std::sync::OnceLock::new();
    H.get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
}

#[cfg(target_os = "linux")]
fn update_and_compute_10s_history(
    snap: &PsCoreSnap,
    user_hz: f64,
) -> std::collections::HashMap<u32, f64> {
    use std::collections::HashMap;
    let mut hist = ps_history().lock().unwrap();
    let now = snap.wall_ms;

    // Drop entries older than 15 s. Also drop anything in the future
    // (defensive — a wall-clock jump backwards via NTP shouldn't
    // poison the history).
    while let Some(front) = hist.front() {
        if now > front.wall_ms && now - front.wall_ms > 15_000 {
            hist.pop_front();
        } else if front.wall_ms > now {
            hist.pop_front();
        } else {
            break;
        }
    }

    // Pick the best anchor: oldest entry with age in [5s, 12s].
    // Falls back to the oldest available if nothing's that old yet.
    let mut anchor: Option<&PsHistEntry> = None;
    for e in hist.iter() {
        if now <= e.wall_ms { continue; }
        let age = now - e.wall_ms;
        if age >= 5_000 && age <= 12_000 {
            anchor = Some(e);
            break;
        }
    }
    if anchor.is_none() {
        anchor = hist.front();
    }

    let mut out = HashMap::with_capacity(snap.threads.len());
    if let Some(a) = anchor {
        let dt_s = (now.saturating_sub(a.wall_ms)) as f64 / 1000.0;
        if dt_s > 0.5 {
            for t in &snap.threads {
                let now_j = (t.utime + t.stime) as i128;
                let then_j = a.jiffies.get(&t.tid).copied().unwrap_or(0) as i128;
                let dj = (now_j - then_j).max(0) as f64;
                let pct = dj / (dt_s * user_hz) * 100.0;
                out.insert(t.tid, pct);
            }
        }
    }

    // Push current snapshot.
    let mut jiffies = HashMap::with_capacity(snap.threads.len());
    for t in &snap.threads {
        jiffies.insert(t.tid, t.utime + t.stime);
    }
    hist.push_back(PsHistEntry { wall_ms: now, jiffies });
    while hist.len() > 32 { hist.pop_front(); }

    out
}

/// Parse `/proc/meminfo` for `MemTotal` and `MemAvailable`.
#[cfg(target_os = "linux")]
fn read_meminfo() -> (Option<u64>, Option<u64>) {
    let Ok(s) = std::fs::read_to_string("/proc/meminfo") else {
        return (None, None);
    };
    let mut total = None;
    let mut avail = None;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = rest.split_ascii_whitespace().next().and_then(|v| v.parse().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail = rest.split_ascii_whitespace().next().and_then(|v| v.parse().ok());
        }
    }
    (total, avail)
}


