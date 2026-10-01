//! The p25-httpd files as the migrator reads them. Every field is optional so an old or
//! hand-edited file still loads; the readers never write.

use std::collections::BTreeMap;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Deserialize;

/// `p25-ui-settings.json`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub recording: Recording,
    pub call: Call,
    pub radio: Radio,
    pub clock: Clock,
    // The live site's settings, mirrored into `sites[site]` since 069. A file from before 069
    // has only these. (Not a flattened struct: serde's flatten cannot read numeric map keys.)
    pub tg_groups: Vec<Group>,
    pub speakers: Speakers,
    pub tg_aliases: BTreeMap<u32, String>,
    pub unit_aliases: BTreeMap<u32, String>,
    pub monitor_tgs: Vec<u32>,
    pub ignore_tgs: Vec<u32>,
    pub site: String,
    pub sites: BTreeMap<String, SiteEntry>,
}

impl Settings {
    /// The live fields as a profile.
    pub fn live_profile(&self, name: &str) -> Profile {
        Profile {
            name: name.to_string(),
            tg_groups: self.tg_groups.clone(),
            speakers: self.speakers.clone(),
            monitor_tgs: self.monitor_tgs.clone(),
            ignore_tgs: self.ignore_tgs.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Recording {
    pub enabled: Option<bool>,
    pub max_count: Option<u32>,
    pub storage: Option<String>,
    pub sd_max_count: Option<u32>,
    pub sd_max_mb: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Call {
    pub hang_ms: Option<u64>,
    pub end_grace_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Radio {
    pub gain_mode: Option<String>,
    pub manual_gain_db: Option<i32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Clock {
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SiteEntry {
    pub tg_aliases: BTreeMap<u32, String>,
    pub unit_aliases: BTreeMap<u32, String>,
    pub profiles: Vec<Profile>,
    pub active_profile: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub tg_groups: Vec<Group>,
    pub speakers: Speakers,
    pub monitor_tgs: Vec<u32>,
    pub ignore_tgs: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Group {
    pub name: String,
    pub tgs: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Speakers {
    pub left: Vec<String>,
    pub right: Vec<String>,
    pub other: String,
    pub preempt: bool,
}

impl Default for Speakers {
    fn default() -> Self {
        Speakers { left: Vec::new(), right: Vec::new(), other: "both".into(), preempt: true }
    }
}

/// A site file (`p25-sites/<name>.json`) or a seed compiled into p25-httpd.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Site {
    pub name: String,
    pub label: String,
    pub protocol: Option<String>,
    pub cc_position: Option<String>,
    pub control_freq_hz: u64,
    pub alt_control_freqs_hz: Vec<u64>,
    pub traffic_freqs_hz: Vec<u64>,
    pub nac: Option<u32>,
    pub wacn: Option<u32>,
    pub system_id: Option<u32>,
    pub rfss_id: Option<u32>,
    pub site_id: Option<u32>,
    pub lra: Option<u32>,
    pub iden_bands: Vec<IdenBand>,
    pub lcn_map: BTreeMap<u16, u64>,
    #[serde(rename = "_notes")]
    pub notes: Vec<String>,
    #[serde(rename = "_seed_source")]
    pub seed_source: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IdenBand {
    pub identifier: u8,
    pub base_frequency_hz: u64,
    pub channel_spacing_hz: u32,
    pub bandwidth_hz: u32,
    pub transmit_offset_hz: i64,
}

impl Site {
    /// p25-httpd's overlay rule: the site file wins where it has a value, the seed elsewhere.
    pub fn overlaid(mut self, file: Site) -> Site {
        if !file.label.is_empty() {
            self.label = file.label;
        }
        if file.cc_position.is_some() {
            self.cc_position = file.cc_position;
        }
        if file.control_freq_hz != 0 {
            self.control_freq_hz = file.control_freq_hz;
        }
        if !file.alt_control_freqs_hz.is_empty() {
            self.alt_control_freqs_hz = file.alt_control_freqs_hz;
        }
        if !file.traffic_freqs_hz.is_empty() {
            self.traffic_freqs_hz = file.traffic_freqs_hz;
        }
        self.nac = file.nac.or(self.nac);
        self.wacn = file.wacn.or(self.wacn);
        self.system_id = file.system_id.or(self.system_id);
        self.rfss_id = file.rfss_id.or(self.rfss_id);
        self.site_id = file.site_id.or(self.site_id);
        self.lra = file.lra.or(self.lra);
        if !file.iden_bands.is_empty() {
            self.iden_bands = file.iden_bands;
        }
        self
    }

    pub fn is_dmr(&self) -> bool {
        self.protocol.as_deref().is_some_and(|p| p.eq_ignore_ascii_case("dmr"))
    }
}

/// `p25-sites/active.json`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Active {
    pub name: String,
}

/// `p25-plans/<site>.json`: the window planner's grant counts and switches.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Plan {
    pub grants: BTreeMap<u64, u32>,
    pub auto: Option<bool>,
    pub last_recentre_unix_ms: u64,
    pub min_preset: Option<String>,
}

/// `p25-ppm-cal.json`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Ppm {
    pub lo_shift_hz: i64,
    pub lo_ppm: f64,
    pub rx_lo_hz: u64,
    pub control_freq_hz: Option<u64>,
    pub unix_secs: u64,
    pub method: String,
}

/// Seeds compiled into p25-httpd. Units A and B reference sites that exist only here.
pub const SEEDS: &[(&str, &str)] = &[
    ("clay", include_str!("seeds/clay.json")),
    ("duval", include_str!("seeds/duval.json")),
    ("cec_gcs", include_str!("seeds/cec_gcs.json")),
];

pub fn seed(name: &str) -> Option<Site> {
    SEEDS.iter().find(|(n, _)| *n == name).and_then(|(_, body)| serde_json::from_str(body).ok())
}

/// Read a JSON file; `Ok(None)` when it does not exist.
pub fn read<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(body) => serde_json::from_slice(&body)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
    }
}
