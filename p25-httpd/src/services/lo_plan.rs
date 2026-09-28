//! Change 070: receive-window planning. Where the AD9361 LO goes, and
//! which DDC preset, so the window holds the site's control channel and
//! as many of its traffic channels as possible, weighted by how busy each
//! channel is (grants seen) on top of the site file's channel list.
//!
//! Every grant the follower sees is noted against the active site; the
//! counts persist per site in `/mnt/jffs2/p25-plans/<site>.json` with
//! the auto-recentre switch. The recentre task (`app::recentre_task`)
//! moves the window when a better one exists and both chains are idle.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Part of the sample rate usable on each side of the LO: the traffic
/// DDC and the AD9361 analog filter hold decode quality this far out
/// (measured on unit A, 2026-09-28).
pub const USABLE_FRACTION: f64 = 0.45;
/// Channels are kept this far from the LO (DC-offset correction notch).
pub const DC_GUARD_HZ: i64 = 15_000;
/// Presets the planner may choose, narrowest first (validated live).
pub const PLAN_PRESETS: &[&str] = &["8M", "12M", "16M"];
/// Weight of a channel from the site file that has not been granted yet.
pub const SEED_WEIGHT: f64 = 1.0;
/// Grants after which the site's channel plan counts as learned: a
/// listed channel never granted by then weighs nothing (site files carry
/// stale and mistyped entries, e.g. Clay's 852.4385 MHz off the 6.25 kHz
/// raster). A later grant there brings it back.
pub const LEARNED_AFTER_GRANTS: u64 = 1_000;

pub fn usable_half_hz(sample_rate_hz: u32) -> i64 {
    (sample_rate_hz as f64 * USABLE_FRACTION) as i64
}

/// Is `freq_hz` inside the usable window of an LO at `lo_hz`?
pub fn covers(lo_hz: i64, freq_hz: u64, sample_rate_hz: u32) -> bool {
    (freq_hz as i64 - lo_hz).abs() <= usable_half_hz(sample_rate_hz)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Channel {
    pub freq_hz: u64,
    pub weight: f64,
}

/// The site file's traffic channels plus every granted frequency, each
/// weighted by its grants (a listed channel counts at least
/// `SEED_WEIGHT` until the plan is learned). Sorted by frequency.
pub fn channels(seed: &[u64], grants: &BTreeMap<u64, u32>) -> Vec<Channel> {
    let learned = grants.values().map(|n| *n as u64).sum::<u64>() >= LEARNED_AFTER_GRANTS;
    let mut m: BTreeMap<u64, f64> = BTreeMap::new();
    for f in seed {
        m.insert(*f, if learned { 0.0 } else { SEED_WEIGHT });
    }
    for (f, n) in grants {
        let w = m.entry(*f).or_insert(0.0);
        *w = (*n as f64).max(*w);
    }
    m.into_iter().map(|(freq_hz, weight)| Channel { freq_hz, weight }).collect()
}

/// Best LO at one sample rate, and the weight it covers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub lo_hz: i64,
    pub covered_weight: f64,
}

