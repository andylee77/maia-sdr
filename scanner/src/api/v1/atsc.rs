//! `/api/v1/atsc`: ATSC TV mode's channel finder (`services::atsc`).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use crate::api::v1::scan::Started;
use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::atsc::{self, AtscOptions, AtscRequest, AtscScan, ChannelSpectrum};

pub async fn get(State(s): State<Arc<AppState>>) -> Json<AtscScan> {
    Json(s.atsc.state())
}

/// One channel's spectrum as the last scan read it.
pub async fn channel(State(s): State<Arc<AppState>>, Path(n): Path<u8>) -> ApiResult<ChannelSpectrum> {
    s.atsc.channel_spectrum(n).map(Json).ok_or_else(|| ApiError::not_found(format!("RF channel {n} in the last TV scan")))
}

/// The channel plan, the default settings and the window read at once.
pub async fn options() -> Json<AtscOptions> {
    Json(atsc::options())
}

/// Scan the TV channels (the body may be empty: every channel, the AGC). ATSC mode only.
pub async fn start(State(s): State<Arc<AppState>>, body: Option<Json<AtscRequest>>) -> ApiResult<Started> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    req.check().map_err(ApiError::bad_request)?;
    let id = s.atsc.start(req, s.tuner.clone(), s.log.clone()).map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    Ok(Json(Started { id }))
}

pub async fn cancel(State(s): State<Arc<AppState>>) -> Json<AtscScan> {
    s.atsc.cancel();
    Json(s.atsc.state())
}
