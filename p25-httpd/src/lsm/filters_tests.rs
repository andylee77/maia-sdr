//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.

use super::*;

/// FIR convolution with a Kronecker delta input must reproduce the
/// taps in the output (impulse response). This is the simplest
/// correctness check on `apply_real_fir_complex`: ground truth is
/// the const tap arrays themselves.
#[test]
fn fir_impulse_response_matches_taps() {
    let mut input = vec![Complex32::new(0.0, 0.0); 200];
    input[0] = Complex32::new(1.0, 0.0);

    let out = apply_real_fir_complex(&LPF_TAPS_31250, &input);
    for (i, &h) in LPF_TAPS_31250.iter().enumerate() {
        assert!(
            (out[i].re - h).abs() < 1e-6,
            "LPF impulse response mismatch at tap {i}: got {}, expected {h}",
            out[i].re
        );
        assert!(out[i].im.abs() < 1e-6);
    }
    // Tail past the impulse response should be zero.
    for sample in &out[LPF_TAPS_31250.len()..] {
        assert!(sample.re.abs() < 1e-6);
        assert!(sample.im.abs() < 1e-6);
    }
}

/// Same impulse-response check on the RRC kernel — different length,
/// different shape, same correctness criterion.
#[test]
fn rrc_impulse_response_matches_taps() {
    let mut input = vec![Complex32::new(0.0, 0.0); 200];
    input[0] = Complex32::new(0.0, 1.0); // pure-Q impulse this time

    let out = apply_real_fir_complex(&RRC_TAPS_31250, &input);
    for (i, &h) in RRC_TAPS_31250.iter().enumerate() {
        assert!((out[i].re).abs() < 1e-6);
        assert!(
            (out[i].im - h).abs() < 1e-6,
            "RRC impulse response mismatch at tap {i}: got {}, expected {h}",
            out[i].im
        );
    }
}

/// Decimation by 2 must drop the odd samples.
#[test]
fn decimate_by_2_takes_evens() {
    let input: Vec<Complex32> = (0..10)
        .map(|i| Complex32::new(i as f32, -(i as f32)))
        .collect();
    let out = decimate_by_2(&input);
    assert_eq!(out.len(), 5);
    for (i, c) in out.iter().enumerate() {
        assert_eq!(c.re, (2 * i) as f32);
        assert_eq!(c.im, -((2 * i) as f32));
    }
}

/// LPF DC gain sanity check. scipy.signal.remez does NOT normalise its
/// output, so the design with desired=[1.0, 0.0] and equiripple
/// weighting gives a passband gain near (but not exactly) 1.0. The
/// frozen taps for this design measure to ≈0.9899 which matches the
/// raw scipy output — anything substantially off would indicate a
/// bad copy-paste of the tap export.
#[test]
fn lpf_dc_gain_close_to_unity() {
    let dc_gain: f32 = LPF_TAPS_31250.iter().sum();
    assert!(
        (dc_gain - 0.9899).abs() < 1e-3,
        "LPF DC gain {dc_gain} drifted from frozen value 0.9899; \
         tap export may be wrong"
    );
}

/// Streaming FIR fed in two chunks must produce the same output as a
/// single batch call. This is the bit-exact correctness test for the
/// per-IRQ chunked LSM pipeline.
#[test]
fn streaming_fir_matches_batch_across_chunks() {
    // Build a deterministic input (sin/cos of a frequency sweep).
    let n = 400;
    let input: Vec<Complex32> = (0..n)
        .map(|i| {
            let phi = 0.07 * i as f32 + 0.001 * (i as f32) * (i as f32);
            Complex32::new(phi.cos(), phi.sin())
        })
        .collect();
    let batch = apply_real_fir_complex(&LPF_TAPS_31250, &input);

    // Feed the same input in two chunks of different sizes.
    let mut sf = StreamingFir::new(&LPF_TAPS_31250);
    let split = 137; // intentionally not a multiple of anything
    let a = sf.process(&input[..split]);
    let b = sf.process(&input[split..]);
    let mut streamed: Vec<Complex32> = Vec::with_capacity(n);
    streamed.extend(a);
    streamed.extend(b);

    assert_eq!(streamed.len(), batch.len());
    for i in 0..n {
        let dr = (streamed[i].re - batch[i].re).abs();
        let di = (streamed[i].im - batch[i].im).abs();
        assert!(
            dr < 1e-5 && di < 1e-5,
            "streaming vs batch mismatch at sample {i}: \
             streamed=({:.6},{:.6}) batch=({:.6},{:.6})",
            streamed[i].re, streamed[i].im, batch[i].re, batch[i].im
        );
    }
}

/// Streaming /2 decimator across odd-length chunks must keep the
/// even-grid phase consistent. Concatenating the per-chunk outputs
/// must equal `decimate_by_2` of the concatenated input.
#[test]
fn streaming_decimator_preserves_phase_across_odd_chunks() {
    let input: Vec<Complex32> =
        (0..23).map(|i| Complex32::new(i as f32, 0.0)).collect();
    let batch = decimate_by_2(&input);

    // Chunk into [0..7], [7..15], [15..23] — odd chunk sizes.
    let mut dec = StreamingDecimator2::new();
    let mut streamed: Vec<Complex32> = Vec::new();
    streamed.extend(dec.process(&input[0..7]));
    streamed.extend(dec.process(&input[7..15]));
    streamed.extend(dec.process(&input[15..23]));

    assert_eq!(streamed, batch);
}
