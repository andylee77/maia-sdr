//! AC-3's inverse transform (A/52 7.9.4): a block's 256 coefficients to 512 windowed samples, as
//! one 512-sample transform through a 128-point complex inverse FFT, or (block switching) two
//! 256-sample transforms through 64-point ones; then the window's second half overlapped with the
//! next block's first. The window is the Kaiser-Bessel derived window (alpha 5) Table 7.33
//! prints to 5 decimals, computed here exactly.

use std::f64::consts::PI;

/// A complex inverse FFT of a power-of-two size, unscaled: z[n] = Σ Z[k] e^{+j2πkn/size}.
struct Fft {
    size: usize,
    twiddle: Vec<(f32, f32)>,
    reverse: Vec<usize>,
}

impl Fft {
    fn new(size: usize) -> Fft {
        let bits = size.trailing_zeros();
        let twiddle = (0..size / 2)
            .map(|k| {
                let a = 2.0 * PI * k as f64 / size as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let reverse = (0..size).map(|i| i.reverse_bits() >> (usize::BITS - bits)).collect();
        Fft { size, twiddle, reverse }
    }

    fn run(&self, re: &mut [f32], im: &mut [f32]) {
        for i in 0..self.size {
            let j = self.reverse[i];
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= self.size {
            let step = self.size / len;
            for start in (0..self.size).step_by(len) {
                for k in 0..len / 2 {
                    let (c, s) = self.twiddle[k * step];
                    let (a, b) = (start + k, start + k + len / 2);
                    let tr = re[b] * c - im[b] * s;
                    let ti = re[b] * s + im[b] * c;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len *= 2;
        }
    }
}

/// I0, the modified Bessel function of the first kind, by its series.
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-20 * sum {
        term *= (x / (2.0 * k)) * (x / (2.0 * k));
        sum += term;
        k += 1.0;
    }
    sum
}

/// The window's first half, w[0..256] (the second half is its mirror).
pub fn window() -> [f32; 256] {
    let alpha = 5.0;
    let kaiser: Vec<f64> = (0..=256)
        .map(|j| {
            let r = (j as f64 - 128.0) / 128.0;
            bessel_i0(PI * alpha * (1.0 - r * r).max(0.0).sqrt())
        })
        .collect();
    let total: f64 = kaiser.iter().sum();
    let mut w = [0f32; 256];
    let mut acc = 0.0;
    for (n, v) in w.iter_mut().enumerate() {
        acc += kaiser[n];
        *v = (acc / total).sqrt() as f32;
    }
    w
}

pub struct Imdct {
    cos1: [f32; 128],
    sin1: [f32; 128],
    cos2: [f32; 64],
    sin2: [f32; 64],
    w: [f32; 256],
    fft128: Fft,
    fft64: Fft,
}

impl Default for Imdct {
    fn default() -> Imdct {
        let n = 512.0;
        let mut m = Imdct {
            cos1: [0.0; 128],
            sin1: [0.0; 128],
            cos2: [0.0; 64],
            sin2: [0.0; 64],
            w: window(),
            fft128: Fft::new(128),
            fft64: Fft::new(64),
        };
        for k in 0..128 {
            let a = 2.0 * PI * (8 * k + 1) as f64 / (8.0 * n);
            m.cos1[k] = -a.cos() as f32;
            m.sin1[k] = -a.sin() as f32;
        }
        for k in 0..64 {
            let a = 2.0 * PI * (8 * k + 1) as f64 / (4.0 * n);
            m.cos2[k] = -a.cos() as f32;
            m.sin2[k] = -a.sin() as f32;
        }
        m
    }
}

impl Imdct {
    /// One block: `coef` (256) to 256 output samples, `delay` (256) carrying the overlap.
    pub fn block(&self, coef: &[f32; 256], short: bool, delay: &mut [f32; 256], out: &mut [f32]) {
        let w = &self.w;
        let mut x = [0f32; 512];
        if !short {
            let (mut zr, mut zi) = ([0f32; 128], [0f32; 128]);
            for k in 0..128 {
                let (a, b) = (coef[255 - 2 * k], coef[2 * k]);
                zr[k] = a * self.cos1[k] - b * self.sin1[k];
                zi[k] = b * self.cos1[k] + a * self.sin1[k];
            }
            self.fft128.run(&mut zr, &mut zi);
            let (mut yr, mut yi) = ([0f32; 128], [0f32; 128]);
            for n in 0..128 {
                yr[n] = zr[n] * self.cos1[n] - zi[n] * self.sin1[n];
                yi[n] = zi[n] * self.cos1[n] + zr[n] * self.sin1[n];
            }
            for n in 0..64 {
                x[2 * n] = -yi[64 + n] * w[2 * n];
                x[2 * n + 1] = yr[64 - n - 1] * w[2 * n + 1];
                x[128 + 2 * n] = -yr[n] * w[128 + 2 * n];
                x[128 + 2 * n + 1] = yi[128 - n - 1] * w[128 + 2 * n + 1];
                x[256 + 2 * n] = -yr[64 + n] * w[256 - 2 * n - 1];
                x[256 + 2 * n + 1] = yi[64 - n - 1] * w[256 - 2 * n - 2];
                x[384 + 2 * n] = yi[n] * w[128 - 2 * n - 1];
                x[384 + 2 * n + 1] = -yr[128 - n - 1] * w[128 - 2 * n - 2];
            }
        } else {
            let mut y = [[[0f32; 64]; 2]; 2];
            for (half, yh) in y.iter_mut().enumerate() {
                let (mut zr, mut zi) = ([0f32; 64], [0f32; 64]);
                let xh = |k: usize| coef[2 * k + half];
                for k in 0..64 {
                    let (a, b) = (xh(128 - 2 * k - 1), xh(2 * k));
                    zr[k] = a * self.cos2[k] - b * self.sin2[k];
                    zi[k] = b * self.cos2[k] + a * self.sin2[k];
                }
                self.fft64.run(&mut zr, &mut zi);
                for n in 0..64 {
                    yh[0][n] = zr[n] * self.cos2[n] - zi[n] * self.sin2[n];
                    yh[1][n] = zi[n] * self.cos2[n] + zr[n] * self.sin2[n];
                }
            }
            let ([yr1, yi1], [yr2, yi2]) = (y[0], y[1]);
            for n in 0..64 {
                x[2 * n] = -yi1[n] * w[2 * n];
                x[2 * n + 1] = yr1[64 - n - 1] * w[2 * n + 1];
                x[128 + 2 * n] = -yr1[n] * w[128 + 2 * n];
                x[128 + 2 * n + 1] = yi1[64 - n - 1] * w[128 + 2 * n + 1];
                x[256 + 2 * n] = -yr2[n] * w[256 - 2 * n - 1];
                x[256 + 2 * n + 1] = yi2[64 - n - 1] * w[256 - 2 * n - 2];
                x[384 + 2 * n] = yi2[n] * w[128 - 2 * n - 1];
                x[384 + 2 * n + 1] = -yr2[64 - n - 1] * w[128 - 2 * n - 2];
            }
        }
        for n in 0..256 {
            out[n] = 2.0 * (x[n] + delay[n]);
            delay[n] = x[256 + n];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ac3::tables::WINDOW_TABLE;

    #[test]
    fn the_window_is_table_7_33() {
        let w = window();
        for (n, (a, b)) in w.iter().zip(WINDOW_TABLE.iter()).enumerate() {
            assert!((a - b).abs() <= 0.000_006, "{n}: {a} {b}");
        }
    }

    #[test]
    fn the_fft_is_the_inverse_dft() {
        let f = Fft::new(64);
        let zr: Vec<f32> = (0..64).map(|k| ((k * 7) % 11) as f32 - 5.0).collect();
        let zi: Vec<f32> = (0..64).map(|k| ((k * 5) % 13) as f32 - 6.0).collect();
        let (mut r, mut i) = (zr.clone(), zi.clone());
        f.run(&mut r, &mut i);
        for n in 0..64 {
            let (mut sr, mut si) = (0f64, 0f64);
            for k in 0..64 {
                let a = 2.0 * PI * (k * n) as f64 / 64.0;
                sr += f64::from(zr[k]) * a.cos() - f64::from(zi[k]) * a.sin();
                si += f64::from(zr[k]) * a.sin() + f64::from(zi[k]) * a.cos();
            }
            assert!((f64::from(r[n]) - sr).abs() < 1e-3 && (f64::from(i[n]) - si).abs() < 1e-3, "{n}");
        }
    }

    /// The transform against its definition (A/52 8.2.3.2's forward MDCT inverted):
    /// x[n] = Σ_k X[k] cos(2π/N (n + n0)(k + 1/2)) with n0 = (N/2 + 1)/2, windowed, and the
    /// overlap-add of two blocks rebuilding the middle 256 samples' windowed input.
    #[test]
    fn a_long_block_is_the_imdct() {
        let m = Imdct::default();
        let n = 512usize;
        let coef: [f32; 256] = std::array::from_fn(|k| (((k * 37) % 23) as f32 - 11.0) / 50.0);
        let mut delay = [0f32; 256];
        let mut out = [0f32; 256];
        m.block(&coef, false, &mut delay, &mut out);
        // The first half (out, with zero delay) and the second (now in delay), against the sum.
        let n0 = (n as f64 / 2.0 + 1.0) / 2.0;
        let w = window();
        let wn = |i: usize| f64::from(if i < 256 { w[i] } else { w[511 - i] });
        let mut ratio = None;
        for i in 0..n {
            let s: f64 = (0..256).map(|k| f64::from(coef[k]) * (2.0 * PI / n as f64 * (i as f64 + n0) * (k as f64 + 0.5)).cos()).sum::<f64>() * wn(i);
            let got = if i < 256 { f64::from(out[i]) / 2.0 } else { f64::from(delay[i - 256]) };
            if s.abs() > 0.05 {
                let r = got / s;
                let r0 = *ratio.get_or_insert(r);
                assert!((r - r0).abs() < 1e-3 * r0.abs(), "{i}: {got} {s} ratio {r} against {r0}");
            }
        }
        assert!(ratio.is_some());
    }
}
