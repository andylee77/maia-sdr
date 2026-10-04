//! Finding the systems on the air (the Systems page, and the first run).
//!
//! A scan takes the radio, then:
//!
//! 1. steps the LO across the bands at 16 MSPS and reads the wideband spectrometer at each step;
//!    carriers present in nearly every frame are candidates (`carriers`);
//! 2. points the control channel at each candidate while every protocol's control decoder
//!    listens (`probe`): a control channel gives its protocol and, a few seconds on, the site's
//!    identity and plan; a P25 neighbour's control channel in the bands is probed too;
//! 3. hands the radio back to the live site.
//!
//! The found sites are grouped into systems by identity, a card each for the user to fill in
//! (names, details, each site's identity and channels) and add. Adding matches by identity, or
//! by control channel within 3 kHz: it never renames a configured system or site; it adds what
//! was learned (alternate control channels, the P25 band plan, a control channel that moved)
//! and the sites ticked.

pub mod carriers;
pub mod probe;
pub mod sweep;

use serde::{Deserialize, Serialize};

use crate::protocol::events::SiteIdentity as HeardIdentity;
use crate::services::config::ids::{slug, unique};
use crate::services::config::state::IdenBand;
use crate::services::config::systems::{
    Control, DmrModel, Modulation, Protocol, Site, SiteIdentity, System, SystemDetails, SystemIdentity, SystemsConfig,
};
use carriers::Carrier;

/// The bands a scan offers by name: P25 700, 800 and 900 MHz; UHF 450–470 and VHF 150–174 MHz
/// (DMR and P25 both live there).
pub const BANDS: &[(&str, u64, u64)] = &[
    ("700 MHz", 764_000_000, 776_000_000),
    ("800 MHz", 851_000_000, 869_000_000),
    ("900 MHz", 935_000_000, 941_000_000),
    ("UHF", 450_000_000, 470_000_000),
    ("VHF", 150_000_000, 174_000_000),
];

pub fn default_bands() -> Vec<(u64, u64)> {
    BANDS.iter().map(|&(_, low, high)| (low, high)).collect()
}

/// What a scan offers: its bands by name, the default settings, how much it reads at once and
/// how far apart its windows are.
#[derive(Debug, Clone, Serialize)]
pub struct ScanOptions {
    pub bands: Vec<NamedBand>,
    pub defaults: ScanRequest,
    pub window_hz: u32,
    /// The part of a window clear of the decimator's edges.
    pub usable_hz: u64,
    pub step_hz: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NamedBand {
    pub name: &'static str,
    pub low_hz: u64,
    pub high_hz: u64,
}

pub fn options() -> ScanOptions {
    let uh = crate::radio::plan::usable_half_hz(SWEEP_RATE_HZ) as f64;
    ScanOptions {
        bands: BANDS.iter().map(|&(name, low_hz, high_hz)| NamedBand { name, low_hz, high_hz }).collect(),
        defaults: ScanRequest::default(),
        window_hz: SWEEP_RATE_HZ,
        usable_hz: (2.0 * uh) as u64,
        step_hz: step_hz(uh) as u64,
    }
}

/// Windows overlap by 10 %.
fn step_hz(usable_half_hz: f64) -> f64 {
    2.0 * usable_half_hz * 0.9
}
pub const SWEEP_PRESET: &str = "16M";
pub const SWEEP_RATE_HZ: u32 = 16_000_000;
/// A found control channel this close to a configured one is that site.
pub const SAME_CHANNEL_HZ: u64 = 3_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanRequest {
    /// Ranges in Hz; empty = every band of `BANDS`.
    pub bands: Vec<(u64, u64)>,
    /// Spectrometer frames per step.
    pub frames: usize,
    /// Time on a candidate before deciding it carries no control channel.
    pub probe_ms: u64,
    /// Longest wait for a site's identity.
    pub identity_ms: u64,
    /// Carriers probed at most (the strongest first).
    pub max_candidates: usize,
}

impl Default for ScanRequest {
    fn default() -> Self {
        ScanRequest { bands: Vec::new(), frames: 8, probe_ms: 2_500, identity_ms: 8_000, max_candidates: 80 }
    }
}

impl ScanRequest {
    pub fn bands(&self) -> Vec<(u64, u64)> {
        if self.bands.is_empty() { default_bands() } else { self.bands.clone() }
    }
}

/// LO positions covering `bands` with windows of ±`usable_half_hz` (overlapping by 10 %).
pub fn plan_steps(bands: &[(u64, u64)], usable_half_hz: f64) -> Vec<u64> {
    let step = step_hz(usable_half_hz);
    let mut out = Vec::new();
    for &(lo, hi) in bands {
        let (lo, hi) = (lo as f64, hi as f64);
        let span = hi - lo;
        let n = (span / step).ceil().max(1.0) as usize;
        // The n windows centred on the band.
        let first = lo + (span - (n - 1) as f64 * step) / 2.0;
        for k in 0..n {
            out.push((first + k as f64 * step).round() as u64);
        }
    }
    out
}

/// A P25 neighbour a found site announces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FoundNeighbour {
    pub system: u16,
    pub rfss: u8,
    pub site: u8,
    pub freq_hz: Option<u64>,
}

