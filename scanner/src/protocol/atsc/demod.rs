//! The 8-VSB demodulator: a channel's IQ (its centre at a known offset, sampled at 10 MSPS or
//! faster, so the ±3 MHz channel stays inside ±0.3 of the rate) to equalized symbols, segment by
//! segment.
//!
//! 1. **Receive filter.** 8-VSB's matched filter at the input rate. In the channel-centred
//!    signal it is a real root-raised cosine at RS/2 symbols a second (rolloff 0.1152): symmetric,
//!    so the shared FIR runs it on I and Q. The neighbouring channels fall outside it.
//! 2. **Pilot.** RS/4 below the centre. Its frequency comes from the slope of its phase over short
//!    blocks, then its phase from 0.2 ms blocks, a straight line between their centres.
//! 3. **The real signal where it is needed.** The filtered signal interpolated at any instant
//!    (a windowed sinc), moved up RS/4 and turned by the pilot's phase: its real part is the
//!    8-level signal (the halves of the vestige add up flat about the pilot).
//! 4. **Timing.** From the segment syncs (+5 −5 −5 +5 every 832 symbols): the real signal at two
//!    samples a symbol around where each is expected, summed over blocks of segments and tracked
//!    block to block, then fitted to a line (the sample clock's error is a constant rate).
//! 5. **Symbols.** The real signal at each fitted instant; the pilot's DC taken out; scaled by
//!    the syncs.
//! 6. **Equalizer.** Field syncs found by their PN511. A least-squares equalizer is trained on
//!    their known symbols, then on its own decisions, and applied by fast convolution.

use realfft::RealFftPlanner;
use rustfft::num_complex::{Complex32, Complex64};

use super::vsb::{field_sync, pn511, pn63, FIELD_SEGMENTS, SEGMENT, SEGMENT_SYNC, SYNC_SYMBOLS};
use crate::dsp::fsk4::Fir;

/// 8-VSB's symbol rate: 4.5 MHz × 684 / 286.
pub const SYMBOL_RATE: f64 = 4_500_000.0 * 684.0 / 286.0;
const ROLLOFF: f64 = 0.1152;
/// The receive filter's half-span, in symbols of RS/2.
const FILTER_HALF_SPAN: f64 = 12.0;
/// The interpolator: a windowed sinc of this many taps, at this many fractional phases.
const INTERP_TAPS: usize = 24;
const INTERP_PHASES: usize = 512;
/// Samples a segment at two a symbol.
const PERIOD: f64 = (2 * SEGMENT) as f64;
/// Segments summed for one timing estimate.
const TIMING_BLOCK: usize = 32;
/// Segments searched at every phase for where the syncs are.
const SYNC_SEARCH: usize = 256;
const EQ_TAPS: usize = 128;
/// Equalizer taps ahead of the symbol (pre-echoes).
const EQ_PRE: usize = 32;
/// Field syncs the equalizer trains on, then decisions it refines on.
const EQ_TRAINING_FIELDS: usize = 20;
const EQ_DECISION_ROWS: usize = 12_000;
/// Rows summed in f32 (vectorized) before they go into the f64 totals.
const EQ_BLOCK_ROWS: usize = 128;
/// The fast convolution's transform size.
const FFT_LEN: usize = 4096;

#[derive(Debug, Clone)]
pub struct Demodulated {
    /// Equalized symbols (pilot removed, ±1..±7), segment by segment.
    pub segments: Vec<[f32; SEGMENT]>,
    /// The first field sync segment; the others follow every 313.
    pub first_field_sync: Option<usize>,
    /// The pilot from where it should be.
    pub pilot_offset_hz: f64,
    /// The symbol clock against the sample clock as given, ppm.
    pub clock_ppm: f64,
    /// Modulation error ratio of the equalized data symbols, dB.
    pub mer_db: f32,
}

/// The root-raised cosine at `t` symbols (rolloff 0.1152), peak 1 - a + 4a/π.
pub(crate) fn rrc(t: f64) -> f64 {
    let a = ROLLOFF;
    let pi = std::f64::consts::PI;
    if t.abs() < 1e-9 {
        return 1.0 - a + 4.0 * a / pi;
    }
    if ((4.0 * a * t).abs() - 1.0).abs() < 1e-9 {
        return a / 2f64.sqrt() * ((1.0 + 2.0 / pi) * (pi / (4.0 * a)).sin() + (1.0 - 2.0 / pi) * (pi / (4.0 * a)).cos());
    }
    ((pi * t * (1.0 - a)).sin() + 4.0 * a * t * (pi * t * (1.0 + a)).cos()) / (pi * t * (1.0 - (4.0 * a * t).powi(2)))
}

