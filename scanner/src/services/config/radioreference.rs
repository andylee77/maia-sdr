//! RadioReference's CSV downloads, imported into a system: its talkgroups (`trs_tg_*.csv`) as
//! aliases, its sites (`trs_sites_*.csv`) as sites. The header row says which file it is, and a
//! file is checked whole before anything changes.
//!
//! - **Talkgroups:** an alias per talkgroup no alias covers yet, named by the alpha tag and
//!   grouped by the category, as SDRTrunk's import makes them; fully encrypted talkgroups
//!   (mode `E`) are never followed unless asked. Talkgroups an alias already covers are kept.
//! - **Sites:** RadioReference marks the control channels with `c`. A new site starts on the
//!   first of them, with the others as alternates and every other frequency as a known channel.
//!   A site already configured gains only the channels it lacks; its name and control channel
//!   stay.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::services::config::aliases::Alias;
use crate::services::config::ids::{slug, unique};
use crate::services::config::systems::{Control, Protocol, Site, SiteIdentity, System};
use crate::services::discovery::SAME_CHANNEL_HZ;

/// What to import, and how.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    /// The file's text.
    pub csv: String,
    /// Fully encrypted talkgroups are never followed (SDRTrunk's "set encrypted talkgroups to
    /// muted", on unless turned off).
    #[serde(default = "yes")]
    pub encrypted_do_not_monitor: bool,
    /// A sites file: only these rows (`key`, as a preview names them); all when absent.
    #[serde(default)]
    pub sites: Option<Vec<String>>,
}

impl Default for ImportRequest {
    fn default() -> Self {
        ImportRequest { csv: String::new(), encrypted_do_not_monitor: true, sites: None }
    }
}

fn yes() -> bool {
    true
}

/// One talkgroup row.
#[derive(Debug, Clone, PartialEq)]
pub struct Talkgroup {
    pub id: u32,
    pub name: String,
    pub category: Option<String>,
    /// Mode `E` (fully encrypted; `e` is partly).
    pub encrypted: bool,
}

/// One site row.
#[derive(Debug, Clone, PartialEq)]
pub struct SiteRow {
    /// `RFSS-site` (P25) or `region-site` (DMR).
    pub key: String,
    /// The RFSS (P25) or region (DMR).
    pub area: u32,
    pub site: u32,
    pub nac: Option<u32>,
    pub label: String,
    /// County, location and range, as RadioReference gives them.
    pub place: Option<String>,
    /// The control channels, in the file's order.
    pub control_hz: Vec<u64>,
    pub other_hz: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum File {
    Talkgroups(Vec<Talkgroup>),
    /// P25 sites have an RFSS and a NAC; DMR sites a region.
    Sites { protocol: Protocol, rows: Vec<SiteRow> },
}

/// What an import did, or would do.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Imported {
    /// `talkgroups` or `sites`.
    pub file: &'static str,
    pub aliases_added: Vec<Alias>,
    /// Talkgroups an alias already covered, left as they were.
    pub talkgroups_kept: Vec<u32>,
    pub sites_added: Vec<ImportedSite>,
    /// Configured sites that gained channels, as they are after.
    pub sites_updated: Vec<ImportedSite>,
    /// Rows left out, and why.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportedSite {
    pub key: String,
    pub site: Site,
}

