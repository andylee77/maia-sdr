//! Change 071: find local systems.
//!
//! A sweep takes the radio (the radio lease: the follower, recentre, the
//! PPM tracker and the site clock stand aside, and the tune / preset /
//! site endpoints answer 409), then:
//!
//! 1. steps the LO across the bands at 16 MSPS and reads the hardware
//!    spectrometer at each step; carriers present in nearly every frame
//!    are candidates (a control channel transmits continuously, traffic
//!    channels only during calls);
//! 2. points the control decoders (HDL LSM and software C4FM, both
//!    running) at each candidate: TSBKs make it a P25 control channel,
//!    and a few more seconds give its identity, band table, neighbours
//!    and secondary control channels; a neighbour's control channel in
//!    the bands is probed too;
//! 3. gives the radio back: the active site's control channel and plan.
//!
//! "Add" turns a found site into a site file (`services::sites`); the
//! window planner (070) then learns its traffic channels from grants.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Radio lease: who may move the hardware.
pub const LEASE_NORMAL: u8 = 0;
pub const LEASE_SWEEP: u8 = 1;

/// A lease on the radio, shared by everything that tunes it.
#[derive(Default)]
pub struct RadioLease(AtomicU8);

impl RadioLease {
    pub fn get(&self) -> u8 {
        self.0.load(Ordering::Relaxed)
    }
    pub fn is_normal(&self) -> bool {
        self.get() == LEASE_NORMAL
    }
    /// Take the radio for a sweep; false when it is already taken.
    pub fn take_sweep(&self) -> bool {
        self.0
            .compare_exchange(LEASE_NORMAL, LEASE_SWEEP, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }
    pub fn release(&self) {
        self.0.store(LEASE_NORMAL, Ordering::Relaxed);
    }
}

/// Bands searched by default: P25 700 / 800 / 900 MHz.
pub const DEFAULT_BANDS: &[(u64, u64)] = &[
    (764_000_000, 776_000_000),
    (851_000_000, 869_000_000),
    (935_000_000, 941_000_000),
];
/// With `all`: VHF and UHF public-safety ranges too.
pub const MORE_BANDS: &[(u64, u64)] = &[
    (136_000_000, 174_000_000),
    (380_000_000, 512_000_000),
];

/// Sample rate of the sweep (preset "16M") and its usable half-window.
pub const SWEEP_PRESET: &str = "16M";
pub const SWEEP_RATE_HZ: u32 = 16_000_000;
/// Carriers this close to the LO are the DC spur, not signals.
pub const DC_EXCLUDE_HZ: f64 = 25_000.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanRequest {
    /// Ranges in Hz; empty = the defaults.
    pub bands: Vec<(u64, u64)>,
    /// Add VHF and UHF to the defaults.
    pub all: bool,
    /// Spectrometer frames per step.
    pub frames: usize,
    /// Time on a candidate before deciding it is not P25.
    pub probe_ms: u64,
    /// Longest wait for a P25 site's identity.
    pub identity_ms: u64,
    /// Carriers probed at most (strongest first).
    pub max_candidates: usize,
}

impl Default for ScanRequest {
    fn default() -> Self {
        ScanRequest { bands: Vec::new(), all: false, frames: 8, probe_ms: 2500, identity_ms: 8000, max_candidates: 60 }
    }
}

impl ScanRequest {
    pub fn bands(&self) -> Vec<(u64, u64)> {
        if !self.bands.is_empty() {
            return self.bands.clone();
        }
        let mut b = DEFAULT_BANDS.to_vec();
        if self.all {
            b.extend_from_slice(MORE_BANDS);
        }
        b
    }
}

/// LO positions covering `bands` with windows of +-`usable_half_hz`
/// (steps overlap by 10 %).
pub fn plan_steps(bands: &[(u64, u64)], usable_half_hz: f64) -> Vec<u64> {
    let step = 2.0 * usable_half_hz * 0.9;
    let mut out = Vec::new();
    for &(lo, hi) in bands {
        let (lo, hi) = (lo as f64, hi as f64);
        let span = hi - lo;
        let n = (span / step).ceil().max(1.0) as usize;
        // Centre the n windows on the band.
        let first = lo + (span - (n - 1) as f64 * step) / 2.0;
        for k in 0..n {
            out.push((first + k as f64 * step).round() as u64);
        }
    }
    out
}

/// A continuous carrier found in a spectrum pass.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Carrier {
    pub freq_hz: u64,
    /// dB above the noise floor (median of the frame).
    pub level_db: f32,
    /// Share of the frames it was present in.
    pub persistence: f32,
}

