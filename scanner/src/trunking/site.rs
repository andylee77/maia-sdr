//! Which site is live: one state, one switch.
//!
//! `activate` is the only way to change site. It takes the radio lease (grants decoded during
//! the switch are dropped), stops the old site's receivers, plans the receive window from the
//! site's channels and what was learned there, tunes, loads the site's learned state and active
//! profile, starts the new site's receivers, publishes `Live` and persists the choice.
//!
//! As grants are counted the planner may find a better window. `recentre` moves there while both
//! lanes are idle, without stopping the site: automatically (at most every 10 minutes, at sites
//! whose window is automatic) or by hand.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::sync::{watch, Mutex};

use crate::hardware::presets::{find_preset, DdcPreset};
use crate::radio::lease::{Lease, RadioLease};
use crate::radio::plan::{self, WindowPlan};
use crate::radio::streams::StreamSource;
use crate::radio::tuner::{RadioHw, Tuner, Tuning, TuningPlan};
use crate::services::events::EventLog;
use crate::services::history::store::SiteInfo;
use crate::services::history::HistoryTx;
use crate::hardware::p25core::Lane;
use crate::trunking::calls::CallPolicy;
use crate::trunking::follow::routing::Routing;
use crate::trunking::learned::Learned;
use crate::trunking::receivers::{self, Receivers};
use crate::trunking::trunk::{Setup, Trunking};
use crate::services::config::profiles::Profile;
use crate::services::config::systems::{CcPosition, Protocol, Site, System};
use crate::services::config::{self, Config, Paths, SiteState};

/// Distance of the control channel from the window edge when a site has no known channels.
const EDGE_MARGIN_HZ: f64 = 250_000.0;
/// A profile change waits this many times for a switch to hand the radio back.
const PROFILE_TRIES: u32 = 30;
const PROFILE_RETRY: Duration = Duration::from_millis(100);
/// How often the automatic recentre looks, how long after a site goes live it starts, and the
/// least time between two moves.
const RECENTRE_EVERY: Duration = Duration::from_secs(30);
const RECENTRE_AFTER_START: Duration = Duration::from_secs(120);
const RECENTRE_MIN_INTERVAL_MS: u64 = 10 * 60 * 1_000;

/// A switch, planned.
struct Plan {
    site: Site,
    system: SystemSummary,
    profile: Option<Profile>,
    calls: crate::services::config::radio::Calls,
    learned: Arc<Learned>,
    window: WindowPlan,
    preset: &'static DdcPreset,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LiveState {
    NoSite,
    Switching { to: String },
    /// A scan has the radio; the site it goes back to.
    Scanning { back_to: Option<String> },
    Live(Box<Live>),
}

#[derive(Debug, Clone, Serialize)]
pub struct Live {
    pub site: Site,
    pub system: SystemSummary,
    pub profile: Option<Profile>,
    pub window: WindowPlan,
    pub tuning: Tuning,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemSummary {
    pub id: String,
    pub label: String,
    pub protocol: Protocol,
}

impl From<&System> for SystemSummary {
    fn from(s: &System) -> Self {
        SystemSummary { id: s.id.clone(), label: s.label.clone(), protocol: s.protocol }
    }
}

/// One of the site's channels against the live window.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelView {
    pub freq_hz: u64,
    /// Grants seen here.
    pub grants: u32,
    /// In the site's channel list.
    pub listed: bool,
    /// The planner's weight.
    pub weight: f64,
    pub covered: bool,
}

/// The live window against the site's channels, and the window the planner would choose now.
#[derive(Debug, Clone, Serialize)]
pub struct WindowView {
    pub site: String,
    pub auto: bool,
    pub min_preset: Option<String>,
    pub preset: Option<&'static str>,
    pub sample_rate_hz: u32,
    pub lo_hz: u64,
    pub control_hz: u64,
    pub channels: Vec<ChannelView>,
    pub covered_weight: f64,
    pub total_weight: f64,
    pub best: Option<WindowPlan>,
    /// The planner's window is worth a move.
    pub better: bool,
    pub last_recentre_unix_ms: u64,
}

