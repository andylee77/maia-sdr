//! `/api/v1/config`: the configuration exported as one document, imported back, or reset to a
//! new unit's. An import or a reset restarts the scanner, so every setting applies as at a start.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::boot::version::BUILD_TAG;
use crate::radio::lease::{Lease, LeaseGuard};
use crate::services::config::{self, ConfigDoc};

/// Time for the response to go out before the restart.
const RESTART_AFTER: Duration = Duration::from_millis(500);
/// How long a reset waits for the history writer to catch up before clearing it.
const HISTORY_FLUSH: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
pub struct ExportParams {
    #[serde(default)]
    pub download: bool,
}

pub async fn export(State(s): State<Arc<AppState>>, Query(p): Query<ExportParams>) -> Response {
    let doc = s.config.lock().await.export(BUILD_TAG);
    let mut r = Json(doc).into_response();
    if p.download {
        r.headers_mut().insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"scanner-config.json\""));
    }
    r
}

#[derive(Serialize)]
pub struct Imported {
    pub systems: usize,
    pub sites: usize,
    pub aliases: usize,
    pub live_site: Option<String>,
    /// Sites the document does not have; what they learned is deleted.
    pub removed_sites: Vec<String>,
    pub restarting: bool,
}

/// Replace the configuration with `doc`. Nothing changes unless the whole document checks out.
pub async fn import(State(s): State<Arc<AppState>>, Json(doc): Json<ConfigDoc>) -> ApiResult<Imported> {
    let lease = take_radio(&s)?;
    s.config.lock().await.clone().import(doc.clone()).map_err(ApiError::bad_request)?;
    s.live.stop_discarding().await;
    let out = {
        let mut c = s.config.lock().await;
        let removed_sites = c.import(doc).map_err(ApiError::bad_request)?;
        c.save_all(&s.paths)?;
        let systems = &c.systems.value.systems;
        Imported {
            systems: systems.len(),
            sites: systems.iter().map(|x| x.sites.len()).sum(),
            aliases: systems.iter().map(|x| x.aliases.len()).sum(),
            live_site: c.state.value.live_site.clone(),
            removed_sites,
            restarting: true,
        }
    };
    config::forget_sites(&s.paths, &out.removed_sites);
    s.log.system("config", format!("configuration imported: {} systems, {} sites, {} aliases; restarting", out.systems, out.sites, out.aliases));
    restart_soon(&s, lease);
    Ok(Json(out))
}

#[derive(Serialize)]
pub struct Reset {
    pub sites: usize,
    pub recordings: usize,
    pub calls: usize,
    pub restarting: bool,
}

/// Back to a new unit: no systems, sites, aliases, recordings or history, and the default
/// settings. The crystal calibration and the TLS certificates stay: they are the board's.
pub async fn factory_reset(State(s): State<Arc<AppState>>) -> ApiResult<Reset> {
    let lease = take_radio(&s)?;
    s.live.stop_discarding().await;
    let recordings = s.recordings.clear(None);
    s.history.flush(HISTORY_FLUSH).await;
    let calls = s.history.query(|st| st.clear()).await?;
    let gone = {
        let mut c = s.config.lock().await;
        let gone = c.factory();
        c.save_all(&s.paths)?;
        gone
    };
    config::forget_sites(&s.paths, &gone);
    s.log.system("config", format!("factory reset: {} sites, {recordings} recordings and {calls} calls removed; restarting", gone.len()));
    restart_soon(&s, lease);
    Ok(Json(Reset { sites: gone.len(), recordings, calls, restarting: true }))
}

/// The radio lease, so no scan or site switch runs while the configuration is replaced.
fn take_radio(s: &AppState) -> Result<LeaseGuard, ApiError> {
    s.lease.take(Lease::Switching).ok_or_else(|| ApiError::conflict("the radio is busy (a scan or a site switch)"))
}

/// Restart once the response is out, holding the lease to the end.
fn restart_soon(s: &Arc<AppState>, lease: LeaseGuard) {
    let restart = s.restart.clone();
    tokio::spawn(async move {
        let _lease = lease;
        tokio::time::sleep(RESTART_AFTER).await;
        restart.notify_one();
        std::future::pending::<()>().await;
    });
}
