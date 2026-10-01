//! `GET /api/v1/events`: the event log, for the Diagnostics events box.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::boot::state::AppState;
use crate::services::events::Record;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
    /// Records after this sequence number.
    #[serde(default)]
    pub after: u64,
    /// The newest this many.
    pub limit: Option<usize>,
    /// Housekeeping broadcasts too.
    #[serde(default)]
    pub routine: bool,
}

#[derive(Serialize)]
pub struct Events {
    pub events: Vec<Record>,
}

pub async fn list(State(s): State<Arc<AppState>>, Query(p): Query<Params>) -> Json<Events> {
    Json(Events { events: s.log.since(p.after, p.limit.unwrap_or(200).min(2000), p.routine) })
}
