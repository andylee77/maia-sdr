//! p25-httpd's routes that the bench still reads, in their old shape, until it moves to
//! `/api/v1` (design D7).

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::ApiResult;
use crate::boot::state::AppState;
use crate::boot::version::BUILD_TAG;
use crate::services::config::{self, radio::Clock};
use crate::trunking::trunk::{CallView, CallsView};
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

#[derive(Deserialize)]
pub struct CallsQuery {
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct UiCall {
    pub call_id: u64,
    pub tg: u32,
    pub source: Option<u32>,
    pub sources: Vec<u32>,
    pub freq_hz: Option<u64>,
    pub started_unix_ms: u64,
    pub ended_unix_ms: Option<u64>,
    /// How long the call was (or has been) open: the bench's measure of the call's span.
    pub open_ms: u64,
    /// The call's own voice frames, and their length.
    pub imbe: u64,
    pub voice_ms: u64,
    pub encrypted: bool,
    pub not_followed: Option<String>,
    pub close_reason: Option<String>,
}

impl UiCall {
    fn of(c: CallView, now_unix_ms: u64) -> Self {
        UiCall {
            call_id: c.call,
            tg: c.tg,
            source: c.source,
            sources: c.sources,
            freq_hz: c.freq_hz,
            started_unix_ms: c.started_unix_ms,
            ended_unix_ms: c.ended_unix_ms,
            open_ms: c.ended_unix_ms.unwrap_or(now_unix_ms).saturating_sub(c.started_unix_ms),
            imbe: c.voice_frames,
            voice_ms: c.voice_frames * 20,
            encrypted: c.encrypted,
            not_followed: c.not_followed,
            close_reason: c.close,
        }
    }
}

#[derive(Serialize)]
pub struct UiCalls {
    pub now_unix_ms: u64,
    pub items: Vec<UiCall>,
}

/// The open and newest closed calls, newest first, with each call's voice frame count (the
/// bench scores a replay by them).
pub async fn ui_calls(State(s): State<Arc<AppState>>, Query(q): Query<CallsQuery>) -> Json<UiCalls> {
    let now = unix_ms();
    Json(UiCalls { now_unix_ms: now, items: ui_items(s.trunking.calls(), q.limit.unwrap_or(40), now) })
}

fn ui_items(v: CallsView, limit: usize, now_unix_ms: u64) -> Vec<UiCall> {
    let mut items: Vec<UiCall> = v.open.into_iter().chain(v.recent).map(|c| UiCall::of(c, now_unix_ms)).collect();
    items.sort_by(|a, b| b.started_unix_ms.cmp(&a.started_unix_ms));
    items.truncate(limit);
    items
}

#[derive(Serialize, Deserialize)]
pub struct UiSettings {
    pub clock: Clock,
}

#[derive(Serialize)]
pub struct UiSettingsDoc {
    pub settings: UiSettings,
}

/// The clock source (the bench keeps the board clock steady while it replays another day's
/// control channel).
pub async fn ui_settings(State(s): State<Arc<AppState>>) -> Json<UiSettingsDoc> {
    let clock = s.config.lock().await.radio.value.clock.clone();
    Json(UiSettingsDoc { settings: UiSettings { clock } })
}

pub async fn put_ui_settings(State(s): State<Arc<AppState>>, Json(req): Json<UiSettings>) -> ApiResult<UiSettingsDoc> {
    let mut c = s.config.lock().await;
    c.radio.value.clock = req.clock;
    config::save(&s.paths.radio(), &c.radio)?;
    Ok(Json(UiSettingsDoc { settings: UiSettings { clock: c.radio.value.clock.clone() } }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: u64, started: u64, frames: u64, ended: bool) -> CallView {
        CallView {
            call: id,
            site: "clay".into(),
            tg: 300,
            source: Some(1014),
            speaker: None,
            freq_hz: Some(858_437_500),
            slot: None,
            channel: None,
            encrypted: false,
            not_followed: None,
            lane: Some(1),
            started_unix_ms: started,
            ended_unix_ms: ended.then_some(started + 1_000),
            close: ended.then(|| "call_end".to_string()),
            end_lc: None,
            sources: vec![1014],
            voice_frames: frames,
        }
    }

    #[test]
    fn the_calls_list_is_newest_first_with_the_open_ones() {
        let v = CallsView { open: vec![call(3, 3_000, 9, false)], recent: vec![call(2, 2_000, 45, true), call(1, 1_000, 81, true)] };
        let items = ui_items(v.clone(), 40, 3_500);
        assert_eq!(items.iter().map(|c| (c.call_id, c.imbe)).collect::<Vec<_>>(), [(3, 9), (2, 45), (1, 81)]);
        assert_eq!(items[1].close_reason.as_deref(), Some("call_end"));
        // The span: closed calls to their end, the open one to now.
        assert_eq!(items.iter().map(|c| (c.open_ms, c.voice_ms)).collect::<Vec<_>>(), [(500, 180), (1_000, 900), (1_000, 1_620)]);
        assert_eq!(ui_items(v, 2, 3_500).len(), 2);
    }
}
