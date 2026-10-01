//! `/api/v1/scan`: find the systems on the air (`services::discovery`), and add what was found.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config::{self, Config};
use crate::services::discovery::{self, AddSite, Added, ScanRequest, ScanState};
use crate::trunking::site::LiveState;

pub async fn get(State(s): State<Arc<AppState>>) -> Json<ScanState> {
    Json(s.discovery.state())
}

#[derive(Serialize)]
pub struct Started {
    pub id: u64,
}

/// Start a scan (the body may be empty: the default bands). The live site pauses until it ends.
pub async fn start(State(s): State<Arc<AppState>>, body: Option<Json<ScanRequest>>) -> ApiResult<Started> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    if req.bands().iter().any(|&(a, b)| a >= b || a < 70_000_000 || b > 6_000_000_000) {
        return Err(ApiError::bad_request("bands: (low, high) in Hz within 70 MHz to 6 GHz"));
    }
    if !(1..=64).contains(&req.frames)
        || !(500..=30_000).contains(&req.probe_ms)
        || req.identity_ms > 60_000
        || !(1..=500).contains(&req.max_candidates)
    {
        return Err(ApiError::bad_request("frames 1..=64, probe_ms 500..=30000, identity_ms up to 60000, max_candidates 1..=500"));
    }
    let systems = s.config.lock().await.systems.value.clone();
    let id = s
        .discovery
        .start(req, &s.lease, s.live.clone(), s.tuner.clone(), systems, s.log.clone())
        .map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    Ok(Json(Started { id }))
}

pub async fn cancel(State(s): State<Arc<AppState>>) -> Json<ScanState> {
    s.discovery.cancel();
    Json(s.discovery.state())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddRequest {
    pub sites: Vec<AddSite>,
}

/// Add the ticked sites of the last scan. With no live site, the first one added goes live.
pub async fn add(State(s): State<Arc<AppState>>, Json(req): Json<AddRequest>) -> ApiResult<Added> {
    let scan = s.discovery.state();
    if scan.running() {
        return Err(ApiError::conflict("the scan is still running"));
    }
    let added = {
        let mut c = s.config.lock().await;
        let mut systems = c.systems.value.clone();
        let added = discovery::add(&mut systems, &scan.sites, &req.sites).map_err(ApiError::bad_request)?;
        c.systems.value = systems;
        config::save(&s.paths.systems(), &c.systems)?;
        // What a P25 site announced of its band plan seeds its control decoders at activation.
        for f in &scan.sites {
            let Some(id) = req.sites.iter().find(|a| a.key == f.key()).and(discovery::existing_site(f, &c.systems.value)) else {
                continue;
            };
            if f.bands.is_empty() {
                continue;
            }
            let mut state = Config::site_state(&s.paths, &id)?;
            if state.value.iden_bands.is_empty() {
                state.value.iden_bands = f.bands.clone();
                config::save(&s.paths.site_state(&id), &state)?;
            }
        }
        added
    };
    s.log.system(
        "scan",
        format!("added {} systems and {} sites; {} known sites gained alternate channels", added.systems.len(), added.sites.len(), added.updated.len()),
    );
    if matches!(s.live.state(), LiveState::NoSite) {
        if let Some(first) = added.sites.first() {
            s.live.activate(first).await?;
        }
    }
    Ok(Json(added))
}
