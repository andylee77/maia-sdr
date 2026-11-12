//! The bursts in a channel: each transmission of a device, from its first millisecond over the
//! noise to its last, with where it was in the channel and how strong.
//!
//! The channel's spectrum is read a millisecond at a time (four 64-point transforms averaged:
//! 3.9 kHz bins). Each bin's floor is its median over the last quarter second, so a burst does
//! not raise it and a carrier that stays joins it within a second. A step whose power stands
//! 9 dB over the floor in a narrow run of bins is a device on the air; one that rises all over
//! the channel is a broadband click, counted and left out. Steps at one frequency are one burst
//! until 200 ms pass without it, so the repeats of a message stay together. Two runs are read in
//! a step, so two devices on the air at once are two bursts when they are a run apart; closer
//! (within 10 kHz) they are one burst while both send. A signal wider than a run
//! (frequency-shift keying of 20 kHz or more) is two bursts, one at each of its tones.
//!
//! The limits below are held against unit A's captures at 433.92 MHz (doc/changes/109): four
//! devices in forty seconds, the weakest 12 dB over the floor, under about 130 broadband clicks
//! a second.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::channel::RATE_HZ;

/// Points of a transform, and transforms averaged into one step.
const POINTS: usize = 64;
const AVERAGED: usize = 4;
/// Channel samples a step: 1.024 ms.
pub const STEP: usize = POINTS * AVERAGED;
const BIN_HZ: f32 = RATE_HZ as f32 / POINTS as f32;
/// The bins read, either side of the centre: 98 kHz, a little past the channel's pass band.
const HALF: usize = 25;
const BINS: usize = 2 * HALF + 1;
/// Steps a bin's floor is the median of.
const FLOOR_STEPS: usize = 256;
/// A new median's share of the floor.
const FLOOR_GAIN: f32 = 0.3;
/// A bin this far over its floor is on (9 dB): with the four transforms averaged, noise alone
/// does it about once in five hours.
const ON: f32 = 7.94;
/// The run of bins a burst is read in: 35 kHz.
const RUN: usize = 9;
/// The mean of noise over its median, four transforms averaged.
const NOISE_MEAN: f32 = 1.089;
/// A second run in the same step is a signal of its own when its peak is within this of the
/// first's (10 dB): lower, it may be the first one's skirts.
const SECOND: f32 = 0.1;
/// With a device on, the bins outside the runs stay at their floors: they average under this
/// many times them (noise alone averages 1.09). Over it the step is a broadband click...
const QUIET: f32 = 1.3;
/// ...unless a run holds a signal so strong that its own skirts lift the rest: its peak this far
/// over what is outside (15 dB). In unit A's captures a click's strongest bin is 5 dB over the
/// rest (14 dB at most in a hundred), a device's 30 dB (17 dB at least in a hundred).
const SKIRTS: f32 = 31.6;
/// A burst's width is read where its spectrum is within this of its strongest bin (10 dB).
const WIDTH_UNDER: f32 = 0.1;
/// A step joins the burst this close to it, in bins (9.8 kHz).
const SAME: f32 = 2.5;
/// Steps without the burst that end it: 200 ms.
const HANG: u64 = 195;
/// A burst has at least this many steps on, and this share of its length.
const MIN_ON: u32 = 8;
const MIN_SHARE: f32 = 0.15;
/// A burst is cut at this length (3 s): a carrier that stays is in the floor long before.
const MAX_STEPS: u64 = 2930;

/// A burst that ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Burst {
    /// Its first and one past its last channel sample, counted from the detector's start.
    pub start: u64,
    pub end: u64,
    /// Its centre from the channel's, by power.
    pub offset_hz: f32,
    /// How wide it is 10 dB under its strongest bin, in whole bins.
    pub width_hz: f32,
    /// How long it was on.
    pub on_ms: f32,
    /// Its strongest bin over that bin's floor, at its strongest step.
    pub over_db: f32,
    /// Its power at its strongest step, in squared ADC counts.
    pub power: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Steps read since the start.
    pub steps: u64,
    /// Steps left out as broadband clicks.
    pub clicks: u64,
    /// Bursts too short or too thin to count.
    pub slight: u64,
}