/// Read a talkgroups or sites file.
pub fn parse(text: &str) -> Result<File, String> {
    let mut lines = text.trim_start_matches('\u{feff}').lines().enumerate().filter(|(_, l)| !l.trim().is_empty());
    let (_, header) = lines.next().ok_or("the file is empty")?;
    let header = fields(header);
    let col = |name: &str| header.iter().position(|h| h.eq_ignore_ascii_case(name));
    if let (Some(id_col), Some(tag_col)) = (col("Decimal"), col("Alpha Tag")) {
        let (mode_col, description_col, category_col) = (col("Mode"), col("Description"), col("Category"));
        let mut out = Vec::new();
        for (n, line) in lines {
            let f = fields(line);
            let at = |i: Option<usize>| i.and_then(|i| f.get(i)).map(String::as_str).unwrap_or("");
            let id: u32 = at(Some(id_col)).parse().map_err(|_| format!("line {}: talkgroup {:?} is not a number", n + 1, at(Some(id_col))))?;
            let name = [at(Some(tag_col)), at(description_col)].into_iter().find(|s| !s.is_empty()).map_or_else(|| format!("TG {id}"), str::to_string);
            let category = Some(at(category_col).to_string()).filter(|c| !c.is_empty());
            out.push(Talkgroup { id, name, category, encrypted: at(mode_col).contains('E') });
        }
        return Ok(File::Talkgroups(out));
    }
    let (Some(site_col), Some(freqs_col)) = (col("Site Dec"), col("Frequencies")) else {
        return Err("not a RadioReference talkgroups or sites file (its header has neither Decimal and Alpha Tag nor Site Dec and Frequencies)".into());
    };
    let (protocol, area_col) = match (col("RFSS"), col("Region")) {
        (Some(i), _) => (Protocol::P25, i),
        (None, Some(i)) => (Protocol::DmrTier3, i),
        (None, None) => return Err("a sites file names each site's RFSS or region".into()),
    };
    let (nac_col, description_col, county_col, lat_col, lon_col, range_col) =
        (col("Site NAC"), col("Description"), col("County Name"), col("Lat"), col("Lon"), col("Range"));
    let mut rows = Vec::new();
    for (n, line) in lines {
        let f = fields(line);
        let at = |i: Option<usize>| i.and_then(|i| f.get(i)).map(String::as_str).unwrap_or("");
        let number = |i: usize, what: &str| -> Result<u32, String> {
            at(Some(i)).parse().map_err(|_| format!("line {}: {what} {:?} is not a number", n + 1, at(Some(i))))
        };
        let (area, site) = (number(area_col, "RFSS or region")?, number(site_col, "site")?);
        let nac = match at(nac_col) {
            "" => None,
            s => Some(u32::from_str_radix(s, 16).ok().filter(|&v| v <= 0xFFF).ok_or_else(|| format!("line {}: NAC {s:?} is not 3 hex digits", n + 1))?),
        };
        let (mut control_hz, mut other_hz) = (Vec::new(), Vec::new());
        for s in f.iter().skip(freqs_col).filter(|s| !s.is_empty()) {
            let (hz, control) = frequency(s).map_err(|e| format!("line {}: {e}", n + 1))?;
            if control { control_hz.push(hz) } else { other_hz.push(hz) }
        }
        let mut place = Vec::new();
        if !at(county_col).is_empty() {
            place.push(format!("{} County", at(county_col)));
        }
        if !at(lat_col).is_empty() && !at(lon_col).is_empty() {
            place.push(format!("{}, {}", at(lat_col), at(lon_col)));
        }
        if !at(range_col).is_empty() {
            place.push(format!("range {} mi", at(range_col)));
        }
        let label = match at(description_col) {
            "" if protocol == Protocol::P25 => format!("RFSS {area} site {site}"),
            "" => format!("Region {area} site {site}"),
            d => d.to_string(),
        };
        let place = Some(place.join("; ")).filter(|p| !p.is_empty());
        rows.push(SiteRow { key: format!("{area}-{site}"), area, site, nac, label, place, control_hz, other_hz });
    }
    Ok(File::Sites { protocol, rows })
}

/// One line's fields: commas outside quotes split them, `""` inside quotes is a quote.
fn fields(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|f| f.trim().to_string()).collect()
}

/// `858.987500c`: the frequency in Hz, and whether it is marked a control channel.
fn frequency(s: &str) -> Result<(u64, bool), String> {
    let (mhz, control) = s.strip_suffix('c').map_or((s, false), |m| (m, true));
    let (whole, frac) = mhz.split_once('.').unwrap_or((mhz, ""));
    let digits = |d: &str| d.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(frac) || frac.len() > 6 {
        return Err(format!("{s:?} is not a frequency in MHz"));
    }
    let hz = whole.parse::<u64>().map_err(|e| e.to_string())? * 1_000_000 + format!("{frac:0<6}").parse::<u64>().map_err(|e| e.to_string())?;
    if !(70_000_000..=6_000_000_000).contains(&hz) {
        return Err(format!("{s} MHz is outside 70 MHz to 6 GHz"));
    }
    Ok((hz, control))
}

/// Apply `file` to `sys`; `site_ids` are every configured site's id (a new site's must differ).
pub fn import(sys: &mut System, file: &File, req: &ImportRequest, site_ids: &[String]) -> Result<Imported, String> {
    match file {
        File::Talkgroups(rows) => talkgroups(sys, rows, req),
        File::Sites { protocol, rows } => sites(sys, *protocol, rows, req, site_ids),
    }
}

fn talkgroups(sys: &mut System, rows: &[Talkgroup], req: &ImportRequest) -> Result<Imported, String> {
    let mut out = Imported { file: "talkgroups", ..Default::default() };
    let index = sys.alias_index();
    let mut seen = HashSet::new();
    for tg in rows.iter().filter(|t| seen.insert(t.id)) {
        if index.talkgroup(tg.id).is_some() {
            out.talkgroups_kept.push(tg.id);
            continue;
        }
        let alias = Alias {
            group: tg.category.clone(),
            do_not_monitor: req.encrypted_do_not_monitor && tg.encrypted,
            ..Alias::talkgroup(tg.id, tg.name.clone())
        };
        alias.check()?;
        out.aliases_added.push(alias);
    }
    sys.aliases.extend(out.aliases_added.iter().cloned());
    Ok(out)
}

