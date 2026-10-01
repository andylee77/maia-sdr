//! `/api/v1/systems`: the configured systems and their sites.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config::systems::System;

pub async fn list(State(s): State<Arc<AppState>>) -> Json<Vec<System>> {
    Json(s.config.lock().await.systems.value.systems.clone())
}

pub async fn get(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<System> {
    let c = s.config.lock().await;
    c.systems.value.system(&id).cloned().map(Json).ok_or_else(|| ApiError::not_found(format!("system {id}")))
}
