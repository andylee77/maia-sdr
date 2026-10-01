//! `/api/v1/radio`: the radio configuration, the hardware found at boot and the tuning; the
//! receiver gain.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::boot::radio::{gain_mode, HardwareInfo};
use crate::boot::state::AppState;
use crate::radio::tuner::{Readback, Tuning};
use crate::services::config::radio::{Gain, GainMode, RadioConfig, GAIN_DB_RANGE};
use crate::services::config::{self, RadioState};

#[derive(Serialize)]
pub struct Radio {
    pub config: RadioConfig,
    pub state: RadioState,
    pub hardware: HardwareInfo,
    pub tuning: Tuning,
    pub readback: Readback,
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Radio> {
    let readback = s.tuner.readback().await;
    let c = s.config.lock().await;
    Json(Radio {
        config: c.radio.value.clone(),
        state: c.state.value.clone(),
        hardware: s.hardware.clone(),
        tuning: s.tuner.tuning(),
        readback,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GainRequest {
    pub mode: GainMode,
    pub manual_db: Option<i32>,
}

/// Apply and keep a new gain setting.
pub async fn put_gain(State(s): State<Arc<AppState>>, Json(req): Json<GainRequest>) -> ApiResult<Gain> {
    if req.mode == GainMode::Manual && !req.manual_db.is_some_and(|db| GAIN_DB_RANGE.contains(&db)) {
        return Err(ApiError::bad_request(format!(
            "manual gain needs manual_db in {}..={}",
            GAIN_DB_RANGE.start(),
            GAIN_DB_RANGE.end()
        )));
    }
    s.tuner.set_gain(gain_mode(req.mode), req.manual_db.map(f64::from)).await?;
    let mut c = s.config.lock().await;
    let gain = Gain { mode: req.mode, manual_db: req.manual_db.or(c.radio.value.gain.manual_db) };
    c.radio.value.gain = gain.clone();
    config::save(&s.paths.radio(), &c.radio)?;
    Ok(Json(gain))
}
