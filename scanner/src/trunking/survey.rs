//! What the live site's receive window carries over time. Each spectrometer frame (131 ms of
//! averaged power) marks the bins 10 dB above the frame's floor (its median: most of a window is
//! noise); the marks decay over ten minutes, so each bin's share of frames on is its recent
//! activity. Adjacent active bins
//! are one carrier, on the channel raster: steady ones (on nearly always) are control channels
//! and the like, intermittent ones carry calls, data or keep-alives. A DMR site looks for the
//! channels its plan lacks among the intermittent ones.

use serde::Serialize;

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
        let mut sorted = db.to_vec();
        sorted.sort_by(f32::total_cmp);
        let floor = sorted[sorted.len() / 2];
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
        let Some((centre_hz, rate)) = self.window else { return Vec::new() };
        let n = self.on.len();
        if n == 0 || self.frames <= 0.0 {
            return Vec::new();
        }
        let active = |i: usize| {
            let dc = i.abs_diff(n / 2) <= DC_BINS;
            !dc && self.on[i] >= MIN_FRAMES_ON && self.on[i] / self.frames >= MIN_ON
        };
        let bin_hz = f64::from(rate) / n as f64;
        // The outer tenth on each side is the decimator's roll-off.
        let (lo, hi) = (n / 10, n - n / 10);
        let mut out: Vec<Carrier> = Vec::new();
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
            // A carrier spans several bins on alike: its centre is the middle of those on at least
            // half as often as its busiest.
            let most = (start..=last).map(|b| self.on[b]).fold(0.0f32, f32::max);
            let wide: Vec<usize> = (start..=last).filter(|&b| self.on[b] >= most / 2.0).collect();
            let centre = (wide[0] + wide[wide.len() - 1]) as f64 / 2.0;
            let share = most / self.frames;
            let offset = (centre - (n / 2) as f64) * bin_hz;
            let freq_hz = on_raster((centre_hz as f64 + offset).round() as u64);
            let peak_db = (start..=last).map(|b| self.peak[b]).fold(f32::MIN, f32::max);
            match out.iter_mut().find(|c| c.freq_hz == freq_hz) {
                Some(c) if c.on_pct >= share * 100.0 => {}
                Some(c) => *c = Carrier { freq_hz, on_pct: share * 100.0, peak_db, steady: share >= STEADY },
                None => out.push(Carrier { freq_hz, on_pct: share * 100.0, peak_db, steady: share >= STEADY }),
            }
        }
        out.sort_by(|a, b| b.on_pct.total_cmp(&a.on_pct));
        out
    }

    /// The intermittent carriers, the most active first: where a DMR site's unmapped channels are
    /// looked for.
    pub fn intermittent(&self) -> Vec<u64> {
        self.carriers().into_iter().filter(|c| !c.steady).map(|c| c.freq_hz).collect()
    }
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
