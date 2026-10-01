//! What the radio learns by itself, kept apart from the user's configuration:
//! `state/radio.json` (live site, crystal calibration) and `state/sites/<id>.json` (per site:
//! identity heard, channel plan, grant counts, encrypted talkgroups).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::VERSION;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RadioState {
    pub version: u32,
    /// The site to bring up at boot; `None` = no site yet.
    pub live_site: Option<String>,
    pub crystal: Option<Crystal>,
}

impl Default for RadioState {
    fn default() -> Self {
        RadioState { version: VERSION, live_site: None, crystal: None }
    }
}

/// Crystal calibration. The ppm is the quantity; the shift is what it came to at the LO it was
/// measured at, kept for reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Crystal {
    pub ppm: f64,
    pub measured_at_lo_hz: u64,
    pub lo_shift_hz: i64,
    pub control_freq_hz: Option<u64>,
    pub method: String,
    pub at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SiteState {
    pub version: u32,
    /// P25 identity bands (IDEN_UP), learned from the control channel.
    pub iden_bands: Vec<IdenBand>,
    /// Grants seen per downlink frequency: the window planner's weights.
    pub grants: BTreeMap<u64, u32>,
    /// Talkgroups seen encrypted.
    pub encrypted_talkgroups: Vec<u32>,
    pub last_recentre_unix_ms: u64,
}

impl Default for SiteState {
    fn default() -> Self {
        SiteState {
            version: VERSION,
            iden_bands: Vec::new(),
            grants: BTreeMap::new(),
            encrypted_talkgroups: Vec::new(),
            last_recentre_unix_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdenBand {
    pub identifier: u8,
    pub base_frequency_hz: u64,
    pub channel_spacing_hz: u32,
    pub bandwidth_hz: u32,
    pub transmit_offset_hz: i64,
    /// Timeslots per carrier (1: FDMA; 2 or more: a TDMA band). Absent in older files.
    #[serde(default)]
    pub slots: u8,
}
