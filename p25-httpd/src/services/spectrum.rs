//! Narrowband RF spectrum via software FFT.
//!
//! Pulls post-DDC IQ samples from either the control-chain
//! `iq_dma` ring (0x1900_0000) or the traffic-chain `traffic_iq_dma`
//! ring (0x1C00_0000, added 2026-04-16), runs a 4096-point Cooley-
//! Tukey radix-2 FFT with a Hann window, and returns the magnitude
//! spectrum in dB. Span is ±31.25 kHz around the respective channel
//! center (post-DDC sample rate is 62.5 kSPS on both chains).
//!
//! This is the SDRTrunk-style "debug-decoder" narrow spectrum view:
//! lets the user see the P25 channel shape, adjacent-channel
//! interference within the DDC passband, and DC-blocker residuals.
//! A future wideband view (full 8 MHz around `rx_lo`) needs a
//! separate HDL IQ tap at the pre-DDC sample rate, scoped for a
//! later bake.
//!
//! The FFT is hand-rolled rather than pulled in from `rustfft` to
//! keep the Tezuka cross-build dep-free (older stable Rust) and
//! avoid MSRV drama. 4096 points is a trivial implementation, and
//! this path runs at ~5 Hz update rate on the 650 MHz Cortex-A9 —
//! CPU cost is negligible.

use std::f32::consts::PI;

/// Default FFT size when the client omits `?fft=`. 4096 complex
/// samples → ~15 Hz bin width at 62.5 kSPS.
pub const DEFAULT_FFT_SIZE: usize = 4096;

/// Legal FFT sizes for `?fft=`. Must be power-of-two (radix-2
/// Cooley-Tukey) and bounded above by a value that keeps the per-
/// request work on the Zynq-7020 Cortex-A9 cheap enough for live
/// polling. 16384 is ~3.8 Hz bin width at 62.5 kSPS — plenty for
/// P25 channel-shape analysis.
pub const LEGAL_FFT_SIZES: &[usize] = &[1024, 2048, 4096, 8192, 16384];

/// Sample rate of the post-DDC IQ stream. Both chains run at this
/// rate (control DDC / traffic DDC both terminate at 62.5 kSPS).
pub const SAMPLE_RATE_HZ: f32 = 62_500.0;

/// Default non-overlapping-segment averages per `/api/spectrum` call.
/// 1 = raw single-FFT snapshot (original behaviour).
pub const DEFAULT_AVERAGES: usize = 1;

/// Upper bound on averages × fft_size. Keeps total IQ-sample demand
/// ≤ ~1 second at 62.5 kSPS so the handler doesn't block waiting for
/// the iq_dma ring to fill. Clients requesting more get clamped and
/// the actual `averages` used is echoed in the response.
pub const MAX_TOTAL_SAMPLES: usize = 65_536;

/// Convert interleaved-IQ bytes (little-endian i16 re, i16 im pairs)
/// to parallel re / im Vec<f32> arrays. Caller concatenates multiple
/// 32 KB sub-buffers and slices down to `fft_size` samples.
///
/// Per the HDL packer (see `iq_packer.py`), the byte layout of each
/// 64-bit DMA word is:
///   bits [15: 0] = re[0] (i16 LE)
///   bits [31:16] = im[0] (i16 LE)
///   bits [47:32] = re[1] (i16 LE)
///   bits [63:48] = im[1] (i16 LE)
/// so natural interleaved-IQ decode at i16 granularity works directly.
fn bytes_to_iq(bytes: &[u8]) -> (Vec<f32>, Vec<f32>) {
    let n = bytes.len() / 4; // 4 bytes per complex (i16 + i16)
    let mut re = Vec::with_capacity(n);
    let mut im = Vec::with_capacity(n);
    for chunk in bytes.chunks_exact(4) {
        let r = i16::from_le_bytes([chunk[0], chunk[1]]);
        let i = i16::from_le_bytes([chunk[2], chunk[3]]);
        re.push(r as f32);
        im.push(i as f32);
    }
    (re, im)
}

