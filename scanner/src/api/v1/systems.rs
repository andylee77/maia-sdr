//! `/api/v1/systems`: the configured systems and their sites; a system's names; the site editor;
//! RadioReference imports; removing a system or a site.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::hardware::presets::find_preset;
use crate::services::config;
use crate::services::config::radioreference::{self, ImportRequest, Imported};
use crate::services::config::systems::{ChannelPlan, Control, Modulation, Site, System, Window};
use crate::services::config::Removed;
use crate::trunking::site::LiveState;

pub async fn list(State(s): State<Arc<AppState>>) -> Json<Vec<System>> {
    Json(s.config.lock().await.systems.value.systems.clone())
}

pub async fn get(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<System> {
    let c = s.config.lock().await;
    c.systems.value.system(&id).cloned().map(Json).ok_or_else(|| ApiError::not_found(format!("system {id}")))
}

/// What the site editor changes: everything but the site's id, identity and origin.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteEdit {
    pub label: String,
    pub control: Control,
    #[serde(default)]
    pub modulation: Modulation,
    #[serde(default)]
    pub channels_hz: Vec<u64>,
    #[serde(default)]
    pub channel_plan: Option<ChannelPlan>,
    #[serde(default)]
    pub window: Window,
    #[serde(default)]
    pub notes: Vec<String>,
}

fn check_freq(what: &str, hz: u64) -> Result<(), ApiError> {
    if (70_000_000..=6_000_000_000).contains(&hz) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!("{what}: {hz} Hz is outside 70 MHz to 6 GHz")))
    }
}

/// Edit a site; the live site is made live again with the change.
pub async fn put_site(State(s): State<Arc<AppState>>, Path((system, site)): Path<(String, String)>, Json(req): Json<SiteEdit>) -> ApiResult<Site> {
    if req.label.trim().is_empty() {
        return Err(ApiError::bad_request("a site needs a name"));
    }
    check_freq("control channel", req.control.freq_hz)?;
    for &f in req.control.alternates_hz.iter().chain(&req.channels_hz) {
        check_freq("channel", f)?;
    }
    for (&lcn, &f) in req.channel_plan.iter().flat_map(|p| p.lcn_hz.iter()) {
        check_freq(&format!("LCN {lcn}"), f)?;
    }
    if let Some(p) = &req.window.min_preset {
        find_preset(p).ok_or_else(|| ApiError::bad_request(format!("no preset {p}")))?;
    }
    let out = {
        let mut c = s.config.lock().await;
        let sys = c.systems.value.systems.iter_mut().find(|x| x.id == system).ok_or_else(|| ApiError::not_found(format!("system {system}")))?;
        let x = sys.sites.iter_mut().find(|x| x.id == site).ok_or_else(|| ApiError::not_found(format!("site {site}")))?;
        x.label = req.label.trim().to_string();
        x.control = req.control;
        x.modulation = req.modulation;
        x.channels_hz = req.channels_hz;
        x.channel_plan = req.channel_plan.filter(|p| !p.lcn_hz.is_empty());
        x.window = req.window;
        x.notes = req.notes;
        let out = x.clone();
        config::save(&s.paths.systems(), &c.systems)?;
        out
    };
    if matches!(s.live.state(), LiveState::Live(l) if l.site.id == site) {
        s.live.activate(&site).await?;
    }
    Ok(Json(out))
}

/// Import a RadioReference CSV into a system. The live site follows new aliases at once, and goes
/// live again when it gained channels.
pub async fn import_radioreference(State(s): State<Arc<AppState>>, Path(id): Path<String>, Json(req): Json<ImportRequest>) -> ApiResult<Imported> {
    let out = radioreference_import(&s, &id, &req, true).await?;
    if !out.aliases_added.is_empty() {
        s.live.aliases_changed().await;
    }
    if let LiveState::Live(l) = s.live.state() {
        if out.sites_updated.iter().any(|u| u.site.id == l.site.id) {
            s.live.activate(&l.site.id).await?;
        }
    }
    Ok(Json(out))
}

/// What importing a RadioReference CSV would change; nothing is saved.
pub async fn preview_radioreference(State(s): State<Arc<AppState>>, Path(id): Path<String>, Json(req): Json<ImportRequest>) -> ApiResult<Imported> {
    Ok(Json(radioreference_import(&s, &id, &req, false).await?))
}

async fn radioreference_import(s: &AppState, id: &str, req: &ImportRequest, save: bool) -> Result<Imported, ApiError> {
    let file = radioreference::parse(&req.csv).map_err(ApiError::bad_request)?;
    let mut c = s.config.lock().await;
    let site_ids: Vec<String> = c.systems.value.systems.iter().flat_map(|x| x.sites.iter().map(|y| y.id.clone())).collect();
    let sys = c.systems.value.systems.iter_mut().find(|x| x.id == id).ok_or_else(|| ApiError::not_found(format!("system {id}")))?;
    let mut next = sys.clone();
    let out = radioreference::import(&mut next, &file, req, &site_ids).map_err(ApiError::bad_request)?;
    if save {
        *sys = next;
        config::save(&s.paths.systems(), &c.systems)?;
    }
    Ok(out)
}

/// Is `site` live, being switched to, or the site a scan goes back to?
fn in_use(s: &AppState, site: &str) -> bool {
    match s.live.state() {
        LiveState::Live(l) => l.site.id == site,
        LiveState::Switching { to } => to == site,
        LiveState::Scanning { back_to } => back_to.as_deref() == Some(site),
        LiveState::NoSite => false,
    }
}

/// Remove a site (not the live one) and what it learned. The history keeps its calls.
pub async fn delete_site(State(s): State<Arc<AppState>>, Path((system, site)): Path<(String, String)>) -> ApiResult<Removed> {
    let removed = {
        let mut c = s.config.lock().await;
        if in_use(&s, &site) {
            return Err(ApiError::conflict(format!("site {site} is live: make another site live first")));
        }
        let r = c.remove_site(&system, &site).ok_or_else(|| ApiError::not_found(format!("site {site} of system {system}")))?;
        config::save(&s.paths.systems(), &c.systems)?;
        r
    };
    config::forget_sites(&s.paths, &removed.sites);
    s.log.system("config", format!("site {site} of system {system} removed"));
    Ok(Json(removed))
}

/// Remove a system with its sites and aliases (none of its sites live). The history keeps their
/// calls.
pub async fn delete_system(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Removed> {
    let removed = {
        let mut c = s.config.lock().await;
        let sys = c.systems.value.system(&id).ok_or_else(|| ApiError::not_found(format!("system {id}")))?;
        if let Some(site) = sys.sites.iter().find(|x| in_use(&s, &x.id)) {
            return Err(ApiError::conflict(format!("site {} of system {id} is live: make another site live first", site.id)));
        }
        let r = c.remove_system(&id).ok_or_else(|| ApiError::not_found(format!("system {id}")))?;
        config::save(&s.paths.systems(), &c.systems)?;
        r
    };
    config::forget_sites(&s.paths, &removed.sites);
    s.log.system("config", format!("system {id} removed with {} sites", removed.sites.len()));
    Ok(Json(removed))
}
