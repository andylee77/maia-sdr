//! Four-level FSK building blocks shared by the P25 C4FM and DMR receivers, ported from SDRTrunk:
//! the dibit phase constellation, a streaming FIR, the linear interpolator and the differential
//! demodulator. Names follow the Java so the two read side by side.
//!
//! Dibit values: 0 = +1, 1 = +3, 2 = −1, 3 = −3 (SDRTrunk `Dibit`).

use super::run;
use super::taps::MMSE_TAPS;

const PI: f32 = std::f32::consts::PI;

/// The phase step a dibit makes (SDRTrunk `Dibit.getIdealPhase`).
pub fn ideal_phase(dibit: u8) -> f32 {
    match dibit & 3 {
        0 => PI / 4.0,
        1 => 3.0 * PI / 4.0,
        2 => -PI / 4.0,
        _ => -3.0 * PI / 4.0,
    }
}

/// Hard decision on a soft symbol (SDRTrunk `toSymbol`).
pub fn to_symbol(sample: f32) -> u8 {
    const BOUNDARY: f32 = PI / 2.0;
    if sample > 0.0 {
        if sample > BOUNDARY { 1 } else { 0 }
    } else if sample < -BOUNDARY {
        3
    } else {
        2
    }
}

/// Linear interpolation between two samples at `mu` in [0, 1].
pub fn linear(x1: f32, x2: f32, mu: f64) -> f32 {
    if mu < 0.0 {
        x1
    } else if mu > 1.0 {
        x2
    } else {
        x1 + (x2 - x1) * mu as f32
    }
}

/// Streaming real FIR, optionally decimating by two (the first input of the stream produces an
/// output). The inputs a later output needs carry over to the next block.
///
/// Equal taps at mirrored places fold in pairs (the two samples added before one multiply), the
/// other taps go one by one, and a decimating half-band runs on two phases of its input: its
/// non-zero taps fall on the samples of the output's parity, its centre tap on the others. Both
/// take a block's outputs in one run (`run::folded`), eight outputs at a time in NEON on the A9.
/// The taps are the given ones (but a half-band's residues of zero); only the order of the
/// additions differs from a plain sum.
pub struct Fir {
    kind: Kind,
}

enum Kind {
    Window(Window),
    HalfBand(HalfBand),
}

/// A half-band tap this small against the centre is a residue of zero (SDRTrunk's designs leave
/// about 1e-17): all of them together stay far below an `f32` output's resolution.
const HALF_BAND_RESIDUE: f32 = 1e-12;

fn symmetric(t: &[f32]) -> bool {
    (0..t.len() / 2).all(|k| t[k] == t[t.len() - 1 - k])
}

impl Fir {
    pub fn new(taps: &[f32], decimate: bool) -> Self {
        let len = taps.len();
        let centre = len / 2;
        // A half-band: every tap at an even distance from the centre (but the centre) is zero.
        let residue = taps[centre].abs() * HALF_BAND_RESIDUE;
        let half_band = decimate
            && symmetric(taps)
            && len % 4 == 3
            && taps.iter().enumerate().all(|(k, t)| k == centre || k.abs_diff(centre) % 2 == 1 || t.abs() <= residue);
        let kind = if half_band {
            Kind::HalfBand(HalfBand::new(taps))
        } else {
            // y[n] = Σ t[k]·x[n−k]; over the window w = x[n−len+1 ..= n], x[n−k] = w[len−1−k].
            let wt: Vec<f32> = taps.iter().rev().copied().collect();
            // The pairs of a run of taps that starts or ends the window, from its ends inwards while
            // the two taps are equal; the run with the most.
            let pairs = |a: usize, b: usize| (0..(b - a) / 2).take_while(|&i| wt[a + i] == wt[b - 1 - i]).count();
            let (a, b) = (0..len)
                .flat_map(|cut| [(cut, len), (0, len - cut)])
                .min_by_key(|&(a, b)| std::cmp::Reverse(pairs(a, b)))
                .unwrap();
            let n = pairs(a, b);
            let window_taps = if n > 0 && !decimate {
                let single = (0..a).chain(a + n..b - n).chain(b..len).map(|j| (j, wt[j])).collect();
                Taps::Folded { at: a, span: b - a - 1, half: wt[a..a + n].to_vec(), single }
            } else {
                Taps::Plain(wt)
            };
            Kind::Window(Window { taps: window_taps, len, work: vec![0.0; len - 1], decimate, emit: true })
        };
        Fir { kind }
    }

