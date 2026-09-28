//! Host tests for `services::lo_plan` (change 070).

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
    // Centred on the span (857.95 MHz), not on the control channel.
    let mid = ((DUVAL_LOW + DUVAL_HIGH) / 2) as i64;
    assert!((p.lo_hz - mid).abs() <= 100_000, "lo {} vs mid {mid}", p.lo_hz);
    assert!(covers(p.lo_hz, DUVAL_LOW, 8_000_000) && covers(p.lo_hz, DUVAL_HIGH, 8_000_000));
    // Off every channel's DC notch.
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
    // CC 855 MHz at 8M (usable +-3.6 MHz): the window holds either the
    // channel at 851.3 or the two at 858.5 / 858.6, not all three.
    let seed = [851_300_000, 858_500_000, 858_600_000];
    let idle = place(855_000_000, &channels(&seed, &BTreeMap::new()), 8_000_000);
    assert!(covers(idle.lo_hz, 858_600_000, 8_000_000) && !covers(idle.lo_hz, 851_300_000, 8_000_000));
    let mut grants = BTreeMap::new();
    grants.insert(851_300_000, 50);
    let busy = place(855_000_000, &channels(&seed, &grants), 8_000_000);
    assert!(covers(busy.lo_hz, 851_300_000, 8_000_000));
    assert!(covers(busy.lo_hz, 855_000_000, 8_000_000), "the control channel stays inside");
    // 858.5 is exactly 7.2 MHz above 851.3: it fits at the far edge.
    assert_eq!(busy.covered_weight, 51.0);
}

#[test]
fn learned_channels_join_the_site_list() {
    let mut grants = BTreeMap::new();
    grants.insert(851_000_000, 3);
    grants.insert(858_437_500, 7);
    let ch = channels(CLAY, &grants);
    assert_eq!(ch.len(), CLAY.len() + 1);
    assert_eq!(ch.iter().find(|c| c.freq_hz == 858_437_500).unwrap().weight, 7.0);
    assert_eq!(ch.iter().find(|c| c.freq_hz == 855_237_500).unwrap().weight, SEED_WEIGHT);
    let (cov, miss) = coverage(858_100_000, 8_000_000, &ch);
    assert!(miss.contains(&851_000_000) && miss.contains(&852_438_500));
    assert_eq!(cov.len() + miss.len(), ch.len());
}

#[test]
fn a_learned_plan_drops_listed_channels_never_granted() {
    // Clay as seen on air: every grant on 856.4-858.5 MHz, none on the
    // listed 852.4385. Before 1000 grants the list still counts (12M);
    // after, 8M holds everything that is used.
    let mut grants = BTreeMap::new();
    for (f, n) in [(856_437_500, 100), (857_987_500, 200), (858_437_500, 400), (858_462_500, 290)] {
        grants.insert(f, n);
    }
    assert_eq!(plan(CLAY_CC, &channels(CLAY, &grants), &presets()).unwrap().preset, "12M");
    grants.insert(857_212_500, 10);
    let ch = channels(CLAY, &grants);
    assert_eq!(ch.iter().find(|c| c.freq_hz == 852_438_500).unwrap().weight, 0.0);
    let p = plan(CLAY_CC, &ch, &presets()).unwrap();
    assert_eq!(p.preset, "8M");
    assert!(p.covered_weight >= p.total_weight);
    // One grant there later and the window widens again.
    grants.insert(852_438_500, 1);
    assert_eq!(plan(CLAY_CC, &channels(CLAY, &grants), &presets()).unwrap().preset, "12M");
}

#[test]
fn control_channel_always_inside() {
    // All traffic far below the CC: the window keeps the CC at its edge.
    let ch = channels(&[840_000_000, 841_000_000], &BTreeMap::new());
    let p = place(860_000_000, &ch, 8_000_000);
    assert!(covers(p.lo_hz, 860_000_000, 8_000_000));
    assert_eq!(p.covered_weight, 0.0);
    // No channels at all: centred on the CC, off its DC notch.
    let p = place(860_000_000, &[], 8_000_000);
    assert!((p.lo_hz - 860_000_000).abs() >= DC_GUARD_HZ && (p.lo_hz - 860_000_000).abs() < 100_000);
}

#[test]
fn a_minimum_preset_keeps_the_window_wide() {
    // Duval fits 8M; with "12M" as the narrowest it gets 12M, centred.
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

#[test]
fn store_notes_grants_per_site_and_persists() {
    let dir = std::env::temp_dir().join(format!("p25_lo_plan_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = PlanStore::new(Some(dir.clone()), "clay");
    s.note_grant(852_438_500);
    s.note_grant(852_438_500);
    s.edit(|p| p.auto = false);
    assert!(s.flush().unwrap());
    assert!(!s.flush().unwrap(), "nothing new");
    // Another site learns separately; leaving clay keeps its counts.
    s.set_site("duval");
    s.note_grant(860_937_500);
    assert_eq!(s.get().grants.len(), 1);
    s.set_site("clay");
    assert_eq!(s.get().grants[&852_438_500], 2);
    // A fresh store reads both back.
    let again = PlanStore::new(Some(dir.clone()), "duval");
    assert_eq!(again.get().grants[&860_937_500], 1);
    again.set_site("clay");
    assert!(!again.get().auto);
    assert_eq!(again.get().grants[&852_438_500], 2);
    // Names that are not plain file names are kept in memory only.
    let odd = PlanStore::new(Some(dir.clone()), "../x");
    odd.note_grant(1);
    assert!(!odd.flush().unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Change 073: grants are held from a site switch until the tune (plus
/// the drain), or the maximum when no tune follows.
#[test]
fn site_switch_holds_grants_until_the_tune() {
    // (Small times: the follower's check with the real clock is never
    // affected by this test.)
    assert!(!grants_held(1_000));
    release_grants_soon(1_000); // no hold: nothing to release
    assert!(!grants_held(1_001));
    hold_grants(1_000);
    assert!(grants_held(1_500));
    release_grants_soon(2_000);
    assert!(grants_held(2_999) && !grants_held(3_000));
    // No tune: the hold ends by itself.
    hold_grants(10_000);
    assert!(grants_held(10_000 + GRANT_HOLD_MAX_MS - 1) && !grants_held(10_000 + GRANT_HOLD_MAX_MS));
    // A tune long after: nothing to release, nothing re-armed.
    release_grants_soon(30_000);
    assert!(!grants_held(30_001));
}
