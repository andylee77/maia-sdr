//! `GET /api/v1/system`: the board's health (`services::system`).

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::boot::state::AppState;
use crate::services::system::Health;

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Health> {
    Json(s.system.health())
}
