//! `/api/v1/adsb`: the aircraft module (`services::adsb`): its listening on its two links and the
//! aircraft heard; and which link it listens to.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Deserialize;

use crate::api::ApiResult;
use crate::boot::state::AppState;
use crate::services::adsb::{Listen, View};
use crate::services::config;

pub async fn get(State(s): State<Arc<AppState>>) -> Json<View> {
    Json(s.adsb.view())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkBody {
    pub link: Listen,
}

/// Listen to one link, or to both in turn; kept in the radio's state.
pub async fn put_link(State(s): State<Arc<AppState>>, Json(b): Json<LinkBody>) -> ApiResult<View> {
    s.adsb.set_link(b.link);
    let mut c = s.config.lock().await;
    if c.state.value.aircraft != b.link {
        c.state.value.aircraft = b.link;
        config::save(&s.paths.radio_state(), &c.state)?;
    }
    Ok(Json(s.adsb.view()))
}