/// e^{jθ} stepped sample by sample, set exactly again every `RESYNC` steps.
struct Phasor {
    start: f64,
    step: f64,
    n: usize,
    p: Complex64,
    w: Complex64,
}

const RESYNC: usize = 4096;

impl Phasor {
    fn new(start: f64, step: f64) -> Phasor {
        Phasor { start, step, n: 0, p: Complex64::from_polar(1.0, start), w: Complex64::from_polar(1.0, step) }
    }

    fn next(&mut self) -> Complex32 {
        let out = Complex32::new(self.p.re as f32, self.p.im as f32);
        self.n += 1;
        if self.n % RESYNC == 0 {
            self.p = Complex64::from_polar(1.0, self.start + self.step * self.n as f64);
        } else {
            self.p *= self.w;
        }
        out
    }
}

/// The channel filtered at the input rate, as I and Q, its centre first moved to DC; and the
/// filter's delay in samples (output n is centred on input n − delay).
fn receive_filter(iq: &[Complex32], fs: f64, centre_offset_hz: f64) -> (Vec<f32>, Vec<f32>, usize) {
    let rb = SYMBOL_RATE / 2.0;
    let half = (FILTER_HALF_SPAN / rb * fs).ceil() as isize;
    let taps: Vec<f32> = (-half..=half).map(|n| (rrc(n as f64 / fs * rb) * rb / fs) as f32).collect();
    let mut shift = Phasor::new(0.0, -2.0 * std::f64::consts::PI * centre_offset_hz / fs);
    let (mut re, mut im) = (Vec::with_capacity(iq.len()), Vec::with_capacity(iq.len()));
    for &s in iq {
        let v = if centre_offset_hz == 0.0 { s } else { s * shift.next() };
        re.push(v.re);
        im.push(v.im);
    }
    let (mut fre, mut fim) = (Vec::with_capacity(iq.len()), Vec::with_capacity(iq.len()));
    Fir::new(&taps, false).process(&re, &mut fre);
    Fir::new(&taps, false).process(&im, &mut fim);
    (fre, fim, half as usize)
}

/// Least-squares slope of `y` against its index.
fn slope(y: &[f64]) -> f64 {
    let n = y.len() as f64;
    let mx = (n - 1.0) / 2.0;
    let my = y.iter().sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for (i, &v) in y.iter().enumerate() {
        sxy += (i as f64 - mx) * (v - my);
        sxx += (i as f64 - mx).powi(2);
    }
    if sxx > 0.0 { sxy / sxx } else { 0.0 }
}

/// Unwrapped phases of the signal's means over blocks of `len`, the signal first turned by
/// `step` radians a sample.
fn block_phases(re: &[f32], im: &[f32], len: usize, step: f64) -> Vec<f64> {
    let mut ph = Phasor::new(0.0, step);
    let mut out = Vec::with_capacity(re.len() / len);
    let mut last = 0.0;
    for (br, bi) in re.chunks_exact(len).zip(im.chunks_exact(len)) {
        let mut m = Complex32::new(0.0, 0.0);
        for (&r, &i) in br.iter().zip(bi) {
            m += Complex32::new(r, i) * ph.next();
        }
        let mut p = f64::from(m.arg());
        while p - last > std::f64::consts::PI {
            p -= 2.0 * std::f64::consts::PI;
        }
        while p - last < -std::f64::consts::PI {
            p += 2.0 * std::f64::consts::PI;
        }
        out.push(p);
        last = p;
    }
    out
}

/// The phase to turn the filtered signal by, at each instant, for its real part to be the
/// symbols: up RS/4, less the pilot's own phase.
struct Carrier {
    /// Radians a sample: RS/4 less the pilot's offset.
    rate: f64,
    offset_hz: f64,
    len: usize,
    /// The pilot's remaining phase at each block's centre.
    phases: Vec<f64>,
}