/// Hann window of length `n`. Precomputed and returned by value;
/// cost is negligible next to the FFT itself.
fn hann_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|k| 0.5 - 0.5 * (2.0 * PI * k as f32 / (n - 1) as f32).cos())
        .collect()
}

/// Bit-reverse an index in a `log2_n`-bit space.
fn reverse_bits(mut x: usize, log2_n: u32) -> usize {
    let mut r = 0usize;
    for _ in 0..log2_n {
        r = (r << 1) | (x & 1);
        x >>= 1;
    }
    r
}

/// In-place Cooley-Tukey radix-2 DIT FFT. `re`/`im` must be the same
/// power-of-two length.
fn fft_in_place(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    assert_eq!(n, im.len());
    assert!(n.is_power_of_two());
    let log2_n = n.trailing_zeros();

    // Bit-reversal permutation
    for i in 0..n {
        let j = reverse_bits(i, log2_n);
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    // Butterflies over log2(n) stages
    let mut stage = 1;
    while stage < n {
        let half = stage;
        let full = stage * 2;
        let theta = -PI / half as f32;
        let w_re = theta.cos();
        let w_im = theta.sin();
        let mut k = 0;
        while k < n {
            let mut wr = 1.0f32;
            let mut wi = 0.0f32;
            for j in 0..half {
                let t_re = wr * re[k + j + half] - wi * im[k + j + half];
                let t_im = wr * im[k + j + half] + wi * re[k + j + half];
                re[k + j + half] = re[k + j] - t_re;
                im[k + j + half] = im[k + j] - t_im;
                re[k + j] += t_re;
                im[k + j] += t_im;
                // advance twiddle: (wr+jwi) *= (w_re+jw_im)
                let nr = wr * w_re - wi * w_im;
                let ni = wr * w_im + wi * w_re;
                wr = nr;
                wi = ni;
            }
            k += full;
        }
        stage = full;
    }
}

/// Result of one spectrum snapshot.
pub struct Snapshot {
    /// Magnitudes in dBFS, fftshifted so bin 0 is the most-negative
    /// frequency and bin n-1 is the most-positive. Length = `fft_size`.
    pub mag_db: Vec<f32>,
    /// Sample rate in Hz (constant 62.5 kHz for both chains today).
    pub sample_rate_hz: f32,
    /// Number of complex samples consumed total (`fft_size * averages_used`).
    pub samples: usize,
    /// FFT length actually used.
    pub fft_size: usize,
    /// Number of non-overlapping segments power-averaged into `mag_db`.
    /// Clamped down from the requested value if the input buffer was
    /// short; `1` is the degenerate single-snapshot case.
    pub averages_used: usize,
}

/// Clamp a requested FFT size to the nearest legal value (power-of-
/// two from `LEGAL_FFT_SIZES`). Invalid / missing returns
/// `DEFAULT_FFT_SIZE`. Exposed so the handler can echo the effective
/// value back to the client.
pub fn clamp_fft_size(requested: Option<usize>) -> usize {
    match requested {
        Some(n) if LEGAL_FFT_SIZES.contains(&n) => n,
        _ => DEFAULT_FFT_SIZE,
    }
}

/// Clamp a requested averages count to `[1, MAX_TOTAL_SAMPLES /
/// fft_size]`. Guarantees `result * fft_size ≤ MAX_TOTAL_SAMPLES`.
pub fn clamp_averages(requested: Option<usize>, fft_size: usize) -> usize {
    let max_for_size = (MAX_TOTAL_SAMPLES / fft_size).max(1);
    requested
        .unwrap_or(DEFAULT_AVERAGES)
        .max(1)
        .min(max_for_size)
}

/// Compute a magnitude spectrum from raw interleaved-IQ bytes with
/// optional non-overlapping-segment averaging.
///
/// `averages` segments of `fft_size` complex samples each are taken
/// from the tail of the buffer (most recent data first). Each segment
/// is Hann-windowed, forward-FFTd, then summed into a running
/// magnitude-squared accumulator. The accumulator is converted to dB
/// at the end, which gives a true power average (noise floor drops by
/// ~10·log10(K) dB relative to a single snapshot, while deterministic
/// tones stay put — so weak carriers pop out).
///
/// `bytes` must contain AT LEAST `fft_size * averages * 4` bytes for
/// the caller's requested `averages` to be honoured; shorter buffers
/// fall back to fewer averages, and the actual count is reported in
/// `Snapshot::averages_used`. Returns `None` only when the buffer
/// can't even fit one `fft_size` segment.
pub fn spectrum_from_bytes(
    bytes: &[u8],
    fft_size: usize,
    averages: usize,
) -> Option<Snapshot> {
    assert!(fft_size.is_power_of_two() && fft_size >= 2);
    let (re_all, im_all) = bytes_to_iq(bytes);
    if re_all.len() < fft_size {
        return None;
    }
    let available_segments = re_all.len() / fft_size;
    let averages_used = averages.max(1).min(available_segments);

    let win = hann_window(fft_size);

    // Accumulate magnitude² across segments (tail-aligned — the most
    // recent block is always included so the snapshot reflects current
    // conditions).
    let mut mag_sq_acc: Vec<f32> = vec![0.0; fft_size];
    for seg in 0..averages_used {
        // Segment k counted from the tail: samples [end - (k+1)·N, end - k·N).
        let end = re_all.len() - seg * fft_size;
        let start = end - fft_size;
        let mut re: Vec<f32> = re_all[start..end].to_vec();
        let mut im: Vec<f32> = im_all[start..end].to_vec();

        for i in 0..fft_size {
            re[i] *= win[i];
            im[i] *= win[i];
        }
        fft_in_place(&mut re, &mut im);

        for i in 0..fft_size {
            mag_sq_acc[i] += re[i] * re[i] + im[i] * im[i];
        }
    }

    // dBFS calibration. Peak-of-single-full-scale-tone convention:
    // a complex tone at amplitude A produces a peak bin with magnitude
    // |X(k)| = A · Σw, so |X|² = A²·(Σw)². Picking A = 32768 (i16 full
    // scale) as the 0 dBFS reference and expressing (Σw) explicitly
    // rather than hard-coding the Hann value means non-Hann windows
    // Just Work if we swap later.
    //
    // Pre-fix: ref_sq was 32768² which effectively reads bin power
    // *without* compensating for coherent FFT gain, so a full-scale
    // tone on one bin was reported as ≈ +20·log10(Σw / 1) dB ≈ +66 dB
    // (N=4096, Hann). Observed on-target: P25 carriers showing peaks
    // above +10 dBFS, with the plot Y-top at 0 dB clipping them.
    let window_sum: f32 = win.iter().sum();
    let ref_amplitude: f32 = 32768.0;
    let ref_sq: f32 = (ref_amplitude * window_sum).powi(2);

    let inv_k = 1.0 / averages_used as f32;
    let mut mag_db: Vec<f32> = mag_sq_acc
        .iter()
        .map(|m2| {
            let m2_avg = m2 * inv_k + 1e-12;
            10.0 * (m2_avg / ref_sq).log10()
        })
        .collect();

    // fftshift: swap halves so bin 0 = most-negative freq.
    let half = fft_size / 2;
    let second = mag_db.split_off(half);
    let mut shifted = second;
    shifted.extend(mag_db);

    Some(Snapshot {
        mag_db: shifted,
        sample_rate_hz: SAMPLE_RATE_HZ,
        samples: fft_size * averages_used,
        fft_size,
        averages_used,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_iq_bytes(re: &[i16], im: &[i16]) -> Vec<u8> {
        assert_eq!(re.len(), im.len());
        let mut b = Vec::with_capacity(re.len() * 4);
        for (r, i) in re.iter().zip(im.iter()) {
            b.extend_from_slice(&r.to_le_bytes());
            b.extend_from_slice(&i.to_le_bytes());
        }
        b
    }

    #[test]
    fn round_trip_decode() {
        let re = vec![1i16, -2, 3, -4];
        let im = vec![10i16, 20, -30, 40];
        let bytes = make_iq_bytes(&re, &im);
        let (r, i) = bytes_to_iq(&bytes);
        assert_eq!(r, vec![1.0, -2.0, 3.0, -4.0]);
        assert_eq!(i, vec![10.0, 20.0, -30.0, 40.0]);
    }

    #[test]
    fn bit_reverse_known() {
        // 12-bit bit-reverse of 0x001 = 0x800 (top bit).
        assert_eq!(reverse_bits(1, 12), 0x800);
        assert_eq!(reverse_bits(0x800, 12), 1);
        assert_eq!(reverse_bits(0, 4), 0);
        assert_eq!(reverse_bits(0b0001, 4), 0b1000);
    }

    #[test]
    fn fft_tone_lands_at_expected_bin() {
        // Pure complex exponential at bin 64: e^(j 2π k 64 / N).
        // After FFT, energy should concentrate at index 64 (pre-shift).
        let n = DEFAULT_FFT_SIZE;
        let k_target = 64usize;
        let mut re = vec![0f32; n];
        let mut im = vec![0f32; n];
        for k in 0..n {
            let phase = 2.0 * PI * k_target as f32 * k as f32 / n as f32;
            re[k] = 10000.0 * phase.cos();
            im[k] = 10000.0 * phase.sin();
        }
        fft_in_place(&mut re, &mut im);
        let mags: Vec<f32> = re.iter().zip(im.iter())
            .map(|(r, i)| r * r + i * i)
            .collect();
        // Find the peak.
        let peak = mags
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert_eq!(peak, k_target,
            "FFT peak at bin {peak}, expected {k_target}");
    }

    #[test]
    fn spectrum_from_dc_input_peaks_at_center() {
        // All-ones (DC) input → after fftshift, the bin at index
        // DEFAULT_FFT_SIZE/2 should be the maximum (DC lives there post-shift).
        let re: Vec<i16> = vec![1000; DEFAULT_FFT_SIZE];
        let im: Vec<i16> = vec![0; DEFAULT_FFT_SIZE];
        let bytes = make_iq_bytes(&re, &im);
        let snap = spectrum_from_bytes(&bytes, DEFAULT_FFT_SIZE, 1).unwrap();
        let peak = snap
            .mag_db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert_eq!(peak, DEFAULT_FFT_SIZE / 2,
            "DC peak at bin {peak}, expected {}", DEFAULT_FFT_SIZE / 2);
    }

    #[test]
    fn spectrum_full_scale_tone_reads_near_0_dbfs() {
        // A complex full-scale tone should read ≈ 0 dBFS on its
        // peak bin. Without the window-sum compensation in
        // spectrum_from_bytes this came out ~+66 dB for N=4096.
        let n = DEFAULT_FFT_SIZE;
        let k_target = 100usize;
        let mut re = vec![0i16; n];
        let mut im = vec![0i16; n];
        for k in 0..n {
            let phase = 2.0 * PI * k_target as f32 * k as f32 / n as f32;
            re[k] = (32767.0 * phase.cos()) as i16;
            im[k] = (32767.0 * phase.sin()) as i16;
        }
        let bytes = make_iq_bytes(&re, &im);
        let snap = spectrum_from_bytes(&bytes, n, 1).unwrap();
        let peak_db = snap
            .mag_db
            .iter()
            .cloned()
            .fold(f32::NEG_INFINITY, f32::max);
        // Tolerance covers Hann scalloping (peak not exactly at bin
        // center under windowing rounding) + 1 ULP of int rounding.
        // Expect -1 to +0.5 dB for a bin-centered full-scale tone.
        assert!(
            peak_db > -2.0 && peak_db < 1.0,
            "full-scale tone peak = {peak_db} dB, expected near 0 dBFS"
        );
    }
}