struct Open {
    first: u64,
    last: u64,
    on: u32,
    /// Σ power and Σ power × bin, for the centre.
    weight: f64,
    moment: f64,
    /// Its power over the floor in each bin, summed over its steps.
    spectrum: [f32; BINS],
    over: f32,
    power: f32,
}

impl Open {
    fn bin(&self) -> f32 {
        (self.moment / self.weight) as f32
    }

    /// The bins around the strongest that stand within `WIDTH_UNDER` of it.
    fn width_bins(&self) -> usize {
        let (top, &most) = self.spectrum.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap_or((0, &0.0));
        let within = |b: &usize| self.spectrum[*b] >= most * WIDTH_UNDER;
        let low = (0..top).rev().take_while(within).count();
        let high = (top + 1..BINS).take_while(within).count();
        low + 1 + high
    }
}

/// One step's signal: the run it is in, and its power over the floor in the run's bins.
struct Found {
    bin: f32,
    from: usize,
    excess: [f32; RUN],
    over: f32,
    power: f32,
}

pub struct Detector {
    fft: Arc<dyn Fft<f32>>,
    window: [f32; POINTS],
    /// 1 / (points × Σ window²): a transform's bins then add up to the mean power.
    scale: f32,
    /// Samples short of a step.
    pending: Vec<Complex32>,
    /// The last `FLOOR_STEPS` steps' bins, and where the next goes.
    history: Vec<f32>,
    filled: usize,
    floor: Option<[f32; BINS]>,
    step: u64,
    open: Vec<Open>,
    counters: Counters,
}

impl Default for Detector {
    fn default() -> Self {
        let mut window = [0.0; POINTS];
        for (i, w) in window.iter_mut().enumerate() {
            *w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / POINTS as f32).cos();
        }
        let energy: f32 = window.iter().map(|w| w * w).sum();
        Detector {
            fft: FftPlanner::new().plan_fft_forward(POINTS),
            window,
            scale: 1.0 / (POINTS as f32 * energy),
            pending: Vec::new(),
            history: vec![0.0; FLOOR_STEPS * BINS],
            filled: 0,
            floor: None,
            step: 0,
            open: Vec::new(),
            counters: Counters::default(),
        }
    }
}

impl Detector {
    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// The noise in the bins read, in squared ADC counts; none until the first quarter second.
    pub fn floor_power(&self) -> Option<f32> {
        self.floor.map(|f| f.iter().sum::<f32>() * NOISE_MEAN)
    }

    /// More of the channel. The bursts that end are added to `out`.
    pub fn push(&mut self, samples: &[Complex32], out: &mut Vec<Burst>) {
        self.pending.extend_from_slice(samples);
        let whole = self.pending.len() / STEP * STEP;
        let mut bins = [0.0f32; BINS];
        let mut work = [Complex32::default(); POINTS];
        for s in 0..whole / STEP {
            bins.fill(0.0);
            for t in 0..AVERAGED {
                let from = s * STEP + t * POINTS;
                for (w, (x, g)) in work.iter_mut().zip(self.pending[from..from + POINTS].iter().zip(&self.window)) {
                    *w = x * g;
                }
                self.fft.process(&mut work);
                for (b, p) in bins.iter_mut().enumerate() {
                    // Bin b is b − HALF bins from the centre.
                    *p += work[(b + POINTS - HALF) % POINTS].norm_sqr();
                }
            }
            let scale = self.scale / AVERAGED as f32;
            bins.iter_mut().for_each(|p| *p *= scale);
            self.one(&bins, out);
        }
        self.pending.drain(..whole);
    }

