//! A playlist taken into the configuration, system by system as the playlist names them.
//!
//! - **Where a system goes.** Into the configured system that has a site on one of its
//!   channels' frequencies (found by itself, or named in the request), or into a new system.
//! - **Its channels.** A playlist does not say which channel is a control channel, so each
//!   channel is a site it may be: taken, it becomes a new site on its frequencies, or gives a
//!   configured site the frequencies and channel numbers it lacks. By default those are taken
//!   that SDRTrunk rotates through several frequencies, starts by itself, or that stand alone at
//!   their site.
//! - **Its aliases.** The alias lists its channels name go into the system, as an import takes
//!   them (`aliases::Intake`).
//!
//! `import` changes the configuration it is given and says what it did; a preview runs it on a
//! copy.

use serde::{Deserialize, Serialize};

use super::{Channel, Playlist};
use crate::services::config::aliases::{self, Alias, Intake, Listening};
use crate::services::config::ids::{self, slug, unique};
use crate::services::config::systems::{ChannelPlan, Control, Modulation, Protocol, Site, System, SystemsConfig};
use crate::services::discovery::SAME_CHANNEL_HZ;

/// What to import.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    /// The playlist file's text.
    pub xml: String,
    /// What to take of each of the playlist's systems. Absent: what a preview suggests. Given: a
    /// system it does not list is not taken.
    #[serde(default)]
    pub systems: Option<Vec<Take>>,
}

/// What to take of one of the playlist's systems.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Take {
    /// The system's name in the playlist.
    pub name: String,
    /// The configured system it goes into; absent or null: a new system.
    #[serde(default)]
    pub into: Option<String>,
    /// The channels to take as sites, by their number in the playlist (from 0). Absent: those
    /// suggested for where it goes.
    #[serde(default)]
    pub channels: Option<Vec<usize>>,
    /// Take its alias lists.
    #[serde(default)]
    pub aliases: bool,
}

/// What an import did, or would do.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Imported {
    /// The playlist's channels and aliases, whatever is taken of them.
    pub channels: usize,
    pub aliases: usize,
    pub systems: Vec<SystemPlan>,
    /// Alias lists no channel names: nothing says which system they are for.
    pub unused_lists: Vec<UnusedList>,
    /// What it adds: new systems and sites and the sites that gain frequencies, by id.
    pub systems_added: Vec<String>,
    pub sites_added: Vec<String>,
    pub sites_updated: Vec<String>,
    pub aliases_added: usize,
    /// Aliases that held a talkgroup under its number and took its name from the playlist.
    pub aliases_named: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnusedList {
    pub name: String,
    pub aliases: usize,
}