impl Carrier {
    /// From the filtered signal at `fs`: short blocks (20 µs) take a pilot offset up to ±25 kHz,
    /// then 0.2 ms blocks the rest and the phase.
    fn find(re: &[f32], im: &[f32], fs: f64) -> Option<Carrier> {
        let two_pi = 2.0 * std::f64::consts::PI;
        let up = two_pi * SYMBOL_RATE / 4.0 / fs;
        let mut offset = 0.0;
        for block_s in [20e-6, 2e-4] {
            let len = (block_s * fs) as usize;
            let ph = block_phases(re, im, len, up - two_pi * offset / fs);
            if ph.len() < 4 {
                return None;
            }
            offset += slope(&ph) / (two_pi * len as f64 / fs);
        }
        let len = (2e-4 * fs) as usize;
        let rate = up - two_pi * offset / fs;
        let phases = block_phases(re, im, len, rate);
        Some(Carrier { rate, offset_hz: offset, len, phases })
    }

    /// The turn at sample `u` of the filtered signal.
    fn at(&self, u: f64) -> f64 {
        let n = self.phases.len();
        let x = (u / self.len as f64 - 0.5).clamp(0.0, (n - 1) as f64);
        let i = (x as usize).min(n.saturating_sub(2));
        let f = x - i as f64;
        let p = self.phases[i] * (1.0 - f) + self.phases.get(i + 1).copied().unwrap_or(self.phases[i]) * f;
        self.rate * u - p
    }
}

/// A windowed sinc (Blackman) at `INTERP_PHASES` fractional phases, each phase's taps summing to
/// one: the filtered signal (inside ±0.3 of its sample rate) at any instant.
struct Interp {
    table: Vec<[f32; INTERP_TAPS]>,
}

impl Interp {
    fn new() -> Interp {
        let n = INTERP_TAPS as f64;
        let table = (0..=INTERP_PHASES)
            .map(|p| {
                let f = p as f64 / INTERP_PHASES as f64;
                let mut t = [0f32; INTERP_TAPS];
                let mut sum = 0.0;
                for (j, v) in t.iter_mut().enumerate() {
                    let x = j as f64 - (INTERP_TAPS / 2 - 1) as f64 - f;
                    let sinc = if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
                    let w = 0.42 + 0.5 * (2.0 * std::f64::consts::PI * x / n).cos() + 0.08 * (4.0 * std::f64::consts::PI * x / n).cos();
                    *v = (sinc * w) as f32;
                    sum += sinc * w;
                }
                t.iter_mut().for_each(|v| *v /= sum as f32);
                t
            })
            .collect();
        Interp { table }
    }

    /// The signal at sample position `u`; zero where the taps leave it.
    fn at(&self, re: &[f32], im: &[f32], u: f64) -> Complex32 {
        let i = u.floor();
        let p = ((u - i) * INTERP_PHASES as f64).round() as usize;
        let start = i as isize - (INTERP_TAPS / 2 - 1) as isize;
        if start < 0 || start as usize + INTERP_TAPS > re.len() {
            return Complex32::new(0.0, 0.0);
        }
        let s = start as usize;
        let t = &self.table[p];
        Complex32::new(dot(t, &re[s..s + INTERP_TAPS]), dot(t, &im[s..s + INTERP_TAPS]))
    }
}

/// Σ a·b in eight running sums.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    let tail: f32 = ca.remainder().iter().zip(cb.remainder()).map(|(x, y)| x * y).sum();
    for (x, y) in ca.zip(cb) {
        for l in 0..8 {
            acc[l] += x[l] * y[l];
        }
    }
    acc.iter().sum::<f32>() + tail
}

/// The real signal (the 8 levels and the pilot's DC) at any instant.
struct Real {
    re: Vec<f32>,
    im: Vec<f32>,
    delay: f64,
    carrier: Carrier,
    interp: Interp,
    /// Input samples per sample at two a symbol.
    ratio: f64,
}

impl Real {
    /// At `t` samples of two a symbol from the start of the input.
    fn at(&self, t: f64) -> f32 {
        let u = t * self.ratio + self.delay;
        let z = self.interp.at(&self.re, &self.im, u);
        (z * Complex32::from_polar(1.0, self.carrier.at(u) as f32)).re
    }

    /// Samples at two a symbol the input holds.
    fn len(&self) -> usize {
        ((self.re.len() as f64 - self.delay - INTERP_TAPS as f64) / self.ratio).max(0.0) as usize
    }
}

