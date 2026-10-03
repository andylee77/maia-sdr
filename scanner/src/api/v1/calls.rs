//! `GET /api/v1/calls`: the live site's open calls and its newest closed ones; one call. Every
//! call, live or stored, has the same shape, with its talkgroup's and radio's names, its
//! recording once saved and its alert tones once known.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config::aliases::AliasIndex;
use crate::services::history::store::{AlertRow, Range};
use crate::services::recordings::Recording;
use crate::trunking::trunk::{view_of_row, CallView};

/// A call with its talkgroup's and radio's names in its site's system.
#[derive(Serialize)]
pub struct NamedCall {
    #[serde(flatten)]
    pub call: CallView,
    pub tg_name: Option<String>,
    pub source_name: Option<String>,
    /// Its recording, once saved (`attach`).
    pub recording: Option<Recording>,
    /// The alert tones heard in it, once it closed (`attach`).
    pub alerts: Vec<AlertRow>,
}

/// An alert with its talkgroup's and radio's names, whether its call's recording is kept
/// (`/api/v1/recordings/{call_id}`, the alert `offset_ms` into it), and the call that carried
/// what it announced (Activity's listing; null until that call is stored).
#[derive(Serialize)]
pub struct NamedAlert {
    #[serde(flatten)]
    pub alert: AlertRow,
    pub tg_name: Option<String>,
    pub source_name: Option<String>,
    pub recorded: bool,
    pub dispatch: Option<Dispatch>,
}

/// The call that carried what an alert announced (`activity::dispatch_of`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dispatch {
    pub call_id: u64,
    pub started_ms: u64,
    pub voice_ms: u64,
    /// Where its speech starts in its recording: past the tone when it is the alert's own call.
    pub offset_ms: u64,
    pub recorded: bool,
}

/// Each site's system's aliases.
pub struct Names(HashMap<String, Arc<AliasIndex>>);

impl Names {
    pub async fn load(s: &AppState) -> Names {
        let c = s.config.lock().await;
        let mut by_site = HashMap::new();
        for sys in &c.systems.value.systems {
            let ix = Arc::new(sys.alias_index());
            for site in &sys.sites {
                by_site.insert(site.id.clone(), ix.clone());
            }
        }
        Names(by_site)
    }

    pub fn call(&self, call: CallView) -> NamedCall {
        let ix = self.0.get(&call.site);
        // A unit-to-unit call's `tg` is the called radio.
        let tg_name = ix.and_then(|ix| if call.private { ix.radio(call.tg) } else { ix.talkgroup(call.tg) }).map(|a| a.name.clone());
        let source_name = ix.and_then(|ix| call.source.and_then(|s| ix.radio(s))).map(|a| a.name.clone());
        NamedCall { call, tg_name, source_name, recording: None, alerts: Vec::new() }
    }

    pub fn alert(&self, alert: AlertRow) -> NamedAlert {
        let ix = self.0.get(&alert.site);
        let tg_name = ix.and_then(|ix| ix.talkgroup(alert.tg)).map(|a| a.name.clone());
        let source_name = ix.and_then(|ix| alert.source.and_then(|s| ix.radio(s))).map(|a| a.name.clone());
        NamedAlert { alert, tg_name, source_name, recorded: false, dispatch: None }
    }
}

/// Alerts with their names and whether their calls' recordings are kept.
pub fn named_alerts(s: &AppState, names: &Names, alerts: Vec<AlertRow>) -> Vec<NamedAlert> {
    let ids: HashSet<u64> = alerts.iter().map(|a| a.call_id).collect();
    let recordings = s.recordings.of_calls(&ids);
    alerts
        .into_iter()
        .map(|a| {
            let recorded = recordings.get(&a.call_id).is_some_and(|rs| {
                rs.iter().any(|r| (r.site == a.site || r.site.is_empty()) && r.started_unix_ms.abs_diff(a.call_started_ms) < 10_000)
            });
            NamedAlert { recorded, ..names.alert(a) }
        })
        .collect()
}

/// Each call's recording, and its alerts: `stored` (the history's, for stored calls), else the
/// recorder's recent ones.
pub fn attach(s: &AppState, calls: &mut [NamedCall], stored: Option<Vec<AlertRow>>) {
    let ids: HashSet<u64> = calls.iter().map(|c| c.call.call).collect();
    let recordings = s.recordings.of_calls(&ids);
    let mut alerts: HashMap<(String, u64, u64), Vec<AlertRow>> = HashMap::new();
    let recent = stored.is_none();
    for a in stored.unwrap_or_default() {
        alerts.entry((a.site.clone(), a.call_id, a.call_started_ms)).or_default().push(a);
    }
    for c in calls.iter_mut() {
        let v = &c.call;
        // The recorder stamps the call's grant time; a file found at boot has its name's.
        c.recording = recordings
            .get(&v.call)
            .and_then(|rs| rs.iter().find(|r| (r.site == v.site || r.site.is_empty()) && r.started_unix_ms.abs_diff(v.started_unix_ms) < 10_000))
            .cloned();
        c.alerts = if recent {
            s.recordings.alerts_of(v.call).into_iter().filter(|a| a.site == v.site && a.call_started_ms == v.started_unix_ms).collect()
        } else {
            alerts.remove(&(v.site.clone(), v.call, v.started_unix_ms)).unwrap_or_default()
        };
    }
}

/// The history's alerts of stored calls: those of the calls that started in their span.
pub async fn stored_alerts(s: &AppState, calls: &[NamedCall]) -> anyhow::Result<Vec<AlertRow>> {
    let (Some(from), Some(to)) = (calls.iter().map(|c| c.call.started_unix_ms).min(), calls.iter().map(|c| c.call.started_unix_ms).max()) else {
        return Ok(Vec::new());
    };
    let sites: HashSet<String> = calls.iter().map(|c| c.call.site.clone()).collect();
    let mut out = Vec::new();
    for site in sites {
        let q = Range::site(&site, from, to + 1);
        out.extend(s.history.query(move |st| st.call_alerts(&q)).await?);
    }
    Ok(out)
}

#[derive(Serialize)]
pub struct Calls {
    pub open: Vec<NamedCall>,
    pub recent: Vec<NamedCall>,
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Calls> {
    let names = Names::load(&s).await;
    let v = s.trunking.calls();
    let mut calls = Calls {
        open: v.open.into_iter().map(|c| names.call(c)).collect(),
        recent: v.recent.into_iter().map(|c| names.call(c)).collect(),
    };
    attach(&s, &mut calls.open, None);
    attach(&s, &mut calls.recent, None);
    Json(calls)
}

/// One call: as the trunking task shows it while it is recent, else from the history.
pub async fn one(State(s): State<Arc<AppState>>, Path(id): Path<u64>) -> ApiResult<NamedCall> {
    let names = Names::load(&s).await;
    let v = s.trunking.calls();
    if let Some(c) = v.open.into_iter().chain(v.recent).find(|c| c.call == id) {
        let mut one = [names.call(c)];
        attach(&s, &mut one, None);
        let [c] = one;
        return Ok(Json(c));
    }
    let row = s.history.query(move |st| st.call(id)).await?;
    let mut one = [names.call(view_of_row(row.ok_or_else(|| ApiError::not_found(format!("call {id}")))?))];
    let stored = stored_alerts(&s, &one).await?;
    attach(&s, &mut one, Some(stored));
    let [c] = one;
    Ok(Json(c))
}
