//! `profiles.json`: what to follow and where it plays. A profile belongs to a system; each site
//! picks its active profile.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::VERSION;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilesConfig {
    pub version: u32,
    pub profiles: Vec<Profile>,
    /// Site id -> profile id.
    pub active: BTreeMap<String, String>,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        ProfilesConfig { version: VERSION, profiles: Vec::new(), active: BTreeMap::new() }
    }
}

impl ProfilesConfig {
    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// The site's active profile, if it has one.
    pub fn active_for(&self, site: &str) -> Option<&Profile> {
        self.active.get(site).and_then(|id| self.profile(id))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// `<system id>/<slug of the name>`.
    pub id: String,
    pub system: String,
    pub name: String,
    /// Talkgroup groups in priority order (first = highest).
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub speakers: Speakers,
    /// Talkgroups to follow, in priority order; empty = every clear call.
    #[serde(default)]
    pub monitor: Vec<u32>,
    /// Talkgroups never followed; wins over the monitor list and the groups.
    #[serde(default)]
    pub ignore: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub talkgroups: Vec<u32>,
}

/// Speaker side of a group, or of the talkgroups in no group. `Off` = not followed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    #[default]
    Both,
    Left,
    Right,
    Off,
}

/// Which groups play on which speaker (by group name). `preempt`: a call of a higher-priority
/// group takes a traffic chain from a call of a lower one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Speakers {
    pub left: Vec<String>,
    pub right: Vec<String>,
    pub other: Side,
    pub preempt: bool,
}

impl Default for Speakers {
    fn default() -> Self {
        Speakers { left: Vec::new(), right: Vec::new(), other: Side::Both, preempt: true }
    }
}
