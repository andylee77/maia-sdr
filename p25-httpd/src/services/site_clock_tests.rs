//! Host tests for `services::site_clock` (change 067).

use super::*;

fn sync(h: u8, mi: u8, microslots: u16, locked: bool) -> SiteSync {
    SiteSync {
        year: 2026,
        month: 5,
        day: 3,
        hours: h,
        minutes: mi,
        microslots,
        microslot_locked: locked,
        ext_locked: false,
        local_offset_min: Some(-240),
    }
}

// 2026-05-03 08:42:00 UTC.
const T0842: u64 = 1_777_797_720_000;

#[test]
fn civil_dates_convert_to_unix_time() {
    assert_eq!(civil_to_unix_ms(1970, 1, 1, 0, 0), Some(0));
    assert_eq!(civil_to_unix_ms(2000, 3, 1, 0, 0), Some(951_868_800_000));
    assert_eq!(civil_to_unix_ms(2026, 5, 3, 8, 42), Some(T0842));
    assert_eq!(civil_to_unix_ms(2024, 2, 29, 12, 0), Some(1_709_208_000_000));
    assert_eq!(civil_to_unix_ms(2025, 2, 29, 0, 0), None);
    assert_eq!(civil_to_unix_ms(2026, 13, 1, 0, 0), None);
    assert_eq!(civil_to_unix_ms(2026, 1, 1, 24, 0), None);
    // A site with no clock (year 2000) is not a time.
    assert_eq!(SiteSync { year: 2000, ..sync(0, 0, 0, true) }.minute_ms(), None);
}

#[test]
fn locked_microslots_give_the_time_directly() {
    let mut c = SiteClock::default();
    // 08:42 + 2000 micro-slots (15 s), decoded at mono 10 s.
    c.observe_at(sync(8, 42, 2000, true), 10_000);
    assert_eq!(c.precision(), Some(Precision::Microslot));
    // The broadcast left the site RX_DELAY_MS before its decode.
    assert_eq!(c.site_ms_at(10_000 - RX_DELAY_MS), Some(T0842 + 15_000));
    assert_eq!(c.site_ms_at(20_000 - RX_DELAY_MS), Some(T0842 + 25_000));
}

#[test]
fn free_running_microslots_use_the_minute_rollover() {
    let mut c = SiteClock::default();
    c.observe_at(sync(8, 42, 7777, false), 1_000);
    assert_eq!(c.precision(), Some(Precision::Minute));
    c.observe_at(sync(8, 42, 1234, false), 1_200);
    assert_eq!(c.precision(), Some(Precision::Minute), "same minute: still coarse");
    // Broadcasts every 200 ms; 08:43 first seen at mono 30.4 s: the
    // rollover was between 30.2 and 30.4 s.
    c.observe_at(sync(8, 42, 99, false), 30_200);
    c.observe_at(sync(8, 43, 5, false), 30_400);
    assert_eq!(c.precision(), Some(Precision::Rollover));
    let at_rollover = c.site_ms_at(30_300 - RX_DELAY_MS).unwrap();
    assert_eq!(at_rollover, T0842 + 60_000);
    // Later broadcasts of the same minute keep that anchor.
    c.observe_at(sync(8, 43, 7, false), 40_000);
    assert_eq!(c.site_ms_at(30_300 - RX_DELAY_MS), Some(at_rollover));
}

#[test]
fn a_rollover_across_a_long_gap_is_not_used() {
    let mut c = SiteClock::default();
    c.observe_at(sync(8, 42, 0, false), 1_000);
    c.observe_at(sync(8, 43, 0, false), 20_000);
    assert_eq!(c.precision(), Some(Precision::Minute));
}

#[test]
fn a_site_clock_that_jumps_replaces_the_anchor() {
    let mut c = SiteClock::default();
    c.observe_at(sync(8, 42, 2000, true), 10_000);
    // A replay (or the site being set) moves to 21:05 the same day.
    c.observe_at(sync(21, 5, 400, false), 11_000);
    let t = c.site_ms_at(11_000).unwrap();
    let t2105 = civil_to_unix_ms(2026, 5, 3, 21, 5).unwrap();
    assert!((t2105..t2105 + 60_000).contains(&t), "{t}");
    assert_eq!(c.precision(), Some(Precision::Minute));
}

#[test]
fn the_board_clock_is_stepped_once_then_slewed() {
    use ClockAction::*;
    let site = T0842;
    // Unset board clock (1970): always stepped, even to minute precision.
    assert_eq!(site_clock_action(5_000_000, site, Precision::Minute, true), Step(site));
    // First correction since start: a step.
    assert_eq!(site_clock_action(site + 2_500, site, Precision::Rollover, false), Step(site));
    assert_eq!(site_clock_action(site + 800, site, Precision::Microslot, false), Slew(-800));
    // Afterwards small differences slew (no backward jump), large step.
    assert_eq!(site_clock_action(site + 2_500, site, Precision::Rollover, true), Slew(-2_500));
    assert_eq!(site_clock_action(site - 700, site, Precision::Microslot, true), Slew(700));
    assert_eq!(site_clock_action(site + 200, site, Precision::Microslot, true), None);
    assert_eq!(site_clock_action(site + 45_000, site, Precision::Microslot, true), Step(site));
    // Minute precision cannot correct a few seconds.
    assert_eq!(site_clock_action(site + 40_000, site, Precision::Minute, false), None);
    assert_eq!(site_clock_action(site + 120_000, site, Precision::Minute, true), Step(site));
}