fn sites(sys: &mut System, protocol: Protocol, rows: &[SiteRow], req: &ImportRequest, site_ids: &[String]) -> Result<Imported, String> {
    if protocol != sys.protocol {
        let name = |p| if p == Protocol::P25 { "P25" } else { "DMR" };
        return Err(format!("a {} sites file does not belong to {}, a {} system", name(protocol), sys.label, name(sys.protocol)));
    }
    let mut out = Imported { file: "sites", ..Default::default() };
    let mut taken = site_ids.to_vec();
    for row in rows.iter().filter(|r| req.sites.as_ref().is_none_or(|keep| keep.contains(&r.key))) {
        let Some((&control, alternates)) = row.control_hz.split_first() else {
            out.skipped.push(format!("{} ({}): no control channel", row.key, row.label));
            continue;
        };
        if let Some(i) = configured(sys, row) {
            if gain(&mut sys.sites[i], row) {
                out.sites_updated.push(ImportedSite { key: row.key.clone(), site: sys.sites[i].clone() });
            }
            continue;
        }
        let id = unique(slug(&format!("{} {}", sys.label, row.label)), |id| taken.iter().any(|t| t == id));
        taken.push(id.clone());
        let p25 = protocol == Protocol::P25;
        let site = Site {
            id,
            label: row.label.clone(),
            identity: if p25 {
                SiteIdentity { rfss: Some(row.area), site: Some(row.site), nac: row.nac, ..Default::default() }
            } else {
                SiteIdentity::default()
            },
            control: Control { freq_hz: control, alternates_hz: alternates.to_vec(), lcn: None, timeslot: None },
            modulation: Default::default(),
            channels_hz: channels(row, control),
            channel_plan: None,
            window: Default::default(),
            notes: row.place.iter().map(|p| format!("RadioReference: {p}")).collect(),
            source: Some(format!("RadioReference sites file ({} {}, site {})", if p25 { "RFSS" } else { "region" }, row.area, row.site)),
        };
        sys.sites.push(site.clone());
        out.sites_added.push(ImportedSite { key: row.key.clone(), site });
    }
    Ok(out)
}

/// The configured site a row is: a P25 site that knows its RFSS and site by those; any other by
/// a control channel within 3 kHz (frequencies repeat across a system's sites, identities do
/// not).
fn configured(sys: &System, row: &SiteRow) -> Option<usize> {
    sys.sites.iter().position(|s| {
        if let (Protocol::P25, Some(rfss), Some(site)) = (sys.protocol, s.identity.rfss, s.identity.site) {
            return rfss == row.area && site == row.site;
        }
        let near = |f: u64| row.control_hz.iter().any(|&c| c.abs_diff(f) <= SAME_CHANNEL_HZ);
        near(s.control.freq_hz) || s.control.alternates_hz.iter().any(|&f| near(f))
    })
}

/// Every frequency of the row but `control`, in order.
fn channels(row: &SiteRow, control: u64) -> Vec<u64> {
    let mut out: Vec<u64> = row.control_hz.iter().chain(&row.other_hz).copied().filter(|&f| f.abs_diff(control) > SAME_CHANNEL_HZ).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Add the row's channels a configured site lacks, and the NAC when it has none.
fn gain(site: &mut Site, row: &SiteRow) -> bool {
    let before = (site.control.alternates_hz.len(), site.channels_hz.len(), site.identity.nac);
    let near = |a: u64, b: u64| a.abs_diff(b) <= SAME_CHANNEL_HZ;
    for &c in &row.control_hz {
        if !near(c, site.control.freq_hz) && !site.control.alternates_hz.iter().any(|&a| near(a, c)) {
            site.control.alternates_hz.push(c);
        }
    }
    for f in channels(row, site.control.freq_hz) {
        if !site.channels_hz.iter().any(|&k| near(k, f)) {
            site.channels_hz.push(f);
        }
    }
    site.channels_hz.sort_unstable();
    if site.identity.rfss == Some(row.area) && site.identity.site == Some(row.site) {
        site.identity.nac = site.identity.nac.or(row.nac);
    }
    before != (site.control.alternates_hz.len(), site.channels_hz.len(), site.identity.nac)
}

#[cfg(test)]
#[path = "radioreference_tests.rs"]
mod tests;
