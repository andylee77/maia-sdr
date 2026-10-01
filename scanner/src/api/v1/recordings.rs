//! `/api/v1/recordings`: the call recordings, newest first, with the stores' state; one
//! recording's WAV (with byte ranges, which the browser's `<audio>` asks for); deletes.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::recordings::{Recording, Store, Summary};

#[derive(Deserialize)]
pub struct ListQuery {
    pub limit: Option<usize>,
    /// One site's recordings ("" = those made before sites were kept); all when absent.
    pub site: Option<String>,
}

#[derive(Serialize)]
pub struct List {
    pub total: usize,
    pub items: Vec<Recording>,
    pub storage: Summary,
}

pub async fn list(State(s): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Json<List> {
    let (items, total) = s.recordings.list(q.site.as_deref(), q.limit.unwrap_or(usize::MAX));
    Json(List { total, items, storage: s.recordings.summary() })
}

/// `{id}` or `{id}.wav`.
fn id_of(raw: &str) -> Result<u64, ApiError> {
    raw.trim_end_matches(".wav").parse().map_err(|_| ApiError::bad_request(format!("bad recording id {raw:?}")))
}

/// The WAV; one recording still waiting for the card is served from RAM.
pub async fn file(State(s): State<Arc<AppState>>, Path(raw): Path<String>, headers: HeaderMap) -> Result<Response, ApiError> {
    let id = id_of(&raw)?;
    let r = s.recordings.get(id).ok_or_else(|| ApiError::not_found(format!("recording {id}")))?;
    let bytes = match r.pending {
        Some(b) => b.as_ref().clone(),
        None => tokio::fs::read(&r.path)
            .await
            .map_err(|e| ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: format!("{}: {e}", r.file) })?,
    };
    let total = bytes.len() as u64;
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).and_then(|v| byte_range(v, total));
    let (status, body) = match range {
        Some((a, b)) => (StatusCode::PARTIAL_CONTENT, bytes[a as usize..=b as usize].to_vec()),
        None => (StatusCode::OK, bytes),
    };
    let mut res = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "audio/wav")
        .header(header::CONTENT_DISPOSITION, format!("inline; filename=\"{}\"", r.file))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, body.len());
    if let Some((a, b)) = range {
        res = res.header(header::CONTENT_RANGE, format!("bytes {a}-{b}/{total}"));
    }
    res.body(Body::from(body)).map_err(|e| anyhow::anyhow!(e).into())
}

/// A single `bytes=<start>-[<end>]` range inside `total`; anything else is served whole.
fn byte_range(v: &str, total: u64) -> Option<(u64, u64)> {
    let (a, b) = v.strip_prefix("bytes=")?.split_once('-')?;
    let a: u64 = a.parse().ok()?;
    let b: u64 = if b.is_empty() { total.checked_sub(1)? } else { b.parse().ok()? };
    (a <= b && a < total).then(|| (a, b.min(total - 1)))
}

#[derive(Serialize)]
pub struct Deleted {
    pub deleted: usize,
}

pub async fn delete(State(s): State<Arc<AppState>>, Path(raw): Path<String>) -> ApiResult<Deleted> {
    let id = id_of(&raw)?;
    if !s.recordings.delete(id) {
        return Err(ApiError::not_found(format!("recording {id}")));
    }
    Ok(Json(Deleted { deleted: 1 }))
}

#[derive(Deserialize)]
pub struct ClearQuery {
    /// `sd`, `ram` or `all`.
    pub store: String,
}

/// Delete every recording of a store; a call still being recorded is kept.
pub async fn clear(State(s): State<Arc<AppState>>, Query(q): Query<ClearQuery>) -> ApiResult<Deleted> {
    let store = match q.store.as_str() {
        "sd" => Some(Store::Sd),
        "ram" => Some(Store::Ram),
        "all" => None,
        other => return Err(ApiError::bad_request(format!("store {other:?}: expected sd, ram or all"))),
    };
    let deleted = s.recordings.clear(store);
    s.log.system("recordings", format!("deleted {deleted} recording(s) ({})", q.store));
    Ok(Json(Deleted { deleted }))
}

#[cfg(test)]
mod tests {
    use super::byte_range;

    #[test]
    fn ranges() {
        assert_eq!(byte_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(byte_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(byte_range("bytes=90-200", 100), Some((90, 99)));
        assert_eq!(byte_range("bytes=100-", 100), None);
        assert_eq!(byte_range("bytes=5-1", 100), None);
        assert_eq!(byte_range("bytes=0-", 0), None);
        assert_eq!(byte_range("items=0-1", 100), None);
    }
}
