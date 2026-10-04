//! Multiply-accumulate runs: a fixed-tap filter's outputs over a block, computed together. On the
//! A9 eight outputs at a time in NEON (stable inline asm; `.fpu neon` in each block, so any ARM
//! build assembles it): each tap's run of eight inputs times the tap into eight sums. Rust does
//! not vectorise `f32` sums on ARMv7, and its scalar VFP code waits on each step; one output at a
//! time in NEON pays a horizontal sum per output. Elsewhere, and for the outputs past the last
//! group of eight, four partial sums an output.
//!
//! Both runs add to `out`: a caller starts the sums (zeros, or a term of its own).

/// out[k] += Σ t[i]·x[k + i], i < `t.len()`, for every k < `out.len()` (`x` holds `out.len()` +
/// `t.len()` − 1 inputs or more).
pub fn plain(t: &[f32], x: &[f32], out: &mut [f32]) {
    assert!(t.is_empty() || x.len() + 1 >= out.len() + t.len());
    let done = plain_neon(t, x, out);
    for (k, o) in out.iter_mut().enumerate().skip(done) {
        *o += dot(t, &x[k..]);
    }
}

/// out[k] += Σ t[i]·(x[k + i] + x[k + span − i]), i < `t.len()`, for every k < `out.len()`: the
/// `t.len()` outer tap pairs of a symmetric run of span + 1 taps, each pair's two inputs added
/// before one multiply.
pub fn folded(t: &[f32], span: usize, x: &[f32], out: &mut [f32]) {
    assert!(x.len() >= out.len() + span && span + 1 >= 2 * t.len());
    let done = folded_neon(t, span, x, out);
    for (k, o) in out.iter_mut().enumerate().skip(done) {
        *o += folded_sum(t, &x[k..], &x[..=k + span]);
    }
}

/// Σ t[i]·w[i], four partial sums at once.
pub fn dot(t: &[f32], w: &[f32]) -> f32 {
    let mut s = [0.0f32; 4];
    let full = t.len() / 4 * 4;
    for (tc, wc) in t[..full].chunks_exact(4).zip(w[..full].chunks_exact(4)) {
        s[0] += tc[0] * wc[0];
        s[1] += tc[1] * wc[1];
        s[2] += tc[2] * wc[2];
        s[3] += tc[3] * wc[3];
    }
    for i in full..t.len() {
        s[0] += t[i] * w[i];
    }
    (s[0] + s[1]) + (s[2] + s[3])
}

/// Σ t[i]·(lo[i] + hi[n−1−i]) for the n = `t.len()` taps of a symmetric pair set (`hi` holds the
/// pairs' later samples, in input order), four partial sums at once.
fn folded_sum(t: &[f32], lo: &[f32], hi: &[f32]) -> f32 {
    let n = t.len();
    let (lo, hi) = (&lo[..n], &hi[hi.len() - n..]);
    let mut s = [0.0f32; 4];
    let full = n / 4 * 4;
    // lo[i]'s partner is hi[n−1−i]: the first full chunks of lo pair with hi's last ones, read
    // from the end.
    for ((tc, lc), hc) in t[..full].chunks_exact(4).zip(lo[..full].chunks_exact(4)).zip(hi[n - full..].rchunks_exact(4)) {
        s[0] += tc[0] * (lc[0] + hc[3]);
        s[1] += tc[1] * (lc[1] + hc[2]);
        s[2] += tc[2] * (lc[2] + hc[1]);
        s[3] += tc[3] * (lc[3] + hc[0]);
    }
    for i in full..n {
        s[0] += t[i] * (lo[i] + hi[n - 1 - i]);
    }
    (s[0] + s[1]) + (s[2] + s[3])
}

/// `plain`'s outputs in whole groups of eight. Returns the outputs done.
#[cfg(target_arch = "arm")]
fn plain_neon(t: &[f32], x: &[f32], out: &mut [f32]) -> usize {
    if t.is_empty() {
        return 0;
    }
    let groups = out.len() / 8;
    for g in 0..groups {
        let k = 8 * g;
        // SAFETY: tap i reads x[k + i .. k + i + 8], inside x as `plain` checks (i < t.len(),
        // k + 8 ≤ out.len()); reads and writes out[k .. k + 8] and only the registers it names.
        unsafe {
            std::arch::asm!(
                ".fpu neon",
                "vld1.32 {{d8-d11}}, [{dst}]",
                "2:",
                "vld1.32 {{d0-d3}}, [{x}]",
                "vld1.32 {{d12[]}}, [{t}]!",
                "add {x}, {x}, #4",
                "vmla.f32 q4, q0, d12[0]",
                "vmla.f32 q5, q1, d12[0]",
                "subs {n}, {n}, #1",
                "bne 2b",
                "vst1.32 {{d8-d11}}, [{dst}]",
                x = inout(reg) x.as_ptr().add(k) => _,
                t = inout(reg) t.as_ptr() => _,
                n = inout(reg) t.len() => _,
                dst = in(reg) out.as_mut_ptr().add(k),
                out("d0") _, out("d1") _, out("d2") _, out("d3") _,
                out("d8") _, out("d9") _, out("d10") _, out("d11") _, out("d12") _,
                options(nostack),
            );
        }
    }
    groups * 8
}