    fn one(&mut self, bins: &[f32; BINS], out: &mut Vec<Burst>) {
        let at = self.filled % FLOOR_STEPS * BINS;
        self.history[at..at + BINS].copy_from_slice(bins);
        self.filled += 1;
        if self.filled % FLOOR_STEPS == 0 {
            self.new_floor();
        }
        if let Some(floor) = self.floor {
            match found(bins, &floor) {
                Ok(signals) => signals.into_iter().flatten().for_each(|f| self.take(f)),
                Err(Click) => self.counters.clicks += 1,
            }
        }
        self.close(out, false);
        self.step += 1;
        self.counters.steps += 1;
    }

    fn new_floor(&mut self) {
        let mut column = [0.0f32; FLOOR_STEPS];
        let mut median = [0.0f32; BINS];
        for (b, m) in median.iter_mut().enumerate() {
            for (s, c) in column.iter_mut().enumerate() {
                *c = self.history[s * BINS + b];
            }
            let (_, mid, _) = column.select_nth_unstable_by(FLOOR_STEPS / 2, f32::total_cmp);
            // A silent bin (a replayed file's padding) still has a floor to stand over.
            *m = mid.max(1e-6);
        }
        self.floor = Some(match self.floor {
            None => median,
            Some(mut f) => {
                f.iter_mut().zip(&median).for_each(|(f, m)| *f += FLOOR_GAIN * (m - *f));
                f
            }
        });
    }

    fn take(&mut self, f: Found) {
        let step = self.step;
        let weight = f64::from(f.power);
        let at = match self.open.iter().position(|o| (o.bin() - f.bin).abs() <= SAME) {
            Some(at) => at,
            None => {
                self.open.push(Open { first: step, last: step, on: 0, weight: 0.0, moment: 0.0, spectrum: [0.0; BINS], over: 0.0, power: 0.0 });
                self.open.len() - 1
            }
        };
        let o = &mut self.open[at];
        o.last = step;
        o.on += 1;
        o.weight += weight;
        o.moment += weight * f64::from(f.bin);
        o.spectrum[f.from..f.from + RUN].iter_mut().zip(&f.excess).for_each(|(s, e)| *s += e);
        o.over = o.over.max(f.over);
        o.power = o.power.max(f.power);
    }

    /// End the bursts whose time is up (`all`: every one, at the end of the samples).
    fn close(&mut self, out: &mut Vec<Burst>, all: bool) {
        let step = self.step;
        let mut i = 0;
        while i < self.open.len() {
            let o = &self.open[i];
            if !(all || step - o.last >= HANG || step - o.first >= MAX_STEPS) {
                i += 1;
                continue;
            }
            let o = self.open.swap_remove(i);
            let steps = (o.last - o.first + 1) as f32;
            if o.on < MIN_ON || (o.on as f32) < MIN_SHARE * steps {
                self.counters.slight += 1;
                continue;
            }
            out.push(Burst {
                start: o.first * STEP as u64,
                end: (o.last + 1) * STEP as u64,
                offset_hz: (o.bin() - HALF as f32) * BIN_HZ,
                width_hz: o.width_bins() as f32 * BIN_HZ,
                on_ms: o.on as f32 * STEP as f32 * 1e3 / RATE_HZ as f32,
                over_db: 10.0 * o.over.log10(),
                power: o.power,
            });
        }
    }

    /// The samples end here (a gap follows): the open bursts end, and the next sample is the
    /// first again. The floor is kept.
    pub fn end(&mut self, out: &mut Vec<Burst>) {
        self.close(out, true);
        self.pending.clear();
        self.step = 0;
    }
}

/// The step rose all over the channel.
struct Click;

