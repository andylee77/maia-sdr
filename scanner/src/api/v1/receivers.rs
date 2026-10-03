//! `GET /api/v1/receivers`: the receive chains for the diagnostics: the control channel's
//! status, carrier loop and decoder counters, and each traffic lane's state, counters and loop.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::boot::state::AppState;
use crate::trunking::receivers::{ControlStatus, Counters};
use crate::trunking::trunk::LaneStatus;

#[derive(Serialize)]
pub struct Receivers {
    pub control: Control,
    pub lanes: Vec<LaneStatus>,
}

#[derive(Serialize)]
pub struct Control {
    #[serde(flatten)]
    pub status: ControlStatus,
    pub counters: Option<Counters>,
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Receivers> {
    Json(Receivers {
        control: Control { status: s.receivers.status(), counters: s.receivers.counters() },
        lanes: s.trunking.lanes(),
    })
}
