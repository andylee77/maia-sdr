//! `GET /api/v1/receivers`: the receive chains for the diagnostics: the control channel's
//! status, carrier loop and decoder counters, and each traffic lane's state, counters and loop.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::boot::state::AppState;
use crate::hardware::p25core::Lane;
use crate::services::crystal::pll_q213_to_hz;
use crate::trunking::receivers::{ControlStatus, Counters};
use crate::trunking::trunk::LaneStatus;

#[derive(Serialize)]
pub struct Receivers {
    pub control: Control,
    pub lanes: Vec<LaneView>,
}

#[derive(Serialize)]
pub struct Control {
    #[serde(flatten)]
    pub status: ControlStatus,
    #[serde(rename = "loop")]
    pub carrier_loop: Option<ControlLoopView>,
    pub counters: Option<Counters>,
}

/// The control chain's carrier loop and AGC as the gateware reports them.
#[derive(Serialize)]
pub struct ControlLoopView {
    pub pll_q213: i16,
    /// The loop's correction in Hz (it reads NCO minus signal).
    pub pll_hz: f64,
    /// Q9.7.
    pub agc_gain: u16,
    /// Q1.15.
    pub agc_mag: u16,
}

#[derive(Serialize)]
pub struct LaneView {
    #[serde(flatten)]
    pub status: LaneStatus,
    #[serde(rename = "loop")]
    pub carrier_loop: Option<LaneLoopView>,
}

#[derive(Serialize)]
pub struct LaneLoopView {
    pub pll_q213: i16,
    pub pll_hz: f64,
    pub clamp_q213: i32,
    /// The loop sits at half its clamp or more: it is running on noise.
    pub hot: bool,
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Receivers> {
    let carrier_loop = s.tuner.control_loop().await.map(|l| ControlLoopView {
        pll_q213: l.pll_q213,
        pll_hz: pll_q213_to_hz(f64::from(l.pll_q213)),
        agc_gain: l.agc_gain,
        agc_mag: l.agc_mag,
    });
    let mut lanes = Vec::new();
    for status in s.trunking.lanes() {
        let lane = Lane::ALL.into_iter().find(|l| l.number() == status.lane);
        let carrier_loop = match lane {
            Some(lane) => s.tuner.lane_pll(lane).await.map(|p| LaneLoopView {
                pll_q213: p.pll_q213,
                pll_hz: pll_q213_to_hz(f64::from(p.pll_q213)),
                clamp_q213: p.clamp_q213,
                hot: p.hot(),
            }),
            None => None,
        };
        lanes.push(LaneView { status, carrier_loop });
    }
    Json(Receivers {
        control: Control { status: s.receivers.status(), carrier_loop, counters: s.receivers.counters() },
        lanes,
    })
}
