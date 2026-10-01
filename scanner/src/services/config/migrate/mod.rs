//! First start after p25-httpd: the configuration built from its files.
//!
//! The old files are only read, never changed or deleted, so the old binary still runs on them.
//! The new files are written into `scanner.migrating/` and renamed to `scanner/` once complete,
//! so a power cut never leaves half a configuration. Every decision goes into
//! `migration-076.log`.

pub mod legacy;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::ids;
use super::profiles::{Group, Profile, ProfilesConfig, Side, Speakers};
use super::radio::{self, ClockSource, GainMode, RadioConfig, Storage};
use super::state::{Crystal, IdenBand, RadioState, SiteState};
use super::systems::{
    CcPosition, ChannelPlan, Control, DmrModel, Modulation, Protocol, Site, SiteIdentity, System, SystemIdentity,
    SystemsConfig, Window,
};
use super::Paths;
use crate::util::{atomic_file, time};

/// What the migration did, for the log and the event log.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub lines: Vec<String>,
    pub systems: usize,
    pub sites: usize,
    pub profiles: usize,
    pub live_site: Option<String>,
}

impl Report {
    fn note(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }
}

/// Does this unit have p25-httpd files to migrate?
pub fn legacy_present(paths: &Paths) -> bool {
    ["p25-ui-settings.json", "p25-sites", "p25-plans", "p25-ppm-cal.json"]
        .iter()
        .any(|f| paths.flash.join(f).exists())
}

/// Migrate and write the new files.
pub fn run(paths: &Paths) -> anyhow::Result<Report> {
    let input = Legacy::read(paths)?;
    let (built, mut report) = build(&input);
    let tmp = paths.root.with_file_name("scanner.migrating");
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    let staged = Paths { root: tmp.clone(), ..paths.clone() };
    atomic_file::write_json(&staged.radio(), &built.radio)?;
    atomic_file::write_json(&staged.systems(), &built.systems)?;
    atomic_file::write_json(&staged.profiles(), &built.profiles)?;
    atomic_file::write_json(&staged.radio_state(), &built.state)?;
    for (site, state) in &built.site_states {
        atomic_file::write_json(&staged.site_state(site), state)?;
    }
    report.note(format!("migrated at {} UTC", time::iso_utc(time::unix_ms())));
    atomic_file::write(&staged.migration_log(), (report.lines.join("\n") + "\n").as_bytes())?;
    std::fs::rename(&tmp, &paths.root)?;
    Ok(report)
}

/// Everything read from the old files.
#[derive(Debug, Default)]
pub struct Legacy {
    pub settings: Option<legacy::Settings>,
    /// Site files by name (`p25-sites/<name>.json`).
    pub site_files: BTreeMap<String, legacy::Site>,
    pub active: Option<String>,
    pub plans: BTreeMap<String, legacy::Plan>,
    pub ppm: Option<legacy::Ppm>,
    /// Site names in the history database and the recording file names.
    pub sd_sites: BTreeSet<String>,
    /// Files that could not be read, with the reason.
    pub unreadable: Vec<String>,
}

impl Legacy {
    pub fn read(paths: &Paths) -> anyhow::Result<Legacy> {
        let mut l = Legacy::default();
        let flash = &paths.flash;
        match legacy::read::<legacy::Settings>(&flash.join("p25-ui-settings.json")) {
            Ok(s) => l.settings = s,
            Err(e) => l.unreadable.push(e.to_string()),
        }
        for (name, path) in json_files(&flash.join("p25-sites")) {
            if name == "active" {
                match legacy::read::<legacy::Active>(&path) {
                    Ok(a) => l.active = a.map(|a| a.name).filter(|n| !n.is_empty()),
                    Err(e) => l.unreadable.push(e.to_string()),
                }
                continue;
            }
            match legacy::read::<legacy::Site>(&path) {
                Ok(Some(s)) => {
                    l.site_files.insert(name, s);
                }
                Ok(None) => {}
                Err(e) => l.unreadable.push(e.to_string()),
            }
        }
        for (name, path) in json_files(&flash.join("p25-plans")) {
            match legacy::read::<legacy::Plan>(&path) {
                Ok(Some(p)) => {
                    l.plans.insert(name, p);
                }
                Ok(None) => {}
                Err(e) => l.unreadable.push(e.to_string()),
            }
        }
        match legacy::read::<legacy::Ppm>(&flash.join("p25-ppm-cal.json")) {
            Ok(p) => l.ppm = p,
            Err(e) => l.unreadable.push(e.to_string()),
        }
        l.sd_sites = sd_sites(&paths.sd);
        Ok(l)
    }
}

