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
use crate::services::ui_settings::{SettingsPatch, StorageKind};

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
    let (r_new, r_len, r_pending) = {
        let r = state.recordings.lock().await;
        (r.back().map(|e| e.id), r.len(), r.iter().filter(|e| e.pending.is_some()).count())
    };
    let stats_rev = state.grant_stats_rev.load(Ordering::Relaxed);
    // Change 073: a site switch changes the (default) list.
    let rev = ui_state::calls_rev(c_new, c_len, e_new, e_len, r_new, r_len, stats_rev, r_pending);
    format!("{rev}:{}", state.lo_plans.site())
}

/// Change 057: recording status for `/api/ui/state`.
async fn recording_status(state: &AppState) -> UiRecordingStatus {
    let policy = &state.ui_settings.recording;
    let ((ram_count, _), (sd_count, _)) = {
        let r = state.recordings.lock().await;
        crate::audio::rec_storage::usage(&r)
    };
    let storage = policy.storage();
    let sd_state = (storage == StorageKind::Sd || sd_count > 0)
        .then(|| state.rec_storage.sd_state().to_string());
    UiRecordingStatus {
        enabled: policy.enabled(),
        max_count: policy.max_count(),
        count: ram_count + sd_count,
        storage: storage.as_str().to_string(),
        sd_state,
        sd_count,
        ram_count,
    }
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
    // Change 067: the site time and the board clock source.
    site.clock_source = state.ui_settings.clock.source().as_str().to_string();
    site.site_time = {
        let dec = state.active_control_decoder().read().await;
        let c = &dec.system.site_clock;
        let mono = crate::hardware::dibit_ring::mono_us() / 1_000;
        match (c.site_ms_at(mono), c.precision(), c.last()) {
            (Some(ms), Some(p), Some((s, at))) => Some(p25_json::ui::UiSiteTime {
                unix_ms: ms,
                precision: p.as_str().to_string(),
                ext_locked: s.ext_locked,
                local_offset_min: s.local_offset_min,
                board_offset_ms: ms as i64 - now_unix_ms() as i64,
                age_ms: mono.saturating_sub(at),
            }),
            _ => None,
        }
    };
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
    // Change 066: every traffic chain's call and state, chain 1 first.
    let mut calls = Vec::new();
    let mut chains = Vec::new();
    for lane in &state.traffic_lanes {
        let n = lane.lane.number();
        let snap = lane.active_call.lock().ok().and_then(|g| g.clone());
        if let Some(s) = snap {
            // A call opened while recording was off stays unrecorded
            // even if recording is switched back on mid-call.
            let rec = recording_enabled && !state.ui_settings.recording.was_skipped(s.call_id);
            calls.push(ui_state::build_call(&s, now, aliases, rec, n));
        }
        let mgr = lane.chain.lock().await;
        chains.push(UiChain {
            state: mgr.state_label().to_string(),
            parked_freq_hz: mgr.parked_freq_hz,
            follower_enabled: state.traffic_follower_enabled.load(Ordering::Relaxed),
            lock_freq: state.traffic_lock_freq.load(Ordering::Relaxed),
            delivery_mode: state.dibit_delivery.traffic_ring(lane.lane).active_mode().as_str().to_string(),
            number: n,
            tg: mgr.current_talkgroup().map(|t| u32::from(t.0)),
        });
    }
    let call = calls.iter().find(|c| c.chain == 1).cloned();
    let chain = chains.first().cloned().unwrap_or(UiChain {
        state: "Idle".into(),
        parked_freq_hz: None,
        follower_enabled: false,
        lock_freq: false,
        delivery_mode: String::new(),
        number: 1,
        tg: None,
    });
    let recording = recording_status(&state).await;
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
        calls,
        chains,
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
    // Change 073: one site's calls: `?site=<name>`, the active site by
    // default, `all` for every site. Names from that site's profile.
    let active = state.lo_plans.site();
    let site = match params.get("site").map(String::as_str) {
        Some("all") => None,
        Some("-") => Some(String::new()),
        Some(s) => Some(s.to_string()),
        None => Some(active.clone()),
    };
    let settings = state.ui_settings.snapshot();
    let (tg_names, unit_names) = settings.aliases_for(site.as_deref().unwrap_or(&active));
    let aliases = Aliases {
        tg: Some(&tg_names),
        unit: Some(&unit_names),
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
        CallsQuery { limit, include_not_followed, site: site.clone() },
        now,
        &skipped,
    );
    let mut sites: Vec<p25_json::ui::UiSiteCount> = ui_state::site_counts(&clear, &enc, &recs)
        .into_iter()
        .map(|(s, calls, recordings)| p25_json::ui::UiSiteCount {
            label: site_label(&s),
            active: s == active,
            site: s,
            calls,
            recordings,
        })
        .collect();
    if !sites.iter().any(|s| s.active) {
        sites.insert(0, p25_json::ui::UiSiteCount { label: site_label(&active), site: active.clone(), active: true, ..Default::default() });
    }
    sites.sort_by_key(|s| !s.active);
    Json(UiCalls {
        now_unix_ms: now,
        calls_rev: calls_rev(&state).await,
        recording_enabled: state.ui_settings.recording.enabled(),
        items,
        site,
        sites,
    })
}

