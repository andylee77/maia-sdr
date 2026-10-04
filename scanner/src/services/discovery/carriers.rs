//! Carriers in the wideband spectrometer's frames.
//!
//! A control channel transmits continuously and a traffic channel only during calls, so the
//! candidates are carriers present in nearly every frame of a pass.

use serde::Serialize;

/// Bins of the HDL spectrometer (`fft_order_log2 = 12`).
pub const BINS: usize = 4096;
/// Carriers this close to the LO are its DC spur, not signals.
pub const DC_EXCLUDE_HZ: f64 = 25_000.0;
/// dB offset of the spectrometer's linear power, set so the noise floor reads about -95 dBm at
/// 60 dB of gain as SDRTrunk shows it on the same antenna (not laboratory-calibrated).
const DB_REF: f64 = 148.0;

/// One spectrometer frame as dB per bin, DC-centred (bin N/2 is the LO). Each bin is 64 bits: a
/// 47-bit mantissa and, at bit 56, a 2-bit exponent in powers of 4. Empty when short.
pub fn power_db(bytes: &[u8]) -> Vec<f32> {
    if bytes.len() < BINS * 8 {
        return Vec::new();
    }
    bytes
        .chunks_exact(8)
        .take(BINS)
        .map(|b| {
            let w = u64::from_le_bytes(b.try_into().unwrap_or_default());
            let mantissa = w & ((1u64 << 47) - 1);
            let exponent = ((w >> 56) & 0x03) as u32;
            if mantissa == 0 { -170.0 } else { db_of(mantissa, exponent) - DB_REF as f32 }
        })
        .collect()
}

/// 10·log10 of mantissa · 4^exponent, within 1e-4 dB, in integer and f32 arithmetic (the A9
/// converts a u64 to a float in software, and a logarithm call costs several times this): the
/// top bit's position gives log2's integer part; the 23 bits under it, m in [1, 2), give ln m by
/// its series in s = (m − 1)/(m + 1) ≤ 1/3.
fn db_of(mantissa: u64, exponent: u32) -> f32 {
    let top = 63 - mantissa.leading_zeros();
    let m = f32::from_bits(0x3F80_0000 | (((mantissa << (63 - top)) >> 40) as u32 & 0x7F_FFFF));
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let ln_m = 2.0 * s * (1.0 + s2 * (1.0 / 3.0 + s2 * (1.0 / 5.0 + s2 * (1.0 / 7.0 + s2 / 9.0))));
    const DB_PER_OCTAVE: f32 = 3.010_299_956_6;
    const DB_PER_NEPER: f32 = 4.342_944_819;
    (top + 2 * exponent) as f32 * DB_PER_OCTAVE + ln_m * DB_PER_NEPER
}

/// A continuous carrier found in a pass.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Carrier {
    pub freq_hz: u64,
    /// dB above the noise floor (the frame's median).
    pub level_db: f32,
    /// Share of the frames it was present in.
    pub persistence: f32,
}

