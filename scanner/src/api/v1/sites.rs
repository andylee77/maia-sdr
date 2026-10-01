//! `/api/v1/sites`: every site, and the switch.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config::systems::{Protocol, Site};
use crate::trunking::site::{Live, LiveState};

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
