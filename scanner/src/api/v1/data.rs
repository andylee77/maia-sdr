//! `/api/v1/data`: data mode's listening (`services::data`): what it hears, the devices its
//! decoders have read, the bursts whose IQ it is to keep, and the captures on the card.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::data::captures::Listing;
use crate::services::data::{ArmError, DeviceLog, View};

pub async fn get(State(s): State<Arc<AppState>>) -> Json<View> {
    Json(s.data.view())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmBody {
    /// Bursts to keep.
    pub count: u32,
    /// Only those around this frequency.
    #[serde(default)]
    pub freq_hz: Option<u64>,
    /// How wide "around" is, edge to edge.
    #[serde(default)]
    pub span_hz: Option<u32>,
}

/// Keep the IQ of the next bursts.
pub async fn put_arm(State(s): State<Arc<AppState>>, Json(b): Json<ArmBody>) -> ApiResult<View> {
    s.data.arm(b.count, b.freq_hz, b.span_hz).map_err(|e| match e {
        ArmError::Invalid(m) => ApiError::bad_request(m),
        ArmError::Unable(m) => ApiError::conflict(m),
    })?;
    Ok(Json(s.data.view()))
}

pub async fn delete_arm(State(s): State<Arc<AppState>>) -> Json<View> {
    s.data.disarm();
    Json(s.data.view())
}

/// A device the decoders have read, with the last things it said.
pub async fn device(State(s): State<Arc<AppState>>, Path(key): Path<String>) -> ApiResult<DeviceLog> {
    s.data.device(&key).map(Json).ok_or_else(|| ApiError::not_found(format!("device {key}")))
}

fn no_captures() -> ApiError {
    ApiError::not_found("a place for captures on this unit")
}

pub async fn captures(State(s): State<Arc<AppState>>) -> ApiResult<Listing> {
    s.data.captures().map(|c| Json(c.listing())).ok_or_else(no_captures)
}

/// One of a capture's two files.
pub async fn capture_file(State(s): State<Arc<AppState>>, Path(name): Path<String>) -> Result<Response, ApiError> {
    let path = s.data.captures().ok_or_else(no_captures)?.file(&name).ok_or_else(|| ApiError::not_found(format!("capture file {name}")))?;
    let bytes = tokio::task::spawn_blocking(move || std::fs::read(path)).await.map_err(|e| ApiError::from(anyhow::anyhow!(e)))?.map_err(|e| ApiError::from(anyhow::anyhow!(e)))?;
    let kind = if name.ends_with(".sigmf-meta") { "application/json" } else { "application/octet-stream" };
    Ok((
        [
            (header::CONTENT_TYPE, kind.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response())
}

/// Delete a capture by its id.
pub async fn delete_capture(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Listing> {
    let data = s.data.clone();
    let gone = {
        let id = id.clone();
        tokio::task::spawn_blocking(move || data.delete_capture(&id)).await.map_err(|e| ApiError::from(anyhow::anyhow!(e)))?
    };
    if !gone {
        return Err(ApiError::not_found(format!("capture {id}")));
    }
    captures(State(s)).await
}
