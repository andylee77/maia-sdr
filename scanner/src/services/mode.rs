//! The unit's mode: the scanner (P25 and DMR trunking), ATSC TV or data (which listens to the
//! data bands as soon as it has the radio). A mode has the radio to itself. The scanner's is the radio's own: its live site follows calls, and a scan or a site
//! switch borrows the radio from it. Any other mode takes the radio lease and keeps it while it
//! lasts, with the live site paused; from one such mode to another the hold is handed on, so
//! nothing else can take the radio between them. Back in scanner mode the receiver gain is the
//! configured one again and the paused site goes live. The mode is kept across restarts
//! (`state/radio.json`).
//!
//! A new mode is a variant here with its label, its service's `enter` and `leave`, and their arm
//! each in `Modes::set`.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};

use crate::boot::radio::gain_mode;
use crate::radio::lease::{Lease, LeaseGuard, RadioLease};
use crate::radio::streams::StreamSource;
use crate::radio::tuner::{RadioHw, Tuner};
use crate::services::atsc::sweep::Atsc;
use crate::services::config::{self, Config, Paths};
use crate::services::data::Data;
use crate::services::events::EventLog;
use crate::trunking::site::LiveSite;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Scanner,
    Atsc,
    Data,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Scanner => "scanner",
            Mode::Atsc => "ATSC TV",
            Mode::Data => "data",
        }
    }
}

/// A mode's hold on the radio while it lasts, and the site scanner mode brings back after it.
pub struct Hold {
    _lease: LeaseGuard,
    pub back_to: Option<String>,
}

impl Hold {
    pub fn new(lease: LeaseGuard, back_to: Option<String>) -> Hold {
        Hold { _lease: lease, back_to }
    }
}

/// What a change of mode moves.
pub struct Deps<'a, H> {
    pub lease: &'a RadioLease,
    pub live: &'a LiveSite<H>,
    pub tuner: &'a Arc<Tuner<H>>,
    pub atsc: &'a Atsc,
    pub data: &'a Data,
    pub config: &'a Arc<Mutex<Config>>,
    pub paths: &'a Paths,
    pub log: &'a Arc<EventLog>,
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
        let from = self.current();
        if to == from {
            return Ok(());
        }
        // The mode the radio is in gives its hold back.
        let held = match from {
            Mode::Scanner => None,
            Mode::Atsc => d.atsc.leave().await,
            Mode::Data => d.data.leave().await,
        };
        // The scanner has none to give: the radio is taken from it, and its site paused.
        let hold = match held {
            Some(hold) => hold,
            None => {
                let lease = d.lease.take(Lease::Mode).context("the radio is busy (a site switch or a scan)")?;
                Hold::new(lease, d.live.pause_for_mode(to.label(), boot_site).await)
            }
        };
        match to {
            Mode::Scanner => {
                let Hold { _lease, back_to } = hold;
                drop(_lease);
                self.mode.send_replace(to);
                persist(to, &d).await;
                let gain = d.config.lock().await.radio.value.gain.clone();
                if let Err(e) = d.tuner.set_gain(gain_mode(gain.mode), gain.manual_db.map(f64::from)).await {
                    tracing::error!("gain not set: {e:#}");
                }
                d.live.resume(back_to).await;
            }
            Mode::Atsc => {
                d.atsc.enter(hold).await;
                self.mode.send_replace(to);
                persist(to, &d).await;
            }
            Mode::Data => {
                let gain_db = crate::services::data::gain_db(&d.config.lock().await.radio.value.gain);
                d.data.enter(hold, d.tuner.clone(), gain_db, d.log.clone()).await;
                self.mode.send_replace(to);
                persist(to, &d).await;
            }
        }
        d.log.radio("mode", format!("{} mode", to.label()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_goes_by_its_name_in_the_api_and_on_the_flash() {
        for (mode, name) in [(Mode::Scanner, "\"scanner\""), (Mode::Atsc, "\"atsc\""), (Mode::Data, "\"data\"")] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), name);
            assert_eq!(serde_json::from_str::<Mode>(name).unwrap(), mode);
        }
        assert!(serde_json::from_str::<Mode>("\"tv\"").is_err());
    }
}