/// Place the window at `sample_rate_hz`: the control channel inside,
/// the most channel weight covered, then the covered set centred (most
/// margin at both edges) and no channel on the LO's DC notch.
pub fn place(cc_hz: u64, chans: &[Channel], sample_rate_hz: u32) -> Placement {
    let uh = usable_half_hz(sample_rate_hz);
    let cc = cc_hz as i64;
    let (lo_min, lo_max) = (cc - uh, cc + uh);
    let mut cands = vec![lo_min, lo_max];
    for c in chans {
        let f = c.freq_hz as i64;
        cands.extend([f - uh, f + uh].into_iter().filter(|l| (lo_min..=lo_max).contains(l)));
    }
    // (weight, -span, lo): most weight, then the tightest covered set.
    let mut best: Option<(f64, i64, i64, i64)> = None;
    for lo in cands {
        let mut w = 0.0;
        let (mut lo_f, mut hi_f) = (cc, cc);
        for c in chans {
            let f = c.freq_hz as i64;
            if (f - lo).abs() <= uh {
                w += c.weight;
                lo_f = lo_f.min(f);
                hi_f = hi_f.max(f);
            }
        }
        let better = match best {
            None => true,
            Some((bw, blo, bhi, _)) => w > bw + 1e-9 || ((w - bw).abs() <= 1e-9 && hi_f - lo_f < bhi - blo),
        };
        if better {
            best = Some((w, lo_f, hi_f, lo));
        }
    }
    let (w, lo_f, hi_f, _) = best.expect("at least the CC candidates");
    // Centre the covered set; the LO may move within [hi - uh, lo + uh].
    let mid = (lo_f + hi_f) / 2;
    let (slack_lo, slack_hi) = (hi_f - uh, lo_f + uh);
    let clear = |lo: i64| (lo - cc).abs() >= DC_GUARD_HZ
        && chans.iter().all(|c| (c.freq_hz as i64 - lo).abs() >= DC_GUARD_HZ);
    let mut lo = mid;
    for k in 0..=40i64 {
        let found = [mid + k * 5_000, mid - k * 5_000]
            .into_iter()
            .find(|l| (slack_lo..=slack_hi).contains(l) && clear(*l));
        if let Some(l) = found {
            lo = l;
            break;
        }
    }
    Placement { lo_hz: lo, covered_weight: w }
}

/// Coverage of a window: covered and missed channel frequencies.
pub fn coverage(lo_hz: i64, sample_rate_hz: u32, chans: &[Channel]) -> (Vec<u64>, Vec<u64>) {
    chans.iter().map(|c| c.freq_hz).partition(|f| covers(lo_hz, *f, sample_rate_hz))
}

/// A recommended window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoPlan {
    pub preset: String,
    pub sample_rate_hz: u32,
    pub lo_hz: i64,
    pub usable_half_hz: i64,
    pub covered_weight: f64,
    pub total_weight: f64,
}

/// The narrowest preset of `presets` (name, sample rate; narrowest
/// first) that covers every channel; else the one covering the most
/// weight (the narrower on a tie).
pub fn plan(cc_hz: u64, chans: &[Channel], presets: &[(&str, u32)]) -> Option<LoPlan> {
    let total: f64 = chans.iter().map(|c| c.weight).sum();
    let mut best: Option<LoPlan> = None;
    for (name, sr) in presets {
        let p = place(cc_hz, chans, *sr);
        let cand = LoPlan {
            preset: name.to_string(),
            sample_rate_hz: *sr,
            lo_hz: p.lo_hz,
            usable_half_hz: usable_half_hz(*sr),
            covered_weight: p.covered_weight,
            total_weight: total,
        };
        if p.covered_weight >= total - 1e-9 {
            return Some(cand);
        }
        if best.as_ref().map_or(true, |b| p.covered_weight > b.covered_weight + 1e-9) {
            best = Some(cand);
        }
    }
    best
}

/// `presets` (name, sample rate) without those narrower than `min`
/// (a name among them; unknown or None = all).
pub fn at_least<'a>(presets: &[(&'a str, u32)], min: Option<&str>) -> Vec<(&'a str, u32)> {
    let floor = min
        .and_then(|m| presets.iter().find(|(n, _)| n.eq_ignore_ascii_case(m)))
        .map_or(0, |(_, sr)| *sr);
    presets.iter().copied().filter(|(_, sr)| *sr >= floor).collect()
}

/// Is moving from a window covering `current` weight to one covering
/// `planned` worth a retune? Needs a real gain: all channels where some
/// were missed, or 5 % more of the weight.
pub fn worth_moving(current: f64, planned: f64, total: f64) -> bool {
    if total <= 0.0 || planned <= current + 1e-9 {
        return false;
    }
    let all = planned >= total - 1e-9;
    all || (planned - current) / total >= 0.05
}

