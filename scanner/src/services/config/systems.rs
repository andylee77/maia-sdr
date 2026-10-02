//! `systems.json`: the trunked systems the user keeps, their aliases and listening settings, and
//! their sites. Written on a user change or a scan "Add". What the radio learns on the air goes to
//! the per-site state files instead.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::aliases::{Alias, AliasIndex, Listening};
use super::VERSION;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemsConfig {
    pub version: u32,
    pub systems: Vec<System>,
}

impl Default for SystemsConfig {
    fn default() -> Self {
        SystemsConfig { version: VERSION, systems: Vec::new() }
    }
}

impl SystemsConfig {
    pub fn site(&self, id: &str) -> Option<(&System, &Site)> {
        self.systems.iter().find_map(|sys| sys.sites.iter().find(|s| s.id == id).map(|s| (sys, s)))
    }

    pub fn system(&self, id: &str) -> Option<&System> {
        self.systems.iter().find(|s| s.id == id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    P25,
    DmrTier3,
}

impl Protocol {
    /// As the files spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::P25 => "p25",
            Protocol::DmrTier3 => "dmr_tier3",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct System {
    pub id: String,
    pub label: String,
    pub protocol: Protocol,
    #[serde(default)]
    pub identity: SystemIdentity,
    /// What the radio knows of the system's talkgroups and radios (IDs are system-wide in P25
    /// and DMR): names, priorities, recording, speakers.
    #[serde(default)]
    pub aliases: Vec<Alias>,
    /// Talkgroups with no priority, and pre-emption.
    #[serde(default)]
    pub listening: Listening,
    #[serde(default)]
    pub sites: Vec<Site>,
}

impl System {
    pub fn alias_index(&self) -> AliasIndex {
        AliasIndex::new(&self.aliases)
    }
}

/// P25: WACN and system id. DMR Tier III: network model and network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wacn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<DmrModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmrModel {
    Tiny,
    Small,
    Large,
    Huge,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Site {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub identity: SiteIdentity,
    pub control: Control,
    #[serde(default)]
    pub modulation: Modulation,
    /// Known traffic channels (the window planner's seed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels_hz: Vec<u64>,
    /// DMR: logical channel numbers to downlink frequencies (grants name an LCN).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_plan: Option<ChannelPlan>,
    #[serde(default)]
    pub window: Window,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Where the site came from (the scan that found it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// P25: RFSS, site, NAC, LRA. DMR: site number and colour code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SiteIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rfss: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nac: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lra: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colour_code: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Control {
    pub freq_hz: u64,
    /// Other channels the control channel moves to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternates_hz: Vec<u64>,
    /// DMR: the control channel's LCN and timeslot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lcn: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeslot: Option<u8>,
}

/// P25 control-channel modulation. `Auto` lets the receiver pick LSM or C4FM by decode rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modulation {
    #[default]
    Auto,
    Lsm,
    C4fm,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelPlan {
    pub lcn_hz: BTreeMap<u16, u64>,
}

/// Where the receive window sits around the control channel when there is nothing better to go
/// on (no known channels).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CcPosition {
    /// Traffic below the control channel.
    Top,
    #[default]
    Center,
    /// Traffic above the control channel.
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Window {
    /// Let the planner move the window to the busiest channels.
    pub auto: bool,
    /// Narrowest DDC preset the planner may use.
    pub min_preset: Option<String>,
    pub cc_position: CcPosition,
}

impl Default for Window {
    fn default() -> Self {
        Window { auto: true, min_preset: None, cc_position: CcPosition::Center }
    }
}
