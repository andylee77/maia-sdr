//! Which site is live: one state, one switch.
//!
//! `activate` is the only way to change site. It takes the radio lease (grants decoded during
//! the switch are dropped), plans the receive window from the site's channels and what was
//! learned there, tunes, loads the site's learned state and active profile, publishes `Live` and
//! persists the choice. Receivers and the call book join this sequence in later phases.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::sync::{watch, Mutex};

use crate::hardware::presets::{find_preset, DdcPreset};
use crate::radio::lease::{Lease, RadioLease};
use crate::radio::plan::{self, WindowPlan};
use crate::radio::tuner::{RadioHw, Tuner, Tuning, TuningPlan};
use crate::services::config::profiles::Profile;
use crate::services::config::systems::{CcPosition, Protocol, Site, System};
use crate::services::config::{self, Config, Paths, SiteState, Stored};

/// Distance of the control channel from the window edge when a site has no known channels.
const EDGE_MARGIN_HZ: f64 = 250_000.0;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LiveState {
    NoSite,
    Switching { to: String },
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

pub struct LiveSite<H> {
    paths: Paths,
    config: Arc<Mutex<Config>>,
    tuner: Arc<Tuner<H>>,
    lease: RadioLease,
    state: watch::Sender<LiveState>,
    /// What was learned on the live site.
    learned: Mutex<Option<Stored<SiteState>>>,
}

impl<H: RadioHw> LiveSite<H> {
    pub fn new(paths: Paths, config: Arc<Mutex<Config>>, tuner: Arc<Tuner<H>>, lease: RadioLease) -> Self {
        LiveSite {
            paths,
            config,
            tuner,
            lease,
            state: watch::channel(LiveState::NoSite).0,
            learned: Mutex::new(None),
        }
    }

    pub fn state(&self) -> LiveState {
        self.state.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<LiveState> {
        self.state.subscribe()
    }

    /// Make `site` live. Returns once it is.
    pub async fn activate(&self, site_id: &str) -> Result<Live> {
        let _lease = self.lease.take(Lease::Switching).context("the radio is busy (a scan or another switch)")?;
        let previous = self.state();
        self.state.send_replace(LiveState::Switching { to: site_id.to_string() });
        match self.switch(site_id).await {
            Ok(live) => {
                self.state.send_replace(LiveState::Live(Box::new(live.clone())));
                Ok(live)
            }
            Err(e) => {
                self.state.send_replace(previous);
                Err(e)
            }
        }
    }

    async fn switch(&self, site_id: &str) -> Result<Live> {
        let (site, system, profile, presets_allowed) = {
            let c = self.config.lock().await;
            let Some((system, site)) = c.systems.value.site(site_id) else {
                bail!("no site {site_id:?}");
            };
            let profile = c.profiles.value.active_for(site_id).cloned();
            (site.clone(), SystemSummary::from(system), profile, c.radio.value.presets_allowed.clone())
        };
        let learned = Config::site_state(&self.paths, site_id)?;
        let window = window_for(&site, &learned.value, &presets_allowed)?;
        let preset = find_preset(&window.preset).context("planned preset")?;
        let tuning = self
            .tuner
            .apply(TuningPlan { preset, lo_hz: window.lo_hz as u64, control_hz: site.control.freq_hz })
            .await?;
        *self.learned.lock().await = Some(learned);
        {
            let mut c = self.config.lock().await;
            if c.state.value.live_site.as_deref() != Some(site_id) {
                c.state.value.live_site = Some(site_id.to_string());
                if let Err(e) = config::save(&self.paths.radio_state(), &c.state) {
                    tracing::warn!("live site not persisted: {e:#}");
                }
            }
        }
        tracing::info!("site {} live: {} at LO {} Hz", site.id, window.preset, window.lo_hz);
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

    struct Nothing;

    impl RadioHw for Nothing {
        async fn set_lo(&self, _: u64) -> Result<()> {
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
            sites: vec![Site { id: "clay".into(), ..site(860_962_500, vec![857_987_500], CcPosition::Top) }],
        });
        config::save(&paths.systems(), &config.systems).unwrap();
        let tuner = Arc::new(Tuner::new(Nothing, 0.0));
        let live = LiveSite::new(paths.clone(), Arc::new(Mutex::new(config)), tuner.clone(), RadioLease::default());
        assert!(live.activate("duval").await.is_err());
        assert!(matches!(live.state(), LiveState::NoSite));
        let l = live.activate("clay").await.unwrap();
        assert_eq!(l.system.id, "clay-county");
        assert_eq!(tuner.tuning().control_hz, 860_962_500);
        assert!(matches!(live.state(), LiveState::Live(_)));
        assert_eq!(Config::load(&paths).unwrap().state.value.live_site.as_deref(), Some("clay"));
    }
}
