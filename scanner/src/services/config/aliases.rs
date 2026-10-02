//! Aliases: what the radio knows of a system's talkgroups and radios, as SDRTrunk's alias lists
//! hold it (a name, a group, a color, the IDs, a monitor priority, a record flag), with the
//! speaker each plays on. The follower, the recorder and every name shown come from here; a
//! system's `Listening` settles talkgroups with no priority.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::profiles::Profile;

/// Where a talkgroup plays. `Off`: nowhere (an alias says `do_not_monitor` instead).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    #[default]
    Both,
    Left,
    Right,
    Off,
}

/// The lowest monitor priority (SDRTrunk's range is 1 to 100).
pub const LOWEST_PRIORITY: u8 = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alias {
    pub name: String,
    /// A free label to sort and filter by (SDRTrunk's alias group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub ids: Vec<AliasId>,
    /// Monitor priority, 1 (highest) to 100 (lowest). None: unmonitored (see `Listening`).
    #[serde(default)]
    pub priority: Option<u8>,
    /// Never followed (SDRTrunk's priority -1).
    #[serde(default)]
    pub do_not_monitor: bool,
    /// Its calls are recorded.
    #[serde(default)]
    pub record: bool,
    /// Where it plays: both, left or right.
    #[serde(default)]
    pub speaker: Side,
    /// SDRTrunk's icon name, kept for the round trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AliasId {
    Talkgroup { value: u32 },
    TalkgroupRange { min: u32, max: u32 },
    Radio { value: u32 },
    RadioRange { min: u32, max: u32 },
}

impl Alias {
    /// A talkgroup's alias with only a name.
    pub fn talkgroup(tg: u32, name: impl Into<String>) -> Self {
        Alias::named(name, AliasId::Talkgroup { value: tg })
    }

    pub fn radio(id: u32, name: impl Into<String>) -> Self {
        Alias::named(name, AliasId::Radio { value: id })
    }

    fn named(name: impl Into<String>, id: AliasId) -> Self {
        Alias {
            name: name.into(),
            group: None,
            color: None,
            ids: vec![id],
            priority: None,
            do_not_monitor: false,
            record: false,
            speaker: Side::Both,
            icon: None,
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("an alias needs a name".into());
        }
        if self.ids.is_empty() {
            return Err(format!("alias {} names no talkgroup or radio", self.name));
        }
        if self.priority.is_some_and(|p| !(1..=LOWEST_PRIORITY).contains(&p)) {
            return Err(format!("alias {}: priority is 1 to {LOWEST_PRIORITY}", self.name));
        }
        if self.speaker == Side::Off {
            return Err(format!("alias {}: a speaker is both, left or right (do_not_monitor stops it)", self.name));
        }
        for id in &self.ids {
            if let AliasId::TalkgroupRange { min, max } | AliasId::RadioRange { min, max } = id {
                if min > max {
                    return Err(format!("alias {}: a range runs from its min to its max", self.name));
                }
            }
        }
        Ok(())
    }
}

/// How a system treats talkgroups with no priority (no alias, or an alias without one), and
/// pre-emption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Listening {
    /// They are followed, at the lowest priority. Off: only talkgroups with a priority are
    /// (SDRTrunk's "ignore unmonitored calls").
    pub follow_unmonitored: bool,
    /// Where they play: both, left or right.
    pub unmonitored_speaker: Side,
    /// A call of a higher priority takes a traffic channel from a call of a lower one.
    pub preempt: bool,
}

impl Listening {
    pub fn check(&self) -> Result<(), String> {
        if self.unmonitored_speaker == Side::Off {
            return Err("unmonitored talkgroups play on both, left or right (follow_unmonitored stops them)".into());
        }
        Ok(())
    }
}

impl Default for Listening {
    fn default() -> Self {
        Listening { follow_unmonitored: true, unmonitored_speaker: Side::Both, preempt: true }
    }
}

/// A system's aliases by talkgroup and by radio: an exact ID first, then the first range that
/// holds it.
#[derive(Debug, Clone, Default)]
pub struct AliasIndex {
    aliases: Vec<Alias>,
    talkgroups: HashMap<u32, usize>,
    talkgroup_ranges: Vec<(u32, u32, usize)>,
    radios: HashMap<u32, usize>,
    radio_ranges: Vec<(u32, u32, usize)>,
}

