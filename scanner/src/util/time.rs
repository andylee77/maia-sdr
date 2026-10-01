//! Wall-clock and monotonic time.
//!
//! The board has no battery-backed clock: wall time starts in 1970 and jumps when the site, NTP
//! or a browser sets it. Durations and timeouts use `Instant`; wall time is only for display and
//! storage.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// A moment on both clocks: monotonic for durations and timeouts, wall for records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub mono: Instant,
    pub unix_ms: u64,
}

impl Stamp {
    pub fn now() -> Self {
        Stamp { mono: Instant::now(), unix_ms: unix_ms() }
    }
}

/// Wall-clock unix milliseconds (0 before 1970).
pub fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `YYYY-MM-DD hh:mm:ss` in UTC.
pub fn iso_utc(ms: u64) -> String {
    let s = ms / 1_000;
    let (days, secs) = (s / 86_400, s % 86_400);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", secs / 3_600, secs % 3_600 / 60, secs % 60)
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_utc_formats_known_moments() {
        assert_eq!(iso_utc(0), "1970-01-01 00:00:00");
        assert_eq!(iso_utc(951_782_400_000), "2000-02-29 00:00:00");
        assert_eq!(iso_utc(1_790_851_508_000), "2026-10-01 10:45:08");
    }
}
