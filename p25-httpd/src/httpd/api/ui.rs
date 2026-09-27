//! Change 056 web UI: consolidated state, calls and settings, plus the
//! embedded page and its assets.
//!
//! Consumer orientation: "one document per question". The UI polls
//! `GET /api/ui/state` (~1–2 KB) once a second and fetches
//! `GET /api/ui/calls` only when `calls_rev` changes, instead of the
//! pre-056 dashboard's ~40 endpoints (`/api/grant_decode_stats` alone
//! was 147 KB every 5 s). Call identity comes from ONE source, the
//! lifecycle (`app::grant_follower`): `/api/grants`, `/api/traffic`'s
//! `current_call` and the vocoder's per-HDU baselines are three other
//! notions of "the current call" that the old page mixed.
//!
//! Presentation rules live in `app::ui_state` (host-tested); this
//! module only snapshots live state and applies settings changes.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use p25_json::ui::{UiAudio, UiCalls, UiChain, UiRecordingStatus, UiSite, UiState};

use crate::app::ui_state::{self, Aliases, CallsQuery};
use crate::httpd::{ui_assets, AppState};
use crate::services::ui_settings::SettingsPatch;

type Params = std::collections::HashMap<String, String>;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Page + assets ─────────────────────────────────────────────────

/// `GET /` — the UI shell with versioned asset URLs. Never cached, so
/// a reload after a firmware update picks up the new asset version.
pub async fn get_ui_index() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        ui_assets::render_index(),
    )
        .into_response()
}

/// `GET /ui/{version}/{*path}` — an embedded asset. The current
/// version is served `immutable` (the URL changes with the content);
/// any other version (a stale page) gets today's file uncached.
pub async fn get_ui_asset(
    Path((version, path)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(asset) = ui_assets::find(&path) else {
        return (StatusCode::NOT_FOUND, "no such UI asset").into_response();
    };
    let etag = format!("\"{}\"", ui_assets::asset_version());
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|t| t.trim() == etag))
        .unwrap_or(false)
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    let cache = if version == ui_assets::asset_version() {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, ui_assets::content_type(asset.path).to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
            (header::ETAG, etag),
        ],
        asset.body,
    )
        .into_response()
}

// ── State ─────────────────────────────────────────────────────────

/// Newest-last ring tail of each call source, for `calls_rev`.
async fn calls_rev(state: &AppState) -> String {
    let (c_new, c_len) = state
        .grant_decode_stats
        .lock()
        .map(|r| (r.back().map(|g| g.call_id), r.len()))
        .unwrap_or((None, 0));
    let (e_new, e_len) = state
        .enc_grant_decode_stats
        .lock()
        .map(|r| (r.back().map(|g| g.call_id), r.len()))
        .unwrap_or((None, 0));
    let (r_new, r_len) = {
        let r = state.recordings.lock().await;
        (r.back().map(|e| e.id), r.len())
    };
    ui_state::calls_rev(c_new, c_len, e_new, e_len, r_new, r_len)
}

async fn site(state: &AppState, mono_ms: u64) -> UiSite {
    let (mut site, ok, fail) = {
        let dec = state.active_control_decoder().read().await;
        let s = &dec.system;
        let last_tsbk_age_ms = dec
            .recent_messages
            .last()
            .map(|(t, _, _)| t.elapsed().as_millis() as u64);
        let acquired = s.wacn.is_some();
        (
            UiSite {
                nac: s.nac.map(|n| format!("{n}")),
                wacn: s.wacn.map(|w| format!("{w:05X}")),
                system_id: s.system_id.map(|v| format!("{v:03X}")),
                rfss_id: s.rfss_id,
                site_id: s.site_id,
                acquired,
                last_tsbk_age_ms,
                health: ui_state::site_health(acquired, last_tsbk_age_ms).to_string(),
                ..Default::default()
            },
            dec.tsbk_crc_ok,
            dec.tsbk_crc_failures,
        )
    };
    if let Ok(mut w) = state.ui_cc_rate.lock() {
        w.observe(mono_ms, ok, fail);
        if let Some((per_s, pct)) = w.rates() {
            site.tsbk_per_s = Some((per_s * 10.0).round() / 10.0);
            site.tsbk_ok_pct = pct.map(|p| (p * 10.0).round() / 10.0);
        }
    }
    if let Some(s) = state.active_site.read().await.as_ref() {
        site.name = Some(s.name.clone());
        site.label = Some(s.label.clone());
    }
    site.cc_freq_hz = state.current_control_freq.load(Ordering::Relaxed);
    site.modulation = state.active_modulation_label().to_string();
    site
}