pub struct LiveSite<H> {
    paths: Paths,
    config: Arc<Mutex<Config>>,
    tuner: Arc<Tuner<H>>,
    lease: RadioLease,
    receivers: Arc<Receivers>,
    trunking: Arc<Trunking>,
    lanes: Vec<Lane>,
    log: Arc<EventLog>,
    history: HistoryTx,
    state: watch::Sender<LiveState>,
    /// What the live site taught the radio.
    learned: Mutex<Option<Arc<Learned>>>,
    /// When the live site went live.
    live_since: std::sync::Mutex<Option<Instant>>,
}

impl<H: RadioHw + StreamSource + 'static> LiveSite<H> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: Paths,
        config: Arc<Mutex<Config>>,
        tuner: Arc<Tuner<H>>,
        lease: RadioLease,
        receivers: Arc<Receivers>,
        trunking: Arc<Trunking>,
        lanes: Vec<Lane>,
        log: Arc<EventLog>,
        history: HistoryTx,
    ) -> Self {
        LiveSite {
            paths,
            config,
            tuner,
            lease,
            receivers,
            trunking,
            lanes,
            log,
            history,
            state: watch::channel(LiveState::NoSite).0,
            learned: Mutex::new(None),
            live_since: std::sync::Mutex::new(None),
        }
    }

    pub fn state(&self) -> LiveState {
        self.state.borrow().clone()
    }

    /// Save what the live site taught, when it changed (on the blocking pool: a flash write can
    /// take seconds).
    pub async fn save_learned(&self) {
        let Some(l) = self.learned.lock().await.clone() else { return };
        let paths = self.paths.clone();
        let _ = tokio::task::spawn_blocking(move || l.save(&paths)).await;
    }

    /// Stop the live site and drop what it learned since its last save (a factory reset or an
    /// import replaces the configuration under it). The caller holds the radio lease.
    pub async fn stop_discarding(&self) {
        self.receivers.stop().await;
        self.trunking.stop().await;
        *self.learned.lock().await = None;
        self.state.send_replace(LiveState::NoSite);
    }

    /// Stop the live site, keeping what it learned. No site is live, now or at the next start,
    /// until one is made live.
    pub async fn stop(&self, site_id: &str) -> Result<()> {
        let _lease = self.lease.take(Lease::Switching).context("the radio is busy (a scan or another switch)")?;
        if !matches!(self.state(), LiveState::Live(l) if l.site.id == site_id) {
            bail!("site {site_id} is not live");
        }
        self.save_learned().await;
        self.receivers.stop().await;
        self.trunking.stop().await;
        *self.learned.lock().await = None;
        self.state.send_replace(LiveState::NoSite);
        let mut c = self.config.lock().await;
        c.state.value.live_site = None;
        if let Err(e) = config::save(&self.paths.radio_state(), &c.state) {
            tracing::warn!("no live site not persisted: {e:#}");
        }
        self.log.system("site", format!("site {site_id} stopped; no site is live"));
        Ok(())
    }

    /// A scan takes the radio: the live site's receivers and trunking stop. Returns the site to
    /// go back to.
    pub async fn pause_for_scan(&self) -> Option<String> {
        let back_to = match self.state() {
            LiveState::Live(l) => Some(l.site.id.clone()),
            LiveState::Scanning { back_to } => back_to,
            _ => None,
        };
        self.save_learned().await;
        self.receivers.stop().await;
        self.trunking.stop().await;
        self.state.send_replace(LiveState::Scanning { back_to: back_to.clone() });
        self.log.system("site", "scanning: the live site is paused".to_string());
        back_to
    }

    /// The profiles changed: the live site follows its active profile from now on. A switch in
    /// progress picks the change up as it ends; a scan, when the site comes back.
    pub async fn profile_changed(&self) {
        for _ in 0..PROFILE_TRIES {
            if let Some(_lease) = self.lease.take(Lease::Switching) {
                self.apply_profile().await;
                return;
            }
            tokio::time::sleep(PROFILE_RETRY).await;
        }
    }

    /// Follow the live site's active profile, with the radio lease held (no switch or scan can
    /// change the live site meanwhile).
    async fn apply_profile(&self) {
        let LiveState::Live(live) = self.state() else { return };
        let profile = self.config.lock().await.profiles.value.active_for(&live.site.id).cloned();
        if profile == live.profile {
            return;
        }
        self.trunking.set_routing(profile.as_ref().map(Routing::new).unwrap_or_default()).await;
        let name = profile.as_ref().map_or("none".to_string(), |p| p.name.clone());
        self.state.send_modify(|s| {
            if let LiveState::Live(l) = s {
                l.profile = profile;
            }
        });
        self.log.system("profile", format!("following profile {name}"));
    }

    /// After a scan: the site it paused, live again.
    pub async fn resume_after_scan(&self, back_to: Option<String>) {
        self.state.send_replace(LiveState::NoSite);
        if let Some(site) = back_to {
            if let Err(e) = self.activate(&site).await {
                tracing::error!("site {site} did not go live after the scan: {e:#}");
            }
        }
    }

    /// Make `site` live. Returns once it is. When the radio fails to take the new site after
    /// the old one stopped, the old one is brought back (or none is live).
    pub async fn activate(&self, site_id: &str) -> Result<Live> {
        let _lease = self.lease.take(Lease::Switching).context("the radio is busy (a scan or another switch)")?;
        let previous = self.state();
        let plan = self.plan(site_id).await?;
        self.state.send_replace(LiveState::Switching { to: site_id.to_string() });
        match self.go(plan).await {
            Ok(live) => {
                self.state.send_replace(LiveState::Live(Box::new(live.clone())));
                *self.live_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                self.apply_profile().await;
                Ok(live)
            }
            Err(e) => {
                let back = match &previous {
                    LiveState::Live(l) if l.site.id != site_id => Some(l.site.id.clone()),
                    _ => None,
                };
                let restored = match back {
                    Some(id) => match self.plan(&id).await {
                        Ok(p) => self.go(p).await.ok(),
                        Err(_) => None,
                    },
                    None => None,
                };
                match restored {
                    Some(live) => {
                        self.log.system("site", format!("site {site_id} did not go live ({e:#}); back on {}", live.site.id));
                        self.state.send_replace(LiveState::Live(Box::new(live)));
                    }
                    None => {
                        self.log.system("site", format!("site {site_id} did not go live ({e:#}); no site is live"));
                        self.state.send_replace(LiveState::NoSite);
                    }
                }
                Err(e)
            }
        }
    }

    /// What `site_id` taught the radio: in memory while it is live, else as last saved.
    pub async fn learned(&self, site_id: &str) -> Result<SiteState> {
        if let (LiveState::Live(l), Some(learned)) = (self.state(), self.learned.lock().await.as_ref()) {
            if l.site.id == site_id {
                return Ok(learned.state());
            }
        }
        Ok(Config::site_state(&self.paths, site_id)?.value)
    }

    /// The live window against the site's channels and what was learned there.
    pub async fn window_view(&self) -> Option<WindowView> {
        let LiveState::Live(live) = self.state() else { return None };
        let learned = self.learned.lock().await.as_ref()?.state();
        let presets_allowed = self.config.lock().await.radio.value.presets_allowed.clone();
        let t = self.tuner.tuning();
        let chans = plan::channels(&live.site.channels_hz, &learned.grants);
        let covered = |f: u64| plan::covers(t.lo_hz as i64, f, t.sample_rate_hz);
        let covered_weight: f64 = chans.iter().filter(|c| covered(c.freq_hz)).map(|c| c.weight).sum();
        let total_weight: f64 = chans.iter().map(|c| c.weight).sum();
        let best = window_for(&live.site, &learned, &presets_allowed).ok();
        // A window narrower than the site's minimum is worth widening too.
        let too_narrow = best.as_ref().is_some_and(|b| t.sample_rate_hz < b.sample_rate_hz && live.site.window.min_preset.is_some());
        let better = too_narrow || best.as_ref().is_some_and(|b| plan::worth_moving(covered_weight, b.covered_weight, total_weight));
        Some(WindowView {
            site: live.site.id.clone(),
            auto: live.site.window.auto,
            min_preset: live.site.window.min_preset.clone(),
            preset: t.preset,
            sample_rate_hz: t.sample_rate_hz,
            lo_hz: t.lo_hz,
            control_hz: t.control_hz,
            channels: chans
                .iter()
                .map(|c| ChannelView {
                    freq_hz: c.freq_hz,
                    grants: learned.grants.get(&c.freq_hz).copied().unwrap_or(0),
                    listed: live.site.channels_hz.contains(&c.freq_hz),
                    weight: c.weight,
                    covered: covered(c.freq_hz),
                })
                .collect(),
            covered_weight,
            total_weight,
            best,
            better,
            last_recentre_unix_ms: learned.last_recentre_unix_ms,
        })
    }

    /// Move the window to the planner's choice while both lanes are idle, the site staying live.
    /// `force` moves even when the gain is small. `None` when there was nothing to move.
    pub async fn recentre(&self, force: bool, origin: &str) -> Result<Option<WindowPlan>> {
        let _lease = self.lease.take(Lease::Switching).context("the radio is busy (a switch or a scan)")?;
        let view = self.window_view().await.context("no site is live")?;
        let best = view.best.clone().context("no window plan")?;
        let same = view.preset == Some(best.preset.as_str()) && view.lo_hz as i64 == best.lo_hz;
        if same || !(view.better || force) {
            return Ok(None);
        }
        if self.trunking.calls().open.iter().any(|c| c.lane.is_some()) {
            bail!("a call is on a lane");
        }
        let preset = find_preset(&best.preset).context("planned preset")?;
        let tuning = self.tuner.apply(TuningPlan { preset, lo_hz: best.lo_hz as u64, control_hz: view.control_hz }).await?;
        self.trunking.window_moved().await;
        let now = crate::util::time::unix_ms();
        if let Some(l) = self.learned.lock().await.as_ref() {
            l.recentred(now);
        }
        self.state.send_modify(|s| {
            if let LiveState::Live(l) = s {
                l.window = best.clone();
                l.tuning = tuning;
            }
        });
        self.log.system(
            "site",
            format!(
                "recentre ({origin}): {} LO {:.4} MHz -> {} LO {:.4} MHz, channel weight {:.0}/{:.0} -> {:.0}/{:.0}",
                view.preset.unwrap_or("?"),
                view.lo_hz as f64 / 1e6,
                best.preset,
                best.lo_hz as f64 / 1e6,
                view.covered_weight,
                view.total_weight,
                best.covered_weight,
                best.total_weight
            ),
        );
        Ok(Some(best))
    }

    /// Recentre automatically: a site with an automatic window, live for two minutes, the last
    /// move ten minutes ago or more.
    pub fn start_recentre(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(RECENTRE_EVERY);
            loop {
                tick.tick().await;
                let settled = me.live_since.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|t| t.elapsed() >= RECENTRE_AFTER_START);
                if !settled || !me.lease.is_normal() {
                    continue;
                }
                let Some(view) = me.window_view().await else { continue };
                let rested = crate::util::time::unix_ms().saturating_sub(view.last_recentre_unix_ms) >= RECENTRE_MIN_INTERVAL_MS;
                if !view.auto || !view.better || !rested {
                    continue;
                }
                if let Err(e) = me.recentre(false, "auto").await {
                    tracing::debug!("recentre: {e:#}");
                }
            }
        });
    }

    /// Everything a switch needs, read and planned without touching the radio.
    async fn plan(&self, site_id: &str) -> Result<Plan> {
        let (site, system, profile, presets_allowed, calls) = {
            let c = self.config.lock().await;
            let Some((system, site)) = c.systems.value.site(site_id) else {
                bail!("no site {site_id:?}");
            };
            let profile = c.profiles.value.active_for(site_id).cloned();
            (site.clone(), SystemSummary::from(system), profile, c.radio.value.presets_allowed.clone(), c.radio.value.calls.clone())
        };
        let learned = Arc::new(Learned::new(site_id, Config::site_state(&self.paths, site_id)?));
        let window = window_for(&site, &learned.state(), &presets_allowed)?;
        let preset = find_preset(&window.preset).context("planned preset")?;
        Ok(Plan { site, system, profile, calls, learned, window, preset })
    }

    /// Stop the old site, tune, and start the new one.
    async fn go(&self, plan: Plan) -> Result<Live> {
        let Plan { site, system, profile, calls, learned, window, preset } = plan;
        let site_id = site.id.as_str();
        self.save_learned().await;
        self.receivers.stop().await;
        self.trunking.stop().await;
        let tuning = self
            .tuner
            .apply(TuningPlan { preset, lo_hz: window.lo_hz as u64, control_hz: site.control.freq_hz })
            .await?;
        *self.learned.lock().await = Some(learned.clone());
        {
            let mut c = self.config.lock().await;
            if c.state.value.live_site.as_deref() != Some(site_id) {
                c.state.value.live_site = Some(site_id.to_string());
                if let Err(e) = config::save(&self.paths.radio_state(), &c.state) {
                    tracing::warn!("live site not persisted: {e:#}");
                }
            }
        }
        let lcn_hz: std::collections::HashMap<u16, u64> =
            site.channel_plan.as_ref().map(|p| p.lcn_hz.iter().map(|(k, v)| (*k, *v)).collect()).unwrap_or_default();
        let setup = Setup {
            site: site.id.clone(),
            protocol: system.protocol,
            lcn_hz: lcn_hz.clone(),
            // Lane one carries both protocols; lane two only P25 (it has no IQ tap).
            lanes: match system.protocol {
                Protocol::P25 => self.lanes.clone(),
                Protocol::DmrTier3 => self.lanes.iter().copied().filter(|&l| l == Lane::One).collect(),
            },
            routing: profile.as_ref().map(Routing::new).unwrap_or_default(),
            encrypted: learned.state().encrypted_talkgroups.into_iter().collect(),
            policy: CallPolicy {
                hang: std::time::Duration::from_millis(calls.hang_ms),
                end_grace: std::time::Duration::from_millis(calls.end_grace_ms),
            },
            learned: Some(learned.clone()),
        };
        let trunk = self.trunking.start(setup, self.tuner.clone(), self.log.clone()).await;
        let context = receivers::Context {
            site: site.id.clone(),
            protocol: system.protocol,
            modulation: site.modulation,
            lcn_hz,
            trunk: Some(trunk),
            learned: Some(learned),
            history: self.history.clone(),
        };
        self.history.site(SiteInfo {
            id: site.id.clone(),
            system: system.id.clone(),
            protocol: system.protocol.as_str().into(),
            label: site.label.clone(),
            system_label: system.label.clone(),
        });
        self.receivers.start(context, self.tuner.hw()).await;
        tracing::info!("site {} live: {} at LO {} Hz", site.id, window.preset, window.lo_hz);
        self.log.system("site", format!("site {} ({}) live: {} window, LO {:.4} MHz", site.id, site.label, window.preset, window.lo_hz as f64 / 1e6));
        Ok(Live { site, system, profile, window, tuning })
    }
}

