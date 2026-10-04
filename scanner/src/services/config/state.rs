//! What the radio learns by itself, kept apart from the user's configuration:
//! `state/radio.json` (mode, live site, crystal calibration) and `state/sites/<id>.json` (per site:
//! channel plan, grant counts, encrypted talkgroups, what the site announces of its neighbours
//! and other channels, and the DMR channels a lane heard name themselves).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::VERSION;
use crate::services::mode::Mode;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RadioState {
    pub version: u32,
    /// The unit's mode at boot.
    pub mode: Mode,
    /// The site to bring up at boot (in scanner mode); `None` = no site yet.
    pub live_site: Option<String>,
    pub crystal: Option<Crystal>,
}

impl Default for RadioState {
    fn default() -> Self {
        RadioState { version: VERSION, mode: Mode::Scanner, live_site: None, crystal: None }
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
    /// Adjacent sites the control channel announces.
    pub neighbours: Vec<NeighbourSite>,
    /// The site's secondary control channels, as announced.
    pub secondary_control_hz: Vec<u64>,
    /// The site's packet data channel, as announced.
    pub data_channel_hz: Option<u64>,
    /// DMR: the logical channels whose downlink the radio learned (a call followed there whose
    /// voice header named the granted talkgroup).
    pub lcn_hz: BTreeMap<u16, u64>,
    /// DMR: every logical channel a grant named: the channel table's rows, known or not.
    pub lcns_granted: BTreeSet<u16>,
    /// DMR: the channels a lane heard, by downlink: the network and site each named.
    pub channels_heard: BTreeMap<u64, HeardChannel>,
}

/// A DMR channel as a lane heard it name itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeardChannel {
    /// TINY, SMALL, LARGE or HUGE.
    pub model: String,
    pub network: u32,
    pub site: u32,
    pub colour_code: Option<u8>,
    /// A control channel (else a traffic channel).
    pub control: bool,
    /// It is this site's: its network, site and colour code are the site's. `None`: the site's
    /// own are not configured.
    pub own: Option<bool>,
    pub last_heard_unix_ms: u64,
}

/// An adjacent site as the control channel last announced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeighbourSite {
    pub system: u16,
    pub rfss: u8,
    pub site: u8,
    /// Its control channel, once the band plan names the channel.
    pub control_hz: Option<u64>,
    pub last_heard_unix_ms: u64,
}

impl Default for SiteState {
    fn default() -> Self {
        SiteState {
            version: VERSION,
            iden_bands: Vec::new(),
            grants: BTreeMap::new(),
            encrypted_talkgroups: Vec::new(),
            last_recentre_unix_ms: 0,
            neighbours: Vec::new(),
            secondary_control_hz: Vec::new(),
            data_channel_hz: None,
            lcn_hz: BTreeMap::new(),
            lcns_granted: BTreeSet::new(),
            channels_heard: BTreeMap::new(),
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
