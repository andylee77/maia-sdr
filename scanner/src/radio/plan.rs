//! The receive window: where the AD9361 LO goes and which DDC preset, so the window holds the
//! site's control channel and as many of its traffic channels as possible, weighted by how busy
//! each one is.

use std::collections::BTreeMap;

use serde::Serialize;

/// Part of the sample rate usable on each side of the LO: the traffic DDC and the AD9361 analog
/// filter hold decode quality this far out (measured on unit A).
pub const USABLE_FRACTION: f64 = 0.45;
/// Channels are kept this far from the LO (the DC-offset correction notch).
pub const DC_GUARD_HZ: i64 = 15_000;
/// Weight of a listed channel not granted yet.
pub const SEED_WEIGHT: f64 = 1.0;
/// Grants after which the channel plan counts as learned: a listed channel never granted by
/// then weighs nothing (site lists carry stale and mistyped entries); a later grant there
/// brings it back.
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

/// The listed channels plus every granted frequency, each weighted by its grants (a listed
/// channel counts at least `SEED_WEIGHT` until the plan is learned), sorted by frequency.
pub fn channels(listed: &[u64], grants: &BTreeMap<u64, u32>) -> Vec<Channel> {
    let learned = grants.values().map(|n| *n as u64).sum::<u64>() >= LEARNED_AFTER_GRANTS;
    let mut m: BTreeMap<u64, f64> = BTreeMap::new();
    for f in listed {
        m.insert(*f, if learned { 0.0 } else { SEED_WEIGHT });
    }
    for (f, n) in grants {
        let w = m.entry(*f).or_insert(0.0);
        *w = (*n as f64).max(*w);
    }
    m.into_iter().map(|(freq_hz, weight)| Channel { freq_hz, weight }).collect()
}

/// The best LO at one sample rate and the weight it covers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub lo_hz: i64,
    pub covered_weight: f64,
}

/// Place the window at `sample_rate_hz`: the control channel inside, the most weight covered,
/// then the covered set centred (most margin at both edges) with no channel on the DC notch.
pub fn place(cc_hz: u64, chans: &[Channel], sample_rate_hz: u32) -> Placement {
    let uh = usable_half_hz(sample_rate_hz);
    let cc = cc_hz as i64;
    let (lo_min, lo_max) = (cc - uh, cc + uh);
    let mut cands = vec![lo_min, lo_max];
    for c in chans {
        let f = c.freq_hz as i64;
        cands.extend([f - uh, f + uh].into_iter().filter(|l| (lo_min..=lo_max).contains(l)));
    }
    // (weight, lowest covered, highest covered): most weight, then the tightest set.
    let mut best: Option<(f64, i64, i64)> = None;
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
            Some((bw, blo, bhi)) => w > bw + 1e-9 || ((w - bw).abs() <= 1e-9 && hi_f - lo_f < bhi - blo),
        };
        if better {
            best = Some((w, lo_f, hi_f));
        }
    }
    let (w, lo_f, hi_f) = best.expect("at least the control channel's candidates");
    // Centre the covered set; the LO may move within [hi - uh, lo + uh].
    let mid = (lo_f + hi_f) / 2;
    let (slack_lo, slack_hi) = (hi_f - uh, lo_f + uh);
    let clear = |lo: i64| {
        (lo - cc).abs() >= DC_GUARD_HZ && chans.iter().all(|c| (c.freq_hz as i64 - lo).abs() >= DC_GUARD_HZ)
    };
    let lo = (0..=40i64)
        .flat_map(|k| [mid + k * 5_000, mid - k * 5_000])
        .find(|l| (slack_lo..=slack_hi).contains(l) && clear(*l))
        .unwrap_or(mid);
    Placement { lo_hz: lo, covered_weight: w }
}

/// A recommended window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowPlan {
    pub preset: String,
    pub sample_rate_hz: u32,
    pub lo_hz: i64,
    pub usable_half_hz: i64,
    pub covered_weight: f64,
    pub total_weight: f64,
}

