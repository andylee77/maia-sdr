//! `GET /api/v1/status`: the at-a-glance state.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::boot::state::AppState;
use crate::boot::version::BUILD_TAG;
use crate::radio::lease::Lease;
use crate::radio::tuner::Tuning;
use crate::services::clock::ClockStatus;
use crate::trunking::receivers::ControlStatus;
use crate::trunking::site::LiveState;
use crate::util::time;

#[derive(Serialize)]
pub struct Status {
    pub build: &'static str,
    pub uptime_s: u64,
    pub now_unix_ms: u64,
    pub live: LiveState,
    pub control: ControlStatus,
    pub lease: &'static str,
    pub tuning: Tuning,
    pub clock: ClockStatus,
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Status> {
    Json(Status {
        build: BUILD_TAG,
        uptime_s: s.started.elapsed().as_secs(),
        now_unix_ms: time::unix_ms(),
        live: s.live.state(),
        control: s.receivers.status(),
        lease: match s.lease.current() {
            Lease::Normal => "normal",
            Lease::Switching => "switching",
            Lease::Scan => "scan",
        },
        tuning: s.tuner.tuning(),
        clock: s.clock.status(),
    })
}
