//! `/api/v1/analog`: the analog receiver (`services::analog`): what it is tuned to and hears,
//! and tuning it. Its audio is `/ws/analog`.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::analog::{Tuned, View};

pub async fn get(State(s): State<Arc<AppState>>) -> Json<View> {
    Json(s.analog.view())
}

/// Tune the receiver: its frequency, how it is demodulated and its squelch.
pub async fn put_tune(State(s): State<Arc<AppState>>, Json(to): Json<Tuned>) -> ApiResult<View> {
    s.analog.tune(to).map_err(ApiError::bad_request)?;
    Ok(Json(s.analog.view()))
}