/// The segment sync correlation at sample n (2 samples a symbol).
fn sync_corr(r: &impl Fn(usize) -> f32, n: usize) -> f32 {
    r(n) - r(n + 2) - r(n + 4) + r(n + 6)
}

/// Segment syncs: (first sync's sample, samples a segment) at two samples a symbol, from blocks
/// of segments tracked along the signal and a line fitted through them. `len` samples.
fn timing(r: &impl Fn(usize) -> f32, len: usize) -> Option<(f64, f64)> {
    let period = PERIOD as usize;
    let nseg = len.saturating_sub(8) / period;
    if nseg < 2 * TIMING_BLOCK {
        return None;
    }
    // Where the syncs are, from the first segments summed.
    let search = nseg.min(SYNC_SEARCH);
    let first: Vec<f32> = (0..search * period + 8).map(r).collect();
    let mut acc = vec![0f32; period];
    for s in 0..search {
        for (ph, a) in acc.iter_mut().enumerate() {
            let n = s * period + ph;
            *a += first[n] - first[n + 2] - first[n + 4] + first[n + 6];
        }
    }
    let p0 = acc.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?.0 as f64;
    let (mut start, mut per) = (p0, PERIOD);
    let mut points: Vec<(f64, f64)> = Vec::new();
    let mut s0 = 0;
    while s0 + TIMING_BLOCK <= nseg {
        let mut win = [0f32; 9];
        for s in s0..s0 + TIMING_BLOCK {
            let base = (start + s as f64 * per).round() as isize - 4;
            for (d, w) in win.iter_mut().enumerate() {
                let n = base + d as isize;
                if n >= 0 && (n as usize) + 7 < len {
                    *w += sync_corr(r, n as usize);
                }
            }
        }
        let j = (0..9).max_by(|&a, &b| win[a].total_cmp(&win[b])).unwrap_or(4);
        if (1..8).contains(&j) {
            let (a, b, c) = (win[j - 1], win[j], win[j + 1]);
            let den = a - 2.0 * b + c;
            let frac = if den != 0.0 { 0.5 * (a - c) / den } else { 0.0 };
            let mid = s0 as f64 + (TIMING_BLOCK - 1) as f64 / 2.0;
            let pos = (start + mid * per).round() + (j as f64 - 4.0) + f64::from(frac);
            points.push((mid, pos));
            // Track: the line through what is known so far.
            match fit(&points) {
                Some((a, b)) => {
                    start = a;
                    per = b;
                }
                None => start = pos - mid * per,
            }
        }
        s0 += TIMING_BLOCK;
    }
    // The final line, three times without the blocks a sample off it.
    let mut line = fit(&points)?;
    for _ in 0..3 {
        line = fit(&points.iter().copied().filter(|&(s, p)| (p - (line.0 + s * line.1)).abs() < 1.0).collect::<Vec<_>>())?;
    }
    Some(line)
}

/// Least squares p = a + b s over `points`.
fn fit(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let ms = points.iter().map(|p| p.0).sum::<f64>() / n;
    let mp = points.iter().map(|p| p.1).sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for &(s, p) in points {
        sxy += (s - ms) * (p - mp);
        sxx += (s - ms).powi(2);
    }
    if sxx == 0.0 {
        return None;
    }
    let b = sxy / sxx;
    Some((mp - b * ms, b))
}

