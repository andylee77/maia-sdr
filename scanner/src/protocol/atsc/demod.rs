//! The 8-VSB demodulator: a channel's IQ (its centre at a known offset, sampled a little above
//! 6 MHz or faster) to equalized symbols, segment by segment.
//!
//! 1. **Receive filter.** Resampled to two samples a symbol through 8-VSB's matched filter. In
//!    the channel-centred signal that is a real root-raised cosine at RS/2 symbols a second
//!    (rolloff 0.1152); the neighbouring channels fall outside it.
//! 2. **Pilot.** RS/4 below the centre, moved to DC. Its frequency comes from the slope of its
//!    phase over short blocks, then its phase from each 0.2 ms block; the real part of the
//!    signal is then the 8-level symbols (the halves of the vestige add up flat about the pilot).
//! 3. **Timing.** From the segment syncs (+5 −5 −5 +5 every 832 symbols), summed over blocks of
//!    segments and tracked block to block, then fitted to a line: the sample clock's error is a
//!    constant rate.
//! 4. **Symbols.** Interpolated at the fitted instants; the pilot's DC taken out; scaled by the
//!    syncs.
//! 5. **Equalizer.** Field syncs found by their PN511. A least-squares equalizer is trained on
//!    their known symbols, then on its own decisions.

use rustfft::num_complex::{Complex32, Complex64};

use super::vsb::{field_sync, pn511, pn63, FIELD_SEGMENTS, FIELD_SYNC_KNOWN, SEGMENT, SEGMENT_SYNC, SYNC_SYMBOLS};

/// 8-VSB's symbol rate: 4.5 MHz × 684 / 286.
pub const SYMBOL_RATE: f64 = 4_500_000.0 * 684.0 / 286.0;
const ROLLOFF: f64 = 0.1152;
/// The receive filter's half-span, in symbols of RS/2.
const FILTER_HALF_SPAN: f64 = 12.0;
/// Fractional phases of the resampler's filter.
const PHASES: usize = 256;
/// Samples a segment at two a symbol.
const PERIOD: f64 = (2 * SEGMENT) as f64;
/// Segments summed for one timing estimate.
const TIMING_BLOCK: usize = 32;
const EQ_TAPS: usize = 128;
/// Equalizer taps ahead of the symbol (pre-echoes).
const EQ_PRE: usize = 32;
const EQ_DECISION_ROWS: usize = 12_000;

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
    if t.abs() < 1e-9 {
        return 1.0 - a + 4.0 * a / std::f64::consts::PI;
    }
    if ((4.0 * a * t).abs() - 1.0).abs() < 1e-9 {
        let pi = std::f64::consts::PI;
        return a / 2f64.sqrt() * ((1.0 + 2.0 / pi) * (pi / (4.0 * a)).sin() + (1.0 - 2.0 / pi) * (pi / (4.0 * a)).cos());
    }
    let pi = std::f64::consts::PI;
    ((pi * t * (1.0 - a)).sin() + 4.0 * a * t * (pi * t * (1.0 + a)).cos()) / (pi * t * (1.0 - (4.0 * a * t).powi(2)))
}

/// Σ a·b in eight running sums, so the compiler can vectorize it.
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

