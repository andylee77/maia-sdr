//! Which site is live: one state, one switch.
//!
//! `activate` is the only way to change site. It takes the radio lease (grants decoded during
//! the switch are dropped), stops the old site's receivers, plans the receive window from the
//! site's channels and what was learned there, tunes, loads the site's learned state and active
//! profile, starts the new site's receivers, publishes `Live` and persists the choice. The call
//! book joins this sequence in a later phase.

use std::sync::Arc;

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
        }
    }

    pub fn state(&self) -> LiveState {
        self.state.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<LiveState> {
        self.state.subscribe()
    }

    /// Save what the live site taught, when it changed.
    pub async fn save_learned(&self) {
        if let Some(l) = self.learned.lock().await.as_ref() {
            l.save(&self.paths);
        }
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

    /// The profiles changed: the live site follows its active profile from now on.
    pub async fn profile_changed(&self) {
        let LiveState::Live(mut live) = self.state() else { return };
        let profile = self.config.lock().await.profiles.value.active_for(&live.site.id).cloned();
        if profile == live.profile {
            return;
        }
        self.trunking.set_routing(profile.as_ref().map(Routing::new).unwrap_or_default()).await;
        let name = profile.as_ref().map_or("none".to_string(), |p| p.name.clone());
        live.profile = profile;
        self.state.send_replace(LiveState::Live(live));
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
        let (site, system, profile, presets_allowed, calls) = {
            let c = self.config.lock().await;
            let Some((system, site)) = c.systems.value.site(site_id) else {
                bail!("no site {site_id:?}");
            };
            let profile = c.profiles.value.active_for(site_id).cloned();
            (site.clone(), SystemSummary::from(system), profile, c.radio.value.presets_allowed.clone(), c.radio.value.calls.clone())
        };
        self.save_learned().await;
        let learned = Arc::new(Learned::new(site_id, Config::site_state(&self.paths, site_id)?));
        let window = window_for(&site, &learned.state(), &presets_allowed)?;
        let preset = find_preset(&window.preset).context("planned preset")?;
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
            sites: vec![Site { id: "clay".into(), ..site(860_962_500, vec![857_987_500], CcPosition::Top) }],
        });
        config::save(&paths.systems(), &config.systems).unwrap();
        let tuner = Arc::new(Tuner::new(Nothing, 0.0));
        let log = Arc::new(EventLog::default());
        let receivers = Arc::new(Receivers::new(log.clone()));
        let trunking = Arc::new(Trunking::new(crate::audio::live::Audio::start(&[Lane::One]), Default::default(), Default::default(), 1));
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
        receivers.stop().await;
        trunking.stop().await;
    }
}
