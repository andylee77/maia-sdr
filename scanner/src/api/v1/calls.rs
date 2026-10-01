//! `GET /api/v1/calls`: the open calls and the newest closed ones.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::boot::state::AppState;
use crate::trunking::trunk::CallsView;

pub async fn get(State(s): State<Arc<AppState>>) -> Json<CallsView> {
    Json(s.trunking.calls())
}