/// `<stem>, <path>` of the `.json` files in `dir` (none if it does not exist).
fn json_files(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_string();
            (!stem.starts_with(['.', '_'])).then_some((stem, p))
        })
        .collect();
    out.sort();
    out
}

/// Site names the SD card refers to: history rows and recording file names
/// (`rec_<ms>_<id>_tg<tg>_from<radio>.<site>.wav`).
fn sd_sites(sd: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let db = sd.join("p25-history.sqlite");
    if db.exists() {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
        if let Ok(conn) = rusqlite::Connection::open_with_flags(&db, flags) {
            if let Ok(mut q) = conn.prepare("SELECT DISTINCT site FROM calls") {
                if let Ok(rows) = q.query_map([], |r| r.get::<_, String>(0)) {
                    out.extend(rows.flatten());
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(sd.join("p25_recordings")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(site) = name.strip_suffix(".wav").and_then(|n| n.rsplit_once('.')).map(|(_, s)| s) {
                out.insert(site.to_string());
            }
        }
    }
    out.retain(|s| !s.is_empty());
    out
}

/// The new configuration, before it is written.
#[derive(Debug, Clone, Default)]
pub struct Built {
    pub radio: RadioConfig,
    pub systems: SystemsConfig,
    pub profiles: ProfilesConfig,
    pub state: RadioState,
    pub site_states: BTreeMap<String, SiteState>,
}

/// DMR facts p25-httpd kept only in a seed's notes, and the names its label ran together.
struct DmrSeed {
    site: &'static str,
    model: DmrModel,
    network: u32,
    colour_code: u8,
    control_lcn: u16,
    control_timeslot: u8,
    system_label: &'static str,
    site_label: &'static str,
}

const DMR_SEEDS: &[DmrSeed] = &[DmrSeed {
    site: "cec_gcs",
    model: DmrModel::Small,
    network: 0,
    colour_code: 0,
    control_lcn: 5,
    control_timeslot: 1,
    system_label: "Clay Electric",
    site_label: "Green Cove Springs",
}];

fn dmr_seed(site: &str) -> Option<&'static DmrSeed> {
    DMR_SEEDS.iter().find(|d| d.site == site)
}

/// How sites group into systems.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SystemKey {
    P25 { wacn: u32, system: u32 },
    Dmr { network: u32, model: String },
    Alone(String),
}

pub fn build(input: &Legacy) -> (Built, Report) {
    let mut report = Report::default();
    for e in &input.unreadable {
        report.note(format!("unreadable, skipped: {e}"));
    }
    let settings = input.settings.clone().unwrap_or_default();

    // Sites: every site file, and every seed something refers to.
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    referenced.extend(input.active.iter().cloned());
    if !settings.site.is_empty() {
        referenced.insert(settings.site.clone());
    }
    referenced.extend(settings.sites.keys().cloned());
    referenced.extend(input.plans.keys().cloned());
    referenced.extend(input.sd_sites.iter().cloned());
    let mut sites: BTreeMap<String, legacy::Site> = BTreeMap::new();
    for name in input.site_files.keys().chain(referenced.iter()) {
        if sites.contains_key(name) {
            continue;
        }
        let file = input.site_files.get(name).cloned();
        let site = match (legacy::seed(name), file) {
            (Some(seed), Some(file)) => Some(seed.overlaid(file)),
            (Some(seed), None) => {
                report.note(format!("site {name}: written out from the seed compiled into p25-httpd"));
                Some(seed)
            }
            (None, Some(file)) => Some(file),
            (None, None) => None,
        };
        match site {
            Some(mut s) => {
                s.name = name.clone();
                sites.insert(name.clone(), s);
            }
            None => report.note(format!("site {name}: referenced but no site file or seed; dropped")),
        }
    }

    // The live site: active.json, else the settings' site, else clay (p25-httpd's fallback).
    let live = input
        .active
        .clone()
        .or_else(|| (!settings.site.is_empty()).then(|| settings.site.clone()))
        .or_else(|| sites.contains_key("clay").then(|| "clay".to_string()))
        .filter(|s| sites.contains_key(s));

    // Group into systems by identity.
    let mut groups: BTreeMap<SystemKey, Vec<String>> = BTreeMap::new();
    for (name, s) in &sites {
        let key = if s.is_dmr() {
            match dmr_seed(name) {
                Some(d) => SystemKey::Dmr { network: d.network, model: format!("{:?}", d.model) },
                None => SystemKey::Alone(name.clone()),
            }
        } else {
            match (s.wacn, s.system_id) {
                (Some(wacn), Some(system)) => SystemKey::P25 { wacn, system },
                _ => SystemKey::Alone(name.clone()),
            }
        };
        groups.entry(key).or_default().push(name.clone());
    }

    let entries = site_entries(&settings, live.as_deref(), &sites, &mut report);
    let mut built = Built::default();
    let mut system_ids: BTreeSet<String> = BTreeSet::new();
    for names in groups.values() {
        let mut system = build_system(names, &sites, live.as_deref(), &mut system_ids, input, &mut built, &mut report);
        build_names_and_profiles(&mut system, &entries, live.as_deref(), &mut built, &mut report);
        built.systems.systems.push(system);
    }
    // Two systems with one label (finder sites of different networks): add the WACN.
    let labels: Vec<String> = built.systems.systems.iter().map(|s| s.label.clone()).collect();
    for sys in &mut built.systems.systems {
        if labels.iter().filter(|l| **l == sys.label).count() > 1 {
            if let Some(wacn) = sys.identity.wacn {
                sys.label = format!("{} (WACN {wacn:05X})", sys.label);
            }
        }
    }
    built.systems.systems.sort_by(|a, b| a.id.cmp(&b.id));
    built.profiles.profiles.sort_by(|a, b| a.id.cmp(&b.id));

    built.radio = radio_from(&settings, &mut report);
    built.state.live_site = live.clone();
    built.state.crystal = input.ppm.as_ref().map(|p| Crystal {
        ppm: p.lo_ppm,
        measured_at_lo_hz: p.rx_lo_hz,
        lo_shift_hz: p.lo_shift_hz,
        control_freq_hz: p.control_freq_hz,
        method: p.method.clone(),
        at_unix_ms: p.unix_secs * 1_000,
    });
    if let Some(c) = &built.state.crystal {
        report.note(format!("crystal: {:+.3} ppm, measured at LO {} Hz ({})", c.ppm, c.measured_at_lo_hz, c.method));
    }

    report.systems = built.systems.systems.len();
    report.sites = built.systems.systems.iter().map(|s| s.sites.len()).sum();
    report.profiles = built.profiles.profiles.len();
    report.live_site = live.clone();
    report.note(format!(
        "{} systems, {} sites, {} profiles; live site {}",
        report.systems,
        report.sites,
        report.profiles,
        live.as_deref().unwrap_or("none")
    ));
    (built, report)
}

/// A finder site's label, "System 1D5 site 1-1", as (system label, site label).
fn finder_label(label: &str) -> Option<(String, String)> {
    let rest = label.strip_prefix("System ")?;
    let (system, site) = rest.split_once(" site ")?;
    Some((format!("System {system}"), format!("Site {site}")))
}

/// "Florida Power and Light (Clay)" as ("Florida Power and Light", "Clay").
fn split_parenthesis(label: &str) -> Option<(String, String)> {
    let inner = label.strip_suffix(')')?;
    let (outer, site) = inner.rsplit_once(" (")?;
    Some((outer.trim().to_string(), site.trim().to_string()))
}

#[allow(clippy::too_many_arguments)]
fn build_system(
    names: &[String],
    sites: &BTreeMap<String, legacy::Site>,
    live: Option<&str>,
    system_ids: &mut BTreeSet<String>,
    input: &Legacy,
    built: &mut Built,
    report: &mut Report,
) -> System {
    // The site that names the system: the live one, else one a person named, else the first.
    let primary = names
        .iter()
        .find(|n| Some(n.as_str()) == live)
        .or_else(|| names.iter().find(|n| finder_label(&sites[*n].label).is_none()))
        .unwrap_or(&names[0]);
    let p = &sites[primary];
    let dmr = dmr_seed(primary);
    let system_label = if let Some(d) = dmr {
        d.system_label.to_string()
    } else if let Some((sys, _)) = finder_label(&p.label) {
        sys
    } else if let Some((sys, _)) = split_parenthesis(&p.label) {
        sys
    } else {
        p.label.clone()
    };
    let id = ids::unique(&ids::slug(&system_label), |id| system_ids.contains(id));
    system_ids.insert(id.clone());
    let protocol = if p.is_dmr() { Protocol::DmrTier3 } else { Protocol::P25 };
    let identity = match (protocol, dmr) {
        (Protocol::DmrTier3, Some(d)) => SystemIdentity { model: Some(d.model), network: Some(d.network), ..Default::default() },
        (Protocol::P25, _) => SystemIdentity { wacn: p.wacn, system: p.system_id, ..Default::default() },
        _ => SystemIdentity::default(),
    };
    report.note(format!("system {id} \"{system_label}\" ({protocol:?}): sites {}", names.join(", ")));

    let mut out = System {
        id,
        label: system_label.clone(),
        protocol,
        identity,
        talkgroups: BTreeMap::new(),
        radios: BTreeMap::new(),
        sites: Vec::new(),
    };
    for name in names {
        let s = &sites[name];
        let id = if ids::is_valid(name) {
            name.clone()
        } else {
            let id = ids::slug(name);
            report.note(format!("site {name}: renamed {id} (not a valid id)"));
            id
        };
        let d = dmr_seed(name);
        let label = if let Some(d) = d {
            d.site_label.to_string()
        } else if let Some((_, site)) = finder_label(&s.label) {
            site
        } else {
            match split_parenthesis(&s.label) {
                Some((outer, site)) if outer == system_label => site,
                _ => s.label.clone(),
            }
        };
        let plan = input.plans.get(name).cloned().unwrap_or_default();
        let cc_position = match s.cc_position.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("top") => CcPosition::Top,
            Some("bottom") => CcPosition::Bottom,
            _ => CcPosition::Center,
        };
        let identity = if protocol == Protocol::DmrTier3 {
            SiteIdentity { site: s.site_id, colour_code: d.map(|d| d.colour_code), ..Default::default() }
        } else {
            SiteIdentity { rfss: s.rfss_id, site: s.site_id, nac: s.nac, lra: s.lra, colour_code: None }
        };
        out.sites.push(Site {
            id: id.clone(),
            label,
            identity,
            control: Control {
                freq_hz: s.control_freq_hz,
                alternates_hz: s.alt_control_freqs_hz.clone(),
                lcn: d.map(|d| d.control_lcn),
                timeslot: d.map(|d| d.control_timeslot),
            },
            modulation: Modulation::Auto,
            channels_hz: s.traffic_freqs_hz.clone(),
            channel_plan: (!s.lcn_map.is_empty()).then(|| ChannelPlan { lcn_hz: s.lcn_map.clone() }),
            window: Window { auto: plan.auto.unwrap_or(true), min_preset: plan.min_preset.clone(), cc_position },
            notes: s.notes.clone(),
            source: Some(s.seed_source.clone().unwrap_or_else(|| format!("p25-httpd site {name}"))),
        });
        let state = SiteState {
            iden_bands: s
                .iden_bands
                .iter()
                .map(|b| IdenBand {
                    identifier: b.identifier,
                    base_frequency_hz: b.base_frequency_hz,
                    channel_spacing_hz: b.channel_spacing_hz,
                    bandwidth_hz: b.bandwidth_hz,
                    transmit_offset_hz: b.transmit_offset_hz,
                    slots: 1,
                })
                .collect(),
            grants: plan.grants.clone(),
            last_recentre_unix_ms: plan.last_recentre_unix_ms,
            ..Default::default()
        };
        if state != SiteState::default() {
            built.site_states.insert(id, state);
        }
    }
    out
}