/// Continuous carriers in `frames` (dB per bin, bin N/2 at `center_hz`,
/// `span_hz` wide): above the floor by `min_db` in at least `persist`
/// of the frames, within +-`usable_half_hz`, off the DC spur. Adjacent
/// bins merge; the frequency is the power-weighted centre, rounded to
/// 125 Hz.
pub fn find_carriers(
    frames: &[Vec<f32>],
    center_hz: f64,
    span_hz: f64,
    usable_half_hz: f64,
    min_db: f32,
    persist: f32,
) -> Vec<Carrier> {
    let Some(n) = frames.first().map(|f| f.len()) else { return Vec::new() };
    if n == 0 || frames.iter().any(|f| f.len() != n) {
        return Vec::new();
    }
    let bin_hz = span_hz / n as f64;
    let floors: Vec<f32> = frames
        .iter()
        .map(|f| {
            let mut s = f.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            s[n / 2]
        })
        .collect();
    let mut present = vec![0usize; n];
    let mut excess = vec![0f32; n];
    for (f, floor) in frames.iter().zip(&floors) {
        for i in 0..n {
            let e = f[i] - floor;
            if e >= min_db {
                present[i] += 1;
            }
            excess[i] += e;
        }
    }
    let need = (persist * frames.len() as f32).ceil() as usize;
    let offset = |i: usize| (i as f64 - (n / 2) as f64) * bin_hz;
    let keep: Vec<bool> = (0..n)
        .map(|i| present[i] >= need && offset(i).abs() <= usable_half_hz && offset(i).abs() > DC_EXCLUDE_HZ)
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if !keep[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && keep[i] {
            i += 1;
        }
        let (mut wsum, mut fsum, mut best, mut pers) = (0f64, 0f64, f32::MIN, 0usize);
        for (j, (&e, &p)) in excess.iter().zip(&present).enumerate().take(i).skip(start) {
            let mean_db = e / frames.len() as f32;
            let w = 10f64.powf(mean_db as f64 / 10.0);
            wsum += w;
            fsum += w * offset(j);
            best = best.max(mean_db);
            pers = pers.max(p);
        }
        let f = center_hz + fsum / wsum;
        out.push(Carrier {
            freq_hz: ((f / 125.0).round() * 125.0) as u64,
            level_db: best,
            persistence: pers as f32 / frames.len() as f32,
        });
    }
    out
}

/// A neighbour as listed by a found site.
#[derive(Debug, Clone, Serialize)]
pub struct FoundNeighbour {
    pub system_id: String,
    pub rfss_id: u8,
    pub site_id: u8,
    pub freq_hz: Option<u64>,
}

/// A P25 control channel found by the sweep.
#[derive(Debug, Clone, Serialize)]
pub struct FoundSite {
    pub freq_hz: u64,
    pub level_db: f32,
    /// "LSM" or "C4FM": the decoder that passed more TSBKs.
    pub modulation: String,
    pub tsbk_per_s: f64,
    pub crc_pct: f64,
    pub nac: Option<u16>,
    pub wacn: Option<u32>,
    pub system_id: Option<u16>,
    pub rfss_id: Option<u8>,
    pub site_id: Option<u8>,
    pub lra: Option<u8>,
    pub bands: Vec<crate::services::sites::IdenBand>,
    pub neighbours: Vec<FoundNeighbour>,
    pub secondary_hz: Vec<u64>,
    /// The site file it matches (same identity or control channel).
    pub existing_site: Option<String>,
    /// Found as a neighbour's control channel rather than in a spectrum.
    pub via_neighbour: bool,
}

impl FoundSite {
    /// "WACN-system-RFSS-site", the key for "add".
    pub fn key(&self) -> String {
        format!(
            "{:05X}-{:03X}-{}-{}",
            self.wacn.unwrap_or(0),
            self.system_id.unwrap_or(0),
            self.rfss_id.unwrap_or(0),
            self.site_id.unwrap_or(0)
        )
    }
}

/// Sweep progress and results (`GET /api/discovery`).
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryState {
    pub id: u64,
    /// idle, sweeping, probing, restoring, done, cancelled, error
    pub state: String,
    pub started_unix_ms: u64,
    pub finished_unix_ms: u64,
    pub bands: Vec<(u64, u64)>,
    pub step: usize,
    pub steps: usize,
    pub probed: usize,
    pub to_probe: usize,
    pub probing_hz: Option<u64>,
    pub carriers: usize,
    pub sites: Vec<FoundSite>,
    /// Continuous carriers that were not P25 (freq, dB above floor).
    pub other: Vec<Carrier>,
    /// P25 with NIDs but no TSBKs: a traffic channel in a long call.
    pub p25_voice: Vec<Carrier>,
    pub error: Option<String>,
    #[serde(skip)]
    pub cancel: bool,
}

impl Default for DiscoveryState {
    fn default() -> Self {
        DiscoveryState {
            id: 0,
            state: "idle".into(),
            started_unix_ms: 0,
            finished_unix_ms: 0,
            bands: Vec::new(),
            step: 0,
            steps: 0,
            probed: 0,
            to_probe: 0,
            probing_hz: None,
            carriers: 0,
            sites: Vec::new(),
            other: Vec::new(),
            p25_voice: Vec::new(),
            error: None,
            cancel: false,
        }
    }
}

pub type SharedDiscovery = Arc<Mutex<DiscoveryState>>;

/// A site file name for a found site: its label, slugged.
pub fn site_name(label: &str) -> String {
    let mut s: String = label
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s = s.trim_matches('_').chars().take(32).collect::<String>();
    if s.is_empty() { "site".into() } else { s }
}

/// The site file for a found site.
pub fn to_site(found: &FoundSite, name: &str, label: &str) -> crate::services::sites::Site {
    let mut v = serde_json::json!({
        "name": name,
        "label": label,
        "modulation": found.modulation,
        "preset_default": "8M",
        "cc_position": "Center",
        "control_freq_hz": found.freq_hz,
        "alt_control_freqs_hz": found.secondary_hz.iter().filter(|f| **f != found.freq_hz).collect::<Vec<_>>(),
        "traffic_freqs_hz": [],
        "nac": found.nac,
        "wacn": found.wacn,
        "system_id": found.system_id,
        "rfss_id": found.rfss_id,
        "site_id": found.site_id,
        "lra": found.lra,
        "iden_bands": found.bands,
        "_seed_source": "found by the system finder (change 071)",
    });
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, x| !x.is_null());
    }
    serde_json::from_value(v).expect("site from a found site")
}

#[cfg(target_os = "linux")]
pub use sweep::spawn_scan;

#[cfg(target_os = "linux")]
mod sweep;

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;