/// Continuous carriers in `frames` (dB per bin, bin N/2 at `center_hz`, `span_hz` wide): above
/// the floor by `min_db` in at least `persist` of the frames, within ±`usable_half_hz`, off the
/// DC spur. Adjacent bins merge; the frequency is the power-weighted centre, rounded to 125 Hz.
pub fn find_carriers(frames: &[Vec<f32>], center_hz: f64, span_hz: f64, usable_half_hz: f64, min_db: f32, persist: f32) -> Vec<Carrier> {
    let Some(n) = frames.first().map(|f| f.len()) else { return Vec::new() };
    if n == 0 || frames.iter().any(|f| f.len() != n) {
        return Vec::new();
    }
    let bin_hz = span_hz / n as f64;
    let floors: Vec<f32> = frames
        .iter()
        .map(|f| {
            let mut s = f.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            s[n / 2]
        })
        .collect();
    let mut present = vec![0usize; n];
    let mut excess = vec![0f32; n];
    for (f, floor) in frames.iter().zip(&floors) {
        for i in 0..n {
            let e = f[i] - floor;
            if e >= min_db {
                present[i] += 1;
            }
            excess[i] += e;
        }
    }
    let need = (persist * frames.len() as f32).ceil() as usize;
    let offset = |i: usize| (i as f64 - (n / 2) as f64) * bin_hz;
    let keep: Vec<bool> =
        (0..n).map(|i| present[i] >= need && offset(i).abs() <= usable_half_hz && offset(i).abs() > DC_EXCLUDE_HZ).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if !keep[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && keep[i] {
            i += 1;
        }
        let (mut wsum, mut fsum, mut best, mut pers) = (0f64, 0f64, f32::MIN, 0usize);
        for (j, (&e, &p)) in excess.iter().zip(&present).enumerate().take(i).skip(start) {
            let mean_db = e / frames.len() as f32;
            let w = 10f64.powf(mean_db as f64 / 10.0);
            wsum += w;
            fsum += w * offset(j);
            best = best.max(mean_db);
            pers = pers.max(p);
        }
        let f = center_hz + fsum / wsum;
        out.push(Carrier {
            freq_hz: ((f / 125.0).round() * 125.0) as u64,
            level_db: best,
            persistence: pers as f32 / frames.len() as f32,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_with(carriers: &[(usize, f32, usize)], n_frames: usize) -> Vec<Vec<f32>> {
        // BINS bins at -100 dB; (bin, level, frames present).
        (0..n_frames)
            .map(|k| {
                let mut f = vec![-100.0f32; BINS];
                for &(bin, lvl, present) in carriers {
                    if k < present {
                        f[bin] = -100.0 + lvl;
                        f[bin + 1] = -100.0 + lvl - 6.0;
                    }
                }
                f
            })
            .collect()
    }

    #[test]
    fn continuous_carriers_are_found_bursty_ones_not() {
        let span = 16_000_000.0;
        let bin = span / BINS as f64;
        // A control channel at +1 MHz (always there), a traffic channel at -2 MHz (2 of 8
        // frames), the DC spur, and a carrier beyond the usable window.
        let cc_bin = BINS / 2 + (1_000_000.0 / bin) as usize;
        let tr_bin = BINS / 2 - (2_000_000.0 / bin) as usize;
        let edge_bin = BINS / 2 + (7_800_000.0 / bin) as usize;
        let frames = frames_with(&[(cc_bin, 30.0, 8), (tr_bin, 30.0, 2), (BINS / 2, 40.0, 8), (edge_bin, 30.0, 8)], 8);
        let c = find_carriers(&frames, 860_000_000.0, span, 7_200_000.0, 12.0, 0.8);
        assert_eq!(c.len(), 1, "{c:?}");
        assert!((c[0].freq_hz as f64 - 861_000_000.0).abs() < bin * 1.5, "{}", c[0].freq_hz);
        assert!(c[0].level_db > 29.0 && c[0].persistence == 1.0);
        assert!(find_carriers(&[], 0.0, span, 1.0, 12.0, 0.8).is_empty());
    }

    #[test]
    fn the_db_series_is_the_logarithm() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut worst = 0.0f64;
        for i in 0..20_000u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // Every magnitude from 1 to 2^47 - 1.
            let mantissa = ((x >> 17) >> (i % 47)).max(1);
            let exponent = (i % 4) as u32;
            let exact = 10.0 * (mantissa as f64 * 4f64.powi(exponent as i32)).log10();
            worst = worst.max((db_of(mantissa, exponent) as f64 - exact).abs());
        }
        assert!(worst < 1e-4, "{worst} dB");
    }

    #[test]
    fn spectrometer_words_unpack_to_db() {
        let mut bytes = vec![0u8; BINS * 8];
        // Mantissa 1000, exponent 2: 16,000.
        let w: u64 = 1000 | (2u64 << 56);
        bytes[8..16].copy_from_slice(&w.to_le_bytes());
        let db = power_db(&bytes);
        assert_eq!(db.len(), BINS);
        assert_eq!(db[0], -170.0);
        assert!((db[1] - (10.0 * 16_000f32.log10() - 148.0)).abs() < 1e-3, "{}", db[1]);
        assert!(power_db(&bytes[..100]).is_empty());
    }
}
