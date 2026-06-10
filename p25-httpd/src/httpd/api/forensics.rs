//! Track-2 forensics API: arm/disarm/status for the on-device dibit
//! ring + wideband IQ auto-trigger. See `app/forensics.rs`.

use std::sync::Arc;
use axum::extract::{Query, State};
use axum::Json;
use std::collections::HashMap;

use crate::httpd::AppState;

#[cfg(target_os = "linux")]
pub async fn get_forensics_status(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let s = state.forensics.status().await;
    Json(serde_json::to_value(&s).unwrap_or_else(|_| serde_json::json!({})))
}

#[cfg(target_os = "linux")]
pub async fn post_forensics_arm(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    use crate::app::forensics::ForensicsConfig;
    let mut cfg = ForensicsConfig::default();
    if let Some(v) = params.get("dibit_max_mb").and_then(|s| s.parse::<usize>().ok()) {
        cfg.dibit_max_bytes = v.clamp(1, 64) * 1024 * 1024;
    }
    if let Some(v) = params.get("wideband_seconds").and_then(|s| s.parse::<f64>().ok()) {
        // Cap lifted from 30 s after captures moved to /mnt/sd
        // (2026-05-03). Matches the wideband_iq_task::start() clamp.
        cfg.wideband_seconds = v.clamp(1.0, 600.0);
    }
    let auto_rearm = params.get("auto_rearm")
        .map(|s| s == "1" || s == "true").unwrap_or(true);
    let follow_encrypted = params.get("follow_encrypted")
        .map(|s| s == "1" || s == "true").unwrap_or(false);
    match state.forensics.arm(cfg, auto_rearm, follow_encrypted).await {
        Ok(()) => {
            let s = state.forensics.status().await;
            Json(serde_json::json!({
                "ok": true,
                "status": s,
                "note": "Armed. Will trigger on next CallOpen.",
            }))
        }
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": format!("{e}"),
        })),
    }
}

#[cfg(target_os = "linux")]
pub async fn post_forensics_disarm(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    state.forensics.disarm().await;
    let s = state.forensics.status().await;
    Json(serde_json::json!({
        "ok": true,
        "status": s,
        "note": "Disarmed. In-flight capture (if any) will still finalise.",
    }))
}

// Non-Linux stubs so the workspace compiles on Windows.
#[cfg(not(target_os = "linux"))]
pub async fn get_forensics_status(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "forensics only available on the target (linux/arm)",
    }))
}

#[cfg(not(target_os = "linux"))]
pub async fn post_forensics_arm(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "forensics only available on the target (linux/arm)",
    }))
}

#[cfg(not(target_os = "linux"))]
pub async fn post_forensics_disarm(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "forensics only available on the target (linux/arm)",
    }))
}