/// The run of `RUN` bins from `from` on that holds the most of `excess`, among those `allowed`.
fn best_run(excess: &[f32; BINS], allowed: impl Fn(usize) -> bool) -> Option<(usize, f32)> {
    (0..=BINS - RUN)
        .filter(|&from| allowed(from))
        .map(|from| (from, excess[from..from + RUN].iter().sum::<f32>()))
        .filter(|&(_, sum)| sum > 0.0)
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// What one step holds: up to two signals, each in a narrow run of bins, or a broadband click.
fn found(bins: &[f32; BINS], floor: &[f32; BINS]) -> Result<[Option<Found>; 2], Click> {
    let mut over = [0.0f32; BINS];
    let mut excess = [0.0f32; BINS];
    for b in 0..BINS {
        over[b] = bins[b] / floor[b];
        if over[b] > ON {
            excess[b] = bins[b] - floor[b];
        }
    }
    let Some(first) = best_run(&excess, |_| true) else { return Ok([None, None]) };
    let second = best_run(&excess, |from| from + RUN <= first.0 || from >= first.0 + RUN);
    let peak = |run: (usize, f32)| over[run.0..run.0 + RUN].iter().fold(0.0f32, |m, &v| m.max(v));
    let in_runs = |b: usize| [Some(first), second].into_iter().flatten().any(|(from, _)| (from..from + RUN).contains(&b));
    let others: Vec<f32> = (0..BINS).filter(|&b| !in_runs(b)).map(|b| over[b]).collect();
    let outside = others.iter().sum::<f32>() / others.len() as f32;
    if outside > QUIET && peak(first) < outside * SKIRTS {
        return Err(Click);
    }
    let signal = |run: (usize, f32)| {
        let (from, sum) = run;
        let moment: f32 = (from..from + RUN).map(|b| excess[b] * b as f32).sum();
        let mut kept = [0.0; RUN];
        kept.copy_from_slice(&excess[from..from + RUN]);
        Found { bin: moment / sum, from, excess: kept, over: peak(run), power: sum }
    };
    Ok([Some(signal(first)), second.filter(|&s| peak(s) >= SECOND * peak(first)).map(signal)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Noise of unit variance a sample (deterministic), `amplitude` times.
    struct Noise(u64);

    impl Noise {
        fn uniform(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 + 0.5) / (1u64 << 24) as f32
        }

        fn sample(&mut self, amplitude: f32) -> Complex32 {
            let (r, a) = ((-self.uniform().ln()).sqrt(), 2.0 * std::f32::consts::PI * self.uniform());
            Complex32::new(r * a.cos(), r * a.sin()) * amplitude
        }
    }

    /// `seconds` of noise (10 counts rms), with keyed tones: (start s, length s, offset Hz,
    /// amplitude, on and off ms of its keying; 0: a plain carrier).
    fn channel(seconds: f32, tones: &[(f32, f32, f32, f32, f32)]) -> Vec<Complex32> {
        let mut noise = Noise(7);
        (0..(seconds * RATE_HZ as f32) as usize)
            .map(|n| {
                let t = n as f32 / RATE_HZ as f32;
                let mut s = noise.sample(10.0);
                for &(start, length, hz, amplitude, key_ms) in tones {
                    let keyed = key_ms == 0.0 || ((t - start) * 1e3 / key_ms) as u32 % 2 == 0;
                    if t >= start && t < start + length && keyed {
                        let a = 2.0 * std::f64::consts::PI * f64::from(hz) * n as f64 / f64::from(RATE_HZ);
                        s += Complex32::new(a.cos() as f32, a.sin() as f32) * amplitude;
                    }
                }
                s
            })
            .collect()
    }

    fn detect(samples: &[Complex32]) -> (Vec<Burst>, Detector) {
        let mut d = Detector::default();
        let mut out = Vec::new();
        for piece in samples.chunks(10_000) {
            d.push(piece, &mut out);
        }
        d.end(&mut out);
        (out, d)
    }

    fn ms(samples: u64) -> f32 {
        samples as f32 * 1e3 / RATE_HZ as f32
    }

    #[test]
    fn noise_alone_holds_no_burst() {
        let (bursts, d) = detect(&channel(20.0, &[]));
        assert!(bursts.is_empty(), "{bursts:?}");
        assert_eq!(d.counters().clicks, 0);
        // The floor is the noise's power in the bins read: 100 counts² over 51 of 64 bins.
        let floor = d.floor_power().unwrap();
        assert!((floor / (100.0 * 51.0 / 64.0) - 1.0).abs() < 0.1, "{floor}");
    }

    #[test]
    fn a_keyed_burst_is_found_with_its_time_place_and_level() {
        // An on-off keyed message: 150 ms of 1.5 ms pulses, 40 kHz under the centre.
        let (bursts, _) = detect(&channel(3.0, &[(1.0, 0.15, -40_000.0, 100.0, 1.5)]));
        assert_eq!(bursts.len(), 1, "{bursts:?}");
        let b = bursts[0];
        assert!((ms(b.start) - 1000.0).abs() < 2.0, "starts at {} ms", ms(b.start));
        assert!((ms(b.end) - 1150.0).abs() < 3.0, "ends at {} ms", ms(b.end));
        assert!((b.offset_hz + 40_000.0).abs() < 1_000.0, "{} Hz", b.offset_hz);
        assert!(b.width_hz <= 3.0 * BIN_HZ, "{} Hz wide", b.width_hz);
        // The carrier's power while on, 100² counts; over a 3.9 kHz bin's floor of 100 / 64.
        assert!((10.0 * (b.power / 10_000.0).log10()).abs() < 2.0, "{}", b.power);
        assert!((b.over_db - 36.0).abs() < 4.0, "{} dB over", b.over_db);
        assert!(b.on_ms > 70.0 && b.on_ms < 155.0, "{} ms on", b.on_ms);
    }

    #[test]
    fn the_repeats_of_a_message_are_one_burst_and_two_devices_are_two() {
        let tones = [
            // A message twice, 105 ms apart, as the Oregon Scientific sensor sends it.
            (1.0, 0.144, -40_000.0, 60.0, 1.5),
            (1.249, 0.144, -40_000.0, 60.0, 1.5),
            // Another device 70 kHz away, in the gap and over the second message.
            (1.2, 0.1, 30_000.0, 60.0, 0.5),
            // The first again, half a second after its end: another burst.
            (2.0, 0.1, -40_000.0, 60.0, 1.5),
        ];
        let (mut bursts, _) = detect(&channel(4.0, &tones));
        bursts.sort_by_key(|b| b.start);
        let seen: Vec<(i32, i32, i32)> = bursts.iter().map(|b| (ms(b.start).round() as i32, ms(b.end - b.start).round() as i32, b.offset_hz.round() as i32)).collect();
        assert_eq!(seen.len(), 3, "{seen:?}");
        let is = |got: (i32, i32, i32), start: i32, length: i32, hz: i32| (got.0 - start).abs() <= 2 && (got.1 - length).abs() <= 3 && (got.2 - hz).abs() <= 1500;
        assert!(is(seen[0], 1000, 393, -40_000) && is(seen[1], 1200, 100, 30_000) && is(seen[2], 2000, 100, -40_000), "{seen:?}");
    }

    #[test]
    fn frequency_shift_keying_wider_than_a_run_is_a_burst_at_each_tone() {
        // One carrier moved 40 kHz either side of the centre each half millisecond, its phase
        // unbroken, as a transmitter's oscillator does it.
        let mut samples = channel(3.0, &[]);
        let mut phase = 0.0f64;
        for (n, s) in samples.iter_mut().enumerate() {
            let t = n as f32 / RATE_HZ as f32;
            if (1.0..1.2).contains(&t) {
                let hz = if ((t - 1.0) * 2e3) as u32 % 2 == 0 { -40_000.0 } else { 40_000.0 };
                phase += 2.0 * std::f64::consts::PI * hz / f64::from(RATE_HZ);
                *s += Complex32::new(phase.cos() as f32, phase.sin() as f32) * 100.0;
            }
        }
        let (bursts, d) = detect(&samples);
        assert_eq!(d.counters().clicks, 0);
        assert!(bursts.len() == 2 && bursts.iter().all(|b| (b.offset_hz.abs() - 40_000.0).abs() < 1_500.0 && (ms(b.start) - 1000.0).abs() < 2.0), "{bursts:?}");
    }

    #[test]
    fn a_weak_burst_is_found_and_a_weaker_one_is_not() {
        // 12 dB over a bin's floor: the weakest device of unit A's captures.
        let weak = (100.0f32 / 64.0 * 15.85).sqrt();
        let (bursts, _) = detect(&channel(3.0, &[(1.0, 0.2, 30_000.0, weak, 0.0)]));
        assert_eq!(bursts.len(), 1, "{bursts:?}");
        assert!((bursts[0].over_db - 12.0).abs() < 3.0, "{}", bursts[0].over_db);
        let (bursts, _) = detect(&channel(3.0, &[(1.0, 0.2, 30_000.0, weak / 2.5, 0.0)]));
        assert!(bursts.is_empty(), "{bursts:?}");
    }

    #[test]
    fn broadband_clicks_are_counted_and_make_no_burst() {
        // Every 13 ms the whole channel rises 10 dB for 2 ms: a switching supply's hash.
        let mut samples = channel(5.0, &[(2.0, 0.15, 10_000.0, 100.0, 1.5)]);
        let mut noise = Noise(99);
        for (n, s) in samples.iter_mut().enumerate() {
            let ms = n as f32 * 1e3 / RATE_HZ as f32;
            if ms % 13.0 < 2.0 {
                *s += noise.sample(30.0);
            }
        }
        let (bursts, d) = detect(&samples);
        assert!(d.counters().clicks > 300, "{:?}", d.counters());
        // The device is still found, through them.
        assert_eq!(bursts.len(), 1, "{bursts:?}");
        assert!((ms(bursts[0].start) - 2000.0).abs() < 3.0 && (bursts[0].offset_hz - 10_000.0).abs() < 1_000.0, "{bursts:?}");
    }

    #[test]
    fn a_carrier_that_stays_is_one_burst_and_then_the_floor() {
        let (bursts, _) = detect(&channel(8.0, &[(1.0, 7.0, 50_000.0, 80.0, 0.0)]));
        assert_eq!(bursts.len(), 1, "{bursts:?}");
        assert!(ms(bursts[0].end - bursts[0].start) < 1500.0, "{bursts:?}");
    }

    #[test]
    fn a_stray_millisecond_is_no_burst() {
        let (bursts, d) = detect(&channel(3.0, &[(1.0, 0.004, 0.0, 100.0, 0.0)]));
        assert!(bursts.is_empty(), "{bursts:?}");
        assert_eq!(d.counters().slight, 1);
    }

    #[test]
    fn a_very_strong_burst_is_not_taken_for_a_click() {
        // 50 dB over the floor, keyed in 0.5 ms pulses: its skirts lift the whole channel.
        let (bursts, d) = detect(&channel(3.0, &[(1.0, 0.3, -5_000.0, 500.0, 0.5)]));
        assert_eq!(bursts.len(), 1, "{bursts:?}");
        assert!(d.counters().clicks < 10, "{:?}", d.counters());
        assert!((ms(bursts[0].end - bursts[0].start) - 300.0).abs() < 4.0, "{bursts:?}");
    }

    #[test]
    fn a_gap_ends_what_is_open_and_keeps_the_floor() {
        let samples = channel(2.0, &[(1.9, 0.1, 0.0, 100.0, 0.0)]);
        let mut d = Detector::default();
        let mut out = Vec::new();
        d.push(&samples, &mut out);
        assert!(out.is_empty() && d.open.len() == 1);
        d.end(&mut out);
        assert_eq!(out.len(), 1);
        assert!(d.open.is_empty() && d.floor_power().is_some());
        // Counted from the first sample again.
        d.push(&channel(1.0, &[(0.5, 0.1, 0.0, 100.0, 0.0)]), &mut out);
        d.end(&mut out);
        assert!((ms(out[1].start) - 500.0).abs() < 2.0, "{out:?}");
    }
}