/// A site's names and profiles as p25-httpd kept them. The live site's are its live fields in
/// a file from before profiles (069); otherwise its `sites` entry, which mirrors them.
fn site_entries(
    settings: &legacy::Settings,
    live: Option<&str>,
    sites: &BTreeMap<String, legacy::Site>,
    report: &mut Report,
) -> BTreeMap<String, legacy::SiteEntry> {
    let mut out = settings.sites.clone();
    let live_owner = if settings.site.is_empty() { live } else { Some(settings.site.as_str()) };
    if let Some(owner) = live_owner {
        let entry = out.entry(owner.to_string()).or_default();
        if entry.profiles.is_empty() {
            let s = settings;
            let has_content = !s.tg_groups.is_empty() || !s.monitor_tgs.is_empty() || !s.ignore_tgs.is_empty()
                || s.speakers != legacy::Speakers::default();
            if has_content || settings.sites.is_empty() {
                entry.profiles.push(s.live_profile("Default"));
                entry.active_profile = "Default".into();
                report.note(format!("site {owner}: the live settings became its \"Default\" profile"));
            }
        }
        if entry.tg_aliases.is_empty() && entry.unit_aliases.is_empty() {
            entry.tg_aliases = settings.tg_aliases.clone();
            entry.unit_aliases = settings.unit_aliases.clone();
        }
    }
    out.retain(|name, _| {
        let known = sites.contains_key(name);
        if !known {
            report.note(format!("settings of site {name} dropped: the site is gone"));
        }
        known
    });
    out
}

