//! The 8-VSB demodulator: a channel's IQ (its centre at a known offset, the 6 MHz channel inside
//! the sampled band, as at the unit's 10 MSPS) to equalized symbols, segment by segment.
//!
//! 1. **Pilot.** RS/4 below the centre. The input turned up RS/4 is summed over 2 µs sub-blocks;
//!    the pilot's frequency comes from the slope of their phase over short blocks, then its phase
//!    from 0.2 ms blocks, a straight line between their centres.
//! 2. **Matched filter at any instant.** 8-VSB's receive filter is, in the channel-centred signal,
//!    a real root-raised cosine at RS/2 symbols a second (rolloff 0.1152). It is tabulated at 64
//!    fractional offsets between input samples, so the filtered signal at an instant is one sum
//!    over the inputs around it (NEON on the A9, two instants a pass over their shared inputs).
//!    Turned up RS/4 less the pilot's phase, its real part is the 8-level signal (the halves of
//!    the vestige add up flat about the pilot). The filter spans ±10 symbols of RS/2: the
//!    captures decode alike at ±12, and lose packets at ±8.
//! 3. **Timing.** From the segment syncs (+5 −5 −5 +5 every 832 symbols): the real signal at two
//!    samples a symbol around where each is expected, summed over blocks of segments and tracked
//!    block to block, then fitted to a line (the sample clock's error is a constant rate).
//! 4. **Symbols.** The real signal at each fitted instant; the pilot's DC taken out; scaled by
//!    the syncs.
//! 5. **Equalizer.** Field syncs found by their PN511. A least-squares equalizer is trained on
//!    their known symbols, then on its own decisions over runs of data, and applied by fast
//!    convolution. A run's normal equations take one row of dot products; the rest follow down
//!    the diagonals.
//!
//! The passes over the whole signal (the pilot's sums, the symbols, the convolution) run on both
//! of the A9's cores, half each.

use realfft::RealFftPlanner;
use rustfft::num_complex::{Complex32, Complex64};

use super::on_both_cores;
use super::vsb::{field_sync, pn511, pn63, FIELD_SEGMENTS, SEGMENT, SEGMENT_SYNC, SYNC_SYMBOLS};

/// 8-VSB's symbol rate: 4.5 MHz × 684 / 286.
pub const SYMBOL_RATE: f64 = 4_500_000.0 * 684.0 / 286.0;
const ROLLOFF: f64 = 0.1152;
/// The matched filter's half-span, in symbols of RS/2.
const FILTER_HALF_SPAN: f64 = 10.0;
/// Fractional offsets between input samples the matched filter is tabulated at (a timing error
/// of 1/128 sample at most).
const PHASE_BITS: u32 = 6;
const PHASES: usize = 1 << PHASE_BITS;
/// Zeros either side of each tabulated row: a pair's later instant may start this many inputs
/// after the earlier one and still share its inputs.
const PAIR_REACH: usize = 4;
/// The pilot's sub-blocks, then the blocks its frequency and its phase come from, in seconds.
const PILOT_SUB_BLOCK: f64 = 2e-6;
const PILOT_BLOCKS: [f64; 2] = [20e-6, 2e-4];
/// Samples a segment at two a symbol.
const PERIOD: f64 = (2 * SEGMENT) as f64;
/// Segments summed for one timing estimate.
const TIMING_BLOCK: usize = 32;
/// Segments searched at every phase for where the syncs are.
const SYNC_SEARCH: usize = 256;
const EQ_TAPS: usize = 128;
/// Equalizer taps ahead of the symbol (pre-echoes).
const EQ_PRE: usize = 32;
/// Field syncs the equalizer trains on; then rounds on its own decisions, each over runs of data
/// spread across the signal.
const EQ_TRAINING_FIELDS: usize = 20;
const EQ_ROUNDS: usize = 2;
const EQ_DECISION_RUNS: usize = 16;
const EQ_RUN: usize = 1024;
/// The fast convolution's transform size (on the A9 1024 beat 2048, 4096 and 8192).
const FFT_LEN: usize = 1024;

