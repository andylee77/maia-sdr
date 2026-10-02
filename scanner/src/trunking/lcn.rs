//! Learning a DMR site's channel plan. A grant names a logical channel (LCN) whose downlink the
//! plan lacks: the call is followed on a candidate frequency, and the candidate is kept once the
//! voice link control there names the granted talkgroup (the traffic decoder reports the talking
//! radio only then). A candidate that fails is not tried again for that LCN.
//!
//! Candidates, in order: the control channel (a control repeater carries calls on its other
//! timeslot), the site's known channels, then carriers that keyed up in the receive window just
//! after the grant. One frequency belongs to one LCN, so a frequency another LCN holds is never
//! a candidate.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::hardware::p25core::Lane;
use crate::services::discovery::probe::on_raster;

/// From the trial's lane retune to the voice link control that confirms it: a call joined late
/// carries its link control in each superframe (360 ms).
pub const TRIAL: Duration = Duration::from_secs(4);
/// Frequencies this close are one channel.
const SAME_HZ: u64 = 3_000;
/// A bin this far above its usual level after a grant: a carrier keyed up.
const KEYED_DB: f32 = 10.0;
/// Background frames kept for the bins' usual level (one every `USUAL_EVERY`).
const USUAL_FRAMES: usize = 12;
pub const USUAL_EVERY: Duration = Duration::from_secs(5);
/// Keyed-up carriers tried at most per grant, the biggest rise first.
const KEYED_MAX: usize = 3;

/// A candidate being tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trial {
    pub lcn: u16,
    pub freq_hz: u64,
    pub tg: u32,
    /// The lane following it, once the follower took it.
    pub lane: Option<Lane>,
    pub since: Instant,
}

#[derive(Debug, Default)]
pub struct LcnLearner {
    /// The site's plan: configured and learned.
    plan: HashMap<u16, u64>,
    control_hz: u64,
    /// The site's known channels.
    known: Vec<u64>,
    rejected: HashSet<(u16, u64)>,
    trial: Option<Trial>,
}

impl LcnLearner {
    pub fn new(plan: HashMap<u16, u64>, control_hz: u64, known: Vec<u64>) -> Self {
        LcnLearner { plan, control_hz, known, ..Default::default() }
    }

    pub fn freq(&self, lcn: u16) -> Option<u64> {
        self.plan.get(&lcn).copied()
    }

    /// No trial under way.
    pub fn idle(&self) -> bool {
        self.trial.is_none()
    }

    /// The next frequency to try for `lcn`: the control channel, the known channels, then the
    /// carriers `keyed` up after its grant.
    pub fn candidate(&self, lcn: u16, keyed: &[u64]) -> Option<u64> {
        let taken = |f: u64| self.plan.iter().any(|(&l, &hz)| l != lcn && hz.abs_diff(f) <= SAME_HZ);
        std::iter::once(self.control_hz)
            .chain(self.known.iter().copied())
            .chain(keyed.iter().copied())
            .find(|&f| f > 0 && !taken(f) && !self.rejected.iter().any(|&(l, r)| l == lcn && r.abs_diff(f) <= SAME_HZ))
    }

    pub fn start(&mut self, lcn: u16, freq_hz: u64, tg: u32, now: Instant) {
        self.trial = Some(Trial { lcn, freq_hz, tg, lane: None, since: now });
    }

    /// The follower put the trial's call on `lane` (its retune starts the clock).
    pub fn following(&mut self, lane: Lane, now: Instant) {
        if let Some(t) = self.trial.as_mut() {
            t.lane = Some(lane);
            t.since = now;
        }
    }

    /// The follower did not take the trial's call: nothing was learned or ruled out.
    pub fn abandon(&mut self) {
        self.trial = None;
    }

    /// The voice link control on `lane` named its call's talkgroup: the trial's frequency is its
    /// LCN's.
    pub fn confirmed(&mut self, lane: Lane) -> Option<Trial> {
        let t = self.trial.filter(|t| t.lane == Some(lane))?;
        self.trial = None;
        self.plan.insert(t.lcn, t.freq_hz);
        Some(t)
    }

    /// A trial whose time ran out without the link control: its frequency is not its LCN's.
    pub fn expire(&mut self, now: Instant) -> Option<Trial> {
        let t = self.trial.filter(|t| now.saturating_duration_since(t.since) >= TRIAL)?;
        self.trial = None;
        self.rejected.insert((t.lcn, t.freq_hz));
        Some(t)
    }

    pub fn trial(&self) -> Option<Trial> {
        self.trial
    }
}

/// The receive window's bins at their usual level: the median of the last background frames,
/// while the window stays where they were read.
#[derive(Debug, Default)]
pub struct Usual {
    frames: VecDeque<Vec<f32>>,
    /// (centre, sample rate) the frames were read at.
    window: Option<(u64, u32)>,
}

impl Usual {
    pub fn push(&mut self, db: Vec<f32>, window: (u64, u32)) {
        if self.window != Some(window) || self.frames.front().is_some_and(|f| f.len() != db.len()) {
            self.frames.clear();
            self.window = Some(window);
        }
        if self.frames.len() == USUAL_FRAMES {
            self.frames.pop_front();
        }
        self.frames.push_back(db);
    }

    /// The window moved: the frames describe another one.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.window = None;
    }

    /// Each bin's median, once three frames of `window` were read.
    pub fn level(&self, window: (u64, u32)) -> Option<Vec<f32>> {
        if self.window != Some(window) || self.frames.len() < 3 {
            return None;
        }
        let n = self.frames[0].len();
        Some(
            (0..n)
                .map(|i| {
                    let mut v: Vec<f32> = self.frames.iter().map(|f| f[i]).collect();
                    v.sort_by(f32::total_cmp);
                    v[v.len() / 2]
                })
                .collect(),
        )
    }
}