impl AliasIndex {
    pub fn new(aliases: &[Alias]) -> Self {
        let mut ix = AliasIndex { aliases: aliases.to_vec(), ..Default::default() };
        for (i, a) in aliases.iter().enumerate() {
            for id in &a.ids {
                match *id {
                    AliasId::Talkgroup { value } => {
                        ix.talkgroups.entry(value).or_insert(i);
                    }
                    AliasId::TalkgroupRange { min, max } => ix.talkgroup_ranges.push((min, max, i)),
                    AliasId::Radio { value } => {
                        ix.radios.entry(value).or_insert(i);
                    }
                    AliasId::RadioRange { min, max } => ix.radio_ranges.push((min, max, i)),
                }
            }
        }
        ix
    }

    pub fn talkgroup(&self, tg: u32) -> Option<&Alias> {
        let i = self.talkgroups.get(&tg).copied().or_else(|| in_range(&self.talkgroup_ranges, tg))?;
        self.aliases.get(i)
    }

    pub fn radio(&self, id: u32) -> Option<&Alias> {
        let i = self.radios.get(&id).copied().or_else(|| in_range(&self.radio_ranges, id))?;
        self.aliases.get(i)
    }

    pub fn aliases(&self) -> &[Alias] {
        &self.aliases
    }
}

fn in_range(ranges: &[(u32, u32, usize)], v: u32) -> Option<usize> {
    ranges.iter().find(|(min, max, _)| (*min..=*max).contains(&v)).map(|r| r.2)
}

/// The aliases and listening settings that stand for the name maps and the profile of an
/// earlier version: each name becomes an alias; a profile's groups give their talkgroups a
/// priority (the first group 1), a group and a speaker; a group on no speaker and the ignore
/// list become do-not-monitor; a monitor list leaves only its talkgroups (and the groups') followed.
pub fn from_names_and_profile(
    talkgroups: &BTreeMap<u32, String>,
    radios: &BTreeMap<u32, String>,
    profile: Option<&Profile>,
) -> (Vec<Alias>, Listening) {
    let mut aliases: Vec<Alias> = talkgroups.iter().map(|(&tg, name)| Alias::talkgroup(tg, name.clone())).collect();
    let mut listening = Listening::default();
    if let Some(p) = profile {
        let mut by_tg: HashMap<u32, usize> = aliases.iter().enumerate().map(|(i, a)| (tg_of(a), i)).collect();
        let mut alias_of = |aliases: &mut Vec<Alias>, tg: u32| -> usize {
            *by_tg.entry(tg).or_insert_with(|| {
                aliases.push(Alias::talkgroup(tg, format!("TG {tg}")));
                aliases.len() - 1
            })
        };
        let sp = &p.speakers;
        let monitor: std::collections::HashSet<u32> = p.monitor.iter().copied().collect();
        for (rank, g) in p.groups.iter().enumerate() {
            let side = if sp.left.contains(&g.name) {
                Side::Left
            } else if sp.right.contains(&g.name) {
                Side::Right
            } else {
                Side::Off
            };
            for &tg in &g.talkgroups {
                let i = alias_of(&mut aliases, tg);
                let a = &mut aliases[i];
                if a.group.is_some() {
                    continue; // A talkgroup in several groups keeps the first (highest).
                }
                a.group = Some(g.name.clone());
                a.priority = Some((rank + 1).min(usize::from(LOWEST_PRIORITY) - 1) as u8);
                match side {
                    Side::Off => a.do_not_monitor = true,
                    s => a.speaker = s,
                }
            }
        }
        if !monitor.is_empty() {
            listening.follow_unmonitored = false;
            for &tg in &p.monitor {
                let i = alias_of(&mut aliases, tg);
                let a = &mut aliases[i];
                if a.priority.is_none() {
                    a.priority = Some(LOWEST_PRIORITY);
                    match sp.other {
                        Side::Off => a.do_not_monitor = true,
                        s => a.speaker = s,
                    }
                }
            }
            // A grouped talkgroup off the monitor list was not followed.
            for a in aliases.iter_mut().filter(|a| a.group.is_some() && !monitor.contains(&tg_of(a))) {
                a.do_not_monitor = true;
            }
        }
        for &tg in &p.ignore {
            let i = alias_of(&mut aliases, tg);
            aliases[i].do_not_monitor = true;
        }
        match sp.other {
            Side::Off => listening.follow_unmonitored = false,
            s => {
                listening.unmonitored_speaker = s;
                // Talkgroups in no group played on the "other" speaker.
                for a in aliases.iter_mut().filter(|a| a.priority.is_none() && !a.do_not_monitor) {
                    a.speaker = s;
                }
            }
        }
        listening.preempt = sp.preempt;
    }
    aliases.extend(radios.iter().map(|(&id, name)| Alias::radio(id, name.clone())));
    (aliases, listening)
}

