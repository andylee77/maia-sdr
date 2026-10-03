//! Each radio's speech level over its recent transmissions, for the AGC to start its next one at
//! (`agc`): the median of its last 5, none older than 30 minutes, so a change at a radio (another
//! dispatcher at a console) is followed within a few transmissions. Shared by the lanes.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const KEEP: usize = 5;
const MAX_AGE: Duration = Duration::from_secs(30 * 60);
/// Radios kept before those not heard within `MAX_AGE` are dropped.
const RADIOS: usize = 4096;

#[derive(Debug, Default)]
pub struct LevelBook {
    radios: HashMap<u32, VecDeque<(Instant, f32)>>,
}

impl LevelBook {
    /// A transmission of `radio` ended at `at` with this speech level (RMS).
    pub fn note(&mut self, radio: u32, level: f32, at: Instant) {
        let q = self.radios.entry(radio).or_default();
        q.push_back((at, level));
        while q.len() > KEEP {
            q.pop_front();
        }
        if self.radios.len() > RADIOS {
            self.radios.retain(|_, q| q.back().is_some_and(|(t, _)| at.saturating_duration_since(*t) <= MAX_AGE));
        }
    }

    /// The median of the radio's recent levels; none when it has none.
    pub fn level(&self, radio: u32, now: Instant) -> Option<f32> {
        let mut v: Vec<f32> = self
            .radios
            .get(&radio)?
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) <= MAX_AGE)
            .map(|(_, l)| *l)
            .collect();
        v.sort_by(f32::total_cmp);
        let n = v.len();
        match n {
            0 => None,
            _ if n % 2 == 1 => Some(v[n / 2]),
            _ => Some((v[n / 2 - 1] + v[n / 2]) / 2.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_median_of_the_last_five_recent_ones() {
        let t0 = Instant::now();
        let mut book = LevelBook::default();
        assert_eq!(book.level(1013, t0), None);
        for (i, l) in [100.0, 2000.0, 300.0, 400.0, 500.0, 600.0].into_iter().enumerate() {
            book.note(1013, l, t0 + Duration::from_secs(i as u64));
        }
        // The first (100) is gone: 2000, 300, 400, 500, 600.
        assert_eq!(book.level(1013, t0 + Duration::from_secs(10)), Some(500.0));
        // 31 minutes on, the first two have aged out: 400, 500, 600 ... then none.
        let later = t0 + Duration::from_secs(2 + 30 * 60 + 1);
        assert_eq!(book.level(1013, later), Some(500.0));
        assert_eq!(book.level(1013, t0 + Duration::from_secs(6 + 30 * 60)), None);
        book.note(1014, 250.0, t0);
        book.note(1014, 350.0, t0);
        assert_eq!(book.level(1014, t0), Some(300.0));
    }
}
