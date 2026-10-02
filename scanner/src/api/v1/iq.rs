//! `GET /api/v1/iq/control.wav`: the next seconds of the control chain's IQ (50 kSPS, I left and Q
//! right) as the live site's decoder gets them, for offline decoding and analysis.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::api::ApiError;
use crate::boot::state::AppState;
use crate::services::iq::{wav_bytes, CONTROL_RATE};
use crate::util::time::{iso_utc, unix_ms};

const MAX_SECONDS: u32 = 120;

#[derive(Deserialize)]
pub struct Params {
    pub seconds: Option<u32>,
}

pub async fn control(State(s): State<Arc<AppState>>, Query(p): Query<Params>) -> Result<Response, ApiError> {
    let seconds = p.seconds.unwrap_or(10).clamp(1, MAX_SECONDS);
    let control_hz = s.tuner.tuning().control_hz;
    let started = unix_ms();
    let rx = s.receivers.iq_tap().capture((CONTROL_RATE * seconds) as usize);
    let iq = match tokio::time::timeout(Duration::from_secs(u64::from(seconds) + 5), rx).await {
        Ok(Ok(iq)) => iq,
        _ => return Err(ApiError::conflict("no control IQ: no site is live, or its decoder reads only the gateware's dibits")),
    };
    // "cc_454368750_20260930_204706_60s.wav", as p25-httpd named its dumps.
    let stamp: String = iso_utc(started).chars().filter(|c| c.is_ascii_digit()).take(14).collect();
    let name = format!("cc_{control_hz}_{}_{}_{seconds}s.wav", &stamp[..stamp.len().min(8)], &stamp[stamp.len().min(8)..]);
    Ok((
        [
            (header::CONTENT_TYPE, "audio/wav".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        wav_bytes(&iq, CONTROL_RATE),
    )
        .into_response())
}