/// Symbols at the fitted instants, the pilot's DC out and the syncs at ±5. Within a segment the
/// instants are evenly spaced and the turn almost linear, so one phasor steps through it.
fn symbols(sig: &Real, start: f64, per: f64) -> Vec<[f32; SEGMENT]> {
    let step = per / SEGMENT as f64;
    let nseg = ((sig.len() as f64 - start) / per).floor().max(0.0) as usize;
    let mut segs = Vec::with_capacity(nseg);
    for s in 0..nseg {
        let mut seg = [0f32; SEGMENT];
        let t0 = start + (s * SEGMENT) as f64 * step;
        let u0 = t0 * sig.ratio + sig.delay;
        let du = step * sig.ratio;
        let (a0, a1) = (sig.carrier.at(u0), sig.carrier.at(u0 + du * (SEGMENT - 1) as f64));
        let mut turn = Phasor::new(a0, (a1 - a0) / (SEGMENT - 1) as f64);
        for (k, v) in seg.iter_mut().enumerate() {
            let z = sig.interp.at(&sig.re, &sig.im, u0 + k as f64 * du);
            *v = (z * turn.next()).re;
        }
        segs.push(seg);
    }
    let (mut sum, mut n) = (0f64, 0usize);
    for seg in &segs {
        sum += seg[SYNC_SYMBOLS..].iter().map(|&v| f64::from(v)).sum::<f64>();
        n += SEGMENT - SYNC_SYMBOLS;
    }
    let dc = if n > 0 { (sum / n as f64) as f32 } else { 0.0 };
    let sync_energy: f32 = SEGMENT_SYNC.iter().map(|s| s * s).sum();
    let gain = segs.iter().map(|seg| (0..4).map(|k| (seg[k] - dc) * SEGMENT_SYNC[k]).sum::<f32>()).sum::<f32>()
        / (segs.len().max(1) as f32 * sync_energy);
    if gain.abs() > 1e-9 {
        for seg in segs.iter_mut() {
            for v in seg.iter_mut() {
                *v = (*v - dc) / gain;
            }
        }
    }
    segs
}

/// The first field sync (by its PN511), and for it and each one every 313 segments after,
/// whether its middle PN63 is inverted.
fn field_syncs(segs: &[[f32; SEGMENT]]) -> Option<(usize, Vec<bool>)> {
    let pn = pn511();
    let norm: f32 = pn.iter().map(|s| s * s).sum();
    let score: Vec<f32> = segs.iter().map(|seg| seg[4..515].iter().zip(pn).map(|(a, b)| a * b).sum::<f32>() / norm).collect();
    // The phase (segment index mod 313) whose segments score highest together.
    let mut by_phase = vec![0f32; FIELD_SEGMENTS];
    for (i, &v) in score.iter().enumerate() {
        by_phase[i % FIELD_SEGMENTS] += v;
    }
    let first = (0..FIELD_SEGMENTS.min(segs.len())).max_by(|&a, &b| by_phase[a].total_cmp(&by_phase[b]))?;
    let syncs: Vec<usize> = (first..segs.len()).step_by(FIELD_SEGMENTS).collect();
    if syncs.iter().filter(|&&i| score[i] > 0.5).count() * 2 < syncs.len() {
        return None;
    }
    let mid = 4 + 511 + 63;
    let inverted = syncs.iter().map(|&i| segs[i][mid..mid + 63].iter().zip(pn63()).map(|(a, b)| a * b).sum::<f32>() < 0.0).collect();
    Some((first, inverted))
}

/// Solve the symmetric positive system `a x = b` (n × n, row-major) by Cholesky, a little
/// diagonal loading for safety.
fn solve(mut a: Vec<f64>, mut b: Vec<f64>, n: usize) -> Option<Vec<f64>> {
    let load = (0..n).map(|i| a[i * n + i]).sum::<f64>() / n as f64 * 1e-6;
    for i in 0..n {
        a[i * n + i] += load;
    }
    for j in 0..n {
        let mut d = a[j * n + j];
        for k in 0..j {
            d -= a[j * n + k] * a[j * n + k];
        }
        if d <= 0.0 {
            return None;
        }
        let d = d.sqrt();
        a[j * n + j] = d;
        for i in j + 1..n {
            let mut v = a[i * n + j];
            for k in 0..j {
                v -= a[i * n + k] * a[j * n + k];
            }
            a[i * n + j] = v / d;
        }
    }
    for i in 0..n {
        let mut v = b[i];
        for k in 0..i {
            v -= a[i * n + k] * b[k];
        }
        b[i] = v / a[i * n + i];
    }
    for i in (0..n).rev() {
        let mut v = b[i];
        for k in i + 1..n {
            v -= a[k * n + i] * b[k];
        }
        b[i] = v / a[i * n + i];
    }
    Some(b)
}

fn slice(v: f32) -> f32 {
    (((v + 7.0) / 2.0).round().clamp(0.0, 7.0)) * 2.0 - 7.0
}

