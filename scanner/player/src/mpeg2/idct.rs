//! The 8×8 inverse DCT (H.262 Annex A): rows then columns, each an 8-point transform split into
//! its even and odd halves, in integers with cos(kπ/16) scaled by 2^13 and four extra bits kept
//! between the passes (the columns in 64 bits); the result saturated to [-256, 255]. Rows that
//! are all zero are passed over.

const C1: i64 = 8035;
const C2: i64 = 7568;
const C3: i64 = 6811;
const C4: i64 = 5793;
const C5: i64 = 4551;
const C6: i64 = 3135;
const C7: i64 = 1598;

/// One 8-point transform, y[n] = Σ C(k) X[k] cos((2n+1)kπ/16) (C(0) = 1/√2), scaled by 2^13.
#[inline(always)]
fn transform(x: [i64; 8]) -> [i64; 8] {
    let a0 = (x[0] + x[4]) * C4;
    let a1 = (x[0] - x[4]) * C4;
    let a2 = x[2] * C6 - x[6] * C2;
    let a3 = x[2] * C2 + x[6] * C6;
    let e = [a0 + a3, a1 + a2, a1 - a2, a0 - a3];
    let o = [
        x[1] * C1 + x[3] * C3 + x[5] * C5 + x[7] * C7,
        x[1] * C3 - x[3] * C7 - x[5] * C1 - x[7] * C5,
        x[1] * C5 - x[3] * C1 + x[5] * C7 + x[7] * C3,
        x[1] * C7 - x[3] * C5 + x[5] * C3 - x[7] * C1,
    ];
    [e[0] + o[0], e[1] + o[1], e[2] + o[2], e[3] + o[3], e[3] - o[3], e[2] - o[2], e[1] - o[1], e[0] - o[0]]
}

/// F[v * 8 + u] in, f[y * 8 + x] out (in place).
pub fn idct(block: &mut [i32; 64]) {
    // Rows: (1/2)·transform, four extra bits kept: >> (13 + 1 - 4).
    for v in 0..8 {
        let row = &mut block[v * 8..v * 8 + 8];
        if row.iter().all(|&c| c == 0) {
            continue;
        }
        let y = transform(std::array::from_fn(|u| i64::from(row[u])));
        for (o, t) in row.iter_mut().zip(y) {
            *o = ((t + (1 << 9)) >> 10) as i32;
        }
    }
    // Columns: (1/2)·transform, the extra bits dropped: >> (13 + 1 + 4).
    for u in 0..8 {
        let y = transform(std::array::from_fn(|v| i64::from(block[v * 8 + u])));
        for (v, t) in y.into_iter().enumerate() {
            block[v * 8 + u] = ((t + (1 << 17)) >> 18).clamp(-256, 255) as i32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn c(k: usize) -> f64 {
        if k == 0 {
            std::f64::consts::FRAC_1_SQRT_2
        } else {
            1.0
        }
    }

    /// Annex A's saturated mathematical integer-number IDCT.
    fn reference(f: &[i32; 64]) -> [i32; 64] {
        std::array::from_fn(|i| {
            let (y, x) = (i / 8, i % 8);
            let mut s = 0.0;
            for v in 0..8 {
                for u in 0..8 {
                    s += c(u) * c(v) * f64::from(f[v * 8 + u]) * (((2 * x + 1) * u) as f64 * PI / 16.0).cos() * (((2 * y + 1) * v) as f64 * PI / 16.0).cos();
                }
            }
            let r = (s / 4.0).abs().round() * (s / 4.0).signum();
            (r as i32).clamp(-256, 255)
        })
    }

    fn forward(p: &[i32; 64]) -> [i32; 64] {
        std::array::from_fn(|i| {
            let (v, u) = (i / 8, i % 8);
            let mut s = 0.0;
            for y in 0..8 {
                for x in 0..8 {
                    s += f64::from(p[y * 8 + x]) * (((2 * x + 1) * u) as f64 * PI / 16.0).cos() * (((2 * y + 1) * v) as f64 * PI / 16.0).cos();
                }
            }
            ((c(u) * c(v) * s / 4.0).round() as i32).clamp(-2048, 2047)
        })
    }

    /// IEEE 1180's measures on random blocks: peak error 1, mean square error per position 0.06
    /// and overall 0.02, mean error per position 0.015 and overall 0.0015; each range of inputs
    /// with both signs.
    #[test]
    fn accuracy_meets_ieee_1180() {
        let mut seed: u64 = 1;
        let mut rand = |l: i32, h: i32| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 33) % (l + h + 1) as u64) as i32 - l
        };
        for (l, h) in [(256, 255), (5, 5), (300, 300)] {
            for sign in [1, -1] {
                let blocks = 10_000;
                let (mut err, mut sq) = ([0i64; 64], [0i64; 64]);
                for _ in 0..blocks {
                    let p: [i32; 64] = std::array::from_fn(|_| sign * rand(l, h));
                    let f = forward(&p);
                    let want = reference(&f);
                    let mut got = f;
                    idct(&mut got);
                    for i in 0..64 {
                        let e = i64::from(got[i] - want[i]);
                        assert!(e.abs() <= 1, "peak error {e} at {i} for range {l}/{h}");
                        err[i] += e;
                        sq[i] += e * e;
                    }
                }
                let n = blocks as f64;
                for i in 0..64 {
                    assert!(sq[i] as f64 / n <= 0.06, "mse {} at {i}", sq[i] as f64 / n);
                    assert!((err[i] as f64 / n).abs() <= 0.015, "mean {} at {i}", err[i] as f64 / n);
                }
                let (te, ts): (i64, i64) = (err.iter().sum(), sq.iter().sum());
                assert!(ts as f64 / (n * 64.0) <= 0.02, "overall mse {}", ts as f64 / (n * 64.0));
                assert!((te as f64 / (n * 64.0)).abs() <= 0.0015, "overall mean {}", te as f64 / (n * 64.0));
            }
        }
    }

    /// Annex A item 4: DC alone with F[7][7] = 1 when DC is even, within 1 of the reference.
    #[test]
    fn dc_blocks_within_one() {
        for i in 0..4096 {
            let mut f = [0i32; 64];
            f[0] = i - 2048;
            f[63] = i32::from(f[0] % 2 == 0);
            let want = reference(&f);
            idct(&mut f);
            for k in 0..64 {
                assert!((f[k] - want[k]).abs() <= 1, "dc {}: {k}", i - 2048);
            }
        }
    }
}
