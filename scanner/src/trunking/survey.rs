//! What the live site's receive window carries over time. Each spectrometer frame (131 ms of
//! averaged power) marks the bins 10 dB above the frame's floor (its median: most of a window is
//! noise); the marks decay over ten minutes, so each bin's share of frames on is its recent
//! activity. Adjacent active bins
//! are one carrier, on the channel raster: steady ones (on nearly always) are control channels
//! and the like, intermittent ones carry calls, data or keep-alives. A DMR site looks for the
//! channels its plan lacks among the intermittent ones.

use serde::Serialize;

use crate::radio::plan::usable_bins;
use crate::services::discovery::probe::on_raster;

/// A bin this far above the frame's floor is on.
const ON_DB: f32 = 10.0;
/// Each frame keeps this much of the counts so far: a time constant of 4500 frames, about ten
/// minutes at the spectrometer's 7.6 frames a second.
const DECAY: f32 = 1.0 - 1.0 / 4500.0;
/// A carrier is listed once it was on this share of the frames...
const MIN_ON: f32 = 0.002;
/// ...and in this many frames (decayed).
const MIN_FRAMES_ON: f32 = 2.0;
/// On this share of the frames or more: steady.
const STEADY: f32 = 0.9;
/// Bins either side of the window's centre left out (the DC spur).
const DC_BINS: usize = 2;
/// Active bins this many apart are one carrier.
const GAP_BINS: usize = 2;
/// A carrier's middle: its bins this close to its strongest.
const TOP_DB: f32 = 6.0;

/// One carrier heard in the window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Carrier {
    pub freq_hz: u64,
    /// Share of the recent frames it was on, 0 to 100.
    pub on_pct: f32,
    /// Its strongest level above the floor, dB.
    pub peak_db: f32,
    pub steady: bool,
}

/// The survey of one window.
#[derive(Debug, Default)]
pub struct Survey {
    /// (centre, sample rate) of the window surveyed.
    window: Option<(u64, u32)>,
    frames: f32,
    on: Vec<f32>,
    peak: Vec<f32>,
    /// Frames read since the window was last moved.
    pub read: u64,
}

impl Survey {
    /// One frame of `window` (dB per bin, DC-centred).
    pub fn add(&mut self, db: &[f32], window: (u64, u32)) {
        if db.len() < 16 {
            return;
        }
        if self.window != Some(window) || self.on.len() != db.len() {
            *self = Survey { window: Some(window), on: vec![0.0; db.len()], peak: vec![f32::MIN; db.len()], ..Default::default() };
        }
        let floor = median(db);
        self.frames = self.frames * DECAY + 1.0;
        self.read += 1;
        for (i, &v) in db.iter().enumerate() {
            let above = v - floor;
            self.on[i] = self.on[i] * DECAY + if above >= ON_DB { 1.0 } else { 0.0 };
            self.peak[i] = self.peak[i].max(above);
        }
    }

    /// The window moved: what was counted describes another one.
    pub fn clear(&mut self) {
        *self = Survey::default();
    }

    pub fn window(&self) -> Option<(u64, u32)> {
        self.window
    }

    /// The carriers heard, the most active first.
    pub fn carriers(&self) -> Vec<Carrier> {
        let Some(window) = self.window else { return Vec::new() };
        let n = self.on.len();
        if n == 0 || self.frames <= 0.0 {
            return Vec::new();
        }
        let active = |i: usize| self.on[i] >= MIN_FRAMES_ON && self.on[i] / self.frames >= MIN_ON;
        let mut out: Vec<Carrier> = Vec::new();
        for (start, last) in groups(n, window.1, active) {
            let most = (start..=last).map(|b| self.on[b]).fold(0.0f32, f32::max);
            let (freq_hz, peak_db) = place(&self.peak, start, last, window);
            let share = most / self.frames;
            match out.iter_mut().find(|c| c.freq_hz == freq_hz) {
                Some(c) if c.on_pct >= share * 100.0 => {}
                Some(c) => *c = Carrier { freq_hz, on_pct: share * 100.0, peak_db, steady: share >= STEADY },
                None => out.push(Carrier { freq_hz, on_pct: share * 100.0, peak_db, steady: share >= STEADY }),
            }
        }
        out.sort_by(|a, b| b.on_pct.total_cmp(&a.on_pct));
        out
    }

    /// The intermittent carriers, the strongest first: where a DMR site's unmapped channels are
    /// looked for when none keyed up at the grant (a site's own repeaters stand well above
    /// another system's distant keep-alives).
    pub fn intermittent(&self) -> Vec<u64> {
        let mut c: Vec<Carrier> = self.carriers().into_iter().filter(|c| !c.steady).collect();
        c.sort_by(|a, b| b.peak_db.total_cmp(&a.peak_db));
        c.into_iter().map(|c| c.freq_hz).collect()
    }
}

