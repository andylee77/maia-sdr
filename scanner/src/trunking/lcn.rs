//! Learning a DMR site's channel plan. The LCNs its grants name are the rows to fill (the site
//! is learning while one has no frequency); the channels a lane heard name this site's network
//! and site are the frequencies to fill them with. A grant naming an LCN the plan lacks is
//! followed on a candidate frequency, kept once the voice link control there names the granted
//! talkgroup (the traffic decoder reports the talking radio only then). A candidate that fails,
//! or names another network or site, is not tried again for that LCN.
//!
//! Candidates, in order:
//! 1. the site's own channels that keyed up just after the grant (a traffic repeater keys up for
//!    the call it was granted);
//! 2. other carriers that keyed up;
//! 3. the site's own channels on the air meanwhile;
//! 4. the control channel (a control repeater carries calls on its other timeslot, and is always
//!    on);
//! 5. the site's other own channels;
//! 6. its known channels;
//! 7. the window's intermittent carriers, the strongest first.
//!
//! One frequency belongs to one LCN, so a frequency another LCN holds is never a candidate, nor
//! is one that named another site.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::hardware::p25core::Lane;
use crate::radio::plan::usable_bins;
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

/// A DMR site's identity as configured: what its own channels name in their CACH.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteCode {
    /// tiny, small, large or huge.
    pub model: Option<String>,
    pub network: Option<u32>,
    pub site: Option<u32>,
    pub colour_code: Option<u8>,
}

impl SiteCode {
    /// Whether a channel naming `model`, `network` and `site` (and `colour_code`, once heard) is
    /// this site's: every field both know agrees. `None` while the site's network and site are
    /// not configured.
    pub fn owns(&self, model: &str, network: u32, site: u32, colour_code: Option<u8>) -> Option<bool> {
        if self.network.is_none() && self.site.is_none() {
            return None;
        }
        let colour = match (self.colour_code, colour_code) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        Some(
            self.model.as_deref().is_none_or(|m| m.eq_ignore_ascii_case(model))
                && self.network.is_none_or(|n| n == network)
                && self.site.is_none_or(|s| s == site)
                && colour,
        )
    }
}

/// What the spectrum showed around a grant.
#[derive(Debug, Default, Clone, Copy)]
pub struct Around<'a> {
    /// Carriers that keyed up after it, the biggest rise first.
    pub keyed: &'a [u64],
    /// Carriers on the air meanwhile.
    pub on: &'a [u64],
    /// The window's intermittent carriers, the strongest first.
    pub intermittent: &'a [u64],
}

#[derive(Debug, Default)]
pub struct LcnLearner {
    /// The site's plan: configured and learned.
    plan: HashMap<u16, u64>,
    control_hz: u64,
    /// The site's known channels.
    known: Vec<u64>,
    /// Channels heard naming this site's network and site, and another's.
    own: Vec<u64>,
    foreign: Vec<u64>,
    /// Every LCN a grant named.
    granted: BTreeSet<u16>,
    rejected: HashSet<(u16, u64)>,
    trial: Option<Trial>,
}

fn near(list: &[u64], f: u64) -> bool {
    list.iter().any(|&x| x.abs_diff(f) <= SAME_HZ)
}

impl LcnLearner {
    pub fn new(plan: HashMap<u16, u64>, control_hz: u64, known: Vec<u64>) -> Self {
        LcnLearner { plan, control_hz, known, ..Default::default() }
    }

    /// A grant named `lcn`.
    pub fn granted(&mut self, lcn: u16) {
        self.granted.insert(lcn);
    }

    /// A granted LCN has no frequency yet.
    pub fn learning(&self) -> bool {
        self.granted.iter().any(|l| !self.plan.contains_key(l))
    }

    /// A lane heard the channel on `freq_hz` name this site (`own`) or another.
    pub fn heard(&mut self, freq_hz: u64, own: bool) {
        let (add, other) = if own { (&mut self.own, &mut self.foreign) } else { (&mut self.foreign, &mut self.own) };
        other.retain(|&f| f.abs_diff(freq_hz) > SAME_HZ);
        if !near(add, freq_hz) {
            add.push(freq_hz);
        }
    }

    pub fn freq(&self, lcn: u16) -> Option<u64> {
        self.plan.get(&lcn).copied()
    }