/// One of the playlist's systems, and what is taken of it.
#[derive(Debug, Clone, Serialize)]
pub struct SystemPlan {
    pub name: String,
    /// None when no channel of it has a decoder the scanner follows: nothing of it is taken.
    pub protocol: Option<Protocol>,
    /// The configured system that has a site on one of its channels' frequencies.
    pub matched: Option<String>,
    /// The configured system it goes into; none: a system of its own.
    pub into: Option<String>,
    /// The id of the system it makes, when it goes into none.
    pub new_id: Option<String>,
    /// Its alias lists are taken.
    pub aliases: bool,
    pub channels: Vec<PlannedChannel>,
    pub lists: Vec<ListPlan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelState {
    /// It would be a new site.
    New,
    /// A configured site is on its frequencies, and would gain those it lacks.
    Gains,
    /// A configured site's control channel, with nothing to add.
    Configured,
    /// A traffic channel a configured site lists.
    Known,
    /// Not one the scanner can follow.
    Unsupported,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedChannel {
    /// Its number in the playlist (from 0): what a request names it by.
    pub index: usize,
    pub site: String,
    pub name: String,
    /// SDRTrunk starts it by itself.
    pub enabled: bool,
    pub frequencies_hz: Vec<u64>,
    pub state: ChannelState,
    /// The configured site it is on, by id and name.
    pub site_id: Option<String>,
    pub site_label: Option<String>,
    /// It is taken (only `new` and `gains` can be).
    pub picked: bool,
    /// Why it cannot be taken, or why it is not taken unless asked.
    pub note: Option<String>,
}

/// One alias list a system's channels name, against the system it goes into.
#[derive(Debug, Clone, Serialize)]
pub struct ListPlan {
    pub name: String,
    /// Aliases in the list.
    pub aliases: usize,
    /// Those that become aliases of the system.
    pub added: usize,
    /// The system's aliases that held a talkgroup under its number and take its name.
    pub named: usize,
    /// IDs another alias of the system already names: left with it.
    pub kept: usize,
    /// Aliases with no name, or no talkgroup or radio of the system's protocol.
    pub skipped: usize,
}

fn near(a: u64, b: u64) -> bool {
    a.abs_diff(b) <= SAME_CHANNEL_HZ
}

/// The frequencies of a channel the radio tunes.
fn usable(c: &Channel) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for &f in c.frequencies_hz.iter().filter(|f| (70_000_000..=6_000_000_000).contains(*f)) {
        if !out.iter().any(|&o| near(o, f)) {
            out.push(f);
        }
    }
    out
}

/// The site has one of the channel's frequencies as its control channel or one that takes over
/// from it.
fn on_control(site: &Site, c: &Channel) -> bool {
    usable(c).iter().any(|&f| near(f, site.control.freq_hz) || site.control.alternates_hz.iter().any(|&a| near(a, f)))
}

/// The site lists one of the channel's frequencies as a traffic channel.
fn knows(site: &Site, c: &Channel) -> bool {
    let known = || site.channels_hz.iter().chain(site.channel_plan.iter().flat_map(|p| p.lcn_hz.values()));
    usable(c).iter().any(|&f| known().any(|&k| near(k, f)))
}

/// Give a site the channel's frequencies it lacks, as channels its control channel moves to,
/// and the channel numbers it lacks. Whether it gained any.
fn gain(site: &mut Site, c: &Channel) -> bool {
    let mut gained = false;
    for f in usable(c) {
        if !near(f, site.control.freq_hz) && !site.control.alternates_hz.iter().any(|&a| near(a, f)) {
            site.control.alternates_hz.push(f);
            gained = true;
        }
    }
    for (&lcn, &f) in c.lcn_hz.iter().filter(|(_, f)| (70_000_000..=6_000_000_000).contains(*f)) {
        let plan = site.channel_plan.get_or_insert_with(ChannelPlan::default);
        if !plan.lcn_hz.contains_key(&lcn) {
            plan.lcn_hz.insert(lcn, f);
            gained = true;
        }
    }
    gained
}

/// "935.5 MHz".
fn mhz(hz: u64) -> String {
    let s = format!("{:.4}", hz as f64 / 1e6);
    format!("{} MHz", s.trim_end_matches('0').trim_end_matches('.'))
}

/// SDRTrunk's decoder name as a person reads it: `decodeConfigNBFM` is "NBFM".
fn decoder_label(c: &Channel) -> &str {
    match c.decoder.strip_prefix("decodeConfig") {
        Some(d) if !d.is_empty() => d,
        _ => "no decoder",
    }
}

/// A new site from a channel: its first frequency is the control channel, the others take over
/// from it.
fn new_site(sys: &System, c: &Channel, taken: &[String]) -> Option<Site> {
    let freqs = usable(c);
    let (&control, alternates) = freqs.split_first()?;
    let named = [c.site.trim(), c.name.trim()].into_iter().find(|s| !s.is_empty()).unwrap_or("Site");
    let label = if sys.sites.iter().any(|s| s.label == named) { format!("{named} {}", mhz(control)) } else { named.to_string() };
    // Its system's id, then its own name, as a scan names a site.
    let own = slug(&label);
    let of: String = sys.id.chars().take(ids::MAX_LEN - own.len() - 1).collect();
    let id = unique(format!("{}_{own}", of.trim_end_matches('_')), |id| taken.iter().any(|t| t == id));
    let plan: std::collections::BTreeMap<u16, u64> = c.lcn_hz.iter().filter(|(_, f)| (70_000_000..=6_000_000_000).contains(*f)).map(|(&n, &f)| (n, f)).collect();
    Some(Site {
        id,
        label,
        identity: Default::default(),
        control: Control { freq_hz: control, alternates_hz: alternates.to_vec(), lcn: plan.iter().find(|(_, &f)| near(f, control)).map(|(&n, _)| n), timeslot: None },
        modulation: match c.modulation.as_deref() {
            Some("CQPSK") => Modulation::Lsm,
            Some("C4FM") => Modulation::C4fm,
            _ => Modulation::Auto,
        },
        channels_hz: Vec::new(),
        channel_plan: (!plan.is_empty()).then_some(ChannelPlan { lcn_hz: plan }),
        window: Default::default(),
        notes: Vec::new(),
        source: Some(format!("SDRTrunk playlist ({}, {}, {})", c.system, c.site, c.name)),
    })
}

/// The playlist's systems in its order, each with its channels' numbers.
fn groups(playlist: &Playlist) -> Vec<(&str, Vec<usize>)> {
    let mut out: Vec<(&str, Vec<usize>)> = Vec::new();
    for (i, c) in playlist.channels.iter().enumerate() {
        match out.iter_mut().find(|(name, _)| *name == c.system) {
            Some((_, members)) => members.push(i),
            None => out.push((c.system.as_str(), vec![i])),
        }
    }
    out
}

/// Take `playlist` into `config`: what `takes` says of each system, or what is suggested.
pub fn import(config: &mut SystemsConfig, playlist: &Playlist, takes: Option<&[Take]>) -> Result<Imported, String> {
    let groups = groups(playlist);
    for t in takes.unwrap_or_default() {
        if !groups.iter().any(|(name, _)| *name == t.name) {
            return Err(format!("the playlist has no system {:?}", t.name));
        }
    }
    let mut out = Imported { channels: playlist.channels.len(), aliases: playlist.aliases.len(), ..Default::default() };
    for (name, members) in &groups {
        let plan = system(config, playlist, name, members, takes, &mut out)?;
        out.systems.push(plan);
    }
    let mut unused: Vec<UnusedList> = Vec::new();
    for a in playlist.aliases.iter().filter(|a| !playlist.channels.iter().any(|c| c.alias_list.as_deref() == Some(a.list.as_str()))) {
        match unused.iter_mut().find(|u| u.name == a.list) {
            Some(u) => u.aliases += 1,
            None => unused.push(UnusedList { name: a.list.clone(), aliases: 1 }),
        }
    }
    out.unused_lists = unused;
    // What was built is checked as any other change to the configuration is.
    let touched = |id: &String| out.systems_added.contains(id) || out.systems.iter().any(|p| p.into.as_ref() == Some(id));
    for sys in config.systems.iter().filter(|s| touched(&s.id)) {
        if !ids::is_valid(&sys.id) || sys.sites.iter().any(|s| !ids::is_valid(&s.id)) {
            return Err(format!("system {}: an id is not valid", sys.label));
        }
        for a in &sys.aliases {
            a.check().map_err(|e| format!("system {}: {e}", sys.label))?;
        }
        aliases::check_list(&sys.aliases).map_err(|e| format!("system {}: {e}", sys.label))?;
    }
    Ok(out)
}

/// One of the playlist's systems: where it goes, and each of its channels and lists taken or
/// not.
fn system(config: &mut SystemsConfig, playlist: &Playlist, name: &str, members: &[usize], takes: Option<&[Take]>, out: &mut Imported) -> Result<SystemPlan, String> {
    let chans: Vec<(usize, &Channel)> = members.iter().map(|&i| (i, &playlist.channels[i])).collect();
    let count = |p: Protocol| chans.iter().filter(|(_, c)| c.protocol() == Some(p)).count();
    // The protocol most of its channels have.
    let protocol = [Protocol::DmrTier3, Protocol::P25].into_iter().filter(|&p| count(p) > 0).max_by_key(|&p| count(p));
    let of_protocol = |c: &Channel| protocol.is_some() && c.protocol() == protocol;
    // The configured system with the most of its channels on a site's control frequencies; the
    // first of equals.
    let matched = config
        .systems
        .iter()
        .rev()
        .filter(|s| Some(s.protocol) == protocol)
        .map(|s| (s, chans.iter().filter(|(_, c)| of_protocol(c) && s.sites.iter().any(|site| on_control(site, c))).count()))
        .filter(|(_, n)| *n > 0)
        .max_by_key(|(_, n)| *n)
        .map(|(s, _)| s.id.clone());
    let take = takes.and_then(|t| t.iter().find(|t| t.name == name));
    let (into, want_aliases, want_channels): (Option<String>, bool, Option<&[usize]>) = match (takes, take) {
        (None, _) => (matched.clone(), true, None),
        (Some(_), Some(t)) => (t.into.clone(), t.aliases, t.channels.as_deref()),
        (Some(_), None) => (matched.clone(), false, Some(&[][..])),
    };
    let mut target = match &into {
        Some(id) => {
            let i = config.systems.iter().position(|s| &s.id == id).ok_or_else(|| format!("no system {id}"))?;
            if Some(config.systems[i].protocol) != protocol {
                return Err(format!("{name} cannot go into {}: its channels are not that system's protocol", config.systems[i].label));
            }
            Some(i)
        }
        None => None,
    };
    let before = target.map(|i| config.systems[i].clone());
    let new_id = (target.is_none() && protocol.is_some()).then(|| unique(slug(name), |id| config.systems.iter().any(|s| s.id == id)));

    // Each channel against the system as it is before this import.
    let at_site = |site: &str| chans.iter().filter(|(_, c)| of_protocol(c) && c.site == site && !usable(c).is_empty()).count();
    let mut channels = Vec::new();
    for &(index, c) in &chans {
        let (state, site, mut note) = if c.protocol().is_none() {
            (ChannelState::Unsupported, None, Some(format!("{} is not a decoder the scanner follows", decoder_label(c))))
        } else if !of_protocol(c) {
            (ChannelState::Unsupported, None, Some("another protocol than the system's other channels".to_string()))
        } else if usable(c).is_empty() {
            (ChannelState::Unsupported, None, Some("no frequency the radio tunes".to_string()))
        } else if let Some(site) = before.iter().flat_map(|s| &s.sites).find(|s| on_control(s, c)) {
            let state = if gain(&mut site.clone(), c) { ChannelState::Gains } else { ChannelState::Configured };
            (state, Some(site), None)
        } else if let Some(site) = before.iter().flat_map(|s| &s.sites).find(|s| knows(s, c)) {
            (ChannelState::Known, Some(site), None)
        } else {
            (ChannelState::New, None, None)
        };
        let suggested = match state {
            ChannelState::Gains => true,
            ChannelState::New => c.frequencies_hz.len() > 1 || c.enabled || at_site(&c.site) == 1,
            _ => false,
        };
        if state == ChannelState::New && !suggested {
            note = Some(format!("one of {} channels at this site: the playlist does not say which is a control channel", at_site(&c.site)));
        }
        let takes_it = matches!(state, ChannelState::New | ChannelState::Gains);
        channels.push(PlannedChannel {
            index,
            site: c.site.clone(),
            name: c.name.clone(),
            enabled: c.enabled,
            frequencies_hz: c.frequencies_hz.clone(),
            state,
            site_id: site.map(|s| s.id.clone()),
            site_label: site.map(|s| s.label.clone()),
            picked: takes_it && want_channels.map_or(suggested, |w| w.contains(&index)),
            note,
        });
    }

    // A system of its own is made once a channel of it is taken: one with no site has no use.
    let picked: Vec<&Channel> = channels.iter().filter(|p| p.picked).map(|p| &playlist.channels[p.index]).collect();
    if let (None, Some(protocol), Some(id), false) = (target, protocol, &new_id, picked.is_empty()) {
        let label = Some(name.trim()).filter(|n| !n.is_empty()).unwrap_or("Unnamed system").to_string();
        let listening = Listening { follow_unmonitored: !picked.iter().any(|c| c.ignore_unmonitored == Some(true)), ..Listening::default() };
        config.systems.push(System { id: id.clone(), label, protocol, identity: Default::default(), details: Default::default(), aliases: Vec::new(), listening, sites: Vec::new() });
        out.systems_added.push(id.clone());
        target = Some(config.systems.len() - 1);
    }
    if let Some(ti) = target {
        for c in picked {
            let taken: Vec<String> = config.systems.iter().flat_map(|s| s.sites.iter().map(|x| x.id.clone())).collect();
            let sys = &mut config.systems[ti];
            match sys.sites.iter().position(|s| on_control(s, c)) {
                Some(si) => {
                    if gain(&mut sys.sites[si], c) && !out.sites_updated.contains(&sys.sites[si].id) && !out.sites_added.contains(&sys.sites[si].id) {
                        out.sites_updated.push(sys.sites[si].id.clone());
                    }
                }
                None => {
                    if let Some(site) = new_site(sys, c, &taken) {
                        out.sites_added.push(site.id.clone());
                        sys.sites.push(site);
                    }
                }
            }
        }
    }

    // Its alias lists, into the system when they are taken, else counted against it.
    let take_aliases = want_aliases && target.is_some();
    let mut names: Vec<&str> = Vec::new();
    for (_, c) in &chans {
        if let Some(list) = c.alias_list.as_deref().filter(|l| !names.contains(l)) {
            names.push(list);
        }
    }
    let mut scratch: Vec<Alias> = target.map(|i| config.systems[i].aliases.clone()).unwrap_or_default();
    let mut lists = Vec::new();
    for list in names {
        let mut plan = ListPlan { name: list.to_string(), aliases: 0, added: 0, named: 0, kept: 0, skipped: 0 };
        let mut intake = Intake::new(match (take_aliases, target) {
            (true, Some(ti)) => &mut config.systems[ti].aliases,
            _ => &mut scratch,
        });
        for a in playlist.aliases.iter().filter(|a| a.list == list) {
            plan.aliases += 1;
            match protocol.and_then(|p| a.to_alias(p)) {
                Some(alias) => {
                    let t = intake.take(alias);
                    plan.added += usize::from(t.added);
                    plan.named += t.named;
                    plan.kept += t.kept;
                }
                None => plan.skipped += 1,
            }
        }
        if take_aliases {
            out.aliases_added += plan.added;
            out.aliases_named += plan.named;
        }
        lists.push(plan);
    }

    Ok(SystemPlan { name: name.to_string(), protocol, matched, into, new_id, aliases: take_aliases && !lists.is_empty(), channels, lists })
}