/// The system's talkgroup and radio names, merged from its sites, and its profiles. Every site
/// gets an active profile; a system with none gets an empty "Default".
fn build_names_and_profiles(
    system: &mut System,
    entries: &BTreeMap<String, legacy::SiteEntry>,
    live: Option<&str>,
    built: &mut Built,
    report: &mut Report,
) {
    // Precedence on a conflict: the live site, then the site with more names.
    let names_of = |id: &str| entries.get(id).map_or(0, |e| e.tg_aliases.len() + e.unit_aliases.len());
    let mut order: Vec<(String, String)> = system.sites.iter().map(|s| (s.id.clone(), s.label.clone())).collect();
    order.sort_by_key(|(id, _)| (Some(id.as_str()) != live, std::cmp::Reverse(names_of(id)), id.clone()));

    let mut talkgroups: BTreeMap<u32, (String, String)> = BTreeMap::new();
    let mut radios: BTreeMap<u32, (String, String)> = BTreeMap::new();
    for (site, _) in &order {
        let Some(e) = entries.get(site) else { continue };
        merge_names(&mut talkgroups, &e.tg_aliases, site, "talkgroup", report);
        merge_names(&mut radios, &e.unit_aliases, site, "radio", report);
    }
    system.talkgroups = talkgroups.into_iter().map(|(k, (v, _))| (k, v)).collect();
    system.radios = radios.into_iter().map(|(k, (v, _))| (k, v)).collect();

    for (site, site_label) in &order {
        let Some(e) = entries.get(site) else { continue };
        let (mut first, mut active) = (None, None);
        for lp in e.profiles.iter().filter(|p| !p.name.trim().is_empty()) {
            let content = convert_profile(lp);
            let name = lp.name.trim();
            let same_name = built.profiles.profiles.iter()
                .find(|p| p.system == system.id && p.name.eq_ignore_ascii_case(name));
            let id = match same_name {
                Some(p) if same_content(p, &content) => p.id.clone(),
                Some(_) => {
                    let label = format!("{name} ({site_label})");
                    report.note(format!("profile \"{name}\" of site {site} differs from another site's: named \"{label}\""));
                    push_profile(built, &system.id, &label, content)
                }
                None => push_profile(built, &system.id, name, content),
            };
            first.get_or_insert_with(|| id.clone());
            if lp.name == e.active_profile {
                active = Some(id);
            }
        }
        if let Some(id) = active.or(first) {
            built.profiles.active.insert(site.clone(), id);
        }
    }

    if !built.profiles.profiles.iter().any(|p| p.system == system.id) {
        push_profile(built, &system.id, "Default", Profile::default());
    }
    let fallback = built.profiles.profiles.iter().find(|p| p.system == system.id).map(|p| p.id.clone());
    for site in &system.sites {
        if let (false, Some(id)) = (built.profiles.active.contains_key(&site.id), &fallback) {
            built.profiles.active.insert(site.id.clone(), id.clone());
        }
    }
}

