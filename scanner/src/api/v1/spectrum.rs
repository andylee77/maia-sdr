//! `GET /api/v1/spectrum`: the receive window as the wideband spectrometer sees it (dB per bin,
//! the strongest of each group when fewer bins are asked for), with the LO, the rate and where
//! the control channel and the lanes sit. During a scan the last frame is served: the sweep needs
//! every frame.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::boot::state::AppState;
use crate::radio::tuner::RadioHw;
use crate::services::discovery::carriers::{power_db, BINS};

/// The newest frame read, served while no newer one is ready.
static LAST: Mutex<Vec<f32>> = Mutex::new(Vec::new());
const WAIT: Duration = Duration::from_millis(300);

#[derive(Deserialize)]
pub struct Params {
    /// Bins wanted (a divisor of 4096; default 1024).
    pub bins: Option<usize>,
}

#[derive(Serialize)]
pub struct Spectrum {
    pub lo_hz: u64,
    pub sample_rate_hz: u32,
    pub control_hz: u64,
    pub lanes_hz: [Option<u64>; 2],
    /// From -rate/2 to +rate/2.
    pub db: Vec<f32>,
    /// A new frame (not the last one again).
    pub fresh: bool,
}

pub async fn get(State(s): State<Arc<AppState>>, Query(p): Query<Params>) -> Json<Spectrum> {
    let mut fresh = false;
    if s.lease.is_normal() {
        let deadline = tokio::time::Instant::now() + WAIT;
        while tokio::time::Instant::now() < deadline {
            if let Some(bytes) = s.tuner.hw().spectrum().await {
                let db = power_db(&bytes);
                if !db.is_empty() {
                    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = db;
                    fresh = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let bins = p.bins.unwrap_or(1024).clamp(64, BINS);
    let group = (BINS / bins).max(1);
    let db = LAST.lock().unwrap_or_else(|e| e.into_inner()).chunks(group).map(|c| c.iter().copied().fold(f32::MIN, f32::max)).collect();
    let t = s.tuner.tuning();
    Json(Spectrum { lo_hz: t.lo_hz, sample_rate_hz: t.sample_rate_hz, control_hz: t.control_hz, lanes_hz: t.lanes, db, fresh })
}
