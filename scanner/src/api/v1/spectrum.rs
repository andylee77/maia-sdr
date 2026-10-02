//! `GET /api/v1/spectrum`: the receive window as the wideband spectrometer sees it (dB per bin,
//! the strongest of each group when fewer bins are asked for), with the LO, the rate and where
//! the control channel and the lanes sit. While a site is live its trunking reads every frame
//! (for the survey) and the newest is served; with no site live the spectrometer is read here.
//! During a scan the last frame is served: the sweep needs every frame.
//!
//! `GET /api/v1/survey`: what the live site's window carried over the last minutes.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::boot::state::AppState;
use crate::radio::tuner::RadioHw;
use crate::services::discovery::carriers::{power_db, BINS};
use crate::trunking::trunk::SurveyView;

/// The newest frame read, served while no newer one is ready.
static LAST: Mutex<Vec<f32>> = Mutex::new(Vec::new());
const WAIT: Duration = Duration::from_millis(300);
/// The live site's newest frame counts as new this long (a frame is 131 ms).
const FRESH: Duration = Duration::from_millis(400);

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
    if let Some(f) = s.trunking.latest_frame().filter(|f| f.at.elapsed() < FRESH) {
        *LAST.lock().unwrap_or_else(|e| e.into_inner()) = f.db;
        fresh = true;
    } else if s.lease.is_normal() {
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
    let db = LAST.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Json(shape(&s, &db, p.bins.unwrap_or(1024), fresh))
}

/// A frame newer than `since`, for `/ws/live`: the live site's newest, else one the spectrometer
/// finished (none during a scan: the sweep needs every frame).
pub async fn next(s: &AppState, since: Option<Instant>, bins: usize) -> Option<(Spectrum, Instant)> {
    if let Some(f) = s.trunking.latest_frame() {
        return since.is_none_or(|t| f.at > t).then(|| (shape(s, &f.db, bins, true), f.at));
    }
    if !s.lease.is_normal() {
        return None;
    }
    let db = power_db(&s.tuner.hw().spectrum().await?);
    (!db.is_empty()).then(|| (shape(s, &db, bins, true), Instant::now()))
}

/// `db` (every bin) in `bins`, the strongest of each group, with the tuning.
fn shape(s: &AppState, db: &[f32], bins: usize, fresh: bool) -> Spectrum {
    let group = (BINS / bins.clamp(64, BINS)).max(1);
    let db = db.chunks(group).map(|c| c.iter().copied().fold(f32::MIN, f32::max)).collect();
    let t = s.tuner.tuning();
    Spectrum { lo_hz: t.lo_hz, sample_rate_hz: t.sample_rate_hz, control_hz: t.control_hz, lanes_hz: t.lanes, db, fresh }
}

pub async fn survey(State(s): State<Arc<AppState>>) -> Json<SurveyView> {
    Json(s.trunking.survey())
}
