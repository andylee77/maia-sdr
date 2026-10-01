//! What the live site taught the radio (`state/sites/<id>.json`): its IDEN bands, the grants per
//! channel (the window planner's weights), the talkgroups seen encrypted, and its neighbours,
//! secondary control channels and data channel. The receivers and the trunking task add to it;
//! it is saved every 10 minutes when it changed, on a site switch and at shutdown.

use std::sync::Mutex;

use crate::protocol::events::{LogicalChannel, Neighbour};
use crate::protocol::p25::tsbk::FrequencyBand;
use crate::services::config::state::{IdenBand, NeighbourSite, SiteState};
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

    fn with<T>(&self, f: impl FnOnce(&mut SiteState) -> (T, bool)) -> Option<T> {
        let mut s = self.state.lock().ok()?;
        let (out, changed) = f(&mut s.0.value);
        s.1 |= changed;
        Some(out)
    }

    pub fn state(&self) -> SiteState {
        self.state.lock().map(|s| s.0.value.clone()).unwrap_or_default()
    }

    /// The packet data channel the site announces.
    pub fn data_channel_hz(&self) -> Option<u64> {
        self.state.lock().ok().and_then(|s| s.0.value.data_channel_hz)
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

    /// An adjacent site announced. Only a new site or a moved control channel counts as a
    /// change; the time heard is saved with the next one.
    pub fn neighbour(&self, n: &Neighbour, at_unix_ms: u64) {
        let entry = NeighbourSite { system: n.system, rfss: n.rfss, site: n.site, control_hz: n.control.freq_hz, last_heard_unix_ms: at_unix_ms };
        self.with(|s| match s.neighbours.iter_mut().find(|x| (x.system, x.rfss, x.site) == (n.system, n.rfss, n.site)) {
            Some(x) => {
                let moved = x.control_hz != entry.control_hz;
                *x = entry;
                ((), moved)
            }
            None => {
                s.neighbours.push(entry);
                s.neighbours.sort_by_key(|x| (x.system, x.rfss, x.site));
                ((), true)
            }
        });
    }

    /// The secondary control channels announced (those the band plan names).
    pub fn secondary_control(&self, channels: &[LogicalChannel]) {
        let mut hz: Vec<u64> = channels.iter().filter_map(|c| c.freq_hz).collect();
        hz.sort_unstable();
        hz.dedup();
        self.with(|s| {
            let changed = s.secondary_control_hz != hz;
            s.secondary_control_hz = hz;
            ((), changed)
        });
    }

    pub fn data_channel(&self, channel: &LogicalChannel) {
        self.with(|s| {
            let changed = s.data_channel_hz != channel.freq_hz;
            s.data_channel_hz = channel.freq_hz;
            ((), changed)
        });
    }

    /// The window moved to the planner's choice at `at_unix_ms`.
    pub fn recentred(&self, at_unix_ms: u64) {
        self.with(|s| {
            s.last_recentre_unix_ms = at_unix_ms;
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

    #[test]
    fn announced_neighbours_and_channels_are_kept() {
        use crate::protocol::events::{ChannelId, LogicalChannel, Neighbour};
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(&dir.path().join("flash"), &dir.path().join("sd"));
        let learned = Learned::new("clay", Config::site_state(&paths, "clay").unwrap());
        let channel = |hz: Option<u64>| LogicalChannel { id: ChannelId::P25 { iden: 1, number: 100 }, slot: None, freq_hz: hz, tdma: false };
        let neighbour = |hz| Neighbour {
            system: 0x1A2,
            rfss: 1,
            site: 3,
            lra: 0,
            control: channel(hz),
            service_class: 0,
            conventional: false,
            failure: false,
            valid: true,
            active: true,
        };
        learned.neighbour(&neighbour(Some(853_312_500)), 1);
        learned.secondary_control(&[channel(Some(860_437_500)), channel(None)]);
        learned.data_channel(&channel(Some(859_212_500)));
        learned.save(&paths);
        // Heard again unchanged: not worth a flash write on its own.
        learned.neighbour(&neighbour(Some(853_312_500)), 2);
        learned.save(&paths);
        let back = Config::site_state(&paths, "clay").unwrap().value;
        assert_eq!((back.neighbours.len(), back.neighbours[0].last_heard_unix_ms), (1, 1));
        assert_eq!(back.secondary_control_hz, vec![860_437_500]);
        assert_eq!(back.data_channel_hz, Some(859_212_500));
        // Its control channel moved: saved.
        learned.neighbour(&neighbour(Some(853_337_500)), 3);
        learned.save(&paths);
        let back = Config::site_state(&paths, "clay").unwrap().value;
        assert_eq!((back.neighbours[0].control_hz, back.neighbours[0].last_heard_unix_ms), (Some(853_337_500), 3));
    }
}