/// A control channel the scan found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FoundSite {
    /// `key()`, set when the scan keeps it: what "add" names.
    pub id: String,
    pub freq_hz: u64,
    pub level_db: f32,
    pub protocol: Protocol,
    /// P25: the demodulator that decodes it (both tried, by the receivers' rule).
    pub modulation: Option<Modulation>,
    pub msgs_per_s: f64,
    pub ok_pct: f64,
    pub identity: HeardIdentity,
    /// P25: the band plan (IDEN_UP).
    pub bands: Vec<IdenBand>,
    pub neighbours: Vec<FoundNeighbour>,
    /// P25: the control channels the site announces (its own and the secondaries).
    pub secondary_hz: Vec<u64>,
    /// DMR: the timeslot carrying the control messages.
    pub timeslot: Option<u8>,
    /// The configured site it is.
    pub existing_site: Option<String>,
    /// Found as a neighbour's control channel, not in a spectrum.
    pub via_neighbour: bool,
}

impl FoundSite {
    /// The site's identity as a key: P25 `p25:WACN-SYS-RFSS-SITE`, DMR `dmr:MODEL-NET-SITE-CC`.
    pub fn key(&self) -> String {
        match &self.identity {
            HeardIdentity::P25(i) => format!(
                "p25:{:05X}-{:03X}-{}-{}",
                i.wacn.unwrap_or(0),
                i.system.unwrap_or(0),
                i.rfss.unwrap_or(0),
                i.site.unwrap_or(0)
            ),
            HeardIdentity::Dmr(i) => format!("dmr:{}-{}-{}-{}", i.model, i.network, i.site, i.colour_code),
        }
    }

    /// The system's identity, as configured systems keep it.
    pub fn system_identity(&self) -> SystemIdentity {
        match &self.identity {
            HeardIdentity::P25(i) => {
                SystemIdentity { wacn: i.wacn, system: i.system.map(u32::from), ..Default::default() }
            }
            HeardIdentity::Dmr(i) => SystemIdentity { model: dmr_model(i.model), network: Some(i.network), ..Default::default() },
        }
    }

    pub fn site_identity(&self) -> SiteIdentity {
        match &self.identity {
            HeardIdentity::P25(i) => SiteIdentity {
                rfss: i.rfss.map(u32::from),
                site: i.site.map(u32::from),
                nac: i.nac.map(u32::from),
                lra: i.lra.map(u32::from),
                colour_code: None,
            },
            HeardIdentity::Dmr(i) => SiteIdentity { site: Some(i.site), colour_code: Some(i.colour_code), ..Default::default() },
        }
    }

    /// Alternate control channels: the announced ones other than its own.
    pub fn alternates(&self) -> Vec<u64> {
        self.secondary_hz.iter().copied().filter(|&f| f.abs_diff(self.freq_hz) > SAME_CHANNEL_HZ).collect()
    }
}

fn dmr_model(m: &str) -> Option<DmrModel> {
    match m {
        "TINY" => Some(DmrModel::Tiny),
        "SMALL" => Some(DmrModel::Small),
        "LARGE" => Some(DmrModel::Large),
        "HUGE" => Some(DmrModel::Huge),
        _ => None,
    }
}

fn same_system(a: &SystemIdentity, b: &SystemIdentity) -> bool {
    (a.wacn.is_some() && a.wacn == b.wacn && a.system == b.system) || (a.model.is_some() && a.model == b.model && a.network == b.network)
}

