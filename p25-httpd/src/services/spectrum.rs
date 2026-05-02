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
        fft_size,
        averages_used,
    })
}
/// Number of bins produced by the HDL wideband spectrometer.
/// Matches `Spectrometer.fft_order_log2 = 12` in `maia_hdl/spectrometer.py`.
/// 2026-05-01: dropped 14 -> 12 (16384 -> 4096 bins) to free BRAM
/// for the polyphase traffic channelizer rewrite. Bin width back to
/// 1.95 kHz/bin (was 488 Hz/bin); auto-PPM parabolic-interp still
/// resolves sub-kHz so this is acceptable.
pub const WIDEBAND_FFT_SIZE: usize = 4096;

/// Unpack one 32 KB wideband spectrometer DMA sub-buffer into a
/// 4096-bin magnitude vector in dB, DC-centered (bin 0 = -Fs/2,
/// bin N/2 = DC, bin N-1 = +Fs/2 - Δf).
///
/// Bit layout matches `maia-httpd/src/spectrometer.rs::buffer_u64fp_to_f32`
/// (Maia SDR upstream):
///
///     bits [ 0:47] = integrator.rdata_value  (unsigned 47-bit mantissa)
///     bits [47:56] = 0 padding
///     bits [56:58] = integrator.rdata_exponent (**2-bit** exponent)
///     bits [58:61] = padding
///     bits [61:64] = fastlock_profile (unused here)
///
/// Linear power = mantissa × 4^exponent (exponent represents powers
/// of 4, NOT 2 — see maia-hdl/maia_hdl/spectrometer.py:128-135 and the
/// upstream PS-side decoder). Equivalent to left-shifting the mantissa
/// by `2 * exponent` bits.
///
/// **The HDL Spectrometer emits its output already DC-centered**, so
/// no PS-side fft-shift is applied here. Empirical verification: the
/// Clay County control channel at +2.863 MHz from LO = HDL bin
/// (N/2 + 1466) = 3514, and the DC/LO-feedthrough spur appears at
/// bin N/2 = 2048.
///
/// Returns an empty Vec if the buffer is shorter than
/// `WIDEBAND_FFT_SIZE * 8` bytes.
pub fn wideband_power_db(bytes: &[u8]) -> Vec<f32> {
    let need = WIDEBAND_FFT_SIZE * 8;
    if bytes.len() < need {
        return Vec::new();
    }
    // Empirical dB reference for wideband FFT output.
    //
    // The HDL `Spectrometer` integrator emits raw linear power in
    // arbitrary units (mantissa * 4^exponent). Converting to dBm
    // requires a constant offset that rolls up:
    //   * FFT+window processing gain
    //   * ADC full-scale to dBm mapping
    //   * Fixed gain/loss between antenna and ADC (cable, matching,
    //     AD9361 internal gain distribution we don't separate out)
    //
    // 2026-04-23 calibration pass: operator reports noise floor
    // around -90 to -100 dBm at AD9361 rx_gain=60 dB (cross-checked
    // against SDRTrunk on the same RF chain). Our raw `10*log10(power)
    // - 96` was reading -42.7 dB for that same floor → offset is
    // ~52 dB low. `148` brings the readout to ~ -95 dBm noise floor
    // at 60 dB, matching SDRTrunk.
    //
    // 2026-05-01: spectrometer order dropped 14 -> 12. Per-bin power
    // for a CW tone falls by ~6 dB and noise floor per bin rises by
    // ~6 dB (smaller N → less FFT processing gain, wider per-bin
    // noise integration). Operator may need to re-calibrate — leave
    // 148 in place for now; tune empirically against SDRTrunk after
    // first bake.
    //
    // NOT laboratory-calibrated — if someone injects a known tone
    // with a lab source, adjust this to match. See memory
    // `project_wideband_narrowband_db_calibration_todo.md`.
    const DB_REF: f64 = 148.0;
    let mut out = Vec::with_capacity(WIDEBAND_FFT_SIZE);
    for i in 0..WIDEBAND_FFT_SIZE {
        let base = i * 8;
        let w = u64::from_le_bytes(bytes[base..base + 8].try_into().unwrap());
        let mantissa = w & ((1u64 << 47) - 1);
        let exponent = ((w >> 56) & 0x03) as u32;
        // power ∝ mantissa × 4^exponent  (powers-of-4 exponent per
        // maia-httpd upstream, which stores `y = value << (2*exp)`).
        let power = (mantissa as f64) * (1u64 << (2 * exponent)) as f64;
        let db = if power > 0.0 {
            (10.0 * power.log10() - DB_REF) as f32
        } else {
            -170.0
        };
        out.push(db);
    }
    out
}

#[cfg(test)]
#[path = "spectrum_tests.rs"]
mod tests;