    pub fn process(&mut self, x: &[f32], out: &mut Vec<f32>) {
        match &mut self.kind {
            Kind::Window(w) => w.process(x, out),
            Kind::HalfBand(h) => h.process(x, out),
        }
    }
}

/// Taps in window order (the oldest sample's first).
enum Taps {
    Plain(Vec<f32>),
    /// A run of `span` + 1 taps from window position `at` whose outer pairs are equal, folded
    /// (`half`: the first tap of each); the taps between the pairs and beyond the run's ends one
    /// by one as (position, tap). Every output: not decimating.
    Folded { at: usize, span: usize, half: Vec<f32>, single: Vec<(usize, f32)> },
}

struct Window {
    taps: Taps,
    len: usize,
    work: Vec<f32>,
    decimate: bool,
    /// The next input sample produces an output (decimation phase).
    emit: bool,
}

impl Window {
    fn process(&mut self, x: &[f32], out: &mut Vec<f32>) {
        let hist = self.len - 1;
        self.work.truncate(hist);
        self.work.extend_from_slice(x);
        match &self.taps {
            Taps::Folded { at, span, half, single } => {
                // Output n's window is work[n ..= n + len − 1]. The first single tap starts the
                // sums, the pairs and the other single taps add to them.
                let start = out.len();
                let n = x.len();
                match single.first() {
                    Some(&(j, tap)) => out.extend(self.work[j..j + n].iter().map(|s| tap * s)),
                    None => out.resize(start + n, 0.0),
                }
                run::folded(half, *span, &self.work[*at..], &mut out[start..]);
                for &(j, tap) in single.iter().skip(1) {
                    out[start..].iter_mut().zip(&self.work[j..]).for_each(|(o, s)| *o += tap * s);
                }
            }
            Taps::Plain(h) if !self.decimate => {
                let start = out.len();
                out.resize(start + x.len(), 0.0);
                run::plain(h, &self.work, &mut out[start..]);
            }
            Taps::Plain(h) => {
                for n in 0..x.len() {
                    if self.emit {
                        out.push(run::dot(h, &self.work[n..n + self.len]));
                    }
                    self.emit = !self.emit;
                }
            }
        }
        let used = self.work.len() - hist;
        self.work.drain(..used);
    }
}

/// A half-band decimating by two (length 4p − 1, centre c = 2p − 1). The output at input 2m
/// takes the 2p even inputs x[2m − 2c ..= 2m] (p symmetric pairs) and the odd input x[2m − c].
struct HalfBand {
    /// The taps of the even inputs' pairs, oldest first.
    pairs: Vec<f32>,
    centre: f32,
    /// Even inputs: `even[i]` is x[2(even_base + i)].
    even: Vec<f32>,
    even_base: i64,
    /// Odd inputs: `odd[i]` is x[2(odd_base + i) + 1].
    odd: Vec<f32>,
    odd_base: i64,
    next_even: bool,
}

impl HalfBand {
    fn new(taps: &[f32]) -> Self {
        let pairs: Vec<f32> = taps[..taps.len() / 2].iter().step_by(2).copied().collect();
        let p = pairs.len();
        HalfBand {
            centre: taps[taps.len() / 2],
            // The stream starts after zeros, as a window FIR's does.
            even: vec![0.0; 2 * p - 1],
            even_base: -(2 * p as i64 - 1),
            odd: vec![0.0; p],
            odd_base: -(p as i64),
            pairs,
            next_even: true,
        }
    }

    fn process(&mut self, x: &[f32], out: &mut Vec<f32>) {
        let p = self.pairs.len();
        let first = self.even_base + self.even.len() as i64;
        for &s in x {
            if self.next_even {
                self.even.push(s);
            } else {
                self.odd.push(s);
            }
            self.next_even = !self.next_even;
        }
        let next = self.even_base + self.even.len() as i64;
        // Output m's even inputs are even[m − even_base − (2p − 1) ..= m − even_base], its odd
        // input odd[m − p − odd_base]: both step by one with m. The centre tap starts the sums.
        let start = out.len();
        let count = (next - first) as usize;
        let mid = &self.odd[(first - p as i64 - self.odd_base) as usize..];
        out.extend(mid[..count].iter().map(|m| self.centre * m));
        let window = (first - self.even_base) as usize + 1 - 2 * p;
        run::folded(&self.pairs, 2 * p - 1, &self.even[window..], &mut out[start..]);
        let drop = self.even.len() - (2 * p - 1);
        self.even.drain(..drop);
        self.even_base += drop as i64;
        // The next output's centre is x[2(next − p) + 1].
        let drop = ((next - p as i64 - self.odd_base).max(0) as usize).min(self.odd.len());
        self.odd.drain(..drop);
        self.odd_base += drop as i64;
    }
}