/// The configured site a found site is: the same system and site identity, or a control channel
/// within 3 kHz.
pub fn existing_site(found: &FoundSite, systems: &SystemsConfig) -> Option<String> {
    let (sys_id, site_id) = (found.system_identity(), found.site_identity());
    systems.systems.iter().filter(|s| s.protocol == found.protocol).find_map(|sys| {
        sys.sites
            .iter()
            .find(|s| {
                let same_id = same_system(&sys.identity, &sys_id)
                    && s.identity.site == site_id.site
                    && s.identity.rfss == site_id.rfss
                    && s.identity.colour_code == site_id.colour_code;
                let near = |f: u64| f.abs_diff(found.freq_hz) <= SAME_CHANNEL_HZ;
                same_id || near(s.control.freq_hz) || s.control.alternates_hz.iter().any(|&f| near(f))
            })
            .map(|s| s.id.clone())
    })
}

/// One found system to add, as the user filled in its card.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddSystem {
    /// A new system's name; a configured one keeps its own, its identity and details.
    pub label: String,
    /// As heard when absent.
    #[serde(default)]
    pub identity: Option<SystemIdentity>,
    #[serde(default)]
    pub details: SystemDetails,
    /// The ticked sites, all of this one system.
    pub sites: Vec<AddSite>,
}

/// One found site to add.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddSite {
    /// `FoundSite::key`.
    pub key: String,
    pub label: String,
    /// As heard when absent.
    #[serde(default)]
    pub identity: Option<SiteIdentity>,
    /// As heard when absent: the channel found and the control channels it announced.
    #[serde(default)]
    pub control: Option<Control>,
    #[serde(default)]
    pub channels_hz: Vec<u64>,
}

/// What adding did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Added {
    pub systems: Vec<String>,
    pub sites: Vec<String>,
    /// Sites already configured that gained alternate control channels or their modulation.
    pub updated: Vec<String>,
}

/// Add one found system's ticked sites to `systems`, as its card says. A configured site gains
/// the alternate control channels it lacks and, set to auto, the modulation found, and moves to
/// the control channel it was heard on when that is another; a new site joins the configured system it belongs to, or a new system
/// made from the card.
pub fn add(systems: &mut SystemsConfig, found: &[FoundSite], card: &AddSystem) -> Result<Added, String> {
    let picked = card
        .sites
        .iter()
        .map(|c| found.iter().find(|f| f.key() == c.key).map(|f| (f, c)).ok_or_else(|| format!("no found site {}", c.key)))
        .collect::<Result<Vec<_>, String>>()?;
    let Some(&(first, _)) = picked.first() else { return Err("no site ticked".into()) };
    if picked.iter().any(|(f, _)| f.protocol != first.protocol || f.system_identity() != first.system_identity()) {
        return Err("the ticked sites belong to different systems".into());
    }
    let mut added = Added::default();
    let configured = |systems: &SystemsConfig| {
        picked
            .iter()
            .find_map(|(f, _)| existing_site(f, systems).and_then(|id| systems.systems.iter().position(|s| s.sites.iter().any(|x| x.id == id))))
            .or_else(|| systems.systems.iter().position(|s| s.protocol == first.protocol && same_system(&s.identity, &first.system_identity())))
    };
    let index = match configured(systems) {
        Some(i) => i,
        None => {
            let name = card.label.trim();
            if name.is_empty() {
                return Err("a system needs a name".into());
            }
            let identity = card.identity.clone().unwrap_or_else(|| first.system_identity());
            identity.check(first.protocol)?;
            let id = unique(slug(name), |id| systems.systems.iter().any(|s| s.id == id));
            systems.systems.push(System {
                id: id.clone(),
                label: name.to_string(),
                protocol: first.protocol,
                identity,
                details: card.details.clone().tidied(),
                aliases: Vec::new(),
                listening: Default::default(),
                sites: Vec::new(),
            });
            added.systems.push(id);
            systems.systems.len() - 1
        }
    };
    for (f, c) in picked {
        if let Some(id) = existing_site(f, systems) {
            if let Some(site) = systems.systems.iter_mut().flat_map(|s| s.sites.iter_mut()).find(|s| s.id == id) {
                let before = (site.control.clone(), site.modulation);
                if site.control.freq_hz.abs_diff(f.freq_hz) > SAME_CHANNEL_HZ {
                    // Heard on another channel: its control channel is there now, and its
                    // alternates are the ones it announces.
                    site.control = Control { freq_hz: f.freq_hz, alternates_hz: f.alternates(), lcn: None, timeslot: f.timeslot };
                }
                for a in f.alternates() {
                    if a.abs_diff(site.control.freq_hz) > SAME_CHANNEL_HZ && !site.control.alternates_hz.contains(&a) {
                        site.control.alternates_hz.push(a);
                    }
                }
                // A site no scan had probed takes the demodulator this one found.
                if site.modulation == Modulation::Auto {
                    site.modulation = f.modulation.unwrap_or_default();
                }
                if (site.control.clone(), site.modulation) != before && !added.updated.contains(&id) {
                    added.updated.push(id);
                }
            }
            continue;
        }
        let label = c.label.trim();
        if label.is_empty() {
            return Err(format!("{}: a site needs a name", c.key));
        }
        let identity = c.identity.clone().unwrap_or_else(|| f.site_identity());
        identity.check(f.protocol).map_err(|e| format!("{label}: {e}"))?;
        let heard = Control { freq_hz: f.freq_hz, alternates_hz: f.alternates(), lcn: None, timeslot: f.timeslot };
        // Site ids are unique across systems and name both ("clay_county_site_1"): sites of
        // different systems are often named alike.
        let site_ids: Vec<String> = systems.systems.iter().flat_map(|s| s.sites.iter().map(|x| x.id.clone())).collect();
        let site_id = unique(slug(&format!("{} {label}", systems.systems[index].label)), |id| site_ids.iter().any(|x| x == id));
        systems.systems[index].sites.push(Site {
            id: site_id.clone(),
            label: label.to_string(),
            identity,
            control: c.control.clone().unwrap_or(heard),
            modulation: f.modulation.unwrap_or_default(),
            channels_hz: c.channels_hz.clone(),
            channel_plan: None,
            window: Default::default(),
            notes: Vec::new(),
            source: Some(format!("found by a scan ({} at {:.5} MHz)", f.key(), f.freq_hz as f64 / 1e6)),
        });
        added.sites.push(site_id);
    }
    Ok(added)
}

