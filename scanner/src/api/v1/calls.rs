//! `GET /api/v1/calls`: the live site's open calls and its newest closed ones; one call. Every
//! call, live or stored, has the same shape, with its talkgroup's and radio's names.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::trunking::trunk::{view_of_row, CallView};

/// A call with its talkgroup's and radio's names in its site's system.
#[derive(Serialize)]
pub struct NamedCall {
    #[serde(flatten)]
    pub call: CallView,
    pub tg_name: Option<String>,
    pub source_name: Option<String>,
}

/// Each site's system's talkgroup and radio names.
pub struct Names(HashMap<String, Arc<(BTreeMap<u32, String>, BTreeMap<u32, String>)>>);

impl Names {
    pub async fn load(s: &AppState) -> Names {
        let c = s.config.lock().await;
        let mut by_site = HashMap::new();
        for sys in &c.systems.value.systems {
            let names = Arc::new((sys.talkgroups.clone(), sys.radios.clone()));
            for site in &sys.sites {
                by_site.insert(site.id.clone(), names.clone());
            }
        }
        Names(by_site)
    }

    pub fn call(&self, call: CallView) -> NamedCall {
        let names = self.0.get(&call.site);
        // A unit-to-unit call's `tg` is the called radio.
        let tg_name = names.and_then(|n| if call.private { n.1.get(&call.tg) } else { n.0.get(&call.tg) }).cloned();
        let source_name = names.and_then(|n| call.source.and_then(|s| n.1.get(&s))).cloned();
        NamedCall { call, tg_name, source_name }
    }
}

#[derive(Serialize)]
pub struct Calls {
    pub open: Vec<NamedCall>,
    pub recent: Vec<NamedCall>,
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Calls> {
    let names = Names::load(&s).await;
    let v = s.trunking.calls();
    Json(Calls {
        open: v.open.into_iter().map(|c| names.call(c)).collect(),
        recent: v.recent.into_iter().map(|c| names.call(c)).collect(),
    })
}

/// One call: as the trunking task shows it while it is recent, else from the history.
pub async fn one(State(s): State<Arc<AppState>>, Path(id): Path<u64>) -> ApiResult<NamedCall> {
    let names = Names::load(&s).await;
    let v = s.trunking.calls();
    if let Some(c) = v.open.into_iter().chain(v.recent).find(|c| c.call == id) {
        return Ok(Json(names.call(c)));
    }
    let row = s.history.query(move |st| st.call(id)).await?;
    row.map(|r| Json(names.call(view_of_row(r)))).ok_or_else(|| ApiError::not_found(format!("call {id}")))
}
