//! Change 070: the receive window against the active site's channels.
//!
//! - `GET /api/site/plan`: the live window, every channel (listed in the
//!   site file or granted) with its grant count and whether the window
//!   covers it, and the planner's best window.
//! - `PUT /api/site/plan` `{"auto": bool, "min_preset": "12M" | null}`
//!   (either): recentre automatically when idle; the narrowest preset
//!   the planner may pick (per site, saved).
//! - `POST /api/site/recentre`: move to the best window now.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;

use crate::app::recentre_task::window_view;
use crate::httpd::AppState;

pub async fn get_site_plan(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    match window_view(&state).await {
        Some(v) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "plan": v }))),
        None => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "ok": false, "error": "no active site" })),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanPatch {
    #[serde(default)]
    pub auto: Option<bool>,
    /// `Some(None)` (JSON null) clears the minimum.
    #[serde(default, deserialize_with = "some_or_null")]
    pub min_preset: Option<Option<String>>,
}

fn some_or_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

pub async fn put_site_plan(
    State(state): State<Arc<AppState>>,
    Json(p): Json<PlanPatch>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(Some(m)) = &p.min_preset {
        if !crate::app::recentre_task::plan_presets(None).iter().any(|(n, _)| n.eq_ignore_ascii_case(m)) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": format!("min_preset: not a planner preset: {m}") })),
            );
        }
    }
    state.lo_plans.edit(|s| {
        if let Some(a) = p.auto {
            s.auto = a;
        }
        if let Some(m) = &p.min_preset {
            s.min_preset = m.as_ref().map(|x| x.to_uppercase());
        }
    });
    let saved = state.lo_plans.flush();
    let now = state.lo_plans.get();
    state.event_log.push(
        crate::services::event_log::LogCategory::System,
        format!(
            "site {}: auto recentre {}, narrowest window {}",
            state.lo_plans.site(),
            if now.auto { "on" } else { "off" },
            now.min_preset.as_deref().unwrap_or("any"),
        ),
        serde_json::json!({ "auto": now.auto, "min_preset": now.min_preset }),
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "auto": now.auto, "min_preset": now.min_preset, "save_error": saved.err() })),
    )
}

#[cfg(target_os = "linux")]
pub async fn post_recentre(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(busy) = state.radio_busy() {
        return busy;
    }
    let Some(best) = window_view(&state).await.and_then(|v| v.best) else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "ok": false, "error": "no active site to plan for" })),
        );
    };
    match crate::app::recentre_task::apply(&state, &best, "operator").await {
        Ok(reply) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "plan": best, "preset": reply }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e, "plan": best })),
        ),
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn post_recentre(State(_state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "ok": false, "error": "recentre is only available on the target" })),
    )
}
