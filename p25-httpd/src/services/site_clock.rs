//! Change 067: the site's time from the control channel (portable,
//! host-tested).
//!
//! A P25 control channel broadcasts SYNC_BCST (TSBK opcode 0x30) several
//! times a second: UTC date, hour and minute, a count of 7.5 ms
//! micro-slots since the minute rollover, a local time offset, and two
//! flags. "Not locked to an external reference" (no GPS: the site's own
//! clock) does not stop us using it: the site's radios show that time.
//! "Micro-slots not locked to the minute" means the micro-slot count
//! runs free, so only the minute is known; the second is then found by
//! watching for the minute to roll over (the first broadcast of a new
//! minute arrives within one broadcast interval of hh:mm:00).
//!
//! `SiteClock` keeps an anchor (site time at a monotonic instant) from
//! those observations; the clock task steers the board clock to it when
//! the clock source is "site" (`ClockSource::Site`).

use crate::hardware::dibit_ring::mono_us;

/// Micro-slot length (ICD: 8000 per minute).
pub const MICROSLOT_US: u64 = 7_500;
/// Typical delay from a TSBK leaving the site to its decode here
/// (control ring delivery, change 054: p50 ~0.1 s).
pub const RX_DELAY_MS: u64 = 100;
/// Earliest plausible site time (2020-01-01); a site broadcasting
/// earlier has no clock.
pub const MIN_VALID_MS: u64 = 1_577_836_800_000;
/// Latest plausible site time (2100-01-01).
pub const MAX_VALID_MS: u64 = 4_102_444_800_000;
/// A rollover seen across a longer gap between broadcasts is too vague.
const MAX_ROLLOVER_GAP_MS: u64 = 5_000;

/// One decoded SYNC_BCST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiteSync {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hours: u8,
    pub minutes: u8,
    pub microslots: u16,
    pub microslot_locked: bool,
    /// The site's clock is locked to an external reference (GPS).
    pub ext_locked: bool,
    pub local_offset_min: Option<i16>,
}

impl SiteSync {
    /// UTC unix ms of the start of the broadcast minute, if a real date.
    pub fn minute_ms(&self) -> Option<u64> {
        let ms = civil_to_unix_ms(self.year, self.month, self.day, self.hours, self.minutes)?;
        (MIN_VALID_MS..MAX_VALID_MS).contains(&ms).then_some(ms)
    }
}

/// UTC unix ms of a civil date and time (proleptic Gregorian).
pub fn civil_to_unix_ms(year: u16, month: u8, day: u8, hours: u8, minutes: u8) -> Option<u64> {
    if !(1..=12).contains(&month) || day == 0 || hours > 23 || minutes > 59 {
        return None;
    }
    let dim = [31, if leap(year) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if day > dim[month as usize - 1] {
        return None;
    }
    // Days from civil (H. Hinnant).
    let y = year as i64 - if month <= 2 { 1 } else { 0 };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hours as i64 * 3_600 + minutes as i64 * 60;
    (secs >= 0).then(|| secs as u64 * 1_000)
}

fn leap(y: u16) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// How well the anchor knows the site time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Precision {
    /// Minute only (±30 s): no rollover seen yet, micro-slots free-running.
    Minute,
    /// From a minute rollover (± half the broadcast interval).
    Rollover,
    /// Minute + locked micro-slots (7.5 ms, plus the decode delay).
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
    /// A broadcast decoded now.
    pub fn observe(&mut self, s: SiteSync) {
        self.observe_at(s, mono_us() / 1_000);
    }

    /// A broadcast decoded at monotonic `mono_ms`.
    pub fn observe_at(&mut self, s: SiteSync, mono_ms: u64) {
        let Some(minute_ms) = s.minute_ms() else {
            return;
        };
        let rx_mono = mono_ms.saturating_sub(RX_DELAY_MS);
        let prev = self.last.replace((s, mono_ms));
        // An anchor that disagrees with this broadcast's minute is stale
        // (the site clock was set, or a replay moved to another day).
        if let Some(a) = self.anchor {
            let predicted = a.site_ms + rx_mono.saturating_sub(a.mono_ms);
            let slack = if a.precision == Precision::Minute { 30_000 } else { 2_000 };
            if predicted + slack < minute_ms || predicted > minute_ms + 60_000 + slack {
                self.anchor = None;
            }
        }
        if s.microslot_locked && s.microslots < 8_000 {
            self.anchor = Some(Anchor {
                site_ms: minute_ms + s.microslots as u64 * MICROSLOT_US / 1_000,
                mono_ms: rx_mono,
                precision: Precision::Microslot,
            });
            return;
        }
        // The first broadcast of a new minute: the rollover happened
        // between the previous broadcast and this one.
        if let Some((p, p_mono)) = prev {
            let rolled = p.minute_ms().is_some_and(|pm| pm + 60_000 == minute_ms);
            let gap = mono_ms.saturating_sub(p_mono);
            if rolled && gap <= MAX_ROLLOVER_GAP_MS {
                self.anchor = Some(Anchor {
                    site_ms: minute_ms,
                    mono_ms: p_mono.saturating_sub(RX_DELAY_MS) + gap / 2,
                    precision: Precision::Rollover,
                });
                return;
            }
        }
        if self.anchor.is_none() {
            self.anchor = Some(Anchor {
                site_ms: minute_ms + 30_000,
                mono_ms: rx_mono,
                precision: Precision::Minute,
            });
        }
    }

    /// Site time (UTC unix ms) at monotonic `mono_ms`.
    pub fn site_ms_at(&self, mono_ms: u64) -> Option<u64> {
        self.anchor.map(|a| a.site_ms + mono_ms.saturating_sub(a.mono_ms))
    }

    pub fn now_ms(&self) -> Option<u64> {
        self.site_ms_at(mono_us() / 1_000)
    }

    pub fn precision(&self) -> Option<Precision> {
        self.anchor.map(|a| a.precision)
    }

    /// The newest broadcast and when it was decoded (monotonic ms).
    pub fn last(&self) -> Option<(SiteSync, u64)> {
        self.last
    }
}

/// What the clock task does to the board clock.
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
/// After the first step, only a difference this large steps again
/// (smaller ones are slewed, so call times never jump backwards).
pub const STEP_MIN_MS: i64 = 30_000;

/// Steer the board clock (`board_ms`) to the site time (`site_ms`, same
/// instant). An unset clock (before 2020) is always stepped. The first
/// sub-minute correction since start (`stepped` false) steps; later ones
/// slew unless far off. Minute precision (±30 s) only fixes a clock more
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
#[path = "site_clock_tests.rs"]
mod tests;