/// Change 073: a site's label from its site file ("Earlier" for calls
/// kept before sites were).
pub(crate) fn site_label(site: &str) -> String {
    if site.is_empty() {
        return "Earlier (no site)".into();
    }
    crate::services::sites::load_site(site).map(|s| s.label).unwrap_or_else(|_| site.to_string())
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
    use crate::services::ui_settings as us;
    let store = &state.ui_settings;
    let mut enc: Vec<u32> = state
        .imbe_forwarder
        .encrypted_tg_history
        .lock()
        .map(|h| h.iter().copied().collect())
        .unwrap_or_default();
    enc.sort_unstable();
    let ((ram_n, ram_bytes), (sd_n, sd_bytes)) = {
        let r = state.recordings.lock().await;
        crate::audio::rec_storage::usage(&r)
    };
    let selected = store.recording.storage();
    let rs = &state.rec_storage;
    // Change 057: where the NEXT recording goes. SD selected but not
    // usable (absent, read-only, full, stalled) saves to RAM.
    let active_sd = selected == StorageKind::Sd && rs.sd_ready().is_ok();
    let ram_free = tmp_free_bytes();
    let mut sd = rs.sd_status();
    if let Some(o) = sd.as_object_mut() {
        o.insert("count".into(), sd_n.into());
        o.insert("bytes".into(), sd_bytes.into());
        o.insert("ready".into(), rs.sd_ready().err().map_or("ok".to_string(), |e| e).into());
    }
    let settings = store.snapshot();
    // Change 069: the live site's profiles, ready for the pickers.
    let (names, active) = settings.profiles();
    let site_label = crate::services::sites::load_site(&settings.site).map(|s| s.label).ok();
    serde_json::json!({
        "profiles":        { "site": settings.site, "site_label": site_label, "names": names, "active": active },
        "settings":        settings,
        "rev":             store.rev(),
        "file":            store.path().map(|p| p.display().to_string()),
        "load_note":       store.load_note(),
        "last_save_error": store.last_save_error(),
        "recording_storage": {
            // Pre-057 keys, now describing where the next recording goes.
            "dir":        if active_sd { rs.sd_dir().display().to_string() }
                          else { rs.ram_dir().display().to_string() },
            "tmpfs":      !active_sd,
            "count":      ram_n + sd_n,
            "free_bytes": if active_sd { sd.get("free_bytes").and_then(|v| v.as_u64()) }
                          else { ram_free },
            // Change 057.
            "selected":   selected.as_str(),
            "active":     if active_sd { "sd" } else { "ram" },
            "ram": {
                "dir":        rs.ram_dir().display().to_string(),
                "count":      ram_n,
                "bytes":      ram_bytes,
                "free_bytes": ram_free,
            },
            "sd": sd,
            "moves_on_change": false,
        },
        "limits": {
            "max_count_max":    us::MAX_RECORDINGS_LIMIT,
            "sd_max_count_max": us::SD_MAX_COUNT_LIMIT,
            "sd_max_mb_min":    us::SD_MAX_MB_MIN,
            "sd_max_mb_max":    us::SD_MAX_MB_LIMIT,
            "hang_ms_min":      us::HANG_MS_MIN,
            "hang_ms_max":      us::HANG_MS_MAX,
            "end_grace_ms_max": us::END_GRACE_MS_MAX,
            "alias_chars_max":  us::MAX_ALIAS_CHARS,
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
    aliases: &std::collections::BTreeMap<u32, String>,
) {
    let map: std::collections::HashMap<u32, String> =
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
    let before = state.ui_settings.snapshot();
    // Change 069: a profile or site switch replaces the whole setup.
    let switching = patch.profile.is_some() || patch.switch_site.is_some();
    let out = state.ui_settings.update(patch)?;
    let mut evicted = 0;
    if out.changed.recording {
        evicted = crate::audio::recorder::enforce_retention(
            &state.recordings,
            &state.rec_storage,
            state.ui_settings.recording.retention(),
        )
        .await;
        // Change 057: switching to the SD card re-checks it at once.
        if out.settings.recording.storage != before.recording.storage {
            state.rec_storage.request_probe();
        }
    }
    if out.changed.tg_aliases {
        apply_tg_aliases(state, &out.settings.tg_aliases).await;
    }
    if out.changed.monitor_tgs {
        state.monitor_list.write().await.set(out.settings.monitor_tgs.clone());
    }
    // Change 068: a talkgroup ignored while on air is dropped now, not at
    // its next grant.
    // Change 069: after a profile or site switch, a call the new setup
    // does not follow (ignored, off, or outside the monitor list) is
    // dropped now as well.
    if switching && (out.changed.tg_groups || out.changed.speakers || out.changed.monitor_tgs || out.changed.ignore_tgs) {
        let routing = state.ui_settings.routing.snapshot();
        let monitor = &out.settings.monitor_tgs;
        let off = |t: u32| routing.route(t).is_none() || (!monitor.is_empty() && !monitor.contains(&t));
        for (lane, tg) in super::talkgroups::release_chains_on(state, off).await {
            state.event_log.push(
                crate::services::event_log::LogCategory::Traffic,
                format!("{lane}: TG={tg} not in profile {:?}, call dropped", out.settings.profiles().1),
                serde_json::json!({ "tg": tg, "chain": lane.label(), "reason": "profile" }),
            );
        }
    } else if out.changed.ignore_tgs {
        let ignore = &out.settings.ignore_tgs;
        for (lane, tg) in super::talkgroups::release_chains_on(state, |t| ignore.binary_search(&t).is_ok()).await {
            state.event_log.push(
                crate::services::event_log::LogCategory::Traffic,
                format!("{lane}: TG={tg} ignored while on air, call dropped"),
                serde_json::json!({ "tg": tg, "chain": lane.label(), "reason": "ignored" }),
            );
        }
    }
    if out.changed.any() {
        state.event_log.push(
            crate::services::event_log::LogCategory::System,
            format!(
                "settings updated via {origin}: recording={} keep={} storage={} \
                 sd_keep={} sd_max_mb={} hang_ms={} end_grace_ms={} tg_aliases={} \
                 unit_aliases={} monitor={:?} ignore={:?} site={} profile={}{}",
                if out.settings.recording.enabled { "on" } else { "off" },
                out.settings.recording.max_count,
                out.settings.recording.storage.as_str(),
                out.settings.recording.sd_max_count,
                out.settings.recording.sd_max_mb,
                out.settings.call.hang_ms,
                out.settings.call.end_grace_ms,
                out.settings.tg_aliases.len(),
                out.settings.unit_aliases.len(),
                out.settings.monitor_tgs,
                out.settings.ignore_tgs,
                out.settings.site,
                out.settings.profiles().1,
                if out.persisted { "" } else { " (NOT persisted)" },
            ),
            serde_json::json!({
                "origin":     origin,
                "changed":    {
                    "recording":    out.changed.recording,
                    "call":         out.changed.call,
                    "tg_aliases":   out.changed.tg_aliases,
                    "unit_aliases": out.changed.unit_aliases,
                    "monitor_tgs":  out.changed.monitor_tgs,
                    "ignore_tgs":   out.changed.ignore_tgs,
                    "tg_groups":    out.changed.tg_groups,
                    "speakers":     out.changed.speakers,
                    "profiles":     out.changed.profiles,
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
/// `{"recording":{"enabled":bool,"max_count":N,"storage":"ram"|"sd",
/// "sd_max_count":N,"sd_max_mb":N},"call":{"hang_ms":N,"end_grace_ms":N},
/// "tg_aliases":{..},"unit_aliases":{..},"monitor_tgs":[..]}`. Maps /
/// lists replace the stored value. 400 on an invalid or unknown field
/// (nothing changes). Change 069: `{"profile":{"select"|"create"|
/// "rename"|"delete":..}}` acts on the live site's profiles (alone in
/// its patch).
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
