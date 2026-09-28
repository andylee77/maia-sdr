//! Change 071: the system finder (`app::discovery`).
//!
//! - `GET /api/discovery`: the sweep's progress and results.
//! - `POST /api/discovery/scan` `{bands?, all?, frames?, probe_ms?,
//!   identity_ms?, max_candidates?}`: start a sweep (409 while one runs).
//!   The radio is taken until it ends, then goes back to the active site.
//! - `POST /api/discovery/cancel`: stop after the current step.
//! - `POST /api/discovery/add` `{key, label?, name?}`: a found site (by
//!   its "WACN-system-RFSS-site" key) becomes a site file.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;

use crate::app::discovery::{site_name, to_site, ScanRequest};
use crate::httpd::AppState;

pub async fn get_discovery(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let d = state.discovery.lock().map(|d| d.clone()).unwrap_or_default();
    let mut v = serde_json::to_value(&d).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("ok".into(), true.into());
        o.insert("radio_busy".into(), (!state.radio_lease.is_normal()).into());
        let keys: Vec<String> = d.sites.iter().map(|s| s.key()).collect();
        o.insert("keys".into(), keys.into());
    }
    Json(v)
}

pub async fn post_scan(
    State(state): State<Arc<AppState>>,
    body: Option<Json<ScanRequest>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    if req.bands().iter().any(|(a, b)| a >= b || *a < 70_000_000 || *b > 6_000_000_000) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "error": "bands: [low, high] in Hz, 70 MHz - 6 GHz" })));
    }
    #[cfg(target_os = "linux")]
    {
        match crate::app::discovery::spawn_scan(state.clone(), req) {
            Ok(id) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "id": id }))),
            Err(e) => (StatusCode::CONFLICT, Json(serde_json::json!({ "ok": false, "error": e }))),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (state, req);
        (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "ok": false, "error": "the sweep needs the target" })))
    }
}

pub async fn post_cancel(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let running = !state.radio_lease.is_normal();
    if let Ok(mut d) = state.discovery.lock() {
        d.cancel = true;
    }
    Json(serde_json::json!({ "ok": true, "running": running }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddBody {
    pub key: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

pub async fn post_add(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let found = state
        .discovery
        .lock()
        .ok()
        .and_then(|d| d.sites.iter().find(|s| s.key() == body.key).cloned());
    let Some(found) = found else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "ok": false, "error": format!("no found site {}", body.key) })));
    };
    let label = body
        .label
        .map(|l| l.trim().chars().take(64).collect::<String>())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| {
            format!(
                "P25 system {:03X} site {}-{}",
                found.system_id.unwrap_or(0),
                found.rfss_id.unwrap_or(0),
                found.site_id.unwrap_or(0)
            )
        });
    let name = site_name(body.name.as_deref().unwrap_or(&label));
    if crate::services::sites::list_sites().contains(&name) {
        return (StatusCode::CONFLICT, Json(serde_json::json!({ "ok": false, "error": format!("a site named {name} exists") })));
    }
    let site = to_site(&found, &name, &label);
    if let Err(e) = crate::services::sites::save_site(&site) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "ok": false, "error": e.to_string() })));
    }
    if let Ok(mut d) = state.discovery.lock() {
        if let Some(s) = d.sites.iter_mut().find(|s| s.key() == body.key) {
            s.existing_site = Some(name.clone());
        }
    }
    state.event_log.push(
        crate::services::event_log::LogCategory::System,
        format!("system finder: site {name} added ({label}, control channel {:.5} MHz)", found.freq_hz as f64 / 1e6),
        serde_json::json!({ "name": name, "key": body.key }),
    );
    (StatusCode::OK, Json(serde_json::json!({ "ok": true, "name": name, "label": label })))
}