/// `folded`'s outputs in whole groups of eight: each tap's pair of eight-input runs added, then
/// multiplied by the tap. Returns the outputs done.
#[cfg(target_arch = "arm")]
fn folded_neon(t: &[f32], span: usize, x: &[f32], out: &mut [f32]) -> usize {
    if t.is_empty() {
        return 0;
    }
    let groups = out.len() / 8;
    for g in 0..groups {
        let k = 8 * g;
        // SAFETY: tap i reads x[k + i .. k + i + 8] and x[k + span − i .. k + span − i + 8],
        // inside x as `folded` checks (i < n ≤ (span + 1)/2, k + 8 ≤ out.len()); reads and
        // writes out[k .. k + 8] and only the registers it names.
        unsafe {
            std::arch::asm!(
                ".fpu neon",
                "vld1.32 {{d8-d11}}, [{dst}]",
                "2:",
                "vld1.32 {{d0-d3}}, [{lo}]",
                "vld1.32 {{d4-d7}}, [{hi}]",
                "vld1.32 {{d12[]}}, [{t}]!",
                "add {lo}, {lo}, #4",
                "sub {hi}, {hi}, #4",
                "vadd.f32 q0, q0, q2",
                "vadd.f32 q1, q1, q3",
                "vmla.f32 q4, q0, d12[0]",
                "vmla.f32 q5, q1, d12[0]",
                "subs {n}, {n}, #1",
                "bne 2b",
                "vst1.32 {{d8-d11}}, [{dst}]",
                lo = inout(reg) x.as_ptr().add(k) => _,
                hi = inout(reg) x.as_ptr().add(k + span) => _,
                t = inout(reg) t.as_ptr() => _,
                n = inout(reg) t.len() => _,
                dst = in(reg) out.as_mut_ptr().add(k),
                out("d0") _, out("d1") _, out("d2") _, out("d3") _, out("d4") _, out("d5") _,
                out("d6") _, out("d7") _, out("d8") _, out("d9") _, out("d10") _, out("d11") _,
                out("d12") _,
                options(nostack),
            );
        }
    }
    groups * 8
}

#[cfg(not(target_arch = "arm"))]
fn plain_neon(_: &[f32], _: &[f32], _: &mut [f32]) -> usize {
    0
}

#[cfg(not(target_arch = "arm"))]
fn folded_neon(_: &[f32], _: usize, _: &[f32], _: &mut [f32]) -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 40) as f32 / (1u64 << 24) as f32 - 0.5
            })
            .collect()
    }

    /// Every tap count from none to past the equalizer's 128, every output count around the
    /// groups of eight: the sums of the definitions, added to what was there.
    #[test]
    fn runs_add_their_sums() {
        let x = noise(600, 7);
        for taps in [0usize, 1, 2, 3, 7, 8, 9, 33, 64, 129] {
            let t = noise(taps, 11 + taps as u64);
            for outputs in [0usize, 1, 7, 8, 9, 16, 23, 200] {
                let start = noise(outputs, 3);
                let mut got = start.clone();
                plain(&t, &x, &mut got);
                for (k, g) in got.iter().enumerate() {
                    let want = start[k] as f64 + t.iter().enumerate().map(|(i, &c)| c as f64 * x[k + i] as f64).sum::<f64>();
                    assert!((*g as f64 - want).abs() < 1e-5 * (taps as f64 + 1.0), "plain {taps} taps, output {k}");
                }
                if taps == 0 {
                    continue;
                }
                let span = 2 * taps - 1 + taps % 2;
                let mut got = start.clone();
                folded(&t, span, &x, &mut got);
                for (k, g) in got.iter().enumerate() {
                    let want = start[k] as f64
                        + t.iter().enumerate().map(|(i, &c)| c as f64 * (x[k + i] as f64 + x[k + span - i] as f64)).sum::<f64>();
                    assert!((*g as f64 - want).abs() < 1e-5 * (taps as f64 + 1.0), "folded {taps} pairs, output {k}");
                }
            }
        }
    }
}
