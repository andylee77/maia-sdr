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
            let power = mantissa as f64 * (1u64 << (2 * exponent)) as f64;
            if power > 0.0 { (10.0 * power.log10() - DB_REF) as f32 } else { -170.0 }
        })
        .collect()
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
