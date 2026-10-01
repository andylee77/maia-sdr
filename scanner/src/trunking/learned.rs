//! What the live site taught the radio (`state/sites/<id>.json`): its IDEN bands, the grants per
//! channel (the window planner's weights) and the talkgroups seen encrypted. The receivers and
//! the trunking task add to it; it is saved every 10 minutes when it changed, on a site switch
//! and at shutdown.

use std::sync::Mutex;

use crate::protocol::p25::tsbk::FrequencyBand;
use crate::services::config::state::{IdenBand, SiteState};
use crate::services::config::{self, Paths, Stored};

pub const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Debug)]
pub struct Learned {
    site: String,
    state: Mutex<(Stored<SiteState>, bool)>,
}

fn band_of(b: &IdenBand) -> FrequencyBand {
    FrequencyBand {
        identifier: b.identifier,
        bandwidth_hz: b.bandwidth_hz,
        transmit_offset_hz: b.transmit_offset_hz as i32,
        channel_spacing_hz: b.channel_spacing_hz,
        base_frequency_hz: b.base_frequency_hz,
        slots: b.slots.max(1),
    }
}

impl Learned {
    pub fn new(site: &str, stored: Stored<SiteState>) -> Self {
        Learned { site: site.to_string(), state: Mutex::new((stored, false)) }
    }

    pub fn site(&self) -> &str {
        &self.site
    }

    fn with<T>(&self, f: impl FnOnce(&mut SiteState) -> (T, bool)) -> Option<T> {
        let mut s = self.state.lock().ok()?;
        let (out, changed) = f(&mut s.0.value);
        s.1 |= changed;
        Some(out)
    }

    pub fn state(&self) -> SiteState {
        self.state.lock().map(|s| s.0.value.clone()).unwrap_or_default()
    }

    /// The stored IDEN bands, for the control decoder to start with.
    pub fn bands(&self) -> Vec<FrequencyBand> {
        self.state().iden_bands.iter().map(band_of).collect()
    }

    pub fn band(&self, b: &FrequencyBand) {
        let entry = iden_band(b);
        self.with(|s| match s.iden_bands.iter_mut().find(|x| x.identifier == b.identifier) {
            Some(x) if *x == entry => ((), false),
            Some(x) => {
                *x = entry;
                ((), true)
            }
            None => {
                s.iden_bands.push(entry);
                s.iden_bands.sort_by_key(|x| x.identifier);
                ((), true)
            }
        });
    }

    /// A call opened on `freq_hz`.
    pub fn grant(&self, freq_hz: u64) {
        self.with(|s| {
            *s.grants.entry(freq_hz).or_default() += 1;
            ((), true)
        });
    }

    pub fn encrypted(&self, tg: u32) {
        self.with(|s| {
            if s.encrypted_talkgroups.contains(&tg) {
                return ((), false);
            }
            s.encrypted_talkgroups.push(tg);
            s.encrypted_talkgroups.sort_unstable();
            ((), true)
        });
    }

    /// Save when anything changed. The file is written from a copy, so the decoders updating
    /// the state never wait for the flash.
    pub fn save(&self, paths: &Paths) {
        let snapshot = {
            let Ok(mut s) = self.state.lock() else { return };
            if !s.1 {
                return;
            }
            s.1 = false;
            s.0.clone()
        };
        if let Err(e) = config::save(&paths.site_state(&self.site), &snapshot) {
            tracing::warn!("learned state of site {} not saved: {e:#}", self.site);
            if let Ok(mut s) = self.state.lock() {
                s.1 = true;
            }
        }
    }
}


/// An announced band as the site state keeps it.
pub fn iden_band(b: &FrequencyBand) -> IdenBand {
    IdenBand {
        identifier: b.identifier,
        base_frequency_hz: b.base_frequency_hz,
        channel_spacing_hz: b.channel_spacing_hz,
        bandwidth_hz: b.bandwidth_hz,
        transmit_offset_hz: i64::from(b.transmit_offset_hz),
        slots: b.slots,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::config::Config;

    #[test]
    fn learning_is_saved_when_it_changed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(&dir.path().join("flash"), &dir.path().join("sd"));
        let learned = Learned::new("clay", Config::site_state(&paths, "clay").unwrap());
        let band = FrequencyBand {
            identifier: 0,
            bandwidth_hz: 12_500,
            transmit_offset_hz: -45_000_000,
            channel_spacing_hz: 6_250,
            base_frequency_hz: 851_006_250,
            slots: 1,
        };
        learned.band(&band);
        learned.band(&band);
        learned.grant(857_987_500);
        learned.grant(857_987_500);
        learned.encrypted(402);
        learned.encrypted(402);
        learned.save(&paths);
        let back = Config::site_state(&paths, "clay").unwrap().value;
        assert_eq!(back.iden_bands.len(), 1);
        assert_eq!(back.grants[&857_987_500], 2);
        assert_eq!(back.encrypted_talkgroups, vec![402]);
        let again = Learned::new("clay", Config::site_state(&paths, "clay").unwrap());
        assert_eq!(again.bands(), vec![band]);
    }
}
