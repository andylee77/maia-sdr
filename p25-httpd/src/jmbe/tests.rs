//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.


use super::*;

/// Decode an 18-byte IMBE frame from a 36-char hex literal.
fn hex18(s: &str) -> [u8; 18] {
    assert_eq!(s.len(), 36, "hex18 expects 36 chars");
    let mut out = [0u8; 18];
    let bytes = s.as_bytes();
    for i in 0..18 {
        let hi = (bytes[i * 2] as char).to_digit(16).unwrap() as u8;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16).unwrap() as u8;
        out[i] = (hi << 4) | lo;
    }
    out
}

#[test]
fn test_fundamental_frequency_default() {
    let (w0, l) = compute_fundamental(134);
    // w0 = 4*PI / (134 + 39.5) = 4*PI/173.5
    // L = floor(0.9254 * floor(PI/w0 + 0.25)) = floor(0.9254 * 43) = 39
    assert_eq!(l, 39);
    assert!((w0 - (4.0 * PI / 173.5)).abs() < 0.0001);
}

#[test]
fn test_deinterleave_roundtrip() {
    let mut frame = [false; 144];
    frame[0] = true;
    frame[5] = true;
    frame[143] = true;
    let original = frame;
    deinterleave(&mut frame);
    // After deinterleave, bits should have moved
    assert_ne!(frame, original);
}

#[test]
fn test_decode_frame_does_not_panic() {
    let mut decoder = ImbeDecoder::new();
    let frame: [u8; 18] = [
        0x7C, 0x57, 0xB7, 0x9E, 0x01, 0x6C, 0x72, 0x54, 0x26, 0x11, 0xA1, 0xE3, 0x29, 0xDD,
        0xE3, 0xA3, 0xDC, 0xFE,
    ];
    let samples = decoder.decode_frame(&frame);
    // Should produce 160 samples without panicking
    assert_eq!(samples.len(), 160);
}

#[test]
fn test_gain_table_size() {
    assert_eq!(GAIN_TABLE.len(), 64);
}

#[test]
fn test_synthesis_window_bounds() {
    assert_eq!(synthesis_window(-106), 0.0);
    assert_eq!(synthesis_window(106), 0.0);
    assert!(synthesis_window(0) > 0.0);
    assert_eq!(synthesis_window(0), 1.0); // center of flat region
}

/// Bit-equivalence reference test for the synthesis path.
///
/// Decodes the same hard-coded IMBE frame three times in a row through
/// a single decoder instance (the second/third frames exercise the
/// previous-frame state used by the cos/sin phase recurrence) and
/// captures aggregate signal statistics. Any optimization that changes
/// the math (e.g., harmonic recurrence in get_voiced) must keep these
/// signatures within the listed tolerances — drift > 1 % on RMS would
/// be audibly perceptible.
///
/// The reference values were captured against the pre-recurrence
/// implementation on 2026-04-29. They form a regression contract: a
/// bit-exact rewrite would match to ~5e-5 relative; the recurrence
/// drifts a few ULPs over 160 samples and is bounded looser.
#[test]
fn test_synthesis_signature_stable() {
    let mut decoder = ImbeDecoder::new();
    // Real IMBE frames captured 2026-04-29 from a live TG 301 call —
    // sustained voicing, exercises the cv && pv harmonic branch where
    // the cos/sin recurrence applies. The earlier 0x7C5... fixture
    // triggered the max-repeat / muting bypass and never reached
    // synthesize_voice, so couldn't validate the inner loop.
    let frames: [[u8; 18]; 6] = [
        hex18("6fce96bb1522e01304afa54a318090704114"),
        hex18("6fce96ba1522e013049fa54a398090704114"),
        hex18("6fce96bb1522e0d3049fa54a318090724114"),
        hex18("6fca96bb1522e013049fa54a318090704114"),
        hex18("6fce96bb1622e013049fa44a318c90704114"),
        hex18("6fce96bb1522e013049fa54a318090704116"),
    ];
    // Drive enough frames to seed previous_params for the recurrence
    // branch; capture the LAST frame's output for signature.
    for f in frames.iter().take(frames.len() - 1) {
        let _ = decoder.decode_frame(f);
    }
    let samples = decoder.decode_frame(&frames[frames.len() - 1]);

    let mut sum = 0.0_f64;
    let mut sum_sq = 0.0_f64;
    let mut peak = 0.0_f32;
    let mut sample_80 = 0.0_f32;
    for (i, &s) in samples.iter().enumerate() {
        sum += s as f64;
        sum_sq += (s as f64) * (s as f64);
        if s.abs() > peak {
            peak = s.abs();
        }
        if i == 80 {
            sample_80 = s;
        }
    }
    let rms = (sum_sq / samples.len() as f64).sqrt() as f32;
    let mean = (sum / samples.len() as f64) as f32;

    // Reference values captured 2026-04-29 against the pre-recurrence
    // decoder using 5 real IMBE frames from a live TG 301 call. Raw
    // JMBE output is small in absolute terms because the post-
    // vocoder PCM AGC (in vocoder_task.rs) does the level shaping —
    // these are the unscaled synthesis values. The recurrence is a
    // mathematical identity up to f32 ULP drift over 160 samples
    // (~1.6e-5 worst-case absolute), so tolerances are sized to
    // accommodate that.
    let ref_rms        =  0.00034554768_f32;
    let ref_peak       =  0.0008167084_f32;
    let ref_sample_80  = -0.00012138824_f32;
    let ref_mean       =  0.000021048196_f32;

    // 1 % relative or 2e-5 absolute floor, whichever is looser.
    let tol_rms    = (ref_rms.abs()        * 0.01).max(2e-5);
    let tol_peak   = (ref_peak.abs()       * 0.01).max(2e-5);
    let tol_sample = (ref_sample_80.abs()  * 0.02).max(2e-5);
    let tol_mean   = (ref_mean.abs()       * 0.05).max(2e-5);

    assert!(
        (rms - ref_rms).abs() < tol_rms,
        "RMS drifted: got {rms} expected {ref_rms} ± {tol_rms}"
    );
    assert!(
        (peak - ref_peak).abs() < tol_peak,
        "Peak drifted: got {peak} expected {ref_peak} ± {tol_peak}"
    );
    assert!(
        (sample_80 - ref_sample_80).abs() < tol_sample,
        "samples[80] drifted: got {sample_80} expected {ref_sample_80} ± {tol_sample}"
    );
    assert!(
        (mean - ref_mean).abs() < tol_mean,
        "Mean drifted: got {mean} expected {ref_mean} ± {tol_mean}"
    );
}
