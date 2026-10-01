//! `/api/v1/profiles`: what to follow; each site's active profile. A change to the live site's
//! active profile applies to the calls that follow.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config::profiles::Profile;
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
    let out = c.profiles.value.clone();
    drop(c);
    s.live.profile_changed().await;
    Ok(Json(out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub system: String,
    pub name: String,
    /// A profile of the same system to copy.
    pub copy_from: Option<String>,
}

pub async fn create(State(s): State<Arc<AppState>>, Json(req): Json<Create>) -> ApiResult<Profile> {
    let mut c = s.config.lock().await;
    if c.systems.value.system(&req.system).is_none() {
        return Err(ApiError::not_found(format!("system {}", req.system)));
    }
    let p = c.profiles.value.create(&req.system, &req.name, req.copy_from.as_deref()).map_err(ApiError::bad_request)?;
    config::save(&s.paths.profiles(), &c.profiles)?;
    Ok(Json(p))
}

/// Replace a profile's name, groups, speakers, monitor and ignore lists.
pub async fn update(State(s): State<Arc<AppState>>, Path(id): Path<String>, Json(req): Json<Profile>) -> ApiResult<Profile> {
    let mut c = s.config.lock().await;
    let p = c.profiles.value.update(&id, req).map_err(ApiError::bad_request)?;
    config::save(&s.paths.profiles(), &c.profiles)?;
    drop(c);
    s.live.profile_changed().await;
    Ok(Json(p))
}

pub async fn delete(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<ProfilesConfig> {
    let mut c = s.config.lock().await;
    c.profiles.value.delete(&id).map_err(ApiError::bad_request)?;
    config::save(&s.paths.profiles(), &c.profiles)?;
    Ok(Json(c.profiles.value.clone()))
}