/// `GET /api/ui/state` — see `p25_json::ui::UiState`.
pub async fn get_ui_state(State(state): State<Arc<AppState>>) -> Json<UiState> {
    let now = now_unix_ms();
    let mono_ms = state.boot_instant.elapsed().as_millis() as u64;
    let settings = state.ui_settings.snapshot();
    let aliases = Aliases {
        tg: Some(&settings.tg_aliases),
        unit: Some(&settings.unit_aliases),
    };
    let recording_enabled = state.ui_settings.recording.enabled();

    let site = site(&state, mono_ms).await;
    let call = state
        .active_call_snapshot
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .map(|s| {
            // A call opened while recording was off stays unrecorded
            // even if recording is switched back on mid-call.
            let rec = recording_enabled && !state.ui_settings.recording.was_skipped(s.call_id);
            ui_state::build_call(&s, now, aliases, rec)
        });
    let chain = {
        let mgr = state.traffic_chain.lock().await;
        UiChain {
            state: mgr.state_label().to_string(),
            parked_freq_hz: mgr.parked_freq_hz,
            follower_enabled: state.traffic_follower_enabled.load(Ordering::Relaxed),
            lock_freq: state.traffic_lock_freq.load(Ordering::Relaxed),
            delivery_mode: state.dibit_delivery.traffic.active_mode().as_str().to_string(),
        }
    };
    let recording = UiRecordingStatus {
        enabled: recording_enabled,
        max_count: state.ui_settings.recording.max_count(),
        count: state.recordings.lock().await.len(),
    };
    let audio = UiAudio {
        listeners: state.audio_ws_listeners.load(Ordering::Relaxed),
        lag_total: state.audio_ws_lag_total.load(Ordering::Relaxed),
    };
    Json(UiState {
        v: 1,
        build: crate::BUILD_TAG.to_string(),
        now_unix_ms: now,
        clock_valid: ui_state::clock_valid(now),
        uptime_s: state.boot_instant.elapsed().as_secs(),
        site,
        call,
        chain,
        recording,
        audio,
        calls_rev: calls_rev(&state).await,
        settings_rev: state.ui_settings.rev(),
        log_last_seq: state.event_log.last_seq(),
    })
}

/// `GET /api/ui/calls?limit=N&nf=0|1` — recent calls, newest first,
/// joined with recordings by call_id. `limit` default 40 (max 250);
/// `nf=0` hides encrypted / not-followed grants (default shown).
pub async fn get_ui_calls(
    State(state): State<Arc<AppState>>,
    Query(params): Query<Params>,
) -> Json<UiCalls> {
    let now = now_unix_ms();
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(40)
        .clamp(1, 250);
    let include_not_followed = params.get("nf").map(|v| v != "0").unwrap_or(true);
    let settings = state.ui_settings.snapshot();
    let aliases = Aliases {
        tg: Some(&settings.tg_aliases),
        unit: Some(&settings.unit_aliases),
    };
    let clear: Vec<_> = state
        .grant_decode_stats
        .lock()
        .map(|r| r.iter().cloned().collect())
        .unwrap_or_default();
    let enc: Vec<_> = state
        .enc_grant_decode_stats
        .lock()
        .map(|r| r.iter().cloned().collect())
        .unwrap_or_default();
    let recs: Vec<_> = state.recordings.lock().await.iter().cloned().collect();
    let policy = state.ui_settings.recording.clone();
    let skipped = move |id: u64| policy.was_skipped(id);
    let items = ui_state::build_calls(
        &clear,
        &enc,
        &recs,
        aliases,
        CallsQuery { limit, include_not_followed },
        now,
        &skipped,
    );
    Json(UiCalls {
        now_unix_ms: now,
        calls_rev: calls_rev(&state).await,
        recording_enabled: state.ui_settings.recording.enabled(),
        items,
    })
}

// ── Settings ──────────────────────────────────────────────────────

fn tmp_free_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        crate::httpd::api::system::fs_usage("/tmp").map(|(_, avail)| avail)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