/// The phase of each sample against the one a symbol earlier, the current sample MMSE
/// interpolated (SDRTrunk `DifferentialDemodulatorFloatScalar`).
pub struct DifferentialDemod {
    i_buf: Vec<f32>,
    q_buf: Vec<f32>,
    overlap: usize,
    interp_offset: usize,
    mu: f32,
}

impl DifferentialDemod {
    pub fn new(samples_per_symbol: f64) -> Self {
        let mu = (samples_per_symbol % 1.0) as f32;
        let mut interp_offset = samples_per_symbol.floor() as i64 - 4;
        let mut overlap = samples_per_symbol.floor() as usize + 4;
        while interp_offset < 0 {
            interp_offset += 1;
            overlap += 1;
        }
        DifferentialDemod {
            i_buf: vec![0.0; overlap],
            q_buf: vec![0.0; overlap],
            overlap,
            interp_offset: interp_offset as usize,
            mu,
        }
    }

    fn mmse(samples: &[f32], offset: usize, mu: f32) -> f32 {
        let t = &MMSE_TAPS[((128.0 * mu) as usize).min(128)];
        let s = &samples[offset..offset + 8];
        t[7] * s[0] + t[6] * s[1] + t[5] * s[2] + t[4] * s[3] + t[3] * s[4] + t[2] * s[5] + t[1] * s[6] + t[0] * s[7]
    }