    /// Where a grant of `tg` on `lcn` is followed: the plan's frequency, else the trial's when
    /// the grant is its call's (the control channel repeats a grant while the call stands).
    pub fn freq_for(&self, lcn: u16, tg: u32) -> Option<u64> {
        self.freq(lcn).or_else(|| self.trial.filter(|t| t.lcn == lcn && t.tg == tg).map(|t| t.freq_hz))
    }

    /// No trial under way.
    pub fn idle(&self) -> bool {
        self.trial.is_none()
    }

    /// The next frequency to try for `lcn`, in the order the module header gives.
    pub fn candidate(&self, lcn: u16, around: &Around) -> Option<u64> {
        let taken = |f: u64| self.plan.iter().any(|(&l, &hz)| l != lcn && hz.abs_diff(f) <= SAME_HZ);
        let ruled_out = |f: u64| self.rejected.iter().any(|&(l, r)| l == lcn && r.abs_diff(f) <= SAME_HZ);
        // An own channel by the frequency the lane heard it on.
        let own_keyed = around.keyed.iter().filter_map(|&k| self.own.iter().copied().find(|&o| o.abs_diff(k) <= SAME_HZ));
        let own_on = self.own.iter().copied().filter(|&o| near(around.on, o));
        own_keyed
            .chain(around.keyed.iter().copied())
            .chain(own_on)
            .chain(std::iter::once(self.control_hz))
            .chain(self.own.iter().copied())
            .chain(self.known.iter().copied())
            .chain(around.intermittent.iter().copied())
            .find(|&f| f > 0 && !taken(f) && !ruled_out(f) && !near(&self.foreign, f))
    }

    /// The trial's frequency, being followed, named another network or site: not its LCN's.
    pub fn wrong_site(&mut self, freq_hz: u64) -> Option<Trial> {
        let t = self.trial.filter(|t| t.lane.is_some() && t.freq_hz == freq_hz)?;
        self.trial = None;
        self.rejected.insert((t.lcn, t.freq_hz));
        Some(t)
    }

