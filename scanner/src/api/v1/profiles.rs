//! `/api/v1/profiles`: what to follow; each site's active profile.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config::{self, ProfilesConfig};

pub async fn list(State(s): State<Arc<AppState>>) -> Json<ProfilesConfig> {
    Json(s.config.lock().await.profiles.value.clone())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Select {
    pub profile: String,
}

/// A site's active profile must belong to the site's system.
pub async fn select(
    State(s): State<Arc<AppState>>,
    Path(site): Path<String>,
    Json(req): Json<Select>,
) -> ApiResult<ProfilesConfig> {
    let mut c = s.config.lock().await;
    let (system, _) = c.systems.value.site(&site).ok_or_else(|| ApiError::not_found(format!("site {site}")))?;
    let system = system.id.clone();
    let profile = c.profiles.value.profile(&req.profile).ok_or_else(|| ApiError::not_found(format!("profile {}", req.profile)))?;
    if profile.system != system {
        return Err(ApiError::bad_request(format!("profile {} belongs to system {}, not {system}", profile.id, profile.system)));
    }
    c.profiles.value.active.insert(site, req.profile);
    config::save(&s.paths.profiles(), &c.profiles)?;
    Ok(Json(c.profiles.value.clone()))
}
