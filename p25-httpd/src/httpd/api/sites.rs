//! Per-site baseline endpoints: list, fetch detail, switch active.
//!
//! Consumer orientation: "which P25 systems do I have a baseline for,
//! and which one am I receiving right now?" The site selector
//! dropdown in the dashboard topbar consumes `GET /api/sites` for
//! the option list and `POST /api/site?name=...` to switch.
//!
//! Switching site applies the saved baseline to the live receiver
//! atomically: preset → AD9361 LO snapped per `cc_position` →
//! `current_control_freq` set to the site's CC → IDEN bands seeded
//! into the LSM control-channel decoder.
//!
//! Storage layout: `services::sites::{repo_seed_dir, runtime_overlay_dir}`
//! — repo seed at `p25-httpd/sites/<name>.json` (checked in), runtime
//! overlay at `/mnt/data/p25/<name>.json` (target persistent flash).
//! Both layers hydrate at boot; overlay wins.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use crate::httpd::AppState;
use crate::services::sites::{
    list_sites, load_site, save_site, write_active_site_name, Site,
};

/// `GET /api/sites` — list every known site name + its label and
/// active flag. Cheap; reads only file metadata + the active site
/// name from the AppState.
pub async fn get_sites(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let names = list_sites();
    let active_name = state.active_site.read().await
        .as_ref()
        .map(|s| s.name.clone());

    let mut sites = Vec::with_capacity(names.len());
    for name in &names {
        let label = match load_site(name) {
            Ok(s) => s.label,
            Err(_) => name.clone(),
        };
        sites.push(serde_json::json!({
            "name": name,
            "label": label,
            "active": Some(name) == active_name.as_ref(),
        }));
    }

    Json(serde_json::json!({
        "ok": true,
        "active": active_name,
        "sites": sites,
    }))
}

/// `GET /api/sites/<name>` — full site detail (NAC, WACN, IDEN bands,
/// CC, alt CCs, traffic_freqs_hz, cc_position).
pub async fn get_site(
    Path(name): Path<String>,
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match load_site(&name) {
        Ok(site) => (StatusCode::OK, Json(serde_json::json!({
            "ok": true, "site": site,
        }))).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "ok": false, "error": e.to_string(),
        }))).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct PostSiteQuery {
    pub name: String,
    /// Optional: skip the LO snap / preset apply. Default false.
    #[serde(default)]
    pub no_apply: bool,
}

/// `POST /api/site?name=<name>` — switch active site.
///
/// Effect:
///   1. Load `<name>.json` (overlay ∪ seed).
///   2. Stash into `state.active_site`.
///   3. Persist as the boot default via `write_active_site_name`.
///   4. (Unless `no_apply=true`) ask `tuning::post_preset` to apply
///      the site's `preset_default` — that handler now reads
///      `active_site.cc_position` to snap the LO.
///   5. Return the loaded site for the dashboard to render.
///
/// Step 4's preset apply lives in `tuning::post_preset` rather than
/// being inlined here so the same code path runs for direct preset
/// changes + site switches.
/// Change 073: keep what the radio learned on the site being left (the
/// grant map behind the monitor roster, the talkgroups seen encrypted)
/// and bring back the new site's. A talkgroup encrypted on one system
/// says nothing about the same number on another.
async fn swap_site_memory(state: &AppState, from: &str, to: &str) {
    if from == to {
        return;
    }
    let enc = std::mem::take(&mut *state.imbe_forwarder.encrypted_tg_history.lock().unwrap_or_else(|p| p.into_inner()));
    let grants = std::mem::take(&mut state.traffic_chain.lock().await.grant_map);
    let next = {
        let mut mem = state.site_memory.lock().unwrap_or_else(|p| p.into_inner());
        mem.insert(from.to_string(), crate::httpd::SiteMemory { encrypted_tgs: enc, grant_map: grants });
        mem.remove(to).unwrap_or_default()
    };
    *state.imbe_forwarder.encrypted_tg_history.lock().unwrap_or_else(|p| p.into_inner()) = next.encrypted_tgs;
    state.traffic_chain.lock().await.grant_map = next.grant_map;
}

pub async fn post_site(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PostSiteQuery>,
) -> impl IntoResponse {
    if let Some(busy) = state.radio_busy() {
        return busy.into_response();
    }
    let site = match load_site(&q.name) {
        Ok(s) => s,
        Err(e) => {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({
                "ok": false, "error": e.to_string(),
            }))).into_response();
        }
    };

    // Change 073: from here until the new control channel is tuned, the
    // old one's grants are dropped (they would carry the new site).
    crate::services::lo_plan::hold_grants(crate::app::now_unix_ms());
    let prev_site = state.lo_plans.site();
    {
        let mut active = state.active_site.write().await;
        *active = Some(site.clone());
    }

    // Change 069: the site's talkgroup names and active profile go
    // live with it.
    let switch = crate::services::ui_settings::SettingsPatch {
        switch_site: Some(site.name.clone()),
        ..Default::default()
    };
    if let Err(e) = crate::httpd::api::ui::apply_settings_patch(&state, switch, "site_switch").await {
        tracing::warn!("site switch to '{}': settings not switched: {e}", site.name);
    }
    // Change 070: grants are counted for the new site from now on.
    state.lo_plans.set_site(&site.name);
    // Change 075: a DMR site runs the DMR receiver and follower; a P25
    // site stops them.
    state.dmr_rt.apply_site(&site);
    // Change 073: the grant map and encrypted talkgroups are per site.
    swap_site_memory(&state, &prev_site, &site.name).await;
    state.event_log.push(
        crate::services::event_log::LogCategory::System,
        format!("site switched: {} -> {} ({})", prev_site, site.name, site.label),
        serde_json::json!({ "from": prev_site, "to": site.name, "control_freq_hz": site.control_freq_hz }),
    );

    if let Err(e) = write_active_site_name(&q.name) {
        tracing::warn!(
            "failed to persist active site marker for '{}': {e}",
            q.name
        );
        // Not fatal — runtime state still updated.
    }

    // Update operator-facing control freq so subsequent /api/preset
    // and /api/tune calls have the right anchor.
    let prev_cc = state
        .current_control_freq
        .swap(site.control_freq_hz, std::sync::atomic::Ordering::Relaxed);
    // Change 070: a different control channel is (likely) a different
    // system: drop the old NAC lock, identity and band table.
    if prev_cc != site.control_freq_hz {
        state.decoder.write().await.new_system();
        state.lsm_decoder.write().await.new_system();
    }

    tracing::info!(
        target: "p25_site",
        "active site -> {} ({}), CC={:.4} MHz cc_position={:?} \
         preset_default={}",
        site.name, site.label,
        site.control_freq_hz as f64 / 1e6,
        site.cc_position,
        site.preset_default,
    );

    Json(serde_json::json!({
        "ok": true,
        "site": site,
        "applied_preset": !q.no_apply,
        "note": if q.no_apply {
            "Active site updated; preset/LO unchanged (no_apply=true)."
        } else {
            "Active site updated. Issue POST /api/preset with the \
             site's preset_default to apply the LO snap + DDC tune."
        },
    })).into_response()
}