/// Carriers that keyed up: where the frames `after` a grant (dB per bin, DC-centred, spanning
/// `sample_rate_hz` around `centre_hz`) stand `KEYED_DB` above the bins' `usual` level, away
/// from the window's edges, on the channel raster; the biggest rise first.
pub fn keyed_up(usual: &[f32], after: &[Vec<f32>], centre_hz: u64, sample_rate_hz: u32) -> Vec<u64> {
    let n = usual.len();
    if n < 3 || after.iter().any(|f| f.len() != n) || after.is_empty() {
        return Vec::new();
    }
    let rise: Vec<f32> = (0..n).map(|i| after.iter().map(|f| f[i]).fold(f32::MIN, f32::max) - usual[i]).collect();
    let bin_hz = f64::from(sample_rate_hz) / n as f64;
    // The outer tenth on each side is the decimator's roll-off.
    let (lo, hi) = (n / 10, n - n / 10);
    let mut peaks: Vec<(f32, u64)> = Vec::new();
    let mut i = lo;
    while i < hi {
        if rise[i] < KEYED_DB {
            i += 1;
            continue;
        }
        // One carrier: adjacent bins over the threshold; its centre is the middle of those within
        // 3 dB of its top (a channel spans several bins alike).
        let start = i;
        while i < hi && rise[i] >= KEYED_DB {
            i += 1;
        }
        let top = (start..i).map(|b| rise[b]).fold(f32::MIN, f32::max);
        let wide: Vec<usize> = (start..i).filter(|&b| rise[b] >= top - 3.0).collect();
        let middle = (wide[0] + wide[wide.len() - 1]) as f64 / 2.0;
        let offset = (middle - (n / 2) as f64) * bin_hz;
        peaks.push((top, on_raster((centre_hz as f64 + offset).round() as u64)));
    }
    peaks.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut out: Vec<u64> = Vec::new();
    for (_, f) in peaks {
        if !out.iter().any(|&o| o.abs_diff(f) <= SAME_HZ) {
            out.push(f);
        }
    }
    out.truncate(KEYED_MAX);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CC: u64 = 454_368_750;

    #[test]
    fn the_control_channel_then_known_then_keyed_up_carriers_are_tried() {
        let mut l = LcnLearner::new(HashMap::new(), CC, vec![454_537_500]);
        assert_eq!(l.candidate(5, &[451_087_500]), Some(CC), "a control repeater's other timeslot");
        let now = Instant::now();
        l.start(5, CC, 87925, now);
        l.following(Lane::One, now);
        assert_eq!(l.confirmed(Lane::One).map(|t| (t.lcn, t.freq_hz)), Some((5, CC)));
        assert_eq!(l.freq(5), Some(CC));
        assert_eq!(l.candidate(6, &[451_087_500]), Some(454_537_500), "the control channel is LCN 5's now");
        l.start(6, 454_537_500, 87921, now);
        l.following(Lane::One, now);
        assert_eq!(l.expire(now + TRIAL).map(|t| t.freq_hz), Some(454_537_500));
        assert_eq!(l.candidate(6, &[451_087_500]), Some(451_087_500), "a failed candidate is not tried again");
        assert_eq!(l.candidate(6, &[]), None);
    }

    #[test]
    fn a_trial_is_confirmed_only_on_its_own_lane_and_abandoned_when_not_followed() {
        let mut l = LcnLearner::new(HashMap::new(), CC, Vec::new());
        let now = Instant::now();
        l.start(5, CC, 1, now);
        assert_eq!(l.confirmed(Lane::One), None, "not followed yet");
        l.following(Lane::One, now);
        assert_eq!(l.confirmed(Lane::Two), None);
        assert!(l.expire(now + Duration::from_secs(1)).is_none(), "still in time");
        l.abandon();
        assert!(l.idle());
        assert_eq!(l.candidate(5, &[]), Some(CC), "abandoned rules nothing out");
    }

    #[test]
    fn a_carrier_that_keyed_up_is_found_on_the_raster() {
        // 8 MSPS around 454.0 MHz, 1000 bins of 8 kHz; the usual floor and a steady carrier.
        let (n, centre, rate) = (1000, 454_000_000u64, 8_000_000u32);
        let mut usual = vec![-120.0f32; n];
        usual[546] = -60.0; // 454.368 MHz: the control channel, always there
        let mut after = usual.clone();
        // 451.0875 MHz is 2.9125 MHz below the centre: bin 500 - 364.06.
        after[136] = -70.0;
        after[137] = -80.0;
        after[546] = -60.0;
        let found = keyed_up(&usual, &[after], centre, rate);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].abs_diff(451_087_500) <= 6_250, "{found:?}");
        assert!(keyed_up(&usual, &[usual.clone()], centre, rate).is_empty(), "nothing rose");
    }

    #[test]
    fn the_usual_level_is_a_median_of_one_window() {
        let mut u = Usual::default();
        let w = (454_000_000, 8_000_000);
        for v in [-120.0, -60.0, -121.0] {
            u.push(vec![v; 4], w);
        }
        assert_eq!(u.level(w), Some(vec![-120.0; 4]), "a carrier in one frame does not count");
        assert_eq!(u.level((455_000_000, 8_000_000)), None);
        u.push(vec![-100.0; 4], (455_000_000, 8_000_000));
        assert_eq!(u.level(w), None, "the window moved");
    }
}
