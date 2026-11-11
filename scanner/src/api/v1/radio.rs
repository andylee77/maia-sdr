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
use crate::services::config::radio::{
    Clock, CrystalTracking, Gain, GainMode, PacketDataFollow, RadioConfig, Recording, Storage, GAIN_DB_RANGE, SD_MAX_MB, SD_MIN_MB,
};
use crate::services::crystal::{Calibration, CrystalStatus};
use crate::services::recordings::Policy;
use crate::services::config::{self, RadioState};

#[derive(Serialize)]
pub struct Radio {
    /// Every DDC preset, narrowest first.
    pub presets: Vec<&'static str>,
    pub config: RadioConfig,
    pub state: RadioState,
    pub hardware: HardwareInfo,
    pub tuning: Tuning,
    pub readback: Readback,
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Radio> {
    Json(radio(&s).await)
}

/// The radio as `GET /api/v1/radio` and `/ws/live` give it.
pub async fn radio(s: &AppState) -> Radio {
    let readback = s.tuner.readback().await;
    let c = s.config.lock().await;
    Radio {
        presets: crate::hardware::presets::PRESETS.iter().map(|p| p.name).collect(),
        config: c.radio.value.clone(),
        state: c.state.value.clone(),
        hardware: s.hardware.clone(),
        tuning: s.tuner.tuning(),
        readback,
    }
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

#[derive(Serialize)]
pub struct RecordingSet {
    pub recording: Recording,
    /// Recordings a lowered retention deleted.
    pub deleted: usize,
}

/// The recording settings to change; one left out stays as it is.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingEdit {
    pub enabled: Option<bool>,
    pub every_call: Option<bool>,
    pub storage: Option<Storage>,
    pub ram_max_count: Option<u32>,
    pub sd_keep_days: Option<u32>,
    pub locked_keep_days: Option<u32>,
    pub sd_max_mb: Option<u64>,
    pub folder_max_count: Option<u32>,
}

impl RecordingEdit {
    fn onto(self, mut r: Recording) -> Recording {
        r.enabled = self.enabled.unwrap_or(r.enabled);
        r.every_call = self.every_call.unwrap_or(r.every_call);
        r.storage = self.storage.unwrap_or(r.storage);
        r.ram_max_count = self.ram_max_count.unwrap_or(r.ram_max_count);
        r.sd_keep_days = self.sd_keep_days.unwrap_or(r.sd_keep_days);
        r.locked_keep_days = self.locked_keep_days.unwrap_or(r.locked_keep_days);
        r.sd_max_mb = self.sd_max_mb.unwrap_or(r.sd_max_mb);
        r.folder_max_count = self.folder_max_count.unwrap_or(r.folder_max_count);
        r
    }
}

/// Apply and keep the recording settings given; a lowered retention deletes the oldest at once.
pub async fn put_recording(State(s): State<Arc<AppState>>, Json(req): Json<RecordingEdit>) -> ApiResult<RecordingSet> {
    let mut c = s.config.lock().await;
    let next = req.onto(c.radio.value.recording.clone());
    next.check().map_err(ApiError::bad_request)?;
    c.radio.value.recording = next.clone();
    config::save(&s.paths.radio(), &c.radio)?;
    drop(c);
    let deleted = s.recordings.set_policy(Policy::from(&next));
    Ok(Json(RecordingSet { recording: next, deleted }))
}

/// The radio settings the gain and recording endpoints do not cover, to change; one left out
/// stays as it is, so each part is saved by itself.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SettingsEdit {
    /// DDC presets the window planner may choose, narrowest first.
    pub presets_allowed: Option<Vec<String>>,
    /// Traffic lanes to run; null: every lane the radio core has.
    #[serde(default, with = "crate::api::double_option")]
    pub traffic_chains: Option<Option<u8>>,
    pub calls: Option<CallsEdit>,
    pub history: Option<HistoryEdit>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CallsEdit {
    pub hang_ms: Option<u64>,
    pub end_grace_ms: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HistoryEdit {
    pub retention_days: Option<u32>,
    pub sd_max_mb: Option<u64>,
}

impl SettingsEdit {
    /// The configuration with what is given changed, or why it cannot be.
    fn onto(self, mut c: RadioConfig) -> Result<RadioConfig, String> {
        if let Some(presets) = self.presets_allowed {
            if presets.is_empty() {
                return Err("at least one preset".into());
            }
            for p in &presets {
                find_preset(p).ok_or_else(|| format!("no preset {p}"))?;
            }
            c.presets_allowed = presets;
        }
        if let Some(lanes) = self.traffic_chains {
            c.traffic_chains = lanes;
        }
        if let Some(calls) = self.calls {
            c.calls.hang_ms = calls.hang_ms.unwrap_or(c.calls.hang_ms);
            c.calls.end_grace_ms = calls.end_grace_ms.unwrap_or(c.calls.end_grace_ms);
            if !(500..=30_000).contains(&c.calls.hang_ms) || c.calls.end_grace_ms > 10_000 {
                return Err("hang_ms 500..=30000, end_grace_ms up to 10000".into());
            }
        }
        if let Some(history) = self.history {
            c.history.retention_days = history.retention_days.unwrap_or(c.history.retention_days);
            c.history.sd_max_mb = history.sd_max_mb.unwrap_or(c.history.sd_max_mb);
            if c.history.retention_days == 0 || !(SD_MIN_MB..=SD_MAX_MB).contains(&c.history.sd_max_mb) {
                return Err(format!("history: at least a day, and {SD_MIN_MB}..={SD_MAX_MB} MB"));
            }
        }
        Ok(c)
    }
}

/// Keep the settings given. The history's limits apply at once; the presets and call timings at
/// the next site activation; the lanes at the next start.
pub async fn put_settings(State(s): State<Arc<AppState>>, Json(req): Json<SettingsEdit>) -> ApiResult<RadioConfig> {
    let mut c = s.config.lock().await;
    let next = req.onto(c.radio.value.clone()).map_err(ApiError::bad_request)?;
    let history = (next.history != c.radio.value.history).then(|| next.history.clone());
    c.radio.value = next;
    config::save(&s.paths.radio(), &c.radio)?;
    if let Some(limits) = history {
        s.history.set_limits(&limits);
    }
    Ok(Json(c.radio.value.clone()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacketDataRequest {
    pub follow: bool,
}

/// Follow packet data or not. Kept, and the live site follows the change at once: a lane waiting
/// on the data channel leaves it, or the last lane goes there.
pub async fn put_packet_data(State(s): State<Arc<AppState>>, Json(req): Json<PacketDataRequest>) -> ApiResult<PacketDataFollow> {
    let kept = PacketDataFollow { follow: req.follow };
    {
        let mut c = s.config.lock().await;
        c.radio.value.packet_data = kept;
        config::save(&s.paths.radio(), &c.radio)?;
    }
    s.trunking.set_follow_data(kept.follow).await;
    Ok(Json(kept))
}

/// Where the board clock comes from: the control channel, the internet, or by hand.
pub async fn put_clock(State(s): State<Arc<AppState>>, Json(req): Json<Clock>) -> ApiResult<Clock> {
    let mut c = s.config.lock().await;
    c.radio.value.clock = req.clone();
    config::save(&s.paths.radio(), &c.radio)?;
    Ok(Json(req))
}

/// The crystal correction: applied, calibrated and tracked.
pub async fn crystal(State(s): State<Arc<AppState>>) -> Json<CrystalStatus> {
    Json(s.crystal.status().await)
}

/// Measure the correction on the live control channel now (about 7 s).
pub async fn calibrate_crystal(State(s): State<Arc<AppState>>) -> ApiResult<Calibration> {
    s.crystal.calibrate().await.map(Json).map_err(|e| ApiError::conflict(format!("{e:#}")))
}

/// Tracking on or off, and how far it may move from this run's calibration.
pub async fn put_crystal(State(s): State<Arc<AppState>>, Json(req): Json<CrystalTracking>) -> ApiResult<CrystalStatus> {
    if req.anchor_hz > 1_000 {
        return Err(ApiError::bad_request("anchor_hz up to 1000 (0: no limit)"));
    }
    {
        let mut c = s.config.lock().await;
        c.radio.value.crystal = req;
        config::save(&s.paths.radio(), &c.radio)?;
    }
    Ok(Json(s.crystal.status().await))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetTime {
    pub unix_ms: u64,
}

/// Set the board clock (from a browser's time).
pub async fn set_time(State(s): State<Arc<AppState>>, Json(req): Json<SetTime>) -> ApiResult<crate::services::clock::ClockStatus> {
    if req.unix_ms < crate::services::clock::site::MIN_VALID_MS {
        return Err(ApiError::bad_request("a time before 2020"));
    }
    s.clock.set(req.unix_ms, &s.log).map_err(|e| ApiError::bad_request(format!("the clock was not set: {e}")))?;
    Ok(Json(s.clock.status()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settings_edit_changes_only_what_it_gives() {
        let before = RadioConfig::default();
        let edit = |json: &str| serde_json::from_str::<SettingsEdit>(json).unwrap().onto(before.clone());
        // Nothing given: nothing changed.
        assert_eq!(edit("{}"), Ok(before.clone()));
        // One timing: the other, the history and the presets stay.
        let got = edit(r#"{"calls": {"hang_ms": 4500}}"#).unwrap();
        assert_eq!((got.calls.hang_ms, got.calls.end_grace_ms), (4500, before.calls.end_grace_ms));
        assert_eq!((&got.history, &got.presets_allowed, got.traffic_chains), (&before.history, &before.presets_allowed, before.traffic_chains));
        // The history's limits by themselves.
        let got = edit(r#"{"history": {"retention_days": 30}}"#).unwrap();
        assert_eq!((got.history.retention_days, got.history.sd_max_mb, got.calls.clone()), (30, before.history.sd_max_mb, before.calls.clone()));
        // The lanes: a number, or null for every lane; left out, as they were.
        let two = RadioConfig { traffic_chains: Some(2), ..before.clone() };
        let lanes = |json: &str| serde_json::from_str::<SettingsEdit>(json).unwrap().onto(two.clone()).unwrap().traffic_chains;
        assert_eq!((lanes(r#"{"traffic_chains": 1}"#), lanes(r#"{"traffic_chains": null}"#), lanes("{}")), (Some(1), None, Some(2)));
        // What is given is checked against what stays.
        for bad in [r#"{"presets_allowed": []}"#, r#"{"presets_allowed": ["none such"]}"#, r#"{"calls": {"hang_ms": 100}}"#, r#"{"calls": {"end_grace_ms": 20000}}"#, r#"{"history": {"retention_days": 0}}"#, r#"{"history": {"sd_max_mb": 1}}"#] {
            assert!(edit(bad).is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<SettingsEdit>(r#"{"gain": 3}"#).is_err(), "a field it has not is refused");
        assert!(serde_json::from_str::<SettingsEdit>(r#"{"calls": {"hang": 3}}"#).is_err());
    }
}
