//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.

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
