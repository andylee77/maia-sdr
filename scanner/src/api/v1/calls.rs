//! `GET /api/v1/calls`: the live site's open calls and its newest closed ones; one call.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::history::store::CallRow;
use crate::trunking::trunk::{CallView, CallsView};

pub async fn get(State(s): State<Arc<AppState>>) -> Json<CallsView> {
    Json(s.trunking.calls())
}

/// One call: as the trunking task shows it while it is recent, else its history row.
#[derive(Serialize)]
#[serde(untagged)]
pub enum OneCall {
    Live(Box<CallView>),
    Stored(Box<CallRow>),
}

pub async fn one(State(s): State<Arc<AppState>>, Path(id): Path<u64>) -> ApiResult<OneCall> {
    let v = s.trunking.calls();
    if let Some(c) = v.open.into_iter().chain(v.recent).find(|c| c.call == id) {
        return Ok(Json(OneCall::Live(Box::new(c))));
    }
    let row = s.history.query(move |st| st.call(id)).await?;
    row.map(|r| Json(OneCall::Stored(Box::new(r)))).ok_or_else(|| ApiError::not_found(format!("call {id}")))
}
