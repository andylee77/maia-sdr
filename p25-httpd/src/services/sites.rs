//! Per-site baseline configuration: NAC/WACN, control freq, IDEN
//! bands, LO-snap policy.
//!
//! Two-layer model:
//!
//! 1. **Repo seed** (`p25-httpd/sites/<name>.json`, checked in) —
//!    site name, label, control freq, alt CCs, traffic_freqs_hz,
//!    modulation, preset_default, cc_position. Hand-written from
//!    SDRTrunk playlists.
//! 2. **Runtime overlay** (`/mnt/data/p25/<name>.json`, persistent
//!    flash) — populated as the receiver locks onto the air. NAC,
//!    WACN, system_id, rfss_id, site_id, lra, iden_bands. Saved
//!    on every IDEN_UPDATE / control-freq change.
//!
//! `load_site()` reads both layers and merges with overlay-wins.
//! `save_site()` atomic-replaces the overlay file.
//!
//! See `p25-httpd/sites/README.md` for the full TODO inventory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the operator-loaded CC sits inside the IF window. Drives
/// the LO-snap policy in `httpd::api::tuning::post_preset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CcPosition {
    /// CC sits near the top of the IF window. Use when traffic
    /// channels are below the CC (e.g. Clay County: CC=860.9625,
    /// traffic 852-861 MHz). LO = cc_freq - (half_sr - margin).
    Top,
    /// CC sits at the centre of the IF window. Use when traffic
    /// is roughly symmetric around the CC (e.g. Duval/JAX: CC in
    /// the middle of the LCN spread). LO = cc_freq.
    Center,
    /// CC sits near the bottom of the IF window. Use when traffic
    /// channels are above the CC. LO = cc_freq + (half_sr - margin).
    Bottom,
}

impl Default for CcPosition {
    fn default() -> Self {
        CcPosition::Center
    }
}

/// Mirror of `protocol::p25::tsbk::FrequencyBand` for serde + the
/// site overlay file. Populated at runtime from IDEN_UPDATE TSBKs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdenBand {
    pub identifier: u8,
    pub base_frequency_hz: u64,
    pub channel_spacing_hz: u32,
    pub bandwidth_hz: u32,
    pub transmit_offset_hz: i64,
}

/// Full site baseline. Merged from repo seed + runtime overlay.
///
/// Field order mirrors the JSON schema in
/// `p25-httpd/sites/{clay,duval}.json`. Runtime-only fields (NAC,
/// WACN, etc.) are `Option<>` so the seed file can leave them
/// `null` until the receiver has heard the air.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Site {
    pub name: String,
    pub label: String,
    #[serde(default = "default_modulation")]
    pub modulation: String,
    pub preset_default: String,
    #[serde(default)]
    pub cc_position: CcPosition,

    pub control_freq_hz: u64,
    #[serde(default)]
    pub alt_control_freqs_hz: Vec<u64>,
    #[serde(default)]
    pub traffic_freqs_hz: Vec<u64>,

    pub nac: Option<u16>,
    pub wacn: Option<u32>,
    pub system_id: Option<u32>,
    pub rfss_id: Option<u8>,
    pub site_id: Option<u8>,
    pub lra: Option<u8>,

    #[serde(default)]
    pub iden_bands: Vec<IdenBand>,

    /// Optional last-saved Unix timestamp (ms). Filled in by
    /// `save_site`; the seed file leaves it `None`.
    #[serde(default)]
    pub last_updated_unix_ms: Option<u64>,

    /// Free-form documentation strings from the seed file. Ignored
    /// at runtime but preserved on round-trip.
    #[serde(rename = "_notes", default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,

    /// Optional record of where the seed came from. Ignored at
    /// runtime but preserved on round-trip.
    #[serde(rename = "_seed_source", default, skip_serializing_if = "Option::is_none")]
    pub seed_source: Option<String>,

    /// Optional record of the runtime overlay path. Informational
    /// only; the actual path is computed by the loader.
    #[serde(rename = "_runtime_overlay_path", default, skip_serializing_if = "Option::is_none")]
    pub runtime_overlay_path: Option<String>,
}

