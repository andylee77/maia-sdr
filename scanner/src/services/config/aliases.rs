//! Aliases: what the radio knows of a system's talkgroups and radios, as SDRTrunk's alias lists
//! hold it (a name, a group, a color, the IDs, a monitor priority, a record flag), with the
//! speaker each plays on. The follower, the recorder and every name shown come from here; a
//! system's `Listening` settles talkgroups with no priority.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A talkgroup's traffic lane, named for the speaker the UI plays that lane on: `Left` lane one
/// only, `Right` lane two only, `Both` either. `Off`: none (an alias says `do_not_monitor`
/// instead).
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
    /// Its traffic lane: both (either), left (lane one) or right (lane two).
    #[serde(default)]
    pub speaker: Side,
    /// SDRTrunk's icon name, kept for the round trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    #[cfg(test)]
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

    /// The talkgroup it holds under no name but its number ("TG 300"): what a control on the
    /// live screen makes of a talkgroup that had no alias.
    pub fn unnamed_talkgroup(&self) -> Option<u32> {
        match self.ids.as_slice() {
            [AliasId::Talkgroup { value }] if self.name == format!("TG {value}") => Some(*value),
            _ => None,
        }
    }

    /// Take `from`'s name, and its group, colour and icon where this has none. Its own settings
    /// (followed or not, priority, record, lane) stay.
    pub fn take_name(&mut self, from: &Alias) {
        self.name = from.name.clone();
        self.group = self.group.take().or_else(|| from.group.clone());
        self.color = self.color.take().or_else(|| from.color.clone());
        self.icon = self.icon.take().or_else(|| from.icon.clone());
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
        if self.color.as_deref().is_some_and(|c| !is_color(c)) {
            return Err(format!("alias {}: a colour is #rrggbb", self.name));
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

/// `#rrggbb`.
fn is_color(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// A list as a whole: no talkgroup or radio is named by itself twice. The first alias to name
/// one is the one read for it, so a second would hold settings nothing uses. (A range may hold
/// an ID another alias names: the alias naming it is read before the range.)
pub fn check_list(aliases: &[Alias]) -> Result<(), String> {
    let mut named: HashMap<AliasId, &str> = HashMap::new();
    for a in aliases {
        for id in &a.ids {
            let what = match id {
                AliasId::Talkgroup { value } => format!("talkgroup {value}"),
                AliasId::Radio { value } => format!("radio {value}"),
                AliasId::TalkgroupRange { .. } | AliasId::RadioRange { .. } => continue,
            };
            match named.insert(*id, &a.name) {
                Some(first) if first == a.name => return Err(format!("alias {first} names {what} twice")),
                Some(first) => return Err(format!("{what} is named by {first} and by {}: one alias names an ID", a.name)),
                None => {}
            }
        }
    }
    Ok(())
}

/// What taking one alias into a list did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Taken {
    /// It, or what was left of its IDs, is a new alias.
    pub added: bool,
    /// Aliases that held one of its talkgroups under no name, which took its name.
    pub named: usize,
    /// Its IDs an alias of the list already has: they stay that alias's.
    pub kept: usize,
}

/// A list that aliases are taken into, as an import takes them: an ID an alias of the list
/// already has (a talkgroup, a radio, or the same range) stays that alias's, unless that alias
/// holds only the talkgroup under its number ("TG 300"), which then takes the incoming name and
/// keeps its own settings. The IDs left are added as a new alias. So the same file taken twice
/// adds nothing the second time.
pub struct Intake<'a> {
    aliases: &'a mut Vec<Alias>,
    named_by: HashMap<AliasId, usize>,
}

impl<'a> Intake<'a> {
    pub fn new(aliases: &'a mut Vec<Alias>) -> Self {
        let mut named_by = HashMap::new();
        for (i, a) in aliases.iter().enumerate() {
            for id in &a.ids {
                named_by.entry(*id).or_insert(i);
            }
        }
        Intake { aliases, named_by }
    }

    pub fn take(&mut self, mut incoming: Alias) -> Taken {
        let mut out = Taken::default();
        let mut left: Vec<AliasId> = Vec::new();
        for id in std::mem::take(&mut incoming.ids) {
            match self.named_by.get(&id).copied() {
                Some(i) if self.aliases[i].unnamed_talkgroup().is_some() => {
                    self.aliases[i].take_name(&incoming);
                    out.named += 1;
                }
                Some(_) => out.kept += 1,
                None if left.contains(&id) => {}
                None => left.push(id),
            }
        }
        if !left.is_empty() {
            for id in &left {
                self.named_by.insert(*id, self.aliases.len());
            }
            self.aliases.push(Alias { ids: left, ..incoming });
            out.added = true;
        }
        out
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
    /// Their traffic lane: both (either), left (lane one) or right (lane two).
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn checks_refuse_what_the_follower_cannot_use() {
        let ok = Alias::talkgroup(300, "Fire");
        assert!(ok.check().is_ok());
        assert!(Alias { priority: Some(0), ..ok.clone() }.check().is_err());
        assert!(Alias { priority: Some(101), ..ok.clone() }.check().is_err());
        assert!(Alias { speaker: Side::Off, ..ok.clone() }.check().is_err());
        assert!(Alias { name: " ".into(), ..ok.clone() }.check().is_err());
        assert!(Alias { color: Some("#1f9d55".into()), ..ok.clone() }.check().is_ok());
        for bad in ["red", "#1f9d5", "#1f9d5g", "1f9d55f"] {
            assert!(Alias { color: Some(bad.into()), ..ok.clone() }.check().is_err(), "{bad}");
        }
        assert!(Alias { ids: vec![], ..ok.clone() }.check().is_err());
        assert!(Alias { ids: vec![AliasId::TalkgroupRange { min: 5, max: 1 }], ..ok }.check().is_err());
    }

    #[test]
    fn an_intake_keeps_what_is_named_and_names_what_is_not() {
        let mut off = Alias::talkgroup(850, "TG 850");
        off.do_not_monitor = true;
        off.record = true;
        let mut list = vec![Alias::talkgroup(300, "Fire"), off];
        let mut intake = Intake::new(&mut list);
        // A talkgroup held under its number takes the name and keeps its settings.
        let named = intake.take(Alias { group: Some("Sheriff".into()), color: Some("#00ff00".into()), ..Alias::talkgroup(850, "CCSO Main") });
        assert_eq!(named, Taken { added: false, named: 1, kept: 0 });
        // Named now: a second name for it is not taken.
        assert_eq!(intake.take(Alias::talkgroup(850, "Other")), Taken { added: false, named: 0, kept: 1 });
        // One of its IDs is another alias's, one is twice in it, a range comes as it is.
        let mut tac = Alias::talkgroup(300, "Fire TAC");
        tac.ids.extend([AliasId::Talkgroup { value: 301 }, AliasId::Talkgroup { value: 301 }, AliasId::TalkgroupRange { min: 300, max: 316 }]);
        assert_eq!(intake.take(tac.clone()), Taken { added: true, named: 0, kept: 1 });
        // What was just added is named for the next one, the same range too; another range is not.
        assert_eq!(intake.take(Alias::talkgroup(301, "Again")), Taken { added: false, named: 0, kept: 1 });
        assert_eq!(intake.take(tac), Taken { added: false, named: 0, kept: 4 }, "each of its four IDs, the repeated one twice");
        let wider = Alias { ids: vec![AliasId::TalkgroupRange { min: 300, max: 399 }], ..Alias::talkgroup(0, "Fire, all") };
        assert_eq!(intake.take(wider), Taken { added: true, named: 0, kept: 0 });
        assert_eq!(intake.take(Alias::radio(301, "A radio of that number")), Taken { added: true, named: 0, kept: 0 });
        let got: Vec<(&str, usize, bool, bool, Option<&str>)> = list.iter().map(|a| (a.name.as_str(), a.ids.len(), a.do_not_monitor, a.record, a.group.as_deref())).collect();
        assert_eq!(
            got,
            vec![
                ("Fire", 1, false, false, None),
                ("CCSO Main", 1, true, true, Some("Sheriff")),
                ("Fire TAC", 2, false, false, None),
                ("Fire, all", 1, false, false, None),
                ("A radio of that number", 1, false, false, None)
            ]
        );
        assert_eq!(list[1].color.as_deref(), Some("#00ff00"));
        assert_eq!(check_list(&list), Ok(()));
        // A name that is not the talkgroup's number is a name.
        assert_eq!(Alias::talkgroup(300, "TG 301").unnamed_talkgroup(), None);
        assert_eq!(Alias::radio(300, "TG 300").unnamed_talkgroup(), None);
    }

    #[test]
    fn a_list_names_a_talkgroup_or_a_radio_by_itself_once() {
        let range = Alias { ids: vec![AliasId::TalkgroupRange { min: 300, max: 399 }], ..Alias::talkgroup(0, "Fire, all") };
        // A talkgroup and a radio of the same number are apart, and a range may hold a named one.
        let ok = [Alias::talkgroup(300, "Fire"), Alias::radio(300, "Engine 3"), range.clone(), range];
        assert_eq!(check_list(&ok), Ok(()));
        let twice = [Alias::talkgroup(300, "Fire"), Alias::talkgroup(301, "Rescue"), Alias::talkgroup(300, "Dispatch")];
        assert_eq!(check_list(&twice).unwrap_err(), "talkgroup 300 is named by Fire and by Dispatch: one alias names an ID");
        let mut fire = Alias::radio(1014, "Engine 14");
        fire.ids.push(AliasId::Radio { value: 1014 });
        assert_eq!(check_list(&[fire]).unwrap_err(), "alias Engine 14 names radio 1014 twice");
    }
}
