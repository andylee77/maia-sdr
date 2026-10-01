//! p25-httpd's routes that the bench still reads, in their old shape, until it moves to
//! `/api/v1` (design D7).

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::boot::state::AppState;
use crate::boot::version::BUILD_TAG;
use crate::util::time::unix_ms;

#[derive(Serialize)]
pub struct System {
    pub build: &'static str,
    pub uptime_s: u64,
}

/// The build (the bench's unit identity).
pub async fn system(State(s): State<Arc<AppState>>) -> Json<System> {
    Json(System { build: BUILD_TAG, uptime_s: s.started.elapsed().as_secs() })
}

#[derive(Serialize)]
pub struct UiState {
    pub v: u8,
    pub build: &'static str,
    pub now_unix_ms: u64,
    pub clock_valid: bool,
}

/// The unit's wall clock (the bench maps call times to its own clock).
pub async fn ui_state(State(s): State<Arc<AppState>>) -> Json<UiState> {
    Json(UiState { v: 1, build: BUILD_TAG, now_unix_ms: unix_ms(), clock_valid: s.clock.status().valid })
}

#[derive(Serialize)]
pub struct Frame {
    pub talkgroup: u32,
    pub encrypted: bool,
    pub hex: String,
}

#[derive(Serialize)]
pub struct ImbeDump {
    pub count: usize,
    pub frames: Vec<Frame>,
}

/// The newest raw IMBE frames (the bench compares them with what it transmitted).
pub async fn imbe_dump(State(s): State<Arc<AppState>>) -> Json<ImbeDump> {
    let frames: Vec<Frame> = s
        .trunking
        .frames()
        .into_iter()
        .map(|f| Frame { talkgroup: f.tg, encrypted: f.encrypted, hex: f.bits.iter().map(|b| format!("{b:02x}")).collect() })
        .collect();
    Json(ImbeDump { count: frames.len(), frames })
}