#[derive(Debug, Clone)]
pub struct Demodulated {
    /// Equalized symbols (pilot removed, ±1..±7), segment after segment.
    pub symbols: Vec<f32>,
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

/// Unwrapped phases of the sub-block sums (`sub` samples each) over blocks of `n`, each sum first
/// turned by `step` radians a sample at its sub-block's centre.
fn block_phases(sums: &[Complex32], sub: usize, n: usize, step: f64) -> Vec<f64> {
    let mut turn = Phasor::new(step * (sub - 1) as f64 / 2.0, step * sub as f64);
    let mut out = Vec::with_capacity(sums.len() / n);
    let mut last = 0.0;
    for block in sums.chunks_exact(n) {
        let m: Complex32 = block.iter().map(|&s| s * turn.next()).sum();
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

/// The phase to turn the filtered signal by, at each input instant, for its real part to be the
/// symbols: up RS/4, less the pilot's own phase.
struct Carrier {
    /// Radians a sample: RS/4 less the pilot's offset.
    rate: f64,
    offset_hz: f64,
    /// Samples a phase block.
    len: usize,
    /// The pilot's remaining phase at each block's centre.
    phases: Vec<f64>,
}

impl Carrier {
    /// From the input at `fs`: short blocks take a pilot offset up to ±25 kHz, then longer ones the
    /// rest and the phase. The pilot's phase is the same before and after the matched filter
    /// (real and centred on the instant).
    fn find(x: &[Complex32], fs: f64) -> Option<Carrier> {
        let two_pi = 2.0 * std::f64::consts::PI;
        let up = two_pi * SYMBOL_RATE / 4.0 / fs;
        let sub = ((PILOT_SUB_BLOCK * fs).round() as usize).max(1);
        // The turn across a sub-block from a table; each sub-block's own start stepped once.
        let within: Vec<Complex32> = (0..sub).map(|k| Complex32::from_polar(1.0, (up * k as f64) as f32)).collect();
        let mut sums = vec![Complex32::new(0.0, 0.0); x.len() / sub];
        on_both_cores(&mut sums, 1, |first, out| {
            let mut start = Phasor::new(up * (first * sub) as f64, up * sub as f64);
            for (c, s) in x[first * sub..].chunks_exact(sub).zip(out) {
                *s = start.next() * c.iter().zip(&within).map(|(&v, &w)| v * w).sum::<Complex32>();
            }
        });
        let mut offset = 0.0;
        let mut n = 1;
        for block_s in PILOT_BLOCKS {
            n = ((block_s / PILOT_SUB_BLOCK).round() as usize).max(1);
            let ph = block_phases(&sums, sub, n, -two_pi * offset / fs);
            if ph.len() < 4 {
                return None;
            }
            offset += slope(&ph) / (two_pi * (n * sub) as f64 / fs);
        }
        let phases = block_phases(&sums, sub, n, -two_pi * offset / fs);
        Some(Carrier { rate: up - two_pi * offset / fs, offset_hz: offset, len: n * sub, phases })
    }

    /// The turn at input instant `u`.
    fn at(&self, u: f64) -> f64 {
        let n = self.phases.len();
        let x = ((u + 0.5) / self.len as f64 - 0.5).clamp(0.0, (n - 1) as f64);
        let i = (x as usize).min(n.saturating_sub(2));
        let f = x - i as f64;
        let p = self.phases[i] * (1.0 - f) + self.phases.get(i + 1).copied().unwrap_or(self.phases[i]) * f;
        self.rate * u - p
    }
}

/// The matched filter at any instant between input samples.
struct Matched {
    /// Inputs a sum: i + 1 − taps/2 ..= i + taps/2 around an instant i + f, 0 ≤ f ≤ 1. A whole
    /// number of groups of eight.
    taps: usize,
    /// The taps at f = p / PHASES for p = 0..=PHASES, a row each: PAIR_REACH zeros, the taps,
    /// PAIR_REACH zeros.
    table: Vec<f32>,
}

impl Matched {
    fn new(fs: f64) -> Matched {
        let rb = SYMBOL_RATE / 2.0;
        let h = ((FILTER_HALF_SPAN / rb * fs).ceil() as usize + 1).div_ceil(4) * 4;
        let taps = 2 * h;
        let table = (0..=PHASES)
            .flat_map(|p| {
                let f = p as f64 / PHASES as f64;
                // Input j of the sum is f + h − 1 − j samples before the instant.
                let row = (0..taps).map(move |j| (rrc((f + h as f64 - 1.0 - j as f64) / fs * rb) * rb / fs) as f32);
                std::iter::repeat_n(0.0, PAIR_REACH).chain(row).chain(std::iter::repeat_n(0.0, PAIR_REACH))
            })
            .collect();
        Matched { taps, table }
    }

    /// Where row p starts in the table.
    fn row(&self, p: usize) -> usize {
        p * (self.taps + 2 * PAIR_REACH)
    }

    /// The filtered signal at input instant `tau`; zero where its inputs leave `x`.
    fn at(&self, x: &[Complex32], tau: f64) -> Complex32 {
        let mut z = Complex32::new(0.0, 0.0);
        let h = self.taps / 2;
        if tau >= (h - 1) as f64 {
            let i = tau as usize;
            let p = ((tau - i as f64) * PHASES as f64 + 0.5) as usize;
            let start = i + 1 - h;
            if let Some(w) = x.get(start..start + self.taps) {
                let r = self.row(p) + PAIR_REACH;
                dot_iq(&self.table[r..r + self.taps], w, &mut z);
            }
        }
        z
    }

    /// The filtered signal at the instants u0 + k·du, k < `z.len()`. Where all of them are well
    /// inside the input, the instants step in 32.32 fixed point: the ARM side only finds each
    /// sum's inputs and taps, and never waits on NEON (a float-to-integer conversion, like a
    /// NEON result, crosses to the ARM registers and stalls it on the A9). Two instants a pass:
    /// the later one's taps from its padded row, shifted to line up with the earlier one's inputs.
    fn run(&self, x: &[Complex32], u0: f64, du: f64, z: &mut [Complex32]) {
        const ONE: f64 = 4_294_967_296.0;
        let (h, taps) = (self.taps / 2, self.taps);
        let span = taps + PAIR_REACH;
        let last = u0 + du * z.len().saturating_sub(1) as f64;
        if du < 0.0 || u0 < h as f64 || last + (h + PAIR_REACH + 2) as f64 > x.len() as f64 {
            for (k, v) in z.iter_mut().enumerate() {
                *v = self.at(x, u0 + k as f64 * du);
            }
            return;
        }
        // An instant's first input, and its row (the fraction rounded to 1/PHASES).
        let place = |pos: u64| {
            let p = ((pos & 0xFFFF_FFFF) + (1 << (31 - PHASE_BITS))) >> (32 - PHASE_BITS);
            ((pos >> 32) as usize + 1 - h, self.row(p as usize))
        };
        let (mut pos, step) = ((u0 * ONE) as u64, (du * ONE) as u64);
        let mut pairs = z.chunks_exact_mut(2);
        for pair in &mut pairs {
            let ((a, ra), (b, rb)) = (place(pos), place(pos + step));
            pos += 2 * step;
            let d = b - a;
            if d <= PAIR_REACH {
                let (ta, tb) = (ra + PAIR_REACH, rb + PAIR_REACH - d);
                dot_iq_pair(&self.table[ta..ta + span], &self.table[tb..tb + span], &x[a..a + span], pair);
            } else {
                dot_iq(&self.table[ra + PAIR_REACH..ra + PAIR_REACH + taps], &x[a..a + taps], &mut pair[0]);
                dot_iq(&self.table[rb + PAIR_REACH..rb + PAIR_REACH + taps], &x[b..b + taps], &mut pair[1]);
            }
        }
        if let [v] = pairs.into_remainder() {
            let (a, ra) = place(pos);
            dot_iq(&self.table[ra + PAIR_REACH..ra + PAIR_REACH + taps], &x[a..a + taps], v);
        }
    }
}

/// `out` = Σ t[i]·x[i], real taps on complex inputs; `t.len()` a non-zero multiple of eight.
/// Eight inputs a step in NEON: their I and Q split apart as they load, each times four taps into
/// two sums. The sum is stored from NEON, so the ARM side goes on without it.
#[cfg(target_arch = "arm")]
#[inline(always)]
fn dot_iq(t: &[f32], x: &[Complex32], out: &mut Complex32) {
    assert!(!t.is_empty() && t.len() % 8 == 0 && x.len() >= t.len());
    // SAFETY: reads t[..t.len()] and x[..t.len()], inside both as checked above; writes `out`
    // (8 bytes: re, im) and only the registers it names.
    unsafe {
        std::arch::asm!(
            ".fpu neon",
            "vmov.i32 q6, #0",
            "vmov.i32 q7, #0",
            "2:",
            "vld2.32 {{d0-d3}}, [{x}]!",
            "vld1.32 {{d8-d9}}, [{t}]!",
            "vld2.32 {{d4-d7}}, [{x}]!",
            "vld1.32 {{d10-d11}}, [{t}]!",
            "vmla.f32 q6, q0, q4",
            "vmla.f32 q7, q1, q4",
            "vmla.f32 q6, q2, q5",
            "vmla.f32 q7, q3, q5",
            "subs {n}, {n}, #1",
            "bne 2b",
            "vpadd.f32 d12, d12, d13",
            "vpadd.f32 d14, d14, d15",
            "vpadd.f32 d12, d12, d14",
            "vst1.32 {{d12}}, [{dst}]",
            x = inout(reg) x.as_ptr() => _,
            t = inout(reg) t.as_ptr() => _,
            n = inout(reg) t.len() / 8 => _,
            dst = in(reg) out as *mut Complex32,
            out("d0") _, out("d1") _, out("d2") _, out("d3") _, out("d4") _, out("d5") _,
            out("d6") _, out("d7") _, out("d8") _, out("d9") _, out("d10") _, out("d11") _,
            out("d12") _, out("d13") _, out("d14") _, out("d15") _,
            options(nostack),
        );
    }
}

/// `out[0]` = Σ ta[i]·x[i] and `out[1]` = Σ tb[i]·x[i] over the same inputs; `x.len()` a non-zero
/// multiple of four, the taps as long. Four inputs a step in NEON, each load feeding both sums.
#[cfg(target_arch = "arm")]
#[inline(always)]
fn dot_iq_pair(ta: &[f32], tb: &[f32], x: &[Complex32], out: &mut [Complex32]) {
    let n = x.len();
    assert!(n != 0 && n % 4 == 0 && ta.len() == n && tb.len() == n && out.len() == 2);
    // SAFETY: reads ta, tb and x (n each, as checked above); writes out[..2] (16 bytes) and only
    // the registers it names.
    unsafe {
        std::arch::asm!(
            ".fpu neon",
            "vmov.i32 q4, #0",
            "vmov.i32 q5, #0",
            "vmov.i32 q6, #0",
            "vmov.i32 q7, #0",
            "2:",
            "vld2.32 {{d0-d3}}, [{x}]!",
            "vld1.32 {{d4-d5}}, [{ta}]!",
            "vld1.32 {{d6-d7}}, [{tb}]!",
            "vmla.f32 q4, q0, q2",
            "vmla.f32 q5, q1, q2",
            "vmla.f32 q6, q0, q3",
            "vmla.f32 q7, q1, q3",
            "subs {n}, {n}, #1",
            "bne 2b",
            "vpadd.f32 d8, d8, d9",
            "vpadd.f32 d10, d10, d11",
            "vpadd.f32 d12, d12, d13",
            "vpadd.f32 d14, d14, d15",
            "vpadd.f32 d8, d8, d10",
            "vpadd.f32 d9, d12, d14",
            "vst1.32 {{d8-d9}}, [{dst}]",
            x = inout(reg) x.as_ptr() => _,
            ta = inout(reg) ta.as_ptr() => _,
            tb = inout(reg) tb.as_ptr() => _,
            n = inout(reg) n / 4 => _,
            dst = in(reg) out.as_mut_ptr(),
            out("d0") _, out("d1") _, out("d2") _, out("d3") _, out("d4") _, out("d5") _,
            out("d6") _, out("d7") _, out("d8") _, out("d9") _, out("d10") _, out("d11") _,
            out("d12") _, out("d13") _, out("d14") _, out("d15") _,
            options(nostack),
        );
    }
}

#[cfg(not(target_arch = "arm"))]
fn dot_iq_pair(ta: &[f32], tb: &[f32], x: &[Complex32], out: &mut [Complex32]) {
    dot_iq(ta, x, &mut out[0]);
    dot_iq(tb, x, &mut out[1]);
}

#[cfg(not(target_arch = "arm"))]
fn dot_iq(t: &[f32], x: &[Complex32], out: &mut Complex32) {
    let (mut re, mut im) = ([0f32; 4], [0f32; 4]);
    for (tc, xc) in t.chunks_exact(4).zip(x.chunks_exact(4)) {
        for l in 0..4 {
            re[l] += tc[l] * xc[l].re;
            im[l] += tc[l] * xc[l].im;
        }
    }
    *out = Complex32::new((re[0] + re[1]) + (re[2] + re[3]), (im[0] + im[1]) + (im[2] + im[3]));
}

/// The real signal (the 8 levels and the pilot's DC) at any instant.
struct Real<'a> {
    x: &'a [Complex32],
    matched: Matched,
    carrier: Carrier,
    /// Input samples per sample at two a symbol.
    ratio: f64,
}

impl Real<'_> {
    /// `out.len()` samples from `t0`, `step` apart, in samples of two a symbol from the start of
    /// the input. Over a run this short the carrier's turn is close to linear, so one phasor steps
    /// through it.
    fn run(&self, t0: f64, step: f64, out: &mut [f32]) {
        let n = out.len();
        if n == 0 {
            return;
        }
        let (u0, du) = (t0 * self.ratio, step * self.ratio);
        let mut z = vec![Complex32::new(0.0, 0.0); n];
        self.matched.run(self.x, u0, du, &mut z);
        let (a0, a1) = (self.carrier.at(u0), self.carrier.at(u0 + du * (n - 1) as f64));
        let mut turn = Complex32::from_polar(1.0, (a0 % (2.0 * std::f64::consts::PI)) as f32);
        let w = Complex32::from_polar(1.0, ((a1 - a0) / (n.max(2) - 1) as f64) as f32);
        for (v, z) in out.iter_mut().zip(&z) {
            *v = z.re * turn.re - z.im * turn.im;
            turn *= w;
        }
    }

    /// Samples at two a symbol the input holds.
    fn len(&self) -> usize {
        (self.x.len().saturating_sub(self.matched.taps) as f64 / self.ratio) as usize
    }
}

/// Segment syncs: (first sync's sample, samples a segment) at two samples a symbol, from blocks
/// of segments tracked along the signal and a line fitted through them.
fn timing(sig: &Real) -> Option<(f64, f64)> {
    let period = PERIOD as usize;
    let len = sig.len();
    let nseg = len.saturating_sub(8) / period;
    if nseg < 2 * TIMING_BLOCK {
        return None;
    }
    // Where the syncs are, from the first segments summed.
    let search = nseg.min(SYNC_SEARCH);
    let mut first = vec![0f32; search * period + 8];
    for (s, run) in first.chunks_mut(period).enumerate() {
        sig.run((s * period) as f64, 1.0, run);
    }
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
    // Around each expected sync: its correlation at 9 places, a sample apart.
    let mut near = [0f32; 15];
    let mut s0 = 0;
    while s0 + TIMING_BLOCK <= nseg {
        let mut win = [0f32; 9];
        for s in s0..s0 + TIMING_BLOCK {
            let base = (start + s as f64 * per).round() - 4.0;
            if base < 0.0 || base as usize + near.len() > len {
                continue;
            }
            sig.run(base, 1.0, &mut near);
            for (d, w) in win.iter_mut().enumerate() {
                *w += near[d] - near[d + 2] - near[d + 4] + near[d + 6];
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

/// Symbols at the fitted instants, segment after segment; the pilot's DC out and the syncs at ±5.
fn symbols(sig: &Real, start: f64, per: f64) -> Vec<f32> {
    let step = per / SEGMENT as f64;
    let nseg = ((sig.len() as f64 - start) / per).floor().max(0.0) as usize;
    let mut y = vec![0f32; nseg * SEGMENT];
    on_both_cores(&mut y, SEGMENT, |first, half| {
        for (s, seg) in half.chunks_exact_mut(SEGMENT).enumerate() {
            sig.run(start + (first / SEGMENT + s) as f64 * per, step, seg);
        }
    });
    let (mut sum, mut n) = (0f64, 0usize);
    for seg in y.chunks_exact(SEGMENT) {
        sum += seg[SYNC_SYMBOLS..].iter().map(|&v| f64::from(v)).sum::<f64>();
        n += SEGMENT - SYNC_SYMBOLS;
    }
    let dc = if n > 0 { (sum / n as f64) as f32 } else { 0.0 };
    let sync_energy: f32 = SEGMENT_SYNC.iter().map(|s| s * s).sum();
    let gain = y.chunks_exact(SEGMENT).map(|seg| (0..4).map(|k| (seg[k] - dc) * SEGMENT_SYNC[k]).sum::<f32>()).sum::<f32>()
        / (nseg.max(1) as f32 * sync_energy);
    if gain.abs() > 1e-9 {
        let scale = 1.0 / gain;
        y.iter_mut().for_each(|v| *v = (*v - dc) * scale);
    }
    y
}

/// The first field sync (by its PN511) in the symbols `y`, and for it and each one every 313
/// segments after, whether its middle PN63 is inverted.
fn field_syncs(y: &[f32]) -> Option<(usize, Vec<bool>)> {
    let segs: Vec<&[f32]> = y.chunks_exact(SEGMENT).collect();
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

/// The nearest of the eight levels. (v + 7)/2 rounded is (v + 8)/2 truncated over the levels'
/// range (no rounding call: it is a library call on the A9); `as` saturates.
fn slice(v: f32) -> f32 {
    (2 * (((v + 8.0) * 0.5) as i32).clamp(0, 7) - 7) as f32
}

/// y ⊛ w into `out`: out[i] = c[from + i], c[m] = Σ w[i] y[m − i] (y zero before 0 and from
/// y.len()), by overlap-save.
fn convolve(y: &[f32], w: &[f32], from: usize, out: &mut [f32]) {
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
    let scale = 1.0 / FFT_LEN as f32;
    for (b, chunk) in out.chunks_mut(step).enumerate() {
        // c[from + b·step ..< + step] from y[from + b·step − (m − 1) ..< + FFT_LEN].
        let lo = (from + b * step) as isize - (m as isize - 1);
        let (a, z) = (lo.max(0) as usize, ((lo + FFT_LEN as isize).max(0) as usize).min(y.len()));
        buf.fill(0.0);
        if a < z {
            let at = (a as isize - lo) as usize;
            buf[at..at + (z - a)].copy_from_slice(&y[a..z]);
        }
        let _ = fwd.process(&mut buf, &mut spec);
        for (s, &hk) in spec.iter_mut().zip(&h) {
            *s *= hk;
        }
        let _ = inv.process(&mut spec, &mut buf);
        for (o, v) in chunk.iter_mut().zip(&buf[m - 1..]) {
            *o = v * scale;
        }
    }
}

/// The rows whose windows lie inside y (of `y_len`): row k weighs y[k + PRE − i] by tap i.
fn rows_inside(y_len: usize) -> std::ops::Range<usize> {
    (EQ_TAPS - 1).saturating_sub(EQ_PRE)..y_len.saturating_sub(EQ_PRE)
}

/// The equalizer's output at row k.
fn output(y: &[f32], w: &[f32], k: usize) -> f32 {
    let top = k + EQ_PRE;
    w.iter().enumerate().map(|(i, &c)| c * y[top - i]).sum()
}

/// Adds the normal equations of the rows k0, k0 + 1, … (their wanted values `wanted`, every
/// window inside y) to `a` (n × n) and `b`. Entry (0, d) of the run's matrix is a dot product;
/// entry (i + 1, j + 1) is entry (i, j) with the row before the run in and the run's last row out.
fn add_run(y: &[f32], k0: usize, wanted: &[f32], a: &mut [f64], b: &mut [f64]) {
    let n = EQ_TAPS;
    let len = wanted.len();
    // Row k0 + m's window, newest first: y[top + m − i] for tap i.
    let top = k0 + EQ_PRE;
    let prod = |p: usize, q: usize| f64::from(y[p]) * f64::from(y[q]);
    for (i, bi) in b.iter_mut().enumerate() {
        *bi += y[top - i..top - i + len].iter().zip(wanted).map(|(&v, &d)| f64::from(v) * f64::from(d)).sum::<f64>();
    }
    for d in 0..n {
        let mut r: f64 = (0..len).map(|m| prod(top + m, top + m - d)).sum();
        for i in 0..n - d {
            a[i * n + i + d] += r;
            if d > 0 {
                a[(i + d) * n + i] += r;
            }
            if i + d + 1 < n {
                let (enter, leave) = (top - 1 - i, top + len - 1 - i);
                r += prod(enter, enter - d) - prod(leave, leave - d);
            }
        }
    }
}

/// The taps w minimizing Σ (wanted − Σ w[i] y[k + PRE − i])² over runs of rows (first row,
/// wanted values); rows whose windows leave y are left out.
fn least_squares(y: &[f32], runs: &[(usize, Vec<f32>)]) -> Option<Vec<f32>> {
    let n = EQ_TAPS;
    let (mut a, mut b) = (vec![0f64; n * n], vec![0f64; n]);
    let inside = rows_inside(y.len());
    for (k0, wanted) in runs {
        let lo = (*k0).max(inside.start);
        let hi = (k0 + wanted.len()).min(inside.end);
        if lo < hi {
            add_run(y, lo, &wanted[lo - k0..hi - k0], &mut a, &mut b);
        }
    }
    solve(a, b, n).map(|w| w.into_iter().map(|v| v as f32).collect())
}

/// The least-squares equalizer: out[k] = Σ w[i] y[k + PRE − i]. Trained on the field syncs' known
/// symbols, then on its own decisions over runs spread across the signal, each round's between
/// the last's.
fn equalize(y: &[f32], first_sync: usize, inverted: &[bool]) -> Option<Vec<f32>> {
    let training: Vec<(usize, Vec<f32>)> = inverted
        .iter()
        .take(EQ_TRAINING_FIELDS)
        .enumerate()
        .map(|(f, &inv)| ((first_sync + f * FIELD_SEGMENTS) * SEGMENT, field_sync(inv)))
        .collect();
    let mut w = least_squares(y, &training)?;
    let inside = rows_inside(y.len());
    let spacing = inside.len().saturating_sub(EQ_RUN) / EQ_DECISION_RUNS;
    for round in 0..EQ_ROUNDS {
        let runs: Vec<(usize, Vec<f32>)> = (0..EQ_DECISION_RUNS)
            .map(|r| inside.start + r * spacing + round * spacing / EQ_ROUNDS)
            .filter(|&k0| k0 + EQ_RUN <= inside.end)
            .map(|k0| (k0, (k0..k0 + EQ_RUN).map(|k| slice(output(y, &w, k))).collect()))
            .collect();
        w = least_squares(y, &runs)?;
    }
    // out[k] = (y ⊛ w)[k + PRE].
    let mut out = vec![0f32; y.len()];
    on_both_cores(&mut out, 1, |first, half| convolve(y, &w, EQ_PRE + first, half));
    Some(out)
}

/// Modulation error ratio of the data symbols, from every fourth segment (plenty for the measure).
fn mer(y: &[f32]) -> f32 {
    let (mut sig, mut err) = (0f64, 0f64);
    for seg in y.chunks_exact(SEGMENT).step_by(4) {
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
    let centred: Vec<Complex32>;
    let x = if centre_offset_hz == 0.0 {
        iq
    } else {
        let mut shift = Phasor::new(0.0, -2.0 * std::f64::consts::PI * centre_offset_hz / sample_rate_hz);
        centred = iq.iter().map(|&s| s * shift.next()).collect();
        &centred
    };
    let carrier = Carrier::find(x, sample_rate_hz)?;
    let sig = Real { x, matched: Matched::new(sample_rate_hz), carrier, ratio: sample_rate_hz / (2.0 * SYMBOL_RATE) };
    let (start, per) = timing(&sig)?;
    let y = symbols(&sig, start, per);
    let pilot_offset_hz = sig.carrier.offset_hz;
    let (first, inverted) = field_syncs(&y)?;
    let symbols = equalize(&y, first, &inverted)?;
    Some(Demodulated {
        mer_db: mer(&symbols),
        first_field_sync: Some(first),
        pilot_offset_hz,
        clock_ppm: (PERIOD / per - 1.0) * 1e6,
        symbols,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_convolution_matches_the_direct_sum() {
        let y: Vec<f32> = (0..10_000).map(|i| ((i * 7919) % 101) as f32 - 50.0).collect();
        let w: Vec<f32> = (0..EQ_TAPS).map(|i| ((i * 31) % 17) as f32 / 17.0 - 0.5).collect();
        let skip = 32;
        let mut c = vec![f32::NAN; y.len() + 8];
        convolve(&y, &w, skip, &mut c);
        for m in [32, 33, 127, 128, 4000, 4001 + 3969, 9999, 10_039] {
            let direct: f32 = (0..w.len()).filter(|&i| m >= i && m - i < y.len()).map(|i| w[i] * y[m - i]).sum();
            assert!((c[m - skip] - direct).abs() < 1e-2 * (1.0 + direct.abs()), "{m}: {} {direct}", c[m - skip]);
        }
    }

    #[test]
    fn the_matched_filter_at_an_instant_is_its_sum() {
        let fs = 10e6;
        let rb = SYMBOL_RATE / 2.0;
        let m = Matched::new(fs);
        assert_eq!(m.taps % 8, 0);
        let x: Vec<Complex32> =
            (0..400).map(|i| Complex32::new(((i * 7919) % 101) as f32 - 50.0, ((i * 104_729) % 97) as f32 - 48.0)).collect();
        let h = m.taps / 2;
        // Instants on the table's offsets, the first and last whose inputs are all there.
        for tau in [(h - 1) as f64, 100.0 + 37.0 / 64.0, 123.5, 200.0 + 63.0 / 64.0, (399 - h) as f64 + 0.5] {
            let i = tau as usize;
            let want: Complex64 =
                (i + 1 - h..=i + h).map(|n| Complex64::new(x[n].re.into(), x[n].im.into()) * (rrc((tau - n as f64) / fs * rb) * rb / fs)).sum();
            let got = m.at(&x, tau);
            assert!((f64::from(got.re) - want.re).abs() < 1e-3 && (f64::from(got.im) - want.im).abs() < 1e-3, "{tau}: {got} {want}");
        }
        // A run in fixed point lands on the same sums (instants a whole number of 1/2^32 apart),
        // in pairs and the odd one out.
        for (u0, du) in [(60.0 + 5.0 / 64.0, 0.9375), (61.0, 0.46875), (62.5, 2.25)] {
            let mut z = vec![Complex32::new(0.0, 0.0); 101];
            m.run(&x, u0, du, &mut z);
            for (k, v) in z.iter().enumerate() {
                let want = m.at(&x, u0 + k as f64 * du);
                assert!((*v - want).norm() < 1e-3 && want.norm() > 0.0, "{u0} {du} {k}: {v} {want}");
            }
        }
        assert_eq!(m.at(&x, (h - 2) as f64), Complex32::new(0.0, 0.0));
        assert_eq!(m.at(&x, (400 - h) as f64), Complex32::new(0.0, 0.0));
    }

    #[test]
    fn the_runs_normal_equations_match_row_by_row_sums() {
        let y: Vec<f32> = (0..3000).map(|i| ((i * 7919) % 101) as f32 / 10.0 - 5.0).collect();
        let runs: Vec<(usize, Vec<f32>)> =
            vec![(0, (0..300).map(|k| (k % 7) as f32 - 3.0).collect()), (1500, (0..500).map(|k| (k % 5) as f32 - 2.0).collect())];
        let n = EQ_TAPS;
        let (mut a, mut b) = (vec![0f64; n * n], vec![0f64; n]);
        let inside = rows_inside(y.len());
        for (k0, wanted) in &runs {
            for (m, &d) in wanted.iter().enumerate() {
                let k = k0 + m;
                if !inside.contains(&k) {
                    continue;
                }
                let top = k + EQ_PRE;
                for i in 0..n {
                    b[i] += f64::from(y[top - i]) * f64::from(d);
                    for j in 0..n {
                        a[i * n + j] += f64::from(y[top - i]) * f64::from(y[top - j]);
                    }
                }
            }
        }
        let want = solve(a, b, n).unwrap();
        let got = least_squares(&y, &runs).unwrap();
        for (g, w) in got.iter().zip(&want) {
            assert!((f64::from(*g) - w).abs() < 1e-4 * (1.0 + w.abs()), "{g} {w}");
        }
    }

    #[test]
    fn the_slicer_takes_the_nearest_level() {
        for (v, want) in [(-9.0, -7.0), (-6.1, -7.0), (-5.9, -5.0), (-0.1, -1.0), (0.1, 1.0), (5.9, 5.0), (6.1, 7.0), (30.0, 7.0)] {
            assert_eq!(slice(v), want, "{v}");
        }
    }
}
