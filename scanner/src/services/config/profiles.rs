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

    /// A new profile of `system` named `name` (its id from the name, made unique), empty or a
    /// copy of `from`'s groups, speakers and lists.
    pub fn create(&mut self, system: &str, name: &str, from: Option<&str>) -> Result<Profile, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("a profile needs a name".into());
        }
        let base = format!("{system}/{}", slug(name));
        let id = if self.profile(&base).is_none() {
            base
        } else {
            (2..).map(|n| format!("{base}_{n}")).find(|c| self.profile(c).is_none()).unwrap_or(base)
        };
        let mut p = match from {
            Some(from) => {
                let src = self.profile(from).ok_or_else(|| format!("no profile {from}"))?;
                if src.system != system {
                    return Err(format!("profile {from} belongs to system {}", src.system));
                }
                src.clone()
            }
            None => Profile::default(),
        };
        p.id = id;
        p.system = system.to_string();
        p.name = name.to_string();
        self.profiles.push(p.clone());
        Ok(p)
    }

    /// Replace a profile's name, groups, speakers and lists; its id and system stay.
    pub fn update(&mut self, id: &str, new: Profile) -> Result<Profile, String> {
        let p = self.profiles.iter_mut().find(|p| p.id == id).ok_or_else(|| format!("no profile {id}"))?;
        let next = Profile { id: p.id.clone(), system: p.system.clone(), ..new };
        next.check()?;
        *p = next.clone();
        Ok(next)
    }

    /// Delete a profile no site uses.
    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        if let Some((site, _)) = self.active.iter().find(|(_, p)| *p == id) {
            return Err(format!("profile {id} is site {site}'s active profile"));
        }
        let before = self.profiles.len();
        self.profiles.retain(|p| p.id != id);
        if self.profiles.len() == before {
            return Err(format!("no profile {id}"));
        }
        Ok(())
    }
}

impl Profile {
    /// Names present and distinct; the speakers name only its groups.
    pub fn check(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("a profile needs a name".into());
        }
        let mut names = std::collections::HashSet::new();
        for g in &self.groups {
            if g.name.trim().is_empty() {
                return Err("every group needs a name".into());
            }
            if !names.insert(g.name.as_str()) {
                return Err(format!("two groups are named {}", g.name));
            }
        }
        for n in self.speakers.left.iter().chain(&self.speakers.right) {
            if !names.contains(n.as_str()) {
                return Err(format!("the speakers name group {n}, which the profile does not have"));
            }
        }
        Ok(())
    }
}

/// A file-safe id part from a name.
fn slug(name: &str) -> String {
    let mut s: String = name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "profile".into() } else { s }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_created_edited_and_deleted() {
        let mut c = ProfilesConfig::default();
        let p = c.create("clay-county", "Fire & EMS", None).unwrap();
        assert_eq!(p.id, "clay-county/fire_ems");
        assert_eq!(c.create("clay-county", "Fire & EMS", Some("clay-county/fire_ems")).unwrap().id, "clay-county/fire_ems_2");
        assert!(c.create("clay-county", " ", None).is_err());
        assert!(c.create("duval", "x", Some("clay-county/fire_ems")).is_err(), "a copy stays in its system");
        let edit = Profile {
            id: "ignored".into(),
            system: "ignored".into(),
            name: "Fire".into(),
            groups: vec![Group { name: "Primary".into(), talkgroups: vec![300] }],
            speakers: Speakers { left: vec!["Primary".into()], ..Default::default() },
            ..Default::default()
        };
        let p = c.update("clay-county/fire_ems", edit.clone()).unwrap();
        assert_eq!((p.id.as_str(), p.system.as_str(), p.name.as_str()), ("clay-county/fire_ems", "clay-county", "Fire"));
        let bad = Profile { speakers: Speakers { right: vec!["TAC".into()], ..Default::default() }, ..edit.clone() };
        assert!(c.update("clay-county/fire_ems", bad).unwrap_err().contains("TAC"));
        let twice = Profile { groups: vec![edit.groups[0].clone(), edit.groups[0].clone()], ..edit };
        assert!(c.update("clay-county/fire_ems", twice).is_err());
        c.active.insert("clay".into(), "clay-county/fire_ems".into());
        assert!(c.delete("clay-county/fire_ems").is_err(), "a site's active profile stays");
        c.delete("clay-county/fire_ems_2").unwrap();
        assert!(c.delete("clay-county/fire_ems_2").is_err());
    }
}
