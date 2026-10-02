//! `/api/v1/hold`: keep the live site, or one of its traffic lanes, on one talkgroup. While the
//! site is held only that talkgroup is followed; a lane held takes only its talkgroup, and the
//! talkgroup goes to it, while the other lane follows as before. Either way the talkgroup is
//! followed whatever its alias or the system's listening settings say; a switch to another site
//! releases every hold.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::hardware::p25core::Lane;
use crate::trunking::site::LiveState;

#[derive(Serialize)]
pub struct Hold {
    pub site: Option<String>,
    /// The talkgroup the whole site is held on.
    pub tg: Option<u32>,
    /// The talkgroup's name in its system.
    pub name: Option<String>,
    /// Each traffic lane's own hold.
    pub lanes: Vec<LaneHold>,
}

#[derive(Serialize)]
pub struct LaneHold {
    /// 1 or 2.
    pub lane: u8,
    pub tg: Option<u32>,
    pub name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetHold {
    pub tg: Option<u32>,
    /// Hold only this traffic lane (1 or 2); absent: the whole site.
    #[serde(default)]
    pub lane: Option<u8>,
}

async fn view(s: &AppState) -> Hold {
    let LiveState::Live(live) = s.live.state() else {
        return Hold { site: None, tg: None, name: None, lanes: Vec::new() };
    };
    let index = s.config.lock().await.systems.value.system(&live.system.id).map(|sys| sys.alias_index());
    let name = |tg: Option<u32>| tg.and_then(|tg| index.as_ref().and_then(|ix| ix.talkgroup(tg).map(|a| a.name.clone())));
    let tg = s.trunking.hold(&live.site.id);
    let lanes = s.trunking.lane_holds(&live.site.id).into_iter().map(|(l, tg)| LaneHold { lane: l.number(), tg, name: name(tg) }).collect();
    Hold { site: Some(live.site.id.clone()), tg, name: name(tg), lanes }
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Hold> {
    Json(view(&s).await)
}

pub async fn put(State(s): State<Arc<AppState>>, Json(req): Json<SetHold>) -> ApiResult<Hold> {
    let LiveState::Live(live) = s.live.state() else {
        return Err(ApiError::conflict("no site is live"));
    };
    match req.lane {
        None => s.trunking.set_hold(&live.site.id, req.tg).await,
        Some(n) => {
            let lane = Lane::ALL.into_iter().find(|l| l.number() == n).ok_or_else(|| ApiError::bad_request("lane is 1 or 2"))?;
            s.trunking.set_lane_hold(&live.site.id, lane, req.tg).await;
        }
    }
    Ok(Json(view(&s).await))
}
