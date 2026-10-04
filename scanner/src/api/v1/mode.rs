//! `/api/v1/mode`: the unit's mode (`services::mode`), the scanner or ATSC TV.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::mode::{Deps, Mode};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeBody {
    pub mode: Mode,
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<ModeBody> {
    Json(ModeBody { mode: s.modes.current() })
}

/// Change mode. Returns once the radio is the new mode's (into scanner mode: once the paused site
/// is live again).
pub async fn put(State(s): State<Arc<AppState>>, Json(b): Json<ModeBody>) -> ApiResult<ModeBody> {
    let deps = Deps { lease: &s.lease, live: &s.live, tuner: &s.tuner, atsc: &s.atsc, config: &s.config, paths: &s.paths, log: &s.log };
    s.modes.set(b.mode, None, deps).await.map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    Ok(Json(ModeBody { mode: s.modes.current() }))
}