/// The channel resampled to 2 × RS through the receive filter, its centre moved from
/// `centre_offset_hz` to DC.
fn receive_filter(iq: &[Complex32], fs: f64, centre_offset_hz: f64) -> Vec<Complex32> {
    let fo = 2.0 * SYMBOL_RATE;
    let rb = SYMBOL_RATE / 2.0;
    let half = (FILTER_HALF_SPAN / rb * fs).ceil() as usize;
    let taps = 2 * half;
    // Tap i of phase p weighs input k0 + i, where k0 = floor(u) - half + 1 and p is u's fraction.
    let table: Vec<Vec<f32>> = (0..PHASES)
        .map(|p| {
            let frac = p as f64 / PHASES as f64;
            (0..taps)
                .map(|i| {
                    let dt = (frac + half as f64 - 1.0 - i as f64) / fs;
                    (rrc(dt * rb) * rb / fs) as f32
                })
                .collect()
        })
        .collect();
    let mut shift = Phasor::new(0.0, -2.0 * std::f64::consts::PI * centre_offset_hz / fs);
    let (mut re, mut im) = (Vec::with_capacity(iq.len()), Vec::with_capacity(iq.len()));
    for &s in iq {
        let v = if centre_offset_hz == 0.0 { s } else { s * shift.next() };
        re.push(v.re);
        im.push(v.im);
    }
    let n_out = ((iq.len() as f64 - taps as f64) * fo / fs).max(0.0) as usize;
    let step = fs / fo;
    (0..n_out)
        .map(|n| {
            let u = n as f64 * step + half as f64;
            let k = u.floor();
            let p = (((u - k) * PHASES as f64) as usize).min(PHASES - 1);
            let k0 = k as usize + 1 - half;
            let h = &table[p];
            Complex32::new(dot(h, &re[k0..k0 + taps]), dot(h, &im[k0..k0 + taps]))
        })
        .collect()
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

/// e^{jπk/4}: the pilot's move from RS/4 below the centre to DC, at 2 × RS samples a second.
fn eighth(k: usize) -> Complex32 {
    Complex32::from_polar(1.0, std::f32::consts::FRAC_PI_4 * (k % 8) as f32)
}

/// Unwrapped phases of the pilot's means over blocks of `len`: the signal moved up RS/4 and
/// down `offset_hz`.
fn block_phases(z: &[Complex32], len: usize, offset_hz: f64) -> Vec<f64> {
    let fo = 2.0 * SYMBOL_RATE;
    let rot: [Complex32; 8] = std::array::from_fn(eighth);
    let mut ph = Phasor::new(0.0, -2.0 * std::f64::consts::PI * offset_hz / fo);
    let mut out = Vec::with_capacity(z.len() / len);
    let mut last = 0.0;
    for (b, block) in z.chunks_exact(len).enumerate() {
        let mut m = Complex32::new(0.0, 0.0);
        for (i, &s) in block.iter().enumerate() {
            m += s * rot[(b * len + i) % 8] * ph.next();
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

/// The pilot to DC at phase 0, then the real part. Returns the pilot's offset too.
fn pilot_to_dc(z: &[Complex32]) -> (Vec<f32>, f64) {
    let fo = 2.0 * SYMBOL_RATE;
    let two_pi = 2.0 * std::f64::consts::PI;
    // Frequency: short blocks (20 µs) take up to ±25 kHz; then 0.2 ms blocks take the rest.
    let mut offset = 0.0;
    for block_s in [20e-6, 2e-4] {
        let len = (block_s * fo) as usize;
        let ph = block_phases(z, len, offset);
        if ph.len() < 4 {
            return (Vec::new(), 0.0);
        }
        offset += slope(&ph) / (two_pi * len as f64 / fo);
    }
    // Phase: each 0.2 ms block's at its centre, a straight line between centres, so one phasor
    // steps through each piece.
    let len = (2e-4 * fo) as usize;
    let ph = block_phases(z, len, offset);
    let rot: [Complex32; 8] = std::array::from_fn(eighth);
    let centre = |i: usize| (i as f64 + 0.5) * len as f64;
    let mut r = Vec::with_capacity(z.len());
    for i in 0..ph.len() {
        let from = if i == 0 { 0 } else { centre(i).ceil() as usize };
        let to = if i + 1 == ph.len() { z.len() } else { centre(i + 1).ceil() as usize };
        let j = i.min(ph.len() - 2);
        let rate = two_pi * offset / fo + (ph[j + 1] - ph[j]) / len as f64;
        let at = |n: f64| two_pi * offset * centre(i) / fo + ph[i] + rate * (n - centre(i));
        let mut p = Phasor::new(-at(from as f64), -rate);
        for (n, &s) in z.iter().enumerate().take(to).skip(from) {
            r.push((s * rot[n % 8] * p.next()).re);
        }
    }
    (r, offset)
}

/// The segment sync correlation at sample n (2 samples a symbol).
fn sync_corr(r: &[f32], n: usize) -> f32 {
    r[n] - r[n + 2] - r[n + 4] + r[n + 6]
}

/// Segment syncs: (first sync's sample, samples a segment), from blocks of segments tracked
/// along the signal and a line fitted through them.
fn timing(r: &[f32]) -> Option<(f64, f64)> {
    let period = PERIOD as usize;
    let nseg = r.len().saturating_sub(8) / period;
    if nseg < 2 * TIMING_BLOCK {
        return None;
    }
    // Where the syncs are, from the first segments summed.
    let mut acc = vec![0f32; period];
    for s in 0..nseg.min(256) {
        for (ph, a) in acc.iter_mut().enumerate() {
            *a += sync_corr(r, s * period + ph);
        }
    }
    let p0 = acc.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?.0 as f64;
    let (mut start, mut per) = (p0, PERIOD);
    let mut points: Vec<(f64, f64)> = Vec::new();
    let mut s0 = 0;
    while s0 + TIMING_BLOCK <= nseg {
        let mut win = [0f32; 9];
        for s in s0..s0 + TIMING_BLOCK {
            let pred = start + s as f64 * per;
            let base = pred.round() as isize - 4;
            for (d, w) in win.iter_mut().enumerate() {
                let n = base + d as isize;
                if n >= 0 && (n as usize) + 7 < r.len() {
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
            let pred = start + mid * per;
            let pos = pred.round() + (j as f64 - 4.0) + frac as f64;
            points.push((mid, pos));
            // Track: the line through what is known so far.
            if points.len() >= 2 {
                if let Some((a, b)) = fit(&points) {
                    start = a;
                    per = b;
                }
            } else {
                start = pos - mid * per;
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

/// Symbols at the fitted instants (Catmull-Rom between the 2-a-symbol samples), the pilot's DC
/// out and the syncs at ±5.
fn symbols(r: &[f32], start: f64, per: f64) -> Vec<[f32; SEGMENT]> {
    let step = per / SEGMENT as f64;
    let nseg = ((r.len() as f64 - 3.0 - start) / per).floor().max(0.0) as usize;
    let mut segs = Vec::with_capacity(nseg);
    for s in 0..nseg {
        let mut seg = [0f32; SEGMENT];
        for (k, v) in seg.iter_mut().enumerate() {
            let t = start + (s * SEGMENT + k) as f64 * step;
            let i = t.floor() as usize;
            if i < 1 || i + 2 >= r.len() {
                continue;
            }
            let mu = (t - i as f64) as f32;
            let (y0, y1, y2, y3) = (r[i - 1], r[i], r[i + 1], r[i + 2]);
            *v = y1 + 0.5 * mu * (y2 - y0 + mu * (2.0 * y0 - 5.0 * y1 + 4.0 * y2 - y3 + mu * (3.0 * (y1 - y2) + y3 - y0)));
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

/// The first field sync (by its PN511) and how many follow every 313 segments; whether each has
/// the middle PN63 inverted.
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

/// The least-squares equalizer: out[k] = Σ w[i] y[k + PRE − i]. Rows (k, wanted) from the field
/// syncs, then from its own decisions on data.
fn equalize(y: &[f32], first_sync: usize, inverted: &[bool]) -> Option<Vec<f32>> {
    let row = |k: usize| -> Option<&[f32]> {
        let hi = k + EQ_PRE + 1;
        (hi >= EQ_TAPS && hi <= y.len()).then(|| &y[hi - EQ_TAPS..hi])
    };
    let solve_rows = |rows: &mut dyn Iterator<Item = (usize, f32)>| -> Option<Vec<f32>> {
        let n = EQ_TAPS;
        let mut a = vec![0f64; n * n];
        let mut b = vec![0f64; n];
        for (k, d) in rows {
            let Some(x) = row(k) else { continue };
            // x is y[k+PRE-TAPS+1 ..= k+PRE]; tap i weighs y[k+PRE-i], x[TAPS-1-i].
            for i in 0..n {
                let xi = f64::from(x[n - 1 - i]);
                b[i] += xi * f64::from(d);
                for j in 0..=i {
                    a[i * n + j] += xi * f64::from(x[n - 1 - j]);
                }
            }
        }
        for i in 0..n {
            for j in 0..i {
                a[j * n + i] = a[i * n + j];
            }
        }
        solve(a, b, n).map(|w| w.into_iter().map(|v| v as f32).collect())
    };
    // Tap i weighs x[TAPS - 1 - i]: the taps reversed line up with the window.
    let apply = |wrev: &[f32], k: usize| -> f32 { row(k).map_or(0.0, |x| dot(wrev, x)) };
    let rev = |w: &[f32]| -> Vec<f32> { w.iter().rev().copied().collect() };
    let training: Vec<(usize, f32)> = inverted
        .iter()
        .enumerate()
        .flat_map(|(f, &inv)| {
            let base = (first_sync + f * FIELD_SEGMENTS) * SEGMENT;
            field_sync(inv).into_iter().enumerate().map(move |(j, d)| (base + j, d))
        })
        .take(40 * FIELD_SYNC_KNOWN)
        .collect();
    let mut w = solve_rows(&mut training.into_iter())?;
    for round in 0..2u64 {
        // Data symbols spread over the signal, a fixed pseudo-random pick.
        let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ round;
        let wrev = rev(&w);
        let picks: Vec<(usize, f32)> = (0..EQ_DECISION_ROWS)
            .filter_map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let k = (x % y.len() as u64) as usize;
                (k % SEGMENT >= SYNC_SYMBOLS).then(|| (k, slice(apply(&wrev, k))))
            })
            .collect();
        w = solve_rows(&mut picks.into_iter())?;
    }
    let wrev = rev(&w);
    Some((0..y.len()).map(|k| apply(&wrev, k)).collect())
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
    let z = receive_filter(iq, sample_rate_hz, centre_offset_hz);
    let (r, pilot_offset_hz) = pilot_to_dc(&z);
    drop(z);
    let (start, per) = timing(&r)?;
    let segs = symbols(&r, start, per);
    let (first, inverted) = field_syncs(&segs)?;
    let flat: Vec<f32> = segs.iter().flatten().copied().collect();
    let eq = equalize(&flat, first, &inverted)?;
    let segments: Vec<[f32; SEGMENT]> =
        eq.chunks_exact(SEGMENT).map(|c| c.try_into().unwrap_or([0.0; SEGMENT])).collect();
    Some(Demodulated {
        mer_db: mer(&segments),
        first_field_sync: Some(first),
        pilot_offset_hz,
        clock_ppm: (PERIOD / per - 1.0) * 1e6,
        segments,
    })
}