/// y ⊛ w: c[m] = Σ w[i] y[m − i] for m in 0..len (y zero before 0 and from y.len()), by
/// overlap-save.
fn convolve(y: &[f32], w: &[f32], len: usize) -> Vec<f32> {
    let m = w.len();
    let step = FFT_LEN - m + 1;
    let mut planner = RealFftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(FFT_LEN);
    let inv = planner.plan_fft_inverse(FFT_LEN);
    let mut h = fwd.make_output_vec();
    let mut buf = fwd.make_input_vec();
    buf[..m].copy_from_slice(w);
    let _ = fwd.process(&mut buf, &mut h);
    let mut spec = fwd.make_output_vec();
    let mut out = Vec::with_capacity(len + FFT_LEN);
    let scale = 1.0 / FFT_LEN as f32;
    let mut b = 0usize;
    while out.len() < len {
        // Block b: c[b·step ..< b·step + step] from y[b·step − (m − 1) ..< b·step + step].
        for (j, v) in buf.iter_mut().enumerate() {
            let k = (b * step + j) as isize - (m as isize - 1);
            *v = if k >= 0 && (k as usize) < y.len() { y[k as usize] } else { 0.0 };
        }
        let _ = fwd.process(&mut buf, &mut spec);
        for (s, &hk) in spec.iter_mut().zip(&h) {
            *s *= hk;
        }
        let _ = inv.process(&mut spec, &mut buf);
        out.extend(buf[m - 1..].iter().map(|v| v * scale));
        b += 1;
    }
    out.truncate(len);
    out
}

/// Row k's window, newest first (tap i weighs y[k + PRE − i]); false where it leaves y.
fn window(y: &[f32], k: usize, x: &mut [f32]) -> bool {
    let hi = k + EQ_PRE + 1;
    if hi < EQ_TAPS || hi > y.len() {
        return false;
    }
    x.iter_mut().zip(y[hi - EQ_TAPS..hi].iter().rev()).for_each(|(a, &b)| *a = b);
    true
}

/// The taps w minimizing Σ (wanted − Σ w[i] y[k + PRE − i])² over `rows` (k, wanted).
fn least_squares(y: &[f32], rows: &[(usize, f32)]) -> Option<Vec<f32>> {
    let n = EQ_TAPS;
    let mut a = vec![0f64; n * n];
    let mut b = vec![0f64; n];
    let mut a32 = vec![0f32; n * n];
    let mut b32 = vec![0f32; n];
    let mut x = vec![0f32; n];
    let mut pending = 0;
    for &(k, d) in rows {
        if !window(y, k, &mut x) {
            continue;
        }
        // Row i of the lower triangle gains x[i]·x[..=i]: not a reduction, so it vectorizes.
        for i in 0..n {
            let xi = x[i];
            b32[i] += xi * d;
            a32[i * n..i * n + i + 1].iter_mut().zip(&x[..=i]).for_each(|(s, &xj)| *s += xi * xj);
        }
        pending += 1;
        if pending == EQ_BLOCK_ROWS {
            a.iter_mut().zip(&mut a32).for_each(|(t, s)| *t += f64::from(std::mem::take(s)));
            b.iter_mut().zip(&mut b32).for_each(|(t, s)| *t += f64::from(std::mem::take(s)));
            pending = 0;
        }
    }
    a.iter_mut().zip(&a32).for_each(|(t, s)| *t += f64::from(*s));
    b.iter_mut().zip(&b32).for_each(|(t, s)| *t += f64::from(*s));
    for i in 0..n {
        for j in 0..i {
            a[j * n + i] = a[i * n + j];
        }
    }
    solve(a, b, n).map(|w| w.into_iter().map(|v| v as f32).collect())
}

/// The field syncs' known symbols as rows (k, wanted).
fn training_rows(first_sync: usize, inverted: &[bool]) -> Vec<(usize, f32)> {
    inverted
        .iter()
        .take(EQ_TRAINING_FIELDS)
        .enumerate()
        .flat_map(|(f, &inv)| {
            let base = (first_sync + f * FIELD_SEGMENTS) * SEGMENT;
            field_sync(inv).into_iter().enumerate().map(move |(j, d)| (base + j, d))
        })
        .collect()
}

