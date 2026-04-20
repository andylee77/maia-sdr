//! Per-talkgroup metadata: aliases, monitor list, encryption, grant map.
//!
//! Consumer orientation: "which talkgroups do I know about, and what
//! do I want to do with each?" State held here is process-lifetime
//! only — alias map, monitor list, and encryption blocklist are all
//! cleared on daemon restart (per the `API_CONSUMERS.md` contract).
//! A future persistent-store add would live behind this same API.
//!
//! `/api/grant_map` is read-only — it tallies every grant observed
//! on the control channel into a `(tg, frequency) → count` table.
//! Used by the dashboard's "frequency heatmap" and by a future
//! auto-LO-centering endpoint to pick an RX LO that keeps the most
//! active slots in-band.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
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

/// Phase 10-prep: GET /api/grant_map -- accumulated grant-frequency
/// map. Every grant on the control channel is tallied here by
/// (tg, frequency) — count, last-seen timestamp, encryption count.
///
/// Used by (a) the scanner-mode UI as a TG picker, (b) a future
/// auto-center-LO endpoint to pick an RX LO that keeps the most
/// active traffic channels in-band.
pub async fn get_grant_map(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let mgr = state.traffic_manager.lock().await;
    let rows: Vec<serde_json::Value> = mgr.grant_map.iter()
        .map(|(key, entry)| serde_json::json!({
            "tg":                key.0,
            "frequency_hz":      key.1,
            "count":             entry.count,
            "encrypted_count":   entry.encrypted_count,
            "first_seen_unix_ms": entry.first_seen_unix_ms,
            "last_seen_unix_ms":  entry.last_seen_unix_ms,
        }))
        .collect();
    let total_entries = rows.len();
    let total_grants: u64 = mgr.grant_map.values().map(|e| e.count).sum();

    // Frequency-only roll-up for LO-centering discussion.
    let mut freq_counts: std::collections::HashMap<u64, u64> =
        std::collections::HashMap::new();
    let mut freq_tgs: std::collections::HashMap<u64,
        std::collections::HashSet<u16>> =
        std::collections::HashMap::new();
    for (key, entry) in mgr.grant_map.iter() {
        *freq_counts.entry(key.1).or_insert(0) += entry.count;
        freq_tgs.entry(key.1).or_default().insert(key.0);
    }
    let mut frequencies: Vec<serde_json::Value> = freq_counts.into_iter()
        .map(|(hz, n)| serde_json::json!({
            "frequency_hz": hz,
            "count":        n,
            "distinct_tgs": freq_tgs.get(&hz).map(|s| s.len()).unwrap_or(0),
        }))
        .collect();
    frequencies.sort_by(|a, b| {
        b.get("count").and_then(|v| v.as_u64()).unwrap_or(0)
            .cmp(&a.get("count").and_then(|v| v.as_u64()).unwrap_or(0))
    });

    Json(serde_json::json!({
        "entries":       rows,
        "total_entries": total_entries,
        "total_grants":  total_grants,
        "frequencies":   frequencies,
        "note": "Accumulated since p25-httpd start. Each (tg, frequency) \
                 pair is one row with count + first/last seen. \
                 Frequencies roll-up groups by frequency only, sorted by \
                 activity — use the top entries to decide where to center \
                 the AD9361 LO so the most active slots stay in-band.",
    }))
}


pub async fn get_aliases(State(state): State<Arc<AppState>>) -> Json<AliasMap> {
    let decoder = state.decoder.read().await;
    Json(decoder.aliases.clone())
}


pub async fn put_aliases(
    State(state): State<Arc<AppState>>,
    Json(aliases): Json<AliasMap>,
) -> impl IntoResponse {
    let mut decoder = state.decoder.write().await;
    decoder.aliases = aliases;
    axum::http::StatusCode::OK
}

// ── WebSocket ──────────────────────────────────────────────────────────


/// GET /api/monitor -- return the current monitor list.
/// PUT /api/monitor -- replace the list. Body: {"talkgroups": [300, 402]}
/// GET /api/monitor?add=300 / ?remove=300 -- quick add/remove.
pub async fn get_monitor(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let mut list = state.monitor_list.write().await;

    if let Some(tg_str) = params.get("add") {
        if let Ok(tg) = tg_str.parse::<u16>() {
            list.add(tg);
        }
    }
    if let Some(tg_str) = params.get("remove") {
        if let Ok(tg) = tg_str.parse::<u16>() {
            list.remove(tg);
        }
    }

    Json(serde_json::json!({
        "talkgroups": list.list(),
    }))
}


pub async fn put_monitor(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let mut list = state.monitor_list.write().await;
    if let Some(arr) = body.get("talkgroups").and_then(|v| v.as_array()) {
        let tgs: Vec<u16> = arr
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u16))
            .collect();
        list.set(tgs);
    }
    Json(serde_json::json!({
        "talkgroups": list.list(),
    }))
}

