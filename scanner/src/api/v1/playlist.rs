//! `/api/v1/playlist`: the configuration as an SDRTrunk playlist, and a playlist taken into it
//! (its systems, their channels as sites, their alias lists), after a preview of what it would
//! change.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config;
use crate::services::config::sdrtrunk::import::{ImportRequest, Imported};
use crate::services::config::sdrtrunk::{self, import};
use crate::trunking::site::LiveState;

#[derive(Deserialize)]
pub struct ExportParams {
    #[serde(default)]
    pub download: bool,
}

/// Every system as a playlist file SDRTrunk reads.
pub async fn export(State(s): State<Arc<AppState>>, Query(p): Query<ExportParams>) -> Response {
    let xml = sdrtrunk::export(&s.config.lock().await.systems.value.systems);
    let mut r = ([(header::CONTENT_TYPE, "application/xml; charset=utf-8")], xml).into_response();
    if p.download {
        r.headers_mut().insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"scanner-playlist.xml\""));
    }
    r
}

/// What importing a playlist would change; nothing is saved.
pub async fn preview(State(s): State<Arc<AppState>>, Json(req): Json<ImportRequest>) -> ApiResult<Imported> {
    Ok(Json(run(&s, &req, false).await?))
}

/// Import a playlist. The live site follows new aliases at once, and goes live again when it
/// gained frequencies.
pub async fn put(State(s): State<Arc<AppState>>, Json(req): Json<ImportRequest>) -> ApiResult<Imported> {
    let out = run(&s, &req, true).await?;
    s.log.system(
        "config",
        format!(
            "playlist imported: {} systems and {} sites added, {} sites given frequencies, {} aliases added, {} named",
            out.systems_added.len(),
            out.sites_added.len(),
            out.sites_updated.len(),
            out.aliases_added,
            out.aliases_named
        ),
    );
    if out.aliases_added + out.aliases_named > 0 {
        s.live.aliases_changed().await;
    }
    if let LiveState::Live(l) = s.live.state() {
        if out.sites_updated.contains(&l.site.id) {
            s.live.activate(&l.site.id).await?;
        }
    }
    Ok(Json(out))
}

async fn run(s: &AppState, req: &ImportRequest, save: bool) -> Result<Imported, ApiError> {
    let playlist = sdrtrunk::parse(&req.xml).map_err(ApiError::bad_request)?;
    let mut c = s.config.lock().await;
    let mut next = c.systems.value.clone();
    let out = import::import(&mut next, &playlist, req.systems.as_deref()).map_err(ApiError::bad_request)?;
    if save {
        let before = std::mem::replace(&mut c.systems.value, next);
        if let Err(e) = config::save(&s.paths.systems(), &c.systems) {
            c.systems.value = before;
            return Err(e.into());
        }
    }
    Ok(out)
}