/// Scan progress and results.
#[derive(Debug, Clone, Serialize)]
pub struct ScanState {
    pub id: u64,
    /// idle, sweeping, probing, restoring, done, cancelled, error
    pub state: &'static str,
    pub started_unix_ms: u64,
    pub finished_unix_ms: u64,
    pub bands: Vec<(u64, u64)>,
    /// The band of the window read now; none while the control channels found sites announced
    /// are checked.
    pub band: Option<(u64, u64)>,
    /// The window's centre.
    pub lo_hz: Option<u64>,
    pub step: usize,
    pub steps: usize,
    pub probed: usize,
    pub to_probe: usize,
    pub probing_hz: Option<u64>,
    pub carriers: usize,
    pub sites: Vec<FoundSite>,
    /// A protocol's frames with no control messages: traffic channels in long calls.
    pub traffic: Vec<Carrier>,
    /// Continuous carriers no protocol recognised.
    pub other: Vec<Carrier>,
    pub error: Option<String>,
    #[serde(skip)]
    pub cancel: bool,
}

impl Default for ScanState {
    fn default() -> Self {
        ScanState {
            id: 0,
            state: "idle",
            started_unix_ms: 0,
            finished_unix_ms: 0,
            bands: Vec::new(),
            band: None,
            lo_hz: None,
            step: 0,
            steps: 0,
            probed: 0,
            to_probe: 0,
            probing_hz: None,
            carriers: 0,
            sites: Vec::new(),
            traffic: Vec::new(),
            other: Vec::new(),
            error: None,
            cancel: false,
        }
    }
}

impl ScanState {
    pub fn running(&self) -> bool {
        matches!(self.state, "sweeping" | "probing" | "restoring")
    }

    /// A found site, keeping the stronger of two finds of one site (a secondary channel).
    pub fn found(&mut self, mut site: FoundSite) {
        site.id = site.key();
        match self.sites.iter_mut().find(|x| x.key() == site.key()) {
            Some(prev) if site.msgs_per_s > prev.msgs_per_s => *prev = site,
            Some(_) => {}
            None => self.sites.push(site),
        }
    }
}

#[cfg(test)]
mod tests;