/// What is kept per site.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SitePlan {
    /// Grants seen per frequency.
    pub grants: BTreeMap<u64, u32>,
    /// Recentre automatically when idle.
    pub auto: bool,
    pub last_recentre_unix_ms: u64,
    /// Narrowest preset the planner may pick here (e.g. "12M" keeps
    /// room around the channels in use); None = narrowest that fits.
    pub min_preset: Option<String>,
}

impl Default for SitePlan {
    fn default() -> Self {
        SitePlan { grants: BTreeMap::new(), auto: true, last_recentre_unix_ms: 0, min_preset: None }
    }
}

/// Grant counts are capped so one busy day does not freeze the map.
pub const MAX_COUNT: u32 = 1_000_000;

/// Live per-site plans and where they persist.
pub struct PlanStore {
    dir: Option<PathBuf>,
    inner: Mutex<PlanInner>,
}

struct PlanInner {
    site: String,
    plans: BTreeMap<String, SitePlan>,
    dirty: bool,
}

impl PlanStore {
    /// `P25_PLANS_DIR` overrides; the host build keeps plans in memory.
    pub fn default_dir() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("P25_PLANS_DIR") {
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
        cfg!(target_os = "linux").then(|| PathBuf::from("/mnt/jffs2/p25-plans"))
    }

    pub fn new(dir: Option<PathBuf>, site: &str) -> Self {
        let s = PlanStore {
            dir,
            inner: Mutex::new(PlanInner { site: String::new(), plans: BTreeMap::new(), dirty: false }),
        };
        s.set_site(site);
        s
    }

    fn path(&self, site: &str) -> Option<PathBuf> {
        let ok = !site.is_empty() && site.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        self.dir.as_ref().filter(|_| ok).map(|d| d.join(format!("{site}.json")))
    }

    /// Make `site` the one grants are noted against (loaded from disk
    /// the first time).
    pub fn set_site(&self, site: &str) {
        // The site being left keeps what it learned.
        if let Err(e) = self.flush() {
            tracing::warn!("lo plan: not saved: {e}");
        }
        let loaded = match self.path(site) {
            Some(p) => std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<SitePlan>(&b).ok()),
            None => None,
        };
        let mut g = self.inner.lock().unwrap();
        g.site = site.to_string();
        if !g.plans.contains_key(site) {
            g.plans.insert(site.to_string(), loaded.unwrap_or_default());
        }
    }

    pub fn site(&self) -> String {
        self.inner.lock().unwrap().site.clone()
    }

    pub fn note_grant(&self, freq_hz: u64) {
        let mut g = self.inner.lock().unwrap();
        let site = g.site.clone();
        let n = g.plans.entry(site).or_default().grants.entry(freq_hz).or_insert(0);
        *n = (*n + 1).min(MAX_COUNT);
        g.dirty = true;
    }

    pub fn get(&self) -> SitePlan {
        let g = self.inner.lock().unwrap();
        g.plans.get(&g.site).cloned().unwrap_or_default()
    }

    /// Change the active site's plan (and mark it for saving).
    pub fn edit(&self, f: impl FnOnce(&mut SitePlan)) {
        let mut g = self.inner.lock().unwrap();
        let site = g.site.clone();
        f(g.plans.entry(site).or_default());
        g.dirty = true;
    }

    /// Persist the active site's plan if it changed. Errors are
    /// returned for the log; the live plan is kept either way.
    pub fn flush(&self) -> Result<bool, String> {
        let (site, plan) = {
            let mut g = self.inner.lock().unwrap();
            if !g.dirty {
                return Ok(false);
            }
            g.dirty = false;
            (g.site.clone(), g.plans.get(&g.site).cloned().unwrap_or_default())
        };
        let Some(path) = self.path(&site) else { return Ok(false) };
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&plan).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "lo_plan_tests.rs"]
mod tests;
