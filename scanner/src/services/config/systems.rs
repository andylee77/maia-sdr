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
    #[serde(default)]
    pub details: SystemDetails,
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

/// What describes a system beside its identity, as RadioReference lists it. The radio does not
/// act on any of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemDetails {
    /// "Green Cove Springs, FL".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub county: Option<String>,
    /// "Project 25 Phase I".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_type: Option<String>,
    /// "APCO-25 Common Air Interface Exclusive".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
}

impl SystemDetails {
    /// Each field trimmed; an empty one is none.
    pub fn tidied(self) -> SystemDetails {
        let t = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        SystemDetails { location: t(self.location), county: t(self.county), system_type: t(self.system_type), voice: t(self.voice) }
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

impl SystemIdentity {
    /// The fields `protocol` has, in range: P25 a 20-bit WACN and a 12-bit system ID, DMR a
    /// model and a network.
    pub fn check(&self, protocol: Protocol) -> Result<(), String> {
        match protocol {
            Protocol::P25 if self.model.is_some() || self.network.is_some() => Err("a P25 system has a WACN and a system ID".into()),
            Protocol::P25 if self.wacn.is_some_and(|w| w > 0xF_FFFF) => Err("a WACN is 5 hex digits".into()),
            Protocol::P25 if self.system.is_some_and(|s| s > 0xFFF) => Err("a P25 system ID is 3 hex digits".into()),
            Protocol::DmrTier3 if self.wacn.is_some() || self.system.is_some() => Err("a DMR system has a model and a network".into()),
            _ => Ok(()),
        }
    }
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

impl SiteIdentity {
    /// The fields `protocol` has, in range: P25 an 8-bit RFSS, site and LRA and a 12-bit NAC,
    /// DMR a site and a colour code of 0 to 15.
    pub fn check(&self, protocol: Protocol) -> Result<(), String> {
        let over = |v: Option<u32>, max: u32| v.is_some_and(|v| v > max);
        match protocol {
            Protocol::P25 if self.colour_code.is_some() => Err("a P25 site has no colour code".into()),
            Protocol::P25 if over(self.rfss, 255) || over(self.site, 255) || over(self.lra, 255) => {
                Err("a P25 RFSS, site and LRA are 0 to 255".into())
            }
            Protocol::P25 if over(self.nac, 0xFFF) => Err("a NAC is 3 hex digits".into()),
            Protocol::DmrTier3 if self.rfss.is_some() || self.nac.is_some() || self.lra.is_some() => {
                Err("a DMR site has a site number and a colour code".into())
            }
            Protocol::DmrTier3 if self.colour_code.is_some_and(|c| c > 15) => Err("a colour code is 0 to 15".into()),
            _ => Ok(()),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_system_identity_has_its_protocol_fields_in_range() {
        let p25 = SystemIdentity { wacn: Some(0xBEE00), system: Some(0x8A0), ..Default::default() };
        assert!(p25.check(Protocol::P25).is_ok());
        assert!(SystemIdentity::default().check(Protocol::P25).is_ok(), "not known yet");
        assert!(SystemIdentity { system: Some(0x1000), ..p25.clone() }.check(Protocol::P25).is_err());
        assert!(SystemIdentity { wacn: Some(0x10_0000), ..p25.clone() }.check(Protocol::P25).is_err());
        assert!(p25.check(Protocol::DmrTier3).is_err());
        let dmr = SystemIdentity { model: Some(DmrModel::Small), network: Some(0), ..Default::default() };
        assert!(dmr.check(Protocol::DmrTier3).is_ok());
        assert!(dmr.check(Protocol::P25).is_err());
    }

    #[test]
    fn a_site_identity_has_its_protocol_fields_in_range() {
        let p25 = SiteIdentity { rfss: Some(1), site: Some(1), nac: Some(0x8A1), ..Default::default() };
        assert!(p25.check(Protocol::P25).is_ok());
        assert!(SiteIdentity { nac: Some(0x1000), ..p25.clone() }.check(Protocol::P25).is_err());
        assert!(SiteIdentity { site: Some(256), ..p25.clone() }.check(Protocol::P25).is_err());
        assert!(p25.check(Protocol::DmrTier3).is_err());
        let dmr = SiteIdentity { site: Some(2), colour_code: Some(0), ..Default::default() };
        assert!(dmr.check(Protocol::DmrTier3).is_ok());
        assert!(SiteIdentity { colour_code: Some(16), ..dmr.clone() }.check(Protocol::DmrTier3).is_err());
        assert!(dmr.check(Protocol::P25).is_err());
    }

    #[test]
    fn details_are_trimmed_and_empty_ones_dropped() {
        let d = SystemDetails { location: Some(" Green Cove Springs, FL ".into()), county: Some("  ".into()), ..Default::default() }.tidied();
        assert_eq!((d.location.as_deref(), d.county), (Some("Green Cove Springs, FL"), None));
    }
}