/// The narrowest of `presets` (name, sample rate; narrowest first) that covers every channel,
/// else the one covering the most weight (the narrower on a tie).
pub fn plan(cc_hz: u64, chans: &[Channel], presets: &[(&str, u32)]) -> Option<WindowPlan> {
    let total: f64 = chans.iter().map(|c| c.weight).sum();
    let mut best: Option<WindowPlan> = None;
    for (name, sr) in presets {
        let p = place(cc_hz, chans, *sr);
        let cand = WindowPlan {
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
        if best.as_ref().is_none_or(|b| p.covered_weight > b.covered_weight + 1e-9) {
            best = Some(cand);
        }
    }
    best
}

/// `presets` without those narrower than `min` (unknown or `None`: all).
pub fn at_least<'a>(presets: &[(&'a str, u32)], min: Option<&str>) -> Vec<(&'a str, u32)> {
    let floor = min
        .and_then(|m| presets.iter().find(|(n, _)| n.eq_ignore_ascii_case(m)))
        .map_or(0, |(_, sr)| *sr);
    presets.iter().copied().filter(|(_, sr)| *sr >= floor).collect()
}

/// Is moving from a window covering `current` weight to one covering `planned` worth a retune?
/// Only for every channel where some were missed, or 5 % more of the weight.
pub fn worth_moving(current: f64, planned: f64, total: f64) -> bool {
    if total <= 0.0 || planned <= current + 1e-9 {
        return false;
    }
    planned >= total - 1e-9 || (planned - current) / total >= 0.05
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAY_CC: u64 = 860_962_500;
    const CLAY: &[u64] = &[
        852_438_500, 855_237_500, 856_437_500, 857_212_500, 857_437_500, 857_987_500,
        858_437_500, 858_462_500, 858_987_500, 859_437_500, 860_437_500, 860_962_500,
    ];
    const DUVAL_CC: u64 = 855_487_500;
    const DUVAL_LOW: u64 = 854_962_500;
    const DUVAL_HIGH: u64 = 860_937_500;

    fn duval() -> Vec<u64> {
        vec![
            854_962_500, 855_212_500, 855_487_500, 855_962_500, 855_987_500, 856_212_500, 856_262_500,
            856_462_500, 856_712_500, 856_737_500, 856_937_500, 856_962_500, 856_987_500, 857_237_500,
            857_462_500, 857_712_500, 857_937_500, 857_962_500, 858_712_500, 858_962_500, 859_462_500,
            859_712_500, 859_937_500, 859_962_500, 859_987_500, 860_462_500, 860_712_500, 860_937_500,
        ]
    }

    fn presets() -> Vec<(&'static str, u32)> {
        vec![("8M", 8_000_000), ("12M", 12_000_000), ("16M", 16_000_000)]
    }

    #[test]
    fn duval_fits_8m_centred_on_its_traffic() {
        let ch = channels(&duval(), &BTreeMap::new());
        let p = plan(DUVAL_CC, &ch, &presets()).unwrap();
        assert_eq!(p.preset, "8M");
        assert!(p.covered_weight >= p.total_weight);
        let mid = ((DUVAL_LOW + DUVAL_HIGH) / 2) as i64;
        assert!((p.lo_hz - mid).abs() <= 100_000, "lo {} vs mid {mid}", p.lo_hz);
        assert!(covers(p.lo_hz, DUVAL_LOW, 8_000_000) && covers(p.lo_hz, DUVAL_HIGH, 8_000_000));
        assert!(duval().iter().all(|f| (*f as i64 - p.lo_hz).abs() >= DC_GUARD_HZ));
    }

    #[test]
    fn clay_needs_12m_for_its_low_channel() {
        let ch = channels(CLAY, &BTreeMap::new());
        let at8 = place(CLAY_CC, &ch, 8_000_000);
        assert!(at8.covered_weight < ch.len() as f64, "8.5 MHz span does not fit 8M");
        assert!(covers(at8.lo_hz, CLAY_CC, 8_000_000));
        let p = plan(CLAY_CC, &ch, &presets()).unwrap();
        assert_eq!(p.preset, "12M");
        assert!(covers(p.lo_hz, 852_438_500, 12_000_000) && covers(p.lo_hz, CLAY_CC, 12_000_000));
    }

    #[test]
    fn busy_channels_outweigh_idle_ones() {
        let seed = [851_300_000, 858_500_000, 858_600_000];
        let idle = place(855_000_000, &channels(&seed, &BTreeMap::new()), 8_000_000);
        assert!(covers(idle.lo_hz, 858_600_000, 8_000_000) && !covers(idle.lo_hz, 851_300_000, 8_000_000));
        let grants = BTreeMap::from([(851_300_000, 50)]);
        let busy = place(855_000_000, &channels(&seed, &grants), 8_000_000);
        assert!(covers(busy.lo_hz, 851_300_000, 8_000_000));
        assert!(covers(busy.lo_hz, 855_000_000, 8_000_000), "the control channel stays inside");
        assert_eq!(busy.covered_weight, 51.0);
    }

    #[test]
    fn learned_channels_join_the_site_list() {
        let grants = BTreeMap::from([(851_000_000, 3), (858_437_500, 7)]);
        let ch = channels(CLAY, &grants);
        assert_eq!(ch.len(), CLAY.len() + 1);
        assert_eq!(ch.iter().find(|c| c.freq_hz == 858_437_500).unwrap().weight, 7.0);
        assert_eq!(ch.iter().find(|c| c.freq_hz == 855_237_500).unwrap().weight, SEED_WEIGHT);
        assert!(!covers(858_100_000, 851_000_000, 8_000_000) && !covers(858_100_000, 852_438_500, 8_000_000));
        assert!(covers(858_100_000, 858_437_500, 8_000_000));
    }

    #[test]
    fn a_learned_plan_drops_listed_channels_never_granted() {
        let mut grants = BTreeMap::from([(856_437_500, 100), (857_987_500, 200), (858_437_500, 400), (858_462_500, 290)]);
        assert_eq!(plan(CLAY_CC, &channels(CLAY, &grants), &presets()).unwrap().preset, "12M");
        grants.insert(857_212_500, 10);
        let ch = channels(CLAY, &grants);
        assert_eq!(ch.iter().find(|c| c.freq_hz == 852_438_500).unwrap().weight, 0.0);
        let p = plan(CLAY_CC, &ch, &presets()).unwrap();
        assert_eq!(p.preset, "8M");
        assert!(p.covered_weight >= p.total_weight);
        grants.insert(852_438_500, 1);
        assert_eq!(plan(CLAY_CC, &channels(CLAY, &grants), &presets()).unwrap().preset, "12M");
    }

    #[test]
    fn control_channel_always_inside() {
        let ch = channels(&[840_000_000, 841_000_000], &BTreeMap::new());
        let p = place(860_000_000, &ch, 8_000_000);
        assert!(covers(p.lo_hz, 860_000_000, 8_000_000));
        assert_eq!(p.covered_weight, 0.0);
        let p = place(860_000_000, &[], 8_000_000);
        assert!((p.lo_hz - 860_000_000).abs() >= DC_GUARD_HZ && (p.lo_hz - 860_000_000).abs() < 100_000);
    }

    #[test]
    fn a_minimum_preset_keeps_the_window_wide() {
        let ch = channels(&duval(), &BTreeMap::new());
        let p = plan(DUVAL_CC, &ch, &at_least(&presets(), Some("12M"))).unwrap();
        assert_eq!(p.preset, "12M");
        assert!(covers(p.lo_hz, DUVAL_LOW, 12_000_000) && covers(p.lo_hz, DUVAL_HIGH, 12_000_000));
        assert_eq!(at_least(&presets(), None).len(), 3);
        assert_eq!(at_least(&presets(), Some("nope")).len(), 3);
        assert_eq!(at_least(&presets(), Some("16m")), vec![("16M", 16_000_000)]);
    }

    #[test]
    fn moving_needs_a_real_gain() {
        assert!(!worth_moving(27.0, 27.0, 28.0));
        assert!(worth_moving(27.0, 28.0, 28.0), "everything covered");
        assert!(!worth_moving(90.0, 93.0, 200.0), "1.5 % more");
        assert!(worth_moving(90.0, 101.0, 200.0));
        assert!(!worth_moving(0.0, 0.0, 0.0));
    }
}