/// The receive window for a site: the planner over its listed channels and the grants seen
/// there; with no channels at all, the control channel at the `cc_position` edge of the
/// narrowest allowed preset.
pub fn window_for(site: &Site, learned: &SiteState, presets_allowed: &[String]) -> Result<WindowPlan> {
    let allowed: Vec<(&str, u32)> = presets_allowed
        .iter()
        .filter_map(|n| find_preset(n).map(|p| (p.name, p.sample_rate_hz)))
        .collect();
    let presets = plan::at_least(&allowed, site.window.min_preset.as_deref());
    let Some(&(narrowest, _)) = presets.first() else {
        bail!("no allowed preset for site {}", site.id);
    };
    let cc = site.control.freq_hz;
    let chans = plan::channels(&site.channels_hz, &learned.grants);
    if chans.is_empty() {
        let p: &DdcPreset = find_preset(narrowest).context("preset")?;
        let half = p.sample_rate_hz as f64 / 2.0 - EDGE_MARGIN_HZ;
        let offset = match site.window.cc_position {
            CcPosition::Top => half,
            CcPosition::Center => 0.0,
            CcPosition::Bottom => -half,
        };
        // Keep the control channel off the DC notch when centred.
        let lo = cc as f64 - offset + if offset == 0.0 { plan::DC_GUARD_HZ as f64 } else { 0.0 };
        return Ok(WindowPlan {
            preset: p.name.to_string(),
            sample_rate_hz: p.sample_rate_hz,
            lo_hz: lo.round() as i64,
            usable_half_hz: plan::usable_half_hz(p.sample_rate_hz),
            covered_weight: 0.0,
            total_weight: 0.0,
        });
    }
    plan::plan(cc, &chans, &presets).context("no window plan")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radio::tuner::RadioHw;
    use crate::services::config::systems::{Control, Window};

    /// No radio, with the AD9363's lower LO limit.
    struct Nothing;

    impl RadioHw for Nothing {
        async fn set_lo(&self, hz: u64) -> Result<()> {
            if hz < 325_000_000 {
                bail!("LO {hz} Hz is below the AD9363's range");
            }
            Ok(())
        }
        async fn set_rate(&self, _: u32, _: u32) -> Result<()> {
            Ok(())
        }
        async fn set_gain(&self, _: crate::hardware::ad9361::GainMode, _: Option<f64>) -> Result<()> {
            Ok(())
        }
        async fn configure_control(&self, _: &'static DdcPreset, _: f64) -> Result<()> {
            Ok(())
        }
        async fn set_control_nco(&self, _: f64, _: u32) -> Result<()> {
            Ok(())
        }
        async fn configure_lanes(&self, _: &'static DdcPreset) -> Result<()> {
            Ok(())
        }
        async fn retune_lane(&self, _: crate::hardware::p25core::Lane, _: f64, _: u32, _: bool) -> Result<()> {
            Ok(())
        }
        async fn pause_lane(&self, _: crate::hardware::p25core::Lane) -> Result<()> {
            Ok(())
        }
        async fn readback(&self, _: u32) -> crate::radio::tuner::Readback {
            Default::default()
        }
    }

    impl StreamSource for Nothing {
        fn control_streams(
            &self,
            _: crate::radio::streams::Wants,
            _: std::sync::mpsc::SyncSender<crate::radio::streams::Input>,
            _: Arc<std::sync::atomic::AtomicBool>,
            _: Arc<crate::radio::streams::StreamCounters>,
        ) -> Vec<tokio::task::JoinHandle<()>> {
            Vec::new()
        }
    }

    fn site(cc: u64, channels: Vec<u64>, pos: CcPosition) -> Site {
        Site {
            id: "s".into(),
            label: "S".into(),
            identity: Default::default(),
            control: Control { freq_hz: cc, ..Default::default() },
            modulation: Default::default(),
            channels_hz: channels,
            channel_plan: None,
            window: Window { cc_position: pos, ..Default::default() },
            notes: vec![],
            source: None,
        }
    }

    fn allowed() -> Vec<String> {
        ["8M", "12M", "16M"].map(String::from).to_vec()
    }

    #[test]
    fn a_site_without_channels_puts_its_control_channel_at_the_edge() {
        let w = window_for(&site(454_368_750, vec![], CcPosition::Top), &SiteState::default(), &allowed()).unwrap();
        assert_eq!((w.preset.as_str(), w.lo_hz), ("8M", 454_368_750 - 3_750_000));
        let w = window_for(&site(454_368_750, vec![], CcPosition::Center), &SiteState::default(), &allowed()).unwrap();
        assert_eq!(w.lo_hz, 454_368_750 + plan::DC_GUARD_HZ);
    }

    #[test]
    fn learned_grants_steer_the_window() {
        let clay = site(860_962_500, vec![852_438_500, 857_987_500, 860_962_500], CcPosition::Top);
        assert_eq!(window_for(&clay, &SiteState::default(), &allowed()).unwrap().preset, "12M");
        let learned = SiteState { grants: [(857_987_500, 2_000)].into(), ..Default::default() };
        assert_eq!(window_for(&clay, &learned, &allowed()).unwrap().preset, "8M");
    }

    #[tokio::test]
    async fn activation_tunes_publishes_and_persists_the_live_site() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(&dir.path().join("flash"), &dir.path().join("sd"));
        let mut config = Config::load(&paths).unwrap();
        config.systems.value.systems.push(System {
            id: "clay-county".into(),
            label: "Clay County".into(),
            protocol: Protocol::P25,
            identity: Default::default(),
            talkgroups: Default::default(),
            radios: Default::default(),
            sites: vec![
                Site { id: "clay".into(), ..site(860_962_500, vec![857_987_500], CcPosition::Top) },
                Site { id: "vhf".into(), ..site(155_000_000, vec![], CcPosition::Center) },
            ],
        });
        config::save(&paths.systems(), &config.systems).unwrap();
        let tuner = Arc::new(Tuner::new(Nothing, 0.0));
        let log = Arc::new(EventLog::default());
        let receivers = Arc::new(Receivers::new(log.clone()));
        let trunking = Arc::new(Trunking::new(crate::audio::live::Audio::start(&[Lane::One]), Default::default(), Default::default(), Default::default(), 1));
        let config = Arc::new(Mutex::new(config));
        let live = LiveSite::new(paths.clone(), config, tuner.clone(), RadioLease::default(), receivers.clone(), trunking.clone(), vec![Lane::One], log.clone(), Default::default());
        assert!(live.activate("duval").await.is_err());
        assert!(matches!(live.state(), LiveState::NoSite));
        let l = live.activate("clay").await.unwrap();
        assert_eq!(l.system.id, "clay-county");
        assert_eq!(tuner.tuning().control_hz, 860_962_500);
        assert!(matches!(live.state(), LiveState::Live(_)));
        assert_eq!(Config::load(&paths).unwrap().state.value.live_site.as_deref(), Some("clay"));
        assert!(receivers.status().running && receivers.status().site.as_deref() == Some("clay"));
        assert!(log.since(0, 10, false)[0].text.starts_with("site clay"));
        // The radio refuses the next site after the live one has stopped: the live one comes back.
        assert!(live.activate("vhf").await.is_err());
        assert!(matches!(live.state(), LiveState::Live(l) if l.site.id == "clay"));
        assert!(receivers.status().running && receivers.status().site.as_deref() == Some("clay"));
        assert_eq!(tuner.tuning().control_hz, 860_962_500);
        // Stopped: no site is live, now or at the next start.
        assert!(live.stop("vhf").await.is_err(), "only the live site stops");
        live.stop("clay").await.unwrap();
        assert!(matches!(live.state(), LiveState::NoSite));
        assert!(!receivers.status().running);
        assert_eq!(Config::load(&paths).unwrap().state.value.live_site, None);
        assert!(live.stop("clay").await.is_err());
    }

    #[tokio::test]
    async fn the_window_follows_the_grants_while_the_site_stays_live() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(&dir.path().join("flash"), &dir.path().join("sd"));
        let mut config = Config::load(&paths).unwrap();
        config.systems.value.systems.push(System {
            id: "clay-county".into(),
            label: "Clay County".into(),
            protocol: Protocol::P25,
            identity: Default::default(),
            talkgroups: Default::default(),
            radios: Default::default(),
            sites: vec![Site { id: "clay".into(), ..site(860_962_500, vec![857_987_500], CcPosition::Top) }],
        });
        config::save(&paths.systems(), &config.systems).unwrap();
        let tuner = Arc::new(Tuner::new(Nothing, 0.0));
        let log = Arc::new(EventLog::default());
        let receivers = Arc::new(Receivers::new(log.clone()));
        let trunking = Arc::new(Trunking::new(crate::audio::live::Audio::start(&[Lane::One]), Default::default(), Default::default(), Default::default(), 1));
        let live = LiveSite::new(paths, Arc::new(Mutex::new(config)), tuner.clone(), RadioLease::default(), receivers.clone(), trunking.clone(), vec![Lane::One], log, Default::default());
        live.activate("clay").await.unwrap();
        assert_eq!(tuner.tuning().preset, Some("8M"));
        assert!(!live.window_view().await.unwrap().better);
        assert_eq!(live.recentre(false, "auto").await.unwrap(), None, "nothing better yet");
        // The site turns out busiest on a channel 8.5 MHz below its control channel.
        {
            let learned = live.learned.lock().await.clone().unwrap();
            for _ in 0..1_500 {
                learned.grant(852_438_500);
            }
            for _ in 0..100 {
                learned.grant(857_987_500);
            }
        }
        let view = live.window_view().await.unwrap();
        assert!(view.better && !view.channels.iter().find(|c| c.freq_hz == 852_438_500).unwrap().covered);
        let moved = live.recentre(false, "auto").await.unwrap().unwrap();
        assert_eq!((tuner.tuning().preset, tuner.tuning().lo_hz as i64), (Some(moved.preset.as_str()), moved.lo_hz));
        let view = live.window_view().await.unwrap();
        assert!(!view.better && view.channels.iter().all(|c| c.covered) && view.last_recentre_unix_ms > 0);
        assert!(matches!(live.state(), LiveState::Live(l) if l.window == moved));
        assert_eq!(live.recentre(true, "by hand").await.unwrap(), None, "already there");
        receivers.stop().await;
        trunking.stop().await;
    }
}
