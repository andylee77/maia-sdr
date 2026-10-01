//! The site's time from the control channel. A P25 control channel broadcasts SYNC_BCST several
//! times a second: the UTC date, hour and minute, a count of 7.5 ms micro-slots since the minute,
//! and whether that count is locked to the minute. A site not locked to GPS still serves: its
//! radios show that time. With the micro-slots free-running only the minute is known, and the
//! second comes from watching the minute roll over (the first broadcast of a new minute arrives
//! within one broadcast interval of hh:mm:00).
//!
//! `SiteClock` keeps an anchor (site time at a monotonic instant) from those observations;
//! `site_clock_action` says how to steer the board clock to it.

use std::sync::OnceLock;
use std::time::Instant;

use crate::protocol::events::SiteSync;

/// Micro-slot length (8000 a minute).
pub const MICROSLOT_US: u64 = 7_500;
/// Typical delay from a TSBK leaving the site to its decode here.
pub const RX_DELAY_MS: u64 = 100;
/// Earliest plausible site time (2020-01-01); a site broadcasting earlier has no clock.
pub const MIN_VALID_MS: u64 = 1_577_836_800_000;
/// Latest plausible site time (2100-01-01).
pub const MAX_VALID_MS: u64 = 4_102_444_800_000;
/// A rollover seen across a longer gap between broadcasts is too vague.
const MAX_ROLLOVER_GAP_MS: u64 = 5_000;
/// With no broadcast for this long the site time is not used (the site was lost or switched,
/// or a replay ended): an old anchor carried forward must not set the clock.
pub const STALE_MS: u64 = 120_000;

/// Milliseconds on a monotonic clock that started with the process.
pub fn mono_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// UTC unix ms of the start of the broadcast minute, if a real date.
pub fn minute_ms(s: &SiteSync) -> Option<u64> {
    let ms = civil_to_unix_ms(s.year, s.month, s.day, s.hours, s.minutes)?;
    (MIN_VALID_MS..MAX_VALID_MS).contains(&ms).then_some(ms)
}

/// UTC unix ms of a civil date and time (proleptic Gregorian).
pub fn civil_to_unix_ms(year: u16, month: u8, day: u8, hours: u8, minutes: u8) -> Option<u64> {
    if !(1..=12).contains(&month) || day == 0 || hours > 23 || minutes > 59 {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let dim = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if day > dim[month as usize - 1] {
        return None;
    }
    // Days from civil (H. Hinnant).
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + i64::from(hours) * 3_600 + i64::from(minutes) * 60;
    (secs >= 0).then(|| secs as u64 * 1_000)
}

/// How well the anchor knows the site time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Precision {
    /// Minute only (±30 s): no rollover seen yet, micro-slots free-running.
    Minute,
    /// From a minute rollover (± half the broadcast interval).
    Rollover,
    /// Minute and locked micro-slots (7.5 ms, plus the decode delay).
    Microslot,
}

impl Precision {
    pub fn as_str(self) -> &'static str {
        match self {
            Precision::Minute => "minute",
            Precision::Rollover => "second",
            Precision::Microslot => "precise",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    site_ms: u64,
    mono_ms: u64,
    precision: Precision,
}

/// The site time as far as the broadcasts tell it.
#[derive(Debug, Default, Clone)]
pub struct SiteClock {
    last: Option<(SiteSync, u64)>,
    anchor: Option<Anchor>,
}

impl SiteClock {
    pub fn observe(&mut self, s: SiteSync) {
        self.observe_at(s, mono_ms());
    }

    /// A broadcast decoded at monotonic `mono_ms`.
    pub fn observe_at(&mut self, s: SiteSync, mono_ms: u64) {
        let Some(minute) = minute_ms(&s) else { return };
        let rx_mono = mono_ms.saturating_sub(RX_DELAY_MS);
        let prev = self.last.replace((s, mono_ms));
        // An anchor that disagrees with this broadcast's minute is stale (the site clock was
        // set, or a replay moved to another day).
        if let Some(a) = self.anchor {
            let predicted = a.site_ms + rx_mono.saturating_sub(a.mono_ms);
            let slack = if a.precision == Precision::Minute { 30_000 } else { 2_000 };
            if predicted + slack < minute || predicted > minute + 60_000 + slack {
                self.anchor = None;
            }
        }
        if s.microslot_locked && s.microslots < 8_000 {
            self.anchor = Some(Anchor {
                site_ms: minute + u64::from(s.microslots) * MICROSLOT_US / 1_000,
                mono_ms: rx_mono,
                precision: Precision::Microslot,
            });
            return;
        }
        // The first broadcast of a new minute: the rollover was between the previous one and
        // this one.
        if let Some((p, p_mono)) = prev {
            let rolled = minute_ms(&p).is_some_and(|pm| pm + 60_000 == minute);
            let gap = mono_ms.saturating_sub(p_mono);
            if rolled && gap <= MAX_ROLLOVER_GAP_MS {
                self.anchor = Some(Anchor {
                    site_ms: minute,
                    mono_ms: p_mono.saturating_sub(RX_DELAY_MS) + gap / 2,
                    precision: Precision::Rollover,
                });
                return;
            }
        }
        if self.anchor.is_none() {
            self.anchor = Some(Anchor { site_ms: minute + 30_000, mono_ms: rx_mono, precision: Precision::Minute });
        }
    }