    pub fn demodulate(&mut self, i: &[f32], q: &[f32], out: &mut Vec<f32>) {
        let consumed = self.i_buf.len() - self.overlap;
        self.i_buf.drain(..consumed);
        self.q_buf.drain(..consumed);
        self.i_buf.extend_from_slice(i);
        self.q_buf.extend_from_slice(q);
        for x in 0..i.len() {
            let i_prev = self.i_buf[x];
            let q_prev_conj = -self.q_buf[x];
            let off = self.interp_offset + x;
            let i_cur = Self::mmse(&self.i_buf, off, self.mu);
            let q_cur = Self::mmse(&self.q_buf, off, self.mu);
            let di = i_prev * i_cur - q_prev_conj * q_cur;
            let dq = i_prev * q_cur + i_cur * q_prev_conj;
            out.push(dq.atan2(di));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_round_trip_through_their_ideal_phases() {
        for d in 0..4u8 {
            assert_eq!(to_symbol(ideal_phase(d)), d);
        }
        assert_eq!(linear(1.0, 3.0, 0.25), 1.5);
        assert_eq!((linear(1.0, 3.0, -1.0), linear(1.0, 3.0, 2.0)), (1.0, 3.0));
    }

    #[test]
    fn fir_streams_and_decimates() {
        // An impulse through [1, 2, 3] gives the taps back, across block boundaries.
        let mut fir = Fir::new(&[1.0, 2.0, 3.0], false);
        let mut out = Vec::new();
        fir.process(&[1.0, 0.0], &mut out);
        fir.process(&[0.0, 0.0], &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 0.0]);
        let mut half = Fir::new(&[1.0, 0.0, 1.0], true);
        let mut out = Vec::new();
        half.process(&[1.0, 2.0, 3.0, 4.0, 5.0], &mut out);
        assert_eq!(out, vec![1.0, 4.0, 8.0], "x[n] + x[n−2] at every other n");
    }

    /// (taps folded in pairs, taps one by one) of a filter of every output.
    fn folds(taps: &[f32]) -> (usize, usize) {
        match Fir::new(taps, false).kind {
            Kind::Window(Window { taps: Taps::Folded { half, single, .. }, .. }) => (half.len(), single.len()),
            _ => (0, taps.len()),
        }
    }

    fn dmr_rrc() -> Vec<f32> {
        crate::protocol::dmr::filters::root_raised_cosine(25_000.0 / 4800.0, 22, 5760.0 / 25_000.0)
    }

    #[test]
    fn the_receivers_filters_fold() {
        use crate::dsp::taps::{HALFBAND_63, LPF_C4FM_25K, LPF_LSM_25K, RRC_TAPS_25K};
        use crate::protocol::dmr::filters::LPF_DMR_25K;
        assert!(matches!(Fir::new(&HALFBAND_63, true).kind, Kind::HalfBand(_)), "a half-band but for residues of zero");
        assert_eq!(folds(&LPF_LSM_25K), (33, 1));
        assert_eq!(folds(&LPF_C4FM_25K), (19, 1));
        assert_eq!(folds(&LPF_DMR_25K), (18, 1));
        assert_eq!(folds(&RRC_TAPS_25K), (20, 2), "a symmetric 41 and one more");
        assert_eq!(folds(&dmr_rrc()), (27, 3), "a lone first tap and an unequal inner pair");
    }

    /// The plain sum Σ t[k]·x[n−k] in f64, every output or (decimating) the even ones.
    fn reference(taps: &[f32], decimate: bool, x: &[f32]) -> Vec<f64> {
        (0..x.len())
            .filter(|n| !decimate || n % 2 == 0)
            .map(|n| taps.iter().enumerate().filter(|(k, _)| *k <= n).map(|(k, &t)| t as f64 * x[n - k] as f64).sum())
            .collect()
    }

    #[test]
    fn every_filter_shape_is_the_plain_sum() {
        use crate::dsp::taps::{HALFBAND_63, LPF_LSM_25K, RRC_TAPS_25K};
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let x: Vec<f32> = (0..5000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5
            })
            .collect();
        let lopsided: Vec<f32> = (0..37).map(|k| ((k * 7 % 11) as f32 - 5.0) / 20.0).collect();
        let even: Vec<f32> = LPF_LSM_25K[..33].iter().chain(&LPF_LSM_25K[34..]).copied().collect();
        let dmr_rrc = dmr_rrc();
        for (name, taps, decimate) in [
            ("half-band", &HALFBAND_63[..], true),
            ("half-band, every output", &HALFBAND_63[..], false),
            ("symmetric, odd", &LPF_LSM_25K[..], false),
            ("symmetric, even", &even[..], false),
            ("symmetric and a lone tap", &RRC_TAPS_25K[..], false),
            ("unequal inner pair", &dmr_rrc[..], false),
            ("symmetric, decimating", &LPF_LSM_25K[..], true),
            ("lopsided", &lopsided[..], false),
            ("lopsided, decimating", &lopsided[..], true),
        ] {
            let want = reference(taps, decimate, &x);
            let mut fir = Fir::new(taps, decimate);
            let mut got = Vec::new();
            let mut at = 0;
            for size in [1008, 7, 1, 500, 33, 2, 3].iter().cycle() {
                if at >= x.len() {
                    break;
                }
                let end = (at + size).min(x.len());
                fir.process(&x[at..end], &mut got);
                at = end;
            }
            assert_eq!(got.len(), want.len(), "{name}");
            let scale: f64 = taps.iter().map(|t| t.abs() as f64).sum::<f64>() * 0.5;
            for (n, (g, w)) in got.iter().zip(&want).enumerate() {
                assert!((*g as f64 - w).abs() <= 1e-6 * scale, "{name}: output {n} {g} against {w}");
            }
        }
    }

    #[test]
    fn a_tone_demodulates_to_its_phase_over_the_lag() {
        // At 25 kSPS and 4800 symbols/s the current sample is compared with the one
        // floor(sps) − 1 + frac(sps) = 4.21 samples earlier (about 0.8 symbol, as in SDRTrunk).
        let sps: f64 = 25_000.0 / 4800.0;
        let lag = sps.floor() - 1.0 + sps % 1.0;
        let step = (PI / 4.0) / sps as f32;
        let (i, q): (Vec<f32>, Vec<f32>) = (0..400).map(|n| ((n as f32 * step).cos(), (n as f32 * step).sin())).unzip();
        let mut demod = DifferentialDemod::new(sps);
        let mut out = Vec::new();
        demod.demodulate(&i[..123], &q[..123], &mut out);
        demod.demodulate(&i[123..], &q[123..], &mut out);
        let want = step * lag as f32;
        assert!(out[50..].iter().all(|p| (p - want).abs() < 0.002), "{:?} vs {want}", &out[50..54]);
    }
}