async fn settings_json(state: &AppState) -> serde_json::Value {
    let store = &state.ui_settings;
    let mut enc: Vec<u16> = state
        .imbe_forwarder
        .encrypted_tg_history
        .lock()
        .map(|h| h.iter().copied().collect())
        .unwrap_or_default();
    enc.sort_unstable();
    serde_json::json!({
        "settings":        store.snapshot(),
        "rev":             store.rev(),
        "file":            store.path().map(|p| p.display().to_string()),
        "load_note":       store.load_note(),
        "last_save_error": store.last_save_error(),
        "recording_storage": {
            "dir":        crate::audio::recorder::STORAGE_DIR,
            "tmpfs":      true,
            "count":      state.recordings.lock().await.len(),
            "free_bytes": tmp_free_bytes(),
        },
        "limits": {
            "max_count_max":   crate::services::ui_settings::MAX_RECORDINGS_LIMIT,
            "alias_chars_max": crate::services::ui_settings::MAX_ALIAS_CHARS,
        },
        // Read-only here; edit via PUT /api/encrypted_tgs (learned
        // automatically, process lifetime only).
        "encrypted_tgs": enc,
    })
}

/// `GET /api/ui/settings`.
pub async fn get_ui_settings(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(settings_json(&state).await)
}

/// Push TG aliases into both control-channel decoders (they stamp
/// `talkgroup_alias` on `/ws/events` TSBK events). Pre-056 `PUT
/// /api/aliases` wrote only the C4FM decoder, which is not the active
/// one on LSM sites, so aliases never appeared.
pub async fn apply_tg_aliases(
    state: &AppState,
    aliases: &std::collections::BTreeMap<u16, String>,
) {
    let map: std::collections::HashMap<u16, String> =
        aliases.iter().map(|(k, v)| (*k, v.clone())).collect();
    state.decoder.write().await.aliases = map.clone();
    state.lsm_decoder.write().await.aliases = map;
}

/// Apply a validated patch: persist, then update live state. Shared by
/// `PUT /api/ui/settings`, `PUT /api/aliases` and `/api/monitor`.
pub async fn apply_settings_patch(
    state: &AppState,
    patch: SettingsPatch,
    origin: &str,
) -> Result<serde_json::Value, String> {
    let out = state.ui_settings.update(patch)?;
    let mut evicted = 0;
    if out.changed.recording {
        evicted = crate::audio::recorder::enforce_retention(
            &state.recordings,
            out.settings.recording.max_count,
        )
        .await;
    }
    if out.changed.tg_aliases {
        apply_tg_aliases(state, &out.settings.tg_aliases).await;
    }
    if out.changed.monitor_tgs {
        state.monitor_list.write().await.set(out.settings.monitor_tgs.clone());
    }
    if out.changed.any() {
        state.event_log.push(
            crate::services::event_log::LogCategory::System,
            format!(
                "settings updated via {origin}: recording={} keep={} tg_aliases={} \
                 unit_aliases={} monitor={:?}{}",
                if out.settings.recording.enabled { "on" } else { "off" },
                out.settings.recording.max_count,
                out.settings.tg_aliases.len(),
                out.settings.unit_aliases.len(),
                out.settings.monitor_tgs,
                if out.persisted { "" } else { " (NOT persisted)" },
            ),
            serde_json::json!({
                "origin":     origin,
                "changed":    {
                    "recording":    out.changed.recording,
                    "tg_aliases":   out.changed.tg_aliases,
                    "unit_aliases": out.changed.unit_aliases,
                    "monitor_tgs":  out.changed.monitor_tgs,
                },
                "persisted":  out.persisted,
                "save_error": out.save_error,
                "evicted":    evicted,
            }),
        );
    }
    Ok(serde_json::json!({
        "ok":         true,
        "persisted":  out.persisted,
        "save_error": out.save_error,
        "evicted":    evicted,
    }))
}

/// `PUT /api/ui/settings` — body: any subset of
/// `{"recording":{"enabled":bool,"max_count":N},"tg_aliases":{..},
/// "unit_aliases":{..},"monitor_tgs":[..]}`. Maps / lists replace the
/// stored value. 400 on an invalid or unknown field (nothing changes).
pub async fn put_ui_settings(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let patch: SettingsPatch = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": format!("bad settings JSON: {e}")})),
            )
        }
    };
    match apply_settings_patch(&state, patch, "ui").await {
        Ok(mut v) => {
            if let (Some(obj), serde_json::Value::Object(full)) =
                (v.as_object_mut(), settings_json(&state).await)
            {
                obj.extend(full);
            }
            (StatusCode::OK, Json(v))
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": e})),
        ),
    }
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;
