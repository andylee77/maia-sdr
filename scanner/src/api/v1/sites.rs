//! `/api/v1/sites`: every site, the switch, and the live site's receive window.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config::systems::{Protocol, Site};
use crate::services::config::SiteState;
use crate::radio::plan::WindowPlan;
use crate::trunking::site::{Live, LiveState, WindowView};

#[derive(Serialize)]
pub struct SiteEntry {
    pub system: String,
    pub system_label: String,
    pub protocol: Protocol,
    pub live: bool,
    #[serde(flatten)]
    pub site: Site,
}

pub async fn list(State(s): State<Arc<AppState>>) -> Json<Vec<SiteEntry>> {
    let live = match s.live.state() {
        LiveState::Live(l) => Some(l.site.id),
        _ => None,
    };
    let c = s.config.lock().await;
    let sites = c
        .systems
        .value
        .systems
        .iter()
        .flat_map(|sys| {
            sys.sites.iter().map(|site| SiteEntry {
                system: sys.id.clone(),
                system_label: sys.label.clone(),
                protocol: sys.protocol,
                live: live.as_deref() == Some(site.id.as_str()),
                site: site.clone(),
            })
        })
        .collect();
    Json(sites)
}

pub async fn activate(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Live> {
    if s.config.lock().await.systems.value.site(&id).is_none() {
        return Err(ApiError::not_found(format!("site {id}")));
    }
    if !s.lease.is_normal() {
        return Err(ApiError::conflict("the radio is busy (a scan or another switch)"));
    }
    Ok(Json(s.live.activate(&id).await?))
}

/// The live site's window against its channels, and the planner's choice.
pub async fn plan(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<WindowView> {
    match s.live.window_view().await {
        Some(v) if v.site == id => Ok(Json(v)),
        _ => Err(ApiError::conflict(format!("site {id} is not live"))),
    }
}

#[derive(Serialize)]
pub struct Recentred {
    /// The window moved to; `None` when it already was the planner's choice.
    pub moved_to: Option<WindowPlan>,
}

/// Move the live site's window to the planner's choice now (both lanes idle).
pub async fn recentre(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Recentred> {
    if !matches!(s.live.state(), LiveState::Live(l) if l.site.id == id) {
        return Err(ApiError::conflict(format!("site {id} is not live")));
    }
    let moved_to = s.live.recentre(true, "by hand").await.map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    Ok(Json(Recentred { moved_to }))
}

/// What a site taught the radio: band plan, grant counts, encrypted talkgroups, neighbours and
/// the other channels it announces.
pub async fn learned(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<SiteState> {
    if s.config.lock().await.systems.value.site(&id).is_none() {
        return Err(ApiError::not_found(format!("site {id}")));
    }
    Ok(Json(s.live.learned(&id).await?))
}
