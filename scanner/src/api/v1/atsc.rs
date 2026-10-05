//! `/api/v1/atsc`: ATSC TV mode's channel finder (`services::atsc`).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::api::v1::scan::Started;
use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::radio::tuner::RadioHw;
use crate::services::atsc::live::LiveStats;
use crate::services::atsc::stream::{self, TvRecording};
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

/// The TV recordings a player can be served.
pub async fn recordings(State(s): State<Arc<AppState>>) -> ApiResult<Vec<TvRecording>> {
    let dir = s.paths.tv();
    tokio::task::spawn_blocking(move || stream::recordings(&dir)).await.map(Json).map_err(|e| ApiError::from(anyhow::anyhow!(e)))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamQuery {
    /// One programme alone; none: the whole multiplex.
    pub program: Option<u16>,
}

/// An RF channel's transport stream as it plays: live (`services::atsc::live`), or its recording
/// (`services::atsc::stream`).
pub async fn stream(State(s): State<Arc<AppState>>, Path(rf): Path<u8>, Query(q): Query<StreamQuery>) -> Result<Response, ApiError> {
    let live = s.atsc.live_stream(rf, s.tuner.clone(), s.log.clone()).await;
    if let Ok(Some(rx)) = live {
        let (tx, out) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(stream::relay(rx, q.program, tx));
        return Ok(ts_response(out));
    }
    let path = stream::recording_path(&s.paths.tv(), rf);
    let listed = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || path.is_file().then(|| stream::programs(&path))).await.map_err(|e| ApiError::from(anyhow::anyhow!(e)))?
    };
    let Some(programs) = listed else {
        return Err(match live {
            Err(e) => ApiError::conflict(format!("{e:#}")),
            Ok(_) => ApiError::not_found(format!("live TV or a recording of RF channel {rf}")),
        });
    };
    if let Some(p) = q.program.filter(|p| !programs.contains(p)) {
        return Err(ApiError::not_found(format!("program {p} in RF channel {rf}'s recording (it has {programs:?})")));
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    tokio::spawn(async move {
        if let Err(e) = stream::play(path, q.program, tx).await {
            tracing::warn!("TV stream of RF {rf}: {e:#}");
        }
    });
    Ok(ts_response(rx))
}

fn ts_response(rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Response {
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|c| (Ok::<_, std::io::Error>(c), rx)) }));
    ([(header::CONTENT_TYPE, "video/mp2t"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

/// Live TV's state.
pub async fn live(State(s): State<Arc<AppState>>) -> Json<LiveStats> {
    let mut stats = s.atsc.live.stats();
    stats.available = s.tuner.hw().vsb_present().await;
    Json(stats)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveRequest {
    pub rf: u8,
}

/// Play an RF channel live (ATSC mode).
pub async fn live_start(State(s): State<Arc<AppState>>, Json(req): Json<LiveRequest>) -> ApiResult<LiveStats> {
    s.atsc.live_start(req.rf, s.tuner.clone(), s.log.clone()).await.map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    Ok(Json(s.atsc.live.stats()))
}

pub async fn live_stop(State(s): State<Arc<AppState>>) -> Json<LiveStats> {
    s.atsc.live.stop().await;
    Json(s.atsc.live.stats())
}