fn merge_names(
    into: &mut BTreeMap<u32, (String, String)>,
    from: &BTreeMap<u32, String>,
    site: &str,
    what: &str,
    report: &mut Report,
) {
    for (id, name) in from {
        let name = name.trim();
        if name.is_empty() || *id == 0 || *id > 0x00FF_FFFF {
            continue;
        }
        match into.get(id) {
            Some((kept, owner)) if kept != name => {
                report.note(format!("{what} {id}: \"{kept}\" ({owner}) kept over \"{name}\" ({site})"));
            }
            Some(_) => {}
            None => {
                into.insert(*id, (name.to_string(), site.to_string()));
            }
        }
    }
}

fn convert_profile(p: &legacy::Profile) -> Profile {
    let tg = |t: &u32| *t != 0 && *t <= 0x00FF_FFFF;
    let mut seen = BTreeSet::new();
    let side = |s: &str| match s {
        "left" => Side::Left,
        "right" => Side::Right,
        "off" => Side::Off,
        _ => Side::Both,
    };
    Profile {
        id: String::new(),
        system: String::new(),
        name: p.name.trim().to_string(),
        groups: p
            .tg_groups
            .iter()
            .map(|g| Group { name: g.name.clone(), talkgroups: g.tgs.iter().copied().filter(tg).collect() })
            .collect(),
        speakers: Speakers {
            left: p.speakers.left.clone(),
            right: p.speakers.right.clone(),
            other: side(&p.speakers.other),
            preempt: p.speakers.preempt,
        },
        monitor: p.monitor_tgs.iter().copied().filter(tg).filter(|t| seen.insert(*t)).collect(),
        ignore: p.ignore_tgs.iter().copied().filter(tg).collect::<BTreeSet<_>>().into_iter().collect(),
    }
}

