//! `/api/v1/hold`: keep the live site on one talkgroup. While held, only that talkgroup is
//! followed, whatever its alias or the system's listening settings say; a switch to another site
//! releases it.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::trunking::site::LiveState;

#[derive(Serialize)]
pub struct Hold {
    pub site: Option<String>,
    pub tg: Option<u32>,
    /// The talkgroup's name in its system.
    pub name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetHold {
    pub tg: Option<u32>,
}

async fn view(s: &AppState) -> Hold {
    let LiveState::Live(live) = s.live.state() else {
        return Hold { site: None, tg: None, name: None };
    };
    let tg = s.trunking.hold(&live.site.id);
    let name = match tg {
        Some(tg) => s.config.lock().await.systems.value.system(&live.system.id).and_then(|sys| sys.alias_index().talkgroup(tg).map(|a| a.name.clone())),
        None => None,
    };
    Hold { site: Some(live.site.id.clone()), tg, name }
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Hold> {
    Json(view(&s).await)
}

pub async fn put(State(s): State<Arc<AppState>>, Json(req): Json<SetHold>) -> ApiResult<Hold> {
    let LiveState::Live(live) = s.live.state() else {
        return Err(ApiError::conflict("no site is live"));
    };
    s.trunking.set_hold(&live.site.id, req.tg).await;
    Ok(Json(view(&s).await))
}
