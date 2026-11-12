//! `/api/v1/adsb`: the aircraft module (`services::adsb`): its listening at 1090 MHz and the
//! aircraft heard.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::boot::state::AppState;
use crate::services::adsb::View;

pub async fn get(State(s): State<Arc<AppState>>) -> Json<View> {
    Json(s.adsb.view())
}