fn same_content(a: &Profile, b: &Profile) -> bool {
    (&a.groups, &a.speakers, &a.monitor, &a.ignore) == (&b.groups, &b.speakers, &b.monitor, &b.ignore)
}

fn push_profile(built: &mut Built, system: &str, name: &str, content: Profile) -> String {
    let base = format!("{system}/{}", ids::slug(name));
    let id = ids::unique(&base, |id| built.profiles.profiles.iter().any(|p| p.id == id));
    built.profiles.profiles.push(Profile { id: id.clone(), system: system.to_string(), name: name.to_string(), ..content });
    id
}

fn radio_from(s: &legacy::Settings, report: &mut Report) -> RadioConfig {
    let mode = match s.radio.gain_mode.as_deref() {
        Some("manual") => Some(GainMode::Manual),
        Some("slow_attack") => Some(GainMode::SlowAttack),
        Some("fast_attack") => Some(GainMode::FastAttack),
        Some("hybrid") => Some(GainMode::Hybrid),
        _ => None,
    };
    let gain = match mode {
        Some(mode) => radio::Gain {
            mode,
            manual_db: s.radio.manual_gain_db.filter(|db| radio::GAIN_DB_RANGE.contains(db)),
        },
        None => {
            report.note("gain: never set in p25-httpd; its boot default, manual 60 dB, kept");
            radio::Gain { mode: GainMode::Manual, manual_db: Some(60) }
        }
    };
    let r = &s.recording;
    RadioConfig {
        gain,
        calls: radio::Calls {
            hang_ms: s.call.hang_ms.unwrap_or(3_000).clamp(1_000, 30_000),
            end_grace_ms: s.call.end_grace_ms.unwrap_or(2_000).min(10_000),
        },
        recording: radio::Recording {
            enabled: r.enabled.unwrap_or(true),
            storage: if r.storage.as_deref() == Some("sd") { Storage::Sd } else { Storage::Ram },
            ram_max_count: r.max_count.unwrap_or(40).clamp(1, 500),
            sd_max_count: r.sd_max_count.unwrap_or(2_000).clamp(1, 5_000),
            sd_max_mb: r.sd_max_mb.unwrap_or(2_048).clamp(16, 32_768),
        },
        clock: radio::Clock {
            source: match s.clock.source.as_deref() {
                Some("ntp") => ClockSource::Ntp,
                Some("manual") => ClockSource::Manual,
                _ => ClockSource::Site,
            },
        },
        ..RadioConfig::default()
    }
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