fn default_modulation() -> String {
    "LSM".to_string()
}

impl Site {
    /// LO-snap formula for the active preset.
    ///
    /// Given the CC frequency and the AD9361 sample rate (= IF window
    /// width), return the LO frequency that places the CC at this
    /// site's `cc_position` within the window. `lo_shift_hz` is added
    /// to compensate for crystal ppm calibration.
    ///
    /// `margin_hz` controls how far inside the window edge the CC
    /// sits — 250 kHz default keeps the CC clear of the AD9361's
    /// transition band.
    pub fn snap_lo_hz(
        &self,
        sample_rate_hz: u64,
        lo_shift_hz: f64,
        margin_hz: f64,
    ) -> i64 {
        let cc = self.control_freq_hz as f64;
        let half = sample_rate_hz as f64 / 2.0;
        // Push the CC `(half - margin)` Hz off-centre in the
        // direction opposite traffic so the IF window's free space
        // covers as much traffic as possible.
        let offset = match self.cc_position {
            CcPosition::Top => half - margin_hz,
            CcPosition::Center => 0.0,
            CcPosition::Bottom => -(half - margin_hz),
        };
        // LO is what the AD9361 tunes to. NCO sees `(cc - lo)`; we
        // want NCO to land at +offset (so the CC sits +offset inside
        // the IF window). Solve: cc - lo = offset → lo = cc - offset.
        // Then add lo_shift to compensate ppm.
        (cc - offset + lo_shift_hz).round() as i64
    }
}

// ── Loaders / savers ────────────────────────────────────────────

/// Repo seeds embedded at compile time. CARGO_MANIFEST_DIR-based
/// filesystem lookup doesn't survive cross-compile to the Zynq
/// target (the path baked in points at the Buildroot host build
/// directory which isn't present on the device). Embedding makes
/// the seeds part of the binary so they're always available.
///
/// Add a new site by dropping `<name>.json` into `p25-httpd/sites/`
/// AND adding a tuple to this slice. The runtime overlay layer
/// (writable, persistent flash) handles per-site updates without
/// recompilation.
const EMBEDDED_SEEDS: &[(&str, &str)] = &[
    ("clay",  include_str!("../../sites/clay.json")),
    ("duval", include_str!("../../sites/duval.json")),
];

fn embedded_seed_for(name: &str) -> Option<&'static str> {
    EMBEDDED_SEEDS
        .iter()
        .find_map(|(n, body)| if *n == name { Some(*body) } else { None })
}

/// Runtime-overlay directory: `/mnt/jffs2/p25-sites/` on the target
/// (same persistent JFFS2 partition `app::autoppm` writes
/// `p25-ppm-cal.json` to). Configurable via `P25_SITES_OVERLAY_DIR`
/// env var for testing.
pub fn runtime_overlay_dir() -> PathBuf {
    if let Ok(p) = std::env::var("P25_SITES_OVERLAY_DIR") {
        PathBuf::from(p)
    } else {
        PathBuf::from("/mnt/jffs2/p25-sites")
    }
}

/// List the names of every available site (union of embedded
/// seeds + runtime overlay files).
pub fn list_sites() -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for (n, _) in EMBEDDED_SEEDS {
        names.insert((*n).to_string());
    }
    if let Ok(rd) = std::fs::read_dir(runtime_overlay_dir()) {
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                // Ignore reserved control files (e.g. `active.json`).
                if stem.starts_with('_') || stem == "active" {
                    continue;
                }
                names.insert(stem.to_string());
            }
        }
    }
    names.into_iter().collect()
}

