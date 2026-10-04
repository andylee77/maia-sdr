//! `/api/v1/diag/burst`: the radio core's burst timing check (`services::burst`): where one
//! burst starts in a spectrum frame's header, in the control lane's packets and in the capture.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::burst::{self, BurstRequest, BurstState};

pub async fn get(State(s): State<Arc<AppState>>) -> Json<BurstState> {
    Json(s.burst.state())
}

#[derive(Serialize)]
pub struct Started {
    pub id: u64,
}

/// Start the check. The live site pauses until it ends.
pub async fn start(State(s): State<Arc<AppState>>, Json(req): Json<BurstRequest>) -> ApiResult<Started> {
    if !(70_000_000..=6_000_000_000).contains(&req.freq_hz) {
        return Err(ApiError::bad_request("freq_hz within 70 MHz to 6 GHz"));
    }
    if !(1..=600).contains(&req.wait_s) || !(3.0..=60.0).contains(&req.threshold_db) || !(10_000..=400_000).contains(&req.span_hz) {
        return Err(ApiError::bad_request("wait_s 1..=600, threshold_db 3..=60, span_hz 10000..=400000"));
    }
    if s.burst.state().running() {
        return Err(ApiError::conflict("a burst check is running"));
    }
    let id = burst::start(&s.burst, req, &s.lease, s.live.clone(), s.tuner.clone()).map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    Ok(Json(Started { id }))
}

pub async fn cancel(State(s): State<Arc<AppState>>) -> Json<BurstState> {
    s.burst.cancel();
    Json(s.burst.state())
}