    /// The trial's frequency, while `tg`'s call is on it.
    pub fn trying(&self, freq_hz: u64, tg: u32) -> bool {
        self.trial.is_some_and(|t| t.freq_hz == freq_hz && t.tg == tg)
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
/// `sample_rate_hz` around `centre_hz`) stand `KEYED_DB` above the bins' `usual` level, inside
/// the window a lane can receive, on the channel raster; the biggest rise first.
pub fn keyed_up(usual: &[f32], after: &[Vec<f32>], centre_hz: u64, sample_rate_hz: u32) -> Vec<u64> {
    let n = usual.len();
    if n < 3 || after.iter().any(|f| f.len() != n) || after.is_empty() {
        return Vec::new();
    }
    let rise: Vec<f32> = (0..n).map(|i| after.iter().map(|f| f[i]).fold(f32::MIN, f32::max) - usual[i]).collect();
    let bin_hz = f64::from(sample_rate_hz) / n as f64;
    let (lo, hi) = usable_bins(n, sample_rate_hz);
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
    fn what_keyed_up_then_the_control_channel_known_and_intermittent_carriers_are_tried() {
        const BLIP: u64 = 454_400_000;
        let around = |keyed, intermittent| Around { keyed, on: &[], intermittent };
        let mut l = LcnLearner::new(HashMap::new(), CC, vec![454_537_500]);
        assert_eq!(l.candidate(6, &around(&[451_087_500], &[BLIP])), Some(451_087_500), "the carrier that keyed up at the grant");
        assert_eq!(l.candidate(5, &around(&[], &[BLIP])), Some(CC), "nothing keyed up: a control repeater's other timeslot");
        let now = Instant::now();
        l.start(5, CC, 87925, now);
        l.following(Lane::One, now);
        assert_eq!(l.confirmed(Lane::One).map(|t| (t.lcn, t.freq_hz)), Some((5, CC)));
        assert_eq!(l.freq(5), Some(CC));
        assert_eq!(l.candidate(6, &around(&[CC], &[BLIP])), Some(454_537_500), "the control channel is LCN 5's now");
        l.start(6, 454_537_500, 87921, now);
        l.following(Lane::One, now);
        assert_eq!(l.expire(now + TRIAL).map(|t| t.freq_hz), Some(454_537_500));
        assert_eq!(l.candidate(6, &around(&[], &[BLIP])), Some(BLIP), "a failed candidate is not tried again");
        assert_eq!(l.candidate(6, &around(&[], &[])), None);
    }

    #[test]
    fn the_sites_own_channels_fill_its_lcns_and_another_sites_never_do() {
        const OWN: u64 = 451_087_500;
        const OTHER: u64 = 453_437_500;
        let mut l = LcnLearner::new(HashMap::from([(5, CC)]), CC, Vec::new());
        l.granted(5);
        assert!(!l.learning(), "every LCN granted has its frequency");
        l.granted(6);
        assert!(l.learning());
        l.heard(OWN, true);
        l.heard(OTHER, false);
        // One LCN to fill and one own channel left for it.
        let quiet = Around { intermittent: &[OTHER], ..Default::default() };
        assert_eq!(l.candidate(6, &quiet), Some(OWN));
        // Another site's carrier keying up is never tried; an own one that keyed up comes first,
        // by the frequency the lane heard it on.
        assert_eq!(l.candidate(6, &Around { keyed: &[OTHER, OWN + 1_000], ..Default::default() }), Some(OWN));
        // Its own channels on the air come before the control channel.
        let mut m = LcnLearner::new(HashMap::new(), CC, Vec::new());
        m.heard(OWN, true);
        assert_eq!(m.candidate(6, &Around { on: &[OWN, CC], ..Default::default() }), Some(OWN));
        assert_eq!(m.candidate(5, &Around { on: &[CC], ..Default::default() }), Some(CC));
        // A trial whose channel names another site is ruled out at once.
        let now = Instant::now();
        l.start(7, 452_425_000, 87921, now);
        l.following(Lane::One, now);
        assert_eq!(l.wrong_site(452_500_000), None, "another frequency");
        assert_eq!(l.wrong_site(452_425_000).map(|t| t.freq_hz), Some(452_425_000));
        assert!(l.idle());
        assert_eq!(l.candidate(7, &Around { keyed: &[452_425_000], ..Default::default() }), Some(OWN));
    }

    #[test]
    fn a_channel_is_the_sites_when_its_network_site_and_colour_code_are() {
        let code = SiteCode { model: Some("small".into()), network: Some(0), site: Some(2), colour_code: Some(0) };
        assert_eq!(code.owns("SMALL", 0, 2, Some(0)), Some(true));
        assert_eq!(code.owns("SMALL", 0, 2, None), Some(true), "its colour code not heard yet");
        assert_eq!(code.owns("SMALL", 0, 3, Some(0)), Some(false), "a neighbour site");
        assert_eq!(code.owns("SMALL", 115, 13, None), Some(false), "another network");
        assert_eq!(code.owns("SMALL", 0, 2, Some(1)), Some(false));
        assert_eq!(SiteCode::default().owns("SMALL", 0, 2, None), None, "nothing to compare with");
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
        assert_eq!(l.candidate(5, &Around::default()), Some(CC), "abandoned rules nothing out");
    }

    #[test]
    fn a_repeated_grant_is_followed_where_its_trial_is() {
        let mut l = LcnLearner::new(HashMap::new(), CC, Vec::new());
        l.start(6, CC, 87921, Instant::now());
        assert_eq!(l.freq_for(6, 87921), Some(CC), "the same call, not another one with no channel");
        assert_eq!(l.freq_for(6, 87925), None, "another talkgroup's call is not the trial's");
        assert_eq!(l.freq_for(5, 87921), None);
        l.following(Lane::One, Instant::now());
        l.confirmed(Lane::One);
        assert_eq!(l.freq_for(6, 87925), Some(CC), "learned: every call on it");
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
    fn a_carrier_near_the_edge_a_lane_can_receive_is_found() {
        // Clay Electric's LCN 6, 451.0875 MHz: 3.296 MHz below a window at 454.38375 MHz (8 MSPS,
        // a lane receives ±3.6 MHz).
        let (n, centre, rate) = (4096usize, 454_383_750u64, 8_000_000u32);
        let bin = |hz: f64| (n as f64 / 2.0 + (hz - centre as f64) / (f64::from(rate) / n as f64)).round() as usize;
        let usual = vec![-120.0f32; n];
        let mut after = usual.clone();
        for b in bin(451_087_500.0 - 5_000.0)..=bin(451_087_500.0 + 5_000.0) {
            after[b] = -70.0;
        }
        // Past the usable window: no lane could follow it.
        after[bin(450_500_000.0)] = -60.0;
        assert_eq!(keyed_up(&usual, &[after], centre, rate), vec![451_087_500]);
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
