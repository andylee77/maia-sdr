//! The unit's mode: the scanner (P25 and DMR trunking) or ATSC TV. A mode has the radio to
//! itself. ATSC mode takes the radio lease and keeps it while it lasts, with the live site
//! paused; back in scanner mode the receiver gain is the configured one again and the paused site
//! goes live. The mode is kept across restarts (`state/radio.json`).

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};

use crate::boot::radio::gain_mode;
use crate::radio::lease::{Lease, RadioLease};
use crate::radio::streams::StreamSource;
use crate::radio::tuner::{RadioHw, Tuner};
use crate::services::atsc::sweep::Atsc;
use crate::services::config::{self, Config, Paths};
use crate::services::events::EventLog;
use crate::trunking::site::LiveSite;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Scanner,
    Atsc,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Scanner => "scanner",
            Mode::Atsc => "ATSC TV",
        }
    }
}

/// What a change of mode moves.
pub struct Deps<'a, H> {
    pub lease: &'a RadioLease,
    pub live: &'a LiveSite<H>,
    pub tuner: &'a Tuner<H>,
    pub atsc: &'a Atsc,
    pub config: &'a Arc<Mutex<Config>>,
    pub paths: &'a Paths,
    pub log: &'a EventLog,
}

pub struct Modes {
    mode: watch::Sender<Mode>,
    /// One change at a time.
    changing: Mutex<()>,
}

impl Default for Modes {
    fn default() -> Self {
        Modes { mode: watch::channel(Mode::Scanner).0, changing: Mutex::new(()) }
    }
}

impl Modes {
    pub fn current(&self) -> Mode {
        *self.mode.borrow()
    }

    /// Bring the unit into `to`; returns once the radio is the new mode's. `boot_site` is the site
    /// scanner mode brings back when none is live (at boot, the configured one).
    pub async fn set<H: RadioHw + StreamSource + 'static>(&self, to: Mode, boot_site: Option<String>, d: Deps<'_, H>) -> Result<()> {
        let _one = self.changing.lock().await;
        if to == self.current() {
            return Ok(());
        }
        match to {
            Mode::Atsc => {
                let guard = d.lease.take(Lease::Atsc).context("the radio is busy (a site switch or a scan)")?;
                let back_to = d.live.pause_for_mode(boot_site).await;
                d.atsc.enter(guard, back_to).await;
                self.mode.send_replace(to);
                persist(to, &d).await;
            }
            Mode::Scanner => {
                let back_to = d.atsc.leave().await;
                self.mode.send_replace(to);
                persist(to, &d).await;
                let gain = d.config.lock().await.radio.value.gain.clone();
                if let Err(e) = d.tuner.set_gain(gain_mode(gain.mode), gain.manual_db.map(f64::from)).await {
                    tracing::error!("gain not set: {e:#}");
                }
                d.live.resume(back_to).await;
            }
        }
        d.log.system("mode", format!("{} mode", to.label()));
        Ok(())
    }
}

async fn persist<H>(mode: Mode, d: &Deps<'_, H>) {
    let mut c = d.config.lock().await;
    c.state.value.mode = mode;
    if let Err(e) = config::save(&d.paths.radio_state(), &c.state) {
        tracing::warn!("mode not persisted: {e:#}");
    }
}
