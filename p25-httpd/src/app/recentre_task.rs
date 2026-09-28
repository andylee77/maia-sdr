//! Change 070: keeps the receive window on the site's channels.
//!
//! Every 30 s: when a better window exists (`services::lo_plan`: all
//! channels where some are missed, or 5 % more of the grant weight), the
//! site's auto switch is on, the LO is not locked, both traffic chains
//! are idle and the last move was 10+ minutes ago, the preset and LO move
//! there (a few seconds without control-channel decode). The learned
//! grant counts are saved every 10 minutes.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::hardware::ddc_presets;
use crate::httpd::AppState;
#[cfg(target_os = "linux")]
use crate::services::event_log::LogCategory;
use crate::services::lo_plan::{self, LoPlan};

const TICK: Duration = Duration::from_secs(30);
/// Let the receiver settle after start before the first move.
const START_GRACE: Duration = Duration::from_secs(120);
const MIN_INTERVAL_MS: u64 = 10 * 60 * 1_000;
const FLUSH_EVERY: Duration = Duration::from_secs(600);

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The presets the planner may choose, as (name, sample rate), none
/// narrower than the site's `min_preset`.
pub fn plan_presets(min_preset: Option<&str>) -> Vec<(&'static str, u32)> {
    let all: Vec<(&'static str, u32)> = lo_plan::PLAN_PRESETS
        .iter()
        .filter_map(|n| ddc_presets::find_preset(n).map(|p| (p.name, p.sample_rate_hz)))
        .collect();
    lo_plan::at_least(&all, min_preset)
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelView {
    pub freq_hz: u64,
    /// Grants seen here (this site, since the counts began).
    pub grants: u32,
    /// In the site file's channel list.
    pub listed: bool,
    /// Planner weight: grants, at least 1 for a listed channel until the
    /// plan is learned; 0 = listed but never granted since.
    pub weight: f64,
    pub covered: bool,
}

/// The live window against the site's channels (`GET /api/site/plan`).
#[derive(Debug, Clone, Serialize)]
pub struct WindowView {
    pub site: String,
    pub auto: bool,
    pub locked: bool,
    /// Narrowest preset the planner may pick here (None = any).
    pub min_preset: Option<String>,
    pub preset: String,
    pub sample_rate_hz: u32,
    /// LO as the DDC sees it (crystal trim removed).
    pub lo_hz: i64,
    pub low_hz: i64,
    pub high_hz: i64,
    pub control_hz: u64,
    pub channels: Vec<ChannelView>,
    pub covered_weight: f64,
    pub total_weight: f64,
    pub best: Option<LoPlan>,
    /// The best window is worth a move.
    pub better: bool,
    pub last_recentre_unix_ms: u64,
}

pub async fn window_view(state: &AppState) -> Option<WindowView> {
    let site = state.active_site.read().await.clone()?;
    let plan = state.lo_plans.get();
    let shift = state.current_lo_shift_hz.load(Ordering::Relaxed);
    let lo = state.current_rx_lo.load(Ordering::Relaxed) - shift;
    let sr = state.current_sample_rate_hz.load(Ordering::Relaxed);
    let cc = state.current_control_freq.load(Ordering::Relaxed);
    let preset = ddc_presets::PRESETS
        .get(state.current_preset_idx.load(Ordering::Relaxed))
        .map_or("?", |p| p.name);
    let chans = lo_plan::channels(&site.traffic_freqs_hz, &plan.grants);
    let covered_weight: f64 = chans.iter().filter(|c| lo_plan::covers(lo, c.freq_hz, sr)).map(|c| c.weight).sum();
    let total_weight: f64 = chans.iter().map(|c| c.weight).sum();
    let best = lo_plan::plan(cc, &chans, &plan_presets(plan.min_preset.as_deref()));
    // A window narrower than the site's minimum is worth widening too.
    let too_narrow = best.as_ref().is_some_and(|b| sr < b.sample_rate_hz && plan.min_preset.is_some());
    let better = too_narrow
        || best.as_ref().is_some_and(|b| lo_plan::worth_moving(covered_weight, b.covered_weight, total_weight));
    let half = lo_plan::usable_half_hz(sr);
    Some(WindowView {
        site: site.name.clone(),
        auto: plan.auto,
        locked: state.center_locked.load(Ordering::Relaxed),
        min_preset: plan.min_preset.clone(),
        preset: preset.to_string(),
        sample_rate_hz: sr,
        lo_hz: lo,
        low_hz: lo - half,
        high_hz: lo + half,
        control_hz: cc,
        channels: chans
            .iter()
            .map(|c| ChannelView {
                freq_hz: c.freq_hz,
                grants: plan.grants.get(&c.freq_hz).copied().unwrap_or(0),
                listed: site.traffic_freqs_hz.contains(&c.freq_hz),
                weight: c.weight,
                covered: lo_plan::covers(lo, c.freq_hz, sr),
            })
            .collect(),
        covered_weight,
        total_weight,
        best,
        better,
        last_recentre_unix_ms: plan.last_recentre_unix_ms,
    })
}

/// Move the window to `plan` now. Returns the preset reply.
#[cfg(target_os = "linux")]
pub async fn apply(state: &AppState, plan: &LoPlan, origin: &str) -> Result<serde_json::Value, String> {
    let shift = state.current_lo_shift_hz.load(Ordering::Relaxed);
    let before = window_view(state).await;
    let body = crate::httpd::api::tuning::PresetBody {
        preset: plan.preset.clone(),
        center_freq_hz: Some((plan.lo_hz + shift) as u64),
        gain_mode: None,
        gain_db: None,
    };
    let (status, reply) = crate::httpd::api::tuning::apply_preset(state, body).await;
    let now = now_unix_ms();
    state.lo_plans.edit(|p| p.last_recentre_unix_ms = now);
    let (from, covered) = before.map_or((String::new(), String::new()), |b| {
        (
            format!("{} LO {:.4} MHz", b.preset, b.lo_hz as f64 / 1e6),
            format!("{:.0}/{:.0}", b.covered_weight, b.total_weight),
        )
    });
    state.event_log.push(
        LogCategory::System,
        format!(
            "recentre ({origin}): {from} -> {} LO {:.4} MHz, channel weight {covered} -> {:.0}/{:.0}{}",
            plan.preset,
            plan.lo_hz as f64 / 1e6,
            plan.covered_weight,
            plan.total_weight,
            if status.is_success() { "" } else { " (FAILED)" },
        ),
        serde_json::json!({ "origin": origin, "plan": plan, "reply": reply }),
    );
    if status.is_success() {
        Ok(reply)
    } else {
        Err(reply.get("errors").map(|e| e.to_string()).unwrap_or_else(|| "preset apply failed".into()))
    }
}

async fn chains_idle(state: &AppState) -> bool {
    for lane in &state.traffic_lanes {
        if lane.chain.lock().await.current_talkgroup().is_some() {
            return false;
        }
    }
    true
}

pub fn spawn_recentre_task(state: Arc<AppState>) {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut last_flush = Instant::now();
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            if last_flush.elapsed() >= FLUSH_EVERY {
                last_flush = Instant::now();
                if let Err(e) = state.lo_plans.flush() {
                    tracing::warn!("lo plan: not saved: {e}");
                }
            }
            if started.elapsed() < START_GRACE {
                continue;
            }
            let Some(v) = window_view(&state).await else { continue };
            if !v.auto || v.locked || !v.better {
                continue;
            }
            if now_unix_ms().saturating_sub(v.last_recentre_unix_ms) < MIN_INTERVAL_MS {
                continue;
            }
            if !chains_idle(&state).await {
                continue;
            }
            let Some(best) = v.best else { continue };
            #[cfg(target_os = "linux")]
            if let Err(e) = apply(&state, &best, "auto").await {
                tracing::warn!("recentre failed: {e}");
            }
            #[cfg(not(target_os = "linux"))]
            let _ = best;
        }
    });
}
