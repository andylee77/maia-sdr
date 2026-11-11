//! `/api/v1/files`: what is on the card as files to take away (`services::files`): the listing,
//! the recordings as one ZIP archive, the history's database, a TV recording's file.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::v1::calls::Names;
use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::atsc::stream::{self, TvRecording};
use crate::services::files::{self, Chunks, Download, Refused, Selection, Span};
use crate::services::recordings::storage::SdState;
use crate::services::recordings::StoreUsage;

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ListQuery {
    /// How long a span of the recordings' listing is, minutes: 15, 30, 60 (when none is given) or
    /// 1440, a UTC day.
    pub span: Option<u64>,
}

#[derive(Serialize)]
pub struct Card {
    pub state: SdState,
    pub detail: Option<String>,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

#[derive(Serialize)]
pub struct RecordingFiles {
    /// On the card, the locked ones among them, and in RAM.
    pub sd: StoreUsage,
    pub locked: StoreUsage,
    pub ram: StoreUsage,
    /// By the span of time their calls started in, the newest first; each is `span_ms` long.
    pub span_ms: u64,
    pub spans: Vec<Span>,
}

#[derive(Serialize)]
pub struct HistoryFile {
    pub name: &'static str,
    pub bytes: u64,
    pub on_sd: bool,
}

#[derive(Serialize)]
pub struct Listing {
    pub card: Card,
    pub recordings: RecordingFiles,
    pub history: HistoryFile,
    pub tv: Vec<TvRecording>,
    /// The download running: one at a time.
    pub download: Option<Download>,
}

fn off_runtime<T>(r: Result<T, tokio::task::JoinError>) -> Result<T, ApiError> {
    r.map_err(|e| ApiError::from(anyhow::anyhow!(e)))
}

/// What is on the card.
pub async fn list(State(s): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> ApiResult<Listing> {
    let span_ms = match q.span.unwrap_or(60) {
        m @ (15 | 30 | 60 | 1440) => m * 60_000,
        other => return Err(ApiError::bad_request(format!("span: 15, 30, 60 or 1440 minutes, not {other}"))),
    };
    let recordings = s.recordings.clone();
    let tv_dir = s.paths.tv();
    let store = s.history.store().clone();
    let (spans, tv, history_bytes) =
        off_runtime(tokio::task::spawn_blocking(move || (recordings.read(|list| files::spans(list, span_ms)), stream::recordings(&tv_dir), store.size_bytes())).await)?;
    let sum = s.recordings.summary();
    Ok(Json(Listing {
        card: Card { state: sum.sd_status.state, detail: sum.sd_status.detail, total_bytes: sum.sd_status.total_bytes, free_bytes: sum.sd_status.free_bytes },
        recordings: RecordingFiles { sd: sum.sd, locked: sum.locked, ram: sum.ram, span_ms, spans },
        history: HistoryFile { name: files::HISTORY_NAME, bytes: history_bytes, on_sd: s.history.on_sd },
        tv,
        download: s.files.download(),
    }))
}

#[derive(Serialize)]
pub struct Count {
    pub count: usize,
    pub bytes: u64,
}

/// How many recordings a selection takes, and their size: what its archive would hold.
pub async fn count(State(s): State<Arc<AppState>>, Query(sel): Query<Selection>) -> ApiResult<Count> {
    let recordings = s.recordings.clone();
    let (count, bytes) = off_runtime(tokio::task::spawn_blocking(move || recordings.read(|list| sel.count(list))).await)?;
    Ok(Json(Count { count, bytes }))
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / f64::from(1 << 20))
}

fn refused(e: Refused) -> ApiError {
    match e {
        Refused::Busy(d) => ApiError::conflict(format!("another download is running ({}, {} of {} sent): one at a time", d.name, megabytes(d.bytes_done), megabytes(d.bytes))),
        Refused::Failed(e) => ApiError { status: StatusCode::SERVICE_UNAVAILABLE, message: e },
    }
}

/// A download's response: saved as `name`; with its length when that is known before it is sent.
fn attachment(name: &str, kind: &'static str, bytes: Option<u64>, chunks: Chunks) -> Response {
    let body = Body::from_stream(futures_util::stream::unfold(chunks, |mut rx| async move { rx.recv().await.map(|c| (c, rx)) }));
    let mut res = Response::builder()
        .header(header::CONTENT_TYPE, kind)
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""))
        .header(header::CACHE_CONTROL, "no-store");
    if let Some(n) = bytes {
        res = res.header(header::CONTENT_LENGTH, n);
    }
    res.body(body).map_or_else(|e| ApiError::from(anyhow::anyhow!(e)).into_response(), IntoResponse::into_response)
}

/// The selection's recordings as one ZIP archive (stored, not compressed): each in the folder of
/// its call's UTC date, as on the card, and `recordings.csv`, the table of them with names.
pub async fn recordings_zip(State(s): State<Arc<AppState>>, Query(sel): Query<Selection>) -> Result<Response, ApiError> {
    let recordings = s.recordings.clone();
    let pick = sel.clone();
    let (picked, bytes) = off_runtime(tokio::task::spawn_blocking(move || recordings.read(|list| (pick.pick(list), pick.count(list).1))).await)?;
    if picked.is_empty() {
        return Err(ApiError { status: StatusCode::NOT_FOUND, message: "no recording is in the selection".into() });
    }
    let name = sel.archive_name(&picked);
    let count = picked.len();
    let names = Names::load(&s).await.into_indexes();
    let chunks = s.files.recordings(picked, bytes, names, name.clone()).map_err(refused)?;
    s.log.radio("files", format!("{name}: {count} recording(s), {}", megabytes(bytes)));
    Ok(attachment(&name, "application/zip", None, chunks))
}

/// The history's database (SQLite) as one consistent file. A copy is made on the card first, so
/// the answer starts when that is done.
pub async fn history(State(s): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let (bytes, chunks) = s.files.history(s.history.store().clone()).await.map_err(refused)?;
    s.log.radio("files", format!("{}: a copy of {}", files::HISTORY_NAME, megabytes(bytes)));
    Ok(attachment(files::HISTORY_NAME, "application/vnd.sqlite3", Some(bytes), chunks))
}

/// A TV recording's file, `rf<N>.ts`, as it is on the card.
pub async fn tv(State(s): State<Arc<AppState>>, Path(file): Path<String>) -> Result<Response, ApiError> {
    let rf: u8 = file.strip_prefix("rf").and_then(|f| f.strip_suffix(".ts")).and_then(|n| n.parse().ok()).ok_or_else(|| ApiError::bad_request(format!("{file:?}: a TV recording is rf<N>.ts")))?;
    let path = stream::recording_path(&s.paths.tv(), rf);
    let name = format!("rf{rf}.ts");
    let service = s.files.clone();
    let started = {
        let (path, name) = (path.clone(), name.clone());
        off_runtime(tokio::task::spawn_blocking(move || path.is_file().then(|| service.file("tv", path, name))).await)?
    };
    let (bytes, chunks) = started.ok_or_else(|| ApiError::not_found(format!("a recording of RF channel {rf}")))?.map_err(refused)?;
    s.log.radio("files", format!("{name}: {}", megabytes(bytes)));
    Ok(attachment(&name, "video/mp2t", Some(bytes), chunks))
}
