//! Change 075: `/api/dmr`, DMR receive on the control channel.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::httpd::AppState;

/// `GET /api/dmr`: the DMR path's counters (`DmrRuntime::snapshot`).
pub async fn get_dmr(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(state.dmr_rt.snapshot())
}

/// `PUT /api/dmr?enabled=1|0`: run the DMR demodulator on the control IQ
/// (beside the P25 decoders) or stop it. Enabling zeroes the counters.
pub async fn put_dmr(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let on = match params.get("enabled").map(|s| s.as_str()) {
        Some("1") | Some("true") => true,
        Some("0") | Some("false") => false,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": "expected ?enabled=1 or ?enabled=0" })),
            );
        }
    };
    let rt = &state.dmr_rt;
    if on && !rt.enabled.load(Ordering::Relaxed) {
        rt.clear();
    }
    rt.enabled.store(on, Ordering::Relaxed);
    let mut body = rt.snapshot();
    body["ok"] = serde_json::Value::Bool(true);
    (StatusCode::OK, Json(body))
}

/// `GET /api/dmr/messages?n=100[&class=Grant]`: the last decoded messages
/// (oldest first) in SDRTrunk's text; `class` keeps class names containing it.
pub async fn get_dmr_messages(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let n = params.get("n").and_then(|v| v.parse().ok()).unwrap_or(100usize).min(500);
    let class = params.get("class").map(|s| s.as_str());
    Json(serde_json::json!({ "messages": state.dmr_rt.recent_messages(n, class) }))
}