/// The least-squares equalizer: out[k] = Σ w[i] y[k + PRE − i]. Trained on the field syncs, then
/// on its own decisions on data.
fn equalize(y: &[f32], first_sync: usize, inverted: &[bool]) -> Option<Vec<f32>> {
    let mut w = least_squares(y, &training_rows(first_sync, inverted))?;
    let mut x = vec![0f32; EQ_TAPS];
    for round in 0..2u64 {
        // Data symbols spread over the signal, a fixed pseudo-random pick.
        let mut r = 0x9E37_79B9_7F4A_7C15u64 ^ round;
        let mut picks = Vec::with_capacity(EQ_DECISION_ROWS);
        for _ in 0..EQ_DECISION_ROWS {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            let k = (r % y.len() as u64) as usize;
            if k % SEGMENT >= SYNC_SYMBOLS && window(y, k, &mut x) {
                picks.push((k, slice(dot(&w, &x))));
            }
        }
        w = least_squares(y, &picks)?;
    }
    // out[k] = (y ⊛ w)[k + PRE].
    let c = convolve(y, &w, y.len() + EQ_PRE);
    Some(c[EQ_PRE..].to_vec())
}

fn mer(segs: &[[f32; SEGMENT]]) -> f32 {
    let (mut sig, mut err) = (0f64, 0f64);
    for seg in segs {
        for &v in &seg[SYNC_SYMBOLS..] {
            let d = slice(v);
            sig += f64::from(d * d);
            err += f64::from((v - d) * (v - d));
        }
    }
    if err > 0.0 { (10.0 * (sig / err).log10()) as f32 } else { 99.0 }
}

/// Demodulate a channel: `iq` at `sample_rate_hz` (the true rate: a crystal's error taken out),
/// the channel's centre `centre_offset_hz` from DC. `None` when no segment or field syncs are
/// found.
pub fn demodulate(iq: &[Complex32], sample_rate_hz: f64, centre_offset_hz: f64) -> Option<Demodulated> {
    let (re, im, delay) = receive_filter(iq, sample_rate_hz, centre_offset_hz);
    let carrier = Carrier::find(&re, &im, sample_rate_hz)?;
    let sig = Real { re, im, delay: delay as f64, carrier, interp: Interp::new(), ratio: sample_rate_hz / (2.0 * SYMBOL_RATE) };
    let (start, per) = timing(&|n: usize| sig.at(n as f64), sig.len())?;
    let segs = symbols(&sig, start, per);
    let pilot_offset_hz = sig.carrier.offset_hz;
    drop(sig);
    let (first, inverted) = field_syncs(&segs)?;
    let flat: Vec<f32> = segs.iter().flatten().copied().collect();
    drop(segs);
    let eq = equalize(&flat, first, &inverted)?;
    let segments: Vec<[f32; SEGMENT]> = eq.chunks_exact(SEGMENT).map(|c| c.try_into().unwrap_or([0.0; SEGMENT])).collect();
    Some(Demodulated {
        mer_db: mer(&segments),
        first_field_sync: Some(first),
        pilot_offset_hz,
        clock_ppm: (PERIOD / per - 1.0) * 1e6,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_convolution_matches_the_direct_sum() {
        let y: Vec<f32> = (0..10_000).map(|i| ((i * 7919) % 101) as f32 - 50.0).collect();
        let w: Vec<f32> = (0..EQ_TAPS).map(|i| ((i * 31) % 17) as f32 / 17.0 - 0.5).collect();
        let c = convolve(&y, &w, y.len() + 40);
        for m in [0, 1, 127, 128, 4000, 9999, 10_039] {
            let direct: f32 = (0..w.len()).filter(|&i| m >= i && m - i < y.len()).map(|i| w[i] * y[m - i]).sum();
            assert!((c[m] - direct).abs() < 1e-2 * (1.0 + direct.abs()), "{m}: {} {direct}", c[m]);
        }
    }

    #[test]
    fn the_interpolator_rebuilds_a_tone_between_samples() {
        // A tone at 0.3 of the sample rate, read half a sample in.
        let n = 200;
        let f = 0.3;
        let re: Vec<f32> = (0..n).map(|k| (2.0 * std::f32::consts::PI * f * k as f32).cos()).collect();
        let im = vec![0f32; n];
        let interp = Interp::new();
        let worst = (40..160)
            .map(|k| {
                let u = k as f64 + 0.37;
                let want = (2.0 * std::f64::consts::PI * f as f64 * u).cos() as f32;
                (interp.at(&re, &im, u).re - want).abs()
            })
            .fold(0f32, f32::max);
        assert!(worst < 3e-3, "{worst}");
    }
}