/// The talkgroup of an alias made for one.
fn tg_of(a: &Alias) -> u32 {
    match a.ids.first() {
        Some(AliasId::Talkgroup { value }) => *value,
        _ => u32::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::config::profiles::{Group, Speakers};

    #[test]
    fn the_index_finds_exact_ids_before_ranges() {
        let ix = AliasIndex::new(&[
            Alias { ids: vec![AliasId::TalkgroupRange { min: 200, max: 299 }], ..Alias::talkgroup(0, "Events") },
            Alias::talkgroup(250, "Parade"),
            Alias::radio(1014, "Engine 14"),
            Alias { ids: vec![AliasId::RadioRange { min: 3_000_000, max: 3_999_999 }], ..Alias::radio(0, "Portables") },
        ]);
        assert_eq!(ix.talkgroup(250).map(|a| a.name.as_str()), Some("Parade"));
        assert_eq!(ix.talkgroup(260).map(|a| a.name.as_str()), Some("Events"));
        assert_eq!(ix.talkgroup(300), None);
        assert_eq!(ix.radio(1014).map(|a| a.name.as_str()), Some("Engine 14"));
        assert_eq!(ix.radio(3_409_510).map(|a| a.name.as_str()), Some("Portables"));
        assert_eq!(ix.talkgroup(1014), None, "talkgroups and radios are apart");
    }

    #[test]
    fn names_and_a_profile_become_aliases() {
        let names = BTreeMap::from([(300, "Fire Dispatch".to_string()), (402, "Encrypted".into())]);
        let radios = BTreeMap::from([(1014, "Engine 14".to_string())]);
        let p = Profile {
            groups: vec![Group { name: "Primary".into(), talkgroups: vec![300] }, Group { name: "TAC".into(), talkgroups: vec![301, 302] }],
            speakers: Speakers { left: vec!["Primary".into()], right: vec!["TAC".into()], other: Side::Right, preempt: false },
            ignore: vec![402],
            ..Profile::default()
        };
        let (aliases, listening) = from_names_and_profile(&names, &radios, Some(&p));
        let ix = AliasIndex::new(&aliases);
        let fire = ix.talkgroup(300).unwrap();
        assert_eq!((fire.name.as_str(), fire.priority, fire.speaker, fire.group.as_deref()), ("Fire Dispatch", Some(1), Side::Left, Some("Primary")));
        let tac = ix.talkgroup(302).unwrap();
        assert_eq!((tac.name.as_str(), tac.priority, tac.speaker), ("TG 302", Some(2), Side::Right));
        assert!(ix.talkgroup(402).unwrap().do_not_monitor);
        assert_eq!(ix.radio(1014).unwrap().name, "Engine 14");
        assert_eq!(listening, Listening { follow_unmonitored: true, unmonitored_speaker: Side::Right, preempt: false });
        assert!(aliases.iter().all(|a| a.check().is_ok()));
        // Names alone: aliases with no priority, everything followed as before.
        let (plain, l) = from_names_and_profile(&names, &BTreeMap::new(), None);
        assert_eq!((plain.len(), plain[0].priority, l), (2, None, Listening::default()));
    }

    #[test]
    fn a_monitor_list_follows_only_its_talkgroups() {
        let p = Profile {
            groups: vec![Group { name: "Primary".into(), talkgroups: vec![300, 310] }],
            speakers: Speakers { left: vec!["Primary".into()], ..Default::default() },
            monitor: vec![300, 999],
            ..Profile::default()
        };
        let (aliases, listening) = from_names_and_profile(&BTreeMap::new(), &BTreeMap::new(), Some(&p));
        let ix = AliasIndex::new(&aliases);
        assert!(!listening.follow_unmonitored);
        assert_eq!(ix.talkgroup(300).map(|a| (a.priority, a.do_not_monitor)), Some((Some(1), false)));
        assert!(ix.talkgroup(310).unwrap().do_not_monitor, "grouped but off the monitor list");
        assert_eq!(ix.talkgroup(999).map(|a| (a.priority, a.speaker)), Some((Some(LOWEST_PRIORITY), Side::Both)));
    }

    #[test]
    fn checks_refuse_what_the_follower_cannot_use() {
        let ok = Alias::talkgroup(300, "Fire");
        assert!(ok.check().is_ok());
        assert!(Alias { priority: Some(0), ..ok.clone() }.check().is_err());
        assert!(Alias { priority: Some(101), ..ok.clone() }.check().is_err());
        assert!(Alias { speaker: Side::Off, ..ok.clone() }.check().is_err());
        assert!(Alias { name: " ".into(), ..ok.clone() }.check().is_err());
        assert!(Alias { ids: vec![], ..ok.clone() }.check().is_err());
        assert!(Alias { ids: vec![AliasId::TalkgroupRange { min: 5, max: 1 }], ..ok }.check().is_err());
    }
}