/// The carriers on the air in `frames` (dB per bin, DC-centred) of `window`: where a bin stood
/// `ON_DB` over its frame's floor in any of them. Each with its level over the floor, the
/// strongest first.
pub fn on_air(frames: &[Vec<f32>], window: (u64, u32)) -> Vec<(u64, f32)> {
    let Some(n) = frames.first().map(Vec::len).filter(|&n| n >= 16) else { return Vec::new() };
    let mut level = vec![f32::MIN; n];
    for f in frames.iter().filter(|f| f.len() == n) {
        let floor = median(f);
        for (l, &v) in level.iter_mut().zip(f) {
            *l = l.max(v - floor);
        }
    }
    let mut out: Vec<(u64, f32)> = Vec::new();
    for (start, last) in groups(n, window.1, |i| level[i] >= ON_DB) {
        let (freq_hz, db) = place(&level, start, last, window);
        match out.iter_mut().find(|c| c.0 == freq_hz) {
            Some(c) => c.1 = c.1.max(db),
            None => out.push((freq_hz, db)),
        }
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

fn median(db: &[f32]) -> f32 {
    let mut v = db.to_vec();
    let mid = v.len() / 2;
    *v.select_nth_unstable_by(mid, f32::total_cmp).1
}

/// The runs of `active` bins of an `n`-bin frame, as (first, last): inside the window a lane can
/// receive, the DC spur left out, active bins `GAP_BINS` apart joined.
fn groups(n: usize, rate: u32, active: impl Fn(usize) -> bool) -> Vec<(usize, usize)> {
    let active = |i: usize| i.abs_diff(n / 2) > DC_BINS && active(i);
    let (lo, hi) = usable_bins(n, rate);
    let mut out = Vec::new();
    let mut i = lo;
    while i < hi {
        if !active(i) {
            i += 1;
            continue;
        }
        let start = i;
        let mut last = i;
        while i < hi && (active(i) || i - last <= GAP_BINS) {
            if active(i) {
                last = i;
            }
            i += 1;
        }
        out.push((start, last));
    }
    out
}

/// A carrier spanning bins `start..=last`: its frequency on the raster and its strongest
/// `level`. Its centre is the middle of its bins within `TOP_DB` of the strongest (a strong
/// carrier's skirts stand out as often as its middle, and a weaker neighbour within the gap must
/// not pull it aside).
fn place(level: &[f32], start: usize, last: usize, (centre_hz, rate): (u64, u32)) -> (u64, f32) {
    let n = level.len();
    let top = (start..=last).map(|b| level[b]).fold(f32::MIN, f32::max);
    let wide: Vec<usize> = (start..=last).filter(|&b| level[b] >= top - TOP_DB).collect();
    let middle = (wide[0] + wide[wide.len() - 1]) as f64 / 2.0;
    let offset = (middle - (n / 2) as f64) * f64::from(rate) / n as f64;
    (on_raster((centre_hz as f64 + offset).round() as u64), top)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 8 MSPS around 454.0 MHz in 1000 bins of 8 kHz.
    const W: (u64, u32) = (454_000_000, 8_000_000);
    const N: usize = 1000;

    fn bin(hz: u64) -> usize {
        (N as f64 / 2.0 + (hz as f64 - W.0 as f64) / 8_000.0).round() as usize
    }

    #[test]
    fn steady_and_intermittent_carriers_are_told_apart() {
        let mut s = Survey::default();
        let (cc, voice, blip) = (454_368_750, 451_087_500, 454_537_500);
        for k in 0..400 {
            let mut db = vec![-120.0f32; N];
            db[bin(cc)] = -60.0;
            if k % 10 < 3 {
                db[bin(voice)] = -75.0;
            }
            if k % 50 == 0 {
                db[bin(blip)] = -90.0;
            }
            s.add(&db, W);
        }
        let c = s.carriers();
        let find = |f: u64| c.iter().find(|x| x.freq_hz.abs_diff(f) <= 6_250).cloned();
        let cc_c = find(cc).expect("control channel");
        assert!(cc_c.steady && cc_c.on_pct > 99.0, "{cc_c:?}");
        let v = find(voice).expect("voice channel");
        assert!(!v.steady && (25.0..35.0).contains(&v.on_pct), "{v:?}");
        let b = find(blip).expect("keep-alive blips");
        assert!(!b.steady && b.on_pct < 5.0 && b.peak_db >= 29.0, "{b:?}");
        assert_eq!(s.intermittent().len(), 2, "{c:?}");
        assert_eq!(c[0].freq_hz.abs_diff(cc) <= 6_250, true, "the most active first");
    }

    #[test]
    fn a_carrier_several_bins_wide_is_placed_at_its_middle() {
        // 4096 bins of 1953 Hz, as on the unit: a 12.5 kHz channel spans about six.
        let (n, w) = (4096usize, (454_383_750u64, 8_000_000u32));
        let at = |hz: f64| (n as f64 / 2.0 + (hz - w.0 as f64) / (8e6 / n as f64)).round() as usize;
        let mut s = Survey::default();
        for _ in 0..50 {
            let mut db = vec![-120.0f32; n];
            for b in at(454_368_750.0 - 5_000.0)..=at(454_368_750.0 + 5_000.0) {
                db[b] = -60.0;
            }
            s.add(&db, w);
        }
        let c = s.carriers();
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].freq_hz, 454_368_750, "not an edge rounded to the next channel");
    }

    #[test]
    fn a_strong_carrier_is_placed_at_its_peak_not_pulled_by_its_skirts_or_a_neighbour() {
        // Unit A's spectrum around Clay Electric's control channel (454.36875 MHz; window at
        // 453.4125 MHz, 4096 bins of 1953 Hz), dB above the floor from bin 2533: a weaker signal
        // at about 454.381 MHz sits inside the gap. The middle of every bin on was 454.3715 MHz,
        // which rounded to 454.375.
        const PROFILE: [f32; 13] = [9.0, 23.0, 34.6, 42.4, 46.3, 46.9, 44.0, 37.1, 26.5, 15.3, 15.6, 16.1, 13.0];
        let (n, w) = (4096usize, (453_412_500u64, 8_000_000u32));
        let mut s = Survey::default();
        for _ in 0..50 {
            let mut db = vec![-120.0f32; n];
            for (i, v) in PROFILE.iter().enumerate() {
                db[2533 + i] = -120.0 + v;
            }
            s.add(&db, w);
        }
        let c = s.carriers();
        assert_eq!(c.iter().map(|c| c.freq_hz).collect::<Vec<_>>(), vec![454_368_750], "{c:?}");
    }

    #[test]
    fn a_channel_near_the_edge_a_lane_can_receive_is_surveyed() {
        // Clay Electric's LCN 6: 3.296 MHz below a window at 454.38375 MHz (a lane receives
        // ±3.6 MHz).
        let (n, w) = (4096usize, (454_383_750u64, 8_000_000u32));
        let at = |hz: f64| (n as f64 / 2.0 + (hz - w.0 as f64) / (8e6 / n as f64)).round() as usize;
        let mut s = Survey::default();
        for k in 0..100 {
            let mut db = vec![-120.0f32; n];
            if k % 4 == 0 {
                for b in at(451_087_500.0 - 5_000.0)..=at(451_087_500.0 + 5_000.0) {
                    db[b] = -70.0;
                }
                // Past the usable window: roll-off.
                db[at(450_500_000.0)] = -70.0;
            }
            s.add(&db, w);
        }
        assert_eq!(s.intermittent(), vec![451_087_500]);
    }

    #[test]
    fn what_is_on_the_air_in_a_few_frames_is_found_the_strongest_first() {
        let mut quiet = vec![-120.0f32; N];
        quiet[bin(454_368_750)] = -60.0;
        let mut keyed = quiet.clone();
        keyed[bin(451_087_500)] = -65.0;
        keyed[N / 2] = -40.0; // DC spur
        let on = on_air(&[quiet, keyed], W);
        assert_eq!(on.iter().map(|c| c.0).collect::<Vec<_>>(), vec![454_368_750, 451_087_500], "{on:?}");
        assert!((on[1].1 - 55.0).abs() < 0.1, "{on:?}");
        assert!(on_air(&[], W).is_empty());
    }

    #[test]
    fn noise_and_the_dc_spur_are_not_carriers_and_a_move_starts_afresh() {
        let mut s = Survey::default();
        for _ in 0..100 {
            let mut db = vec![-120.0f32; N];
            db[N / 2] = -40.0; // DC spur
            db[N / 2 + 1] = -45.0;
            s.add(&db, W);
        }
        assert!(s.carriers().is_empty(), "{:?}", s.carriers());
        let mut db = vec![-120.0f32; N];
        db[bin(452_000_000)] = -60.0;
        s.add(&db, (455_000_000, 8_000_000));
        assert_eq!((s.read, s.window()), (1, Some((455_000_000, 8_000_000))));
    }
}
