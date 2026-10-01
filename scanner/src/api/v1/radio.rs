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
use crate::hardware::presets::find_preset;
use crate::services::config::radio::{Calls, Gain, GainMode, History, RadioConfig, Recording, GAIN_DB_RANGE};
use crate::services::recordings::Policy;
use crate::services::config::{self, RadioState};

#[derive(Serialize)]
pub struct Radio {
    /// Every DDC preset the gateware has, narrowest first.
    pub presets: Vec<&'static str>,
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
        presets: crate::hardware::presets::PRESETS.iter().map(|p| p.name).collect(),
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

/// Space for recordings on the card: at least this much.
const SD_MIN_MB: u64 = 16;

#[derive(Serialize)]
pub struct RecordingSet {
    pub recording: Recording,
    /// Recordings a lowered retention deleted.
    pub deleted: usize,
}

/// Apply and keep the recording settings; a lowered retention deletes the oldest at once.
pub async fn put_recording(State(s): State<Arc<AppState>>, Json(req): Json<Recording>) -> ApiResult<RecordingSet> {
    if req.ram_max_count == 0 || req.sd_max_count == 0 {
        return Err(ApiError::bad_request("each store keeps at least one recording"));
    }
    if req.sd_max_mb < SD_MIN_MB {
        return Err(ApiError::bad_request(format!("sd_max_mb is at least {SD_MIN_MB}")));
    }
    let mut c = s.config.lock().await;
    c.radio.value.recording = req.clone();
    config::save(&s.paths.radio(), &c.radio)?;
    drop(c);
    let deleted = s.recordings.set_policy(Policy::from(&req));
    Ok(Json(RecordingSet { recording: req, deleted }))
}

/// The radio settings the gain and recording endpoints do not cover.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// DDC presets the window planner may choose, narrowest first.
    pub presets_allowed: Vec<String>,
    /// Traffic lanes to run; `None` = every lane the gateware has.
    pub traffic_chains: Option<u8>,
    pub calls: Calls,
    pub history: History,
}

/// Keep the settings. The history's limits apply at once; the presets, lanes and call timings at
/// the next site activation.
pub async fn put_settings(State(s): State<Arc<AppState>>, Json(req): Json<Settings>) -> ApiResult<RadioConfig> {
    if req.presets_allowed.is_empty() {
        return Err(ApiError::bad_request("at least one preset"));
    }
    for p in &req.presets_allowed {
        find_preset(p).ok_or_else(|| ApiError::bad_request(format!("no preset {p}")))?;
    }
    if !(500..=30_000).contains(&req.calls.hang_ms) || req.calls.end_grace_ms > 10_000 {
        return Err(ApiError::bad_request("hang_ms 500..=30000, end_grace_ms up to 10000"));
    }
    if req.history.retention_days == 0 || req.history.sd_max_mb < 16 {
        return Err(ApiError::bad_request("history: at least a day and 16 MB"));
    }
    let mut c = s.config.lock().await;
    c.radio.value.presets_allowed = req.presets_allowed;
    c.radio.value.traffic_chains = req.traffic_chains;
    c.radio.value.calls = req.calls;
    c.radio.value.history = req.history.clone();
    config::save(&s.paths.radio(), &c.radio)?;
    s.history.set_limits(&req.history);
    Ok(Json(c.radio.value.clone()))
}