// ── Phase 7F.1 (2026-04-14): Event log tail ──────────────────────────


/// GET /api/encrypted_tgs -- read the current encryption blocklist.
/// Returns the sorted list of TGs in
/// `ImbeForwarder.encrypted_tg_history`.
pub async fn get_encrypted_tgs(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let mut list: Vec<u16> = state
        .imbe_forwarder
        .encrypted_tg_history
        .lock()
        .map(|h| h.iter().copied().collect())
        .unwrap_or_default();
    list.sort_unstable();
    Json(serde_json::json!({
        "count":  list.len(),
        "tgs":    list,
        "note":   "TGs in this list are permanently rejected by the \
                   grant follower. Populated eagerly by the follower \
                   whenever it observes a grant with encrypted=true, \
                   and manually via ?add=N or ?remove=N. Clear all \
                   via ?clear=1. Persists for the lifetime of the \
                   p25-httpd process only (resets on reboot).",
    }))
}


/// PUT /api/encrypted_tgs -- mutate the blocklist.
///
/// Query params (all optional, multiple can be combined):
///   add    = NNN     -- add TG NNN to the blocklist
///   remove = NNN     -- remove TG NNN from the blocklist
///   clear  = 1       -- clear the whole blocklist
///
/// Motivation: on P25 sites that don't consistently set the
/// service_options `encrypted` bit on every GroupVoiceChannelGrant
/// TSBK, the eager-history populate can never block a TG because
/// we never see the flag. Phase 7F.5 adds a manual override so the
/// operator can say "TG 402 is encrypted on this site, trust me"
/// and the follower will skip it thereafter.
pub async fn put_encrypted_tgs(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    if params.get("clear").map(String::as_str) == Some("1") {
        if let Ok(mut h) = state.imbe_forwarder.encrypted_tg_history.lock() {
            let n = h.len();
            h.clear();
            applied.push(format!("cleared ({} entries)", n));
        }
    }
    if let Some(v) = params.get("add") {
        match v.parse::<u16>() {
            Ok(tg) => {
                if let Ok(mut h) =
                    state.imbe_forwarder.encrypted_tg_history.lock()
                {
                    if h.insert(tg) {
                        applied.push(format!("added TG={}", tg));
                    } else {
                        applied.push(format!("TG={} already present", tg));
                    }
                }
            }
            Err(_) => errors.push(format!("add={:?} not an integer", v)),
        }
    }
    if let Some(v) = params.get("remove") {
        match v.parse::<u16>() {
            Ok(tg) => {
                if let Ok(mut h) =
                    state.imbe_forwarder.encrypted_tg_history.lock()
                {
                    if h.remove(&tg) {
                        applied.push(format!("removed TG={}", tg));
                    } else {
                        applied.push(format!("TG={} not in list", tg));
                    }
                }
            }
            Err(_) => errors.push(format!("remove={:?} not an integer", v)),
        }
    }

    // If anything changed, also force-idle the follower if it's
    // currently locked on a newly-blocked TG. Otherwise the manual
    // add takes effect only for the NEXT grant for that TG.
    let currently_locked = state
        .traffic_manager
        .lock()
        .await
        .current_talkgroup()
        .map(|t| t.0);
    if let Some(locked_tg) = currently_locked {
        let blocked_now = state
            .imbe_forwarder
            .encrypted_tg_history
            .lock()
            .map(|h| h.contains(&locked_tg))
            .unwrap_or(false);
        if blocked_now {
            let mut mgr = state.traffic_manager.lock().await;
            mgr.force_idle();
            drop(mgr);
            use std::sync::atomic::Ordering;
            state
                .imbe_forwarder
                .current_talkgroup
                .store(0, Ordering::Relaxed);
            #[cfg(target_os = "linux")]
            {
                let core = state.ip_core.lock().await;
                core.set_traffic_demod_enable(false);
            }
            {
                let mut dec = state.traffic_lsm_decoder.write().await;
                dec.reset_framer_state();
            }
            applied.push(format!(
                "force-idle: was locked on TG={} which is now blocked",
                locked_tg,
            ));
        }
    }

    state.event_log.push(
        crate::services::event_log::LogCategory::System,
        format!("encrypted_tgs update: {}", applied.join(", ")),
        serde_json::json!({
            "applied": applied.clone(),
            "errors":  errors.clone(),
        }),
    );
    let list: Vec<u16> = {
        let mut v: Vec<u16> = state
            .imbe_forwarder
            .encrypted_tg_history
            .lock()
            .map(|h| h.iter().copied().collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    };
    Json(serde_json::json!({
        "applied": applied,
        "errors":  errors,
        "count":   list.len(),
        "tgs":     list,
    }))
}

// ── Phase 7E: Audio streaming ─────────────────────────────────────────