/// Load the named site: embedded seed first, then overlay fields
/// override. Returns Err if neither layer has the site.
pub fn load_site(name: &str) -> anyhow::Result<Site> {
    let overlay_path = runtime_overlay_dir().join(format!("{name}.json"));

    let mut site = if let Some(body) = embedded_seed_for(name) {
        serde_json::from_str::<Site>(body)
            .map_err(|e| anyhow::anyhow!("parse embedded seed '{name}': {e}"))?
    } else {
        // No embedded seed for this name — try overlay alone.
        read_site_file(&overlay_path).map_err(|overlay_err| {
            anyhow::anyhow!(
                "site '{name}' not in embedded seeds (known: {known:?}) \
                 and not in runtime overlay ({overlay_err})",
                known = EMBEDDED_SEEDS.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            )
        })?
    };

    // Overlay wins for runtime-only fields if it exists. Don't
    // overwrite seed-only fields (label / preset_default / etc.) if
    // the overlay didn't set them — defaults to seed.
    if overlay_path.exists() {
        match read_site_file(&overlay_path) {
            Ok(overlay) => merge_overlay(&mut site, overlay),
            Err(e) => {
                tracing::warn!(
                    "site overlay {} unreadable: {e}; using seed only",
                    overlay_path.display()
                );
            }
        }
    }
    Ok(site)
}

fn read_site_file(path: &Path) -> anyhow::Result<Site> {
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let site: Site = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    Ok(site)
}

fn merge_overlay(base: &mut Site, overlay: Site) {
    // Only overwrite fields the overlay actually populates.
    if !overlay.label.is_empty() {
        base.label = overlay.label;
    }
    if !overlay.preset_default.is_empty() {
        base.preset_default = overlay.preset_default;
    }
    base.cc_position = overlay.cc_position;
    if overlay.control_freq_hz != 0 {
        base.control_freq_hz = overlay.control_freq_hz;
    }
    if !overlay.alt_control_freqs_hz.is_empty() {
        base.alt_control_freqs_hz = overlay.alt_control_freqs_hz;
    }
    if !overlay.traffic_freqs_hz.is_empty() {
        base.traffic_freqs_hz = overlay.traffic_freqs_hz;
    }
    base.nac = overlay.nac.or(base.nac);
    base.wacn = overlay.wacn.or(base.wacn);
    base.system_id = overlay.system_id.or(base.system_id);
    base.rfss_id = overlay.rfss_id.or(base.rfss_id);
    base.site_id = overlay.site_id.or(base.site_id);
    base.lra = overlay.lra.or(base.lra);
    if !overlay.iden_bands.is_empty() {
        base.iden_bands = overlay.iden_bands;
    }
    base.last_updated_unix_ms = overlay
        .last_updated_unix_ms
        .or(base.last_updated_unix_ms);
}

/// Atomic-replace save of the runtime overlay file. Creates the
/// overlay directory if missing. Stamps `last_updated_unix_ms`.
pub fn save_site(site: &Site) -> anyhow::Result<()> {
    let dir = runtime_overlay_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("mkdir {}: {e}", dir.display()))?;
    let final_path = dir.join(format!("{}.json", site.name));
    let tmp_path = dir.join(format!(".{}.json.tmp", site.name));

    let mut stamped = site.clone();
    stamped.last_updated_unix_ms = Some(now_unix_ms());

    let body = serde_json::to_vec_pretty(&stamped)
        .map_err(|e| anyhow::anyhow!("serialize {}: {e}", site.name))?;
    std::fs::write(&tmp_path, &body)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &final_path)
        .map_err(|e| anyhow::anyhow!(
            "rename {} -> {}: {e}",
            tmp_path.display(), final_path.display()
        ))?;
    Ok(())
}

/// Path to the active-site marker file (overlay dir).
pub fn active_site_path() -> PathBuf {
    runtime_overlay_dir().join("active.json")
}

/// Read the persisted active-site name, falling back to "clay" if
/// the marker file is missing.
pub fn read_active_site_name() -> String {
    if let Ok(bytes) = std::fs::read(active_site_path()) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(s) = v.get("name").and_then(|x| x.as_str()) {
                return s.to_string();
            }
        }
    }
    "clay".to_string()
}

/// Atomic-replace save of the active-site marker.
pub fn write_active_site_name(name: &str) -> anyhow::Result<()> {
    let dir = runtime_overlay_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("mkdir {}: {e}", dir.display()))?;
    let final_path = active_site_path();
    let tmp_path = dir.join(".active.json.tmp");
    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "name": name,
        "set_at_unix_ms": now_unix_ms(),
    }))?;
    std::fs::write(&tmp_path, &body)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "sites_tests.rs"]
mod tests;
