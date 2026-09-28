//! Change 070: the receive window against the active site's channels.
//!
//! - `GET /api/site/plan`: the live window, every channel (listed in the
//!   site file or granted) with its grant count and whether the window
//!   covers it, and the planner's best window.
//! - `PUT /api/site/plan` `{"auto": bool}`: recentre automatically when
//!   idle (per site, saved).
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
    pub auto: bool,
}

pub async fn put_site_plan(
    State(state): State<Arc<AppState>>,
    Json(p): Json<PlanPatch>,
) -> (StatusCode, Json<serde_json::Value>) {
    state.lo_plans.edit(|s| s.auto = p.auto);
    let saved = state.lo_plans.flush();
    state.event_log.push(
        crate::services::event_log::LogCategory::System,
        format!("site {}: auto recentre {}", state.lo_plans.site(), if p.auto { "on" } else { "off" }),
        serde_json::json!({ "auto": p.auto }),
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "auto": p.auto, "save_error": saved.err() })),
    )
}

#[cfg(target_os = "linux")]
pub async fn post_recentre(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
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