    /// Site time (UTC unix ms) at monotonic `mono_ms`, while the site is still heard.
    pub fn site_ms_at(&self, mono_ms: u64) -> Option<u64> {
        let (_, heard) = self.last?;
        if mono_ms.saturating_sub(heard) > STALE_MS {
            return None;
        }
        self.anchor.map(|a| a.site_ms + mono_ms.saturating_sub(a.mono_ms))
    }

    /// The site time now and how well it is known.
    pub fn now(&self) -> Option<(u64, Precision)> {
        Some((self.site_ms_at(mono_ms())?, self.precision()?))
    }

    pub fn precision(&self) -> Option<Precision> {
        self.anchor.map(|a| a.precision)
    }
}

/// What to do to the board clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockAction {
    None,
    /// Set it to this unix ms (a jump).
    Step(u64),
    /// Slew it by this many ms (gradual; the clock never jumps).
    Slew(i64),
}

/// Board clock differences below this are left alone.
pub const SLEW_MIN_MS: i64 = 500;
/// After the first step, only a difference this large steps again (smaller ones are slewed, so
/// times never jump backwards).
pub const STEP_MIN_MS: i64 = 30_000;

/// Steer the board clock (`board_ms`) to the site time (`site_ms`, same instant). An unset clock
/// (before 2020) is always stepped. The first sub-minute correction since start (`stepped`
/// false) steps; later ones slew unless far off. Minute precision (±30 s) only fixes a clock more
/// than 90 s off.
pub fn site_clock_action(board_ms: u64, site_ms: u64, precision: Precision, stepped: bool) -> ClockAction {
    if board_ms < MIN_VALID_MS {
        return ClockAction::Step(site_ms);
    }
    let diff = site_ms as i64 - board_ms as i64;
    match precision {
        Precision::Minute if diff.abs() > 90_000 => ClockAction::Step(site_ms),
        Precision::Minute => ClockAction::None,
        _ if diff.abs() > STEP_MIN_MS || (!stepped && diff.abs() > 1_000) => ClockAction::Step(site_ms),
        _ if diff.abs() > SLEW_MIN_MS => ClockAction::Slew(diff),
        _ => ClockAction::None,
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(minute_ms(&SiteSync { year: 2000, ..sync(0, 0, 0, true) }), None);
    }

    #[test]
    fn locked_microslots_give_the_time_directly() {
        let mut c = SiteClock::default();
        // 08:42 and 2000 micro-slots (15 s), decoded at mono 10 s.
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
        // Broadcasts every 200 ms; 08:43 first seen at mono 30.4 s: the rollover was between
        // 30.2 and 30.4 s.
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
    fn a_site_no_longer_heard_gives_no_time() {
        let mut c = SiteClock::default();
        c.observe_at(sync(8, 42, 2000, true), 10_000);
        assert!(c.site_ms_at(10_000 + STALE_MS).is_some());
        assert_eq!(c.site_ms_at(10_001 + STALE_MS), None);
        // Heard again: the time is back.
        c.observe_at(sync(8, 44, 2000, true), 130_000);
        assert!(c.site_ms_at(131_000).is_some());
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
        // Afterwards small differences slew (no backward jump), large ones step.
        assert_eq!(site_clock_action(site + 2_500, site, Precision::Rollover, true), Slew(-2_500));
        assert_eq!(site_clock_action(site - 700, site, Precision::Microslot, true), Slew(700));
        assert_eq!(site_clock_action(site + 200, site, Precision::Microslot, true), None);
        assert_eq!(site_clock_action(site + 45_000, site, Precision::Microslot, true), Step(site));
        // Minute precision cannot correct a few seconds.
        assert_eq!(site_clock_action(site + 40_000, site, Precision::Minute, false), None);
        assert_eq!(site_clock_action(site + 120_000, site, Precision::Minute, true), Step(site));
    }
}
