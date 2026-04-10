//! P25 LSM front-end filters: half-band decimator, baseband LPF, RRC.
//!
//! Phase 6D port of `tools/p25_lsm_demod.py` stages 1-3. The taps below are
//! frozen from a one-shot run of the Python reference at the post-DDC input
//! rate (62.5 kSPS), with `select_decimation()` choosing dec=2 → 31.25 kSPS.
//! Embedding the taps here keeps Rust bit-comparable to Python without
//! pulling Parks-McClellan into the embedded build.
//!
//! Pipeline at runtime:
//!
//! ```text
//! 62.5 kSPS IQ from iq_dma ring
//!   ↓ decimate_by_2 (naive — DDC FIR3 already band-limits to ~8 kHz)
//! 31.25 kSPS
//!   ↓ apply_real_fir_complex(LPF_TAPS_31250)   83-tap baseband LPF
//! 31.25 kSPS
//!   ↓ apply_real_fir_complex(RRC_TAPS_31250)  105-tap RRC matched filter
//! 31.25 kSPS, ~6.51 sps  →  demod loop in `lsm::demod`
//! ```
//!
//! Why naive /2 decimation works here: the FPGA DDC's stage-3 FIR
//! (`P25_FIR3_COEFFS` in `fpga.rs`) is a 64-tap Kaiser LPF with 8 kHz
//! passband and >166 dB stopband. By the time IQ reaches the iq_dma ring
//! at 62.5 kSPS, everything outside ±8 kHz is already 166+ dB down, so
//! folding the upper half (8..31.25 kHz at 62.5 kSPS) into 0..8 kHz at
//! 31.25 kSPS adds no measurable distortion to the 6.25 kHz P25 channel.
//! The Python reference uses `scipy.signal.decimate` (Chebyshev IIR
//! zero-phase) for portability with arbitrary input rates; we can drop
//! that here because we control the input rate.

use super::Complex32;

/// Post-DDC sample rate after the /2 decimation stage.
pub const POST_DECIMATION_RATE_HZ: f32 = 31_250.0;

/// 83-tap Parks-McClellan equiripple baseband LPF.
/// Designed at 31.25 kSPS, passband 0..7250 Hz, stopband 8000..15625 Hz,
/// 0.01 ripple in both bands. Frozen from `design_baseband_lpf(31250)` in
/// `tools/p25_lsm_demod.py` (scipy.signal.remez).
#[rustfmt::skip]
pub const LPF_TAPS_31250: [f32; 83] = [
    1.8599540676e-04, -6.2620649561e-03, -3.6394699086e-04,  2.6395369719e-03,
    5.1450542933e-04, -3.1611807556e-03, -8.8228333582e-04,  3.7203046654e-03,
    1.3594551368e-03, -4.3145217852e-03, -1.9670868006e-03,  4.9395977123e-03,
    2.7230176112e-03, -5.5873223218e-03, -3.6520602689e-03,  6.2505818540e-03,
    4.7839919887e-03, -6.9186670918e-03, -6.1556700757e-03,  7.5850955063e-03,
    7.8156079622e-03, -8.2360150360e-03, -9.8258793891e-03,  8.8683630704e-03,
    1.2289056582e-02, -9.4554414724e-03, -1.5349465703e-02,  1.0012688844e-02,
    1.9241454340e-02, -1.0508576436e-02, -2.4388093939e-02,  1.0943656808e-02,
    3.1569350881e-02, -1.1317206147e-02, -4.2465966533e-02,  1.1612428065e-02,
    6.1485703065e-02, -1.1826637973e-02, -1.0478690800e-01,  1.1952966919e-02,
    3.1786922263e-01,  4.8800390754e-01,  3.1786922263e-01,  1.1952966919e-02,
   -1.0478690800e-01, -1.1826637973e-02,  6.1485703065e-02,  1.1612428065e-02,
   -4.2465966533e-02, -1.1317206147e-02,  3.1569350881e-02,  1.0943656808e-02,
   -2.4388093939e-02, -1.0508576436e-02,  1.9241454340e-02,  1.0012688844e-02,
   -1.5349465703e-02, -9.4554414724e-03,  1.2289056582e-02,  8.8683630704e-03,
   -9.8258793891e-03, -8.2360150360e-03,  7.8156079622e-03,  7.5850955063e-03,
   -6.1556700757e-03, -6.9186670918e-03,  4.7839919887e-03,  6.2505818540e-03,
   -3.6520602689e-03, -5.5873223218e-03,  2.7230176112e-03,  4.9395977123e-03,
   -1.9670868006e-03, -4.3145217852e-03,  1.3594551368e-03,  3.7203046654e-03,
   -8.8228333582e-04, -3.1611807556e-03,  5.1450542933e-04,  2.6395369719e-03,
   -3.6394699086e-04, -6.2620649561e-03,  1.8599540676e-04,
];

/// 105-tap unit-energy root raised cosine matched filter.
/// Designed at sps = 31250/4800 ≈ 6.510, alpha=0.2, 16 symbols.
/// Frozen from `design_rrc(31250/4800, 16, 0.2)` in
/// `tools/p25_lsm_demod.py` (closed-form formula, port of SDRTrunk's
/// `FilterFactory.getRootRaisedCosine`).
#[rustfmt::skip]
pub const RRC_TAPS_31250: [f32; 105] = [
   -1.0273903608e-03,  4.9391790526e-04,  1.9210274331e-03,  2.7859127149e-03,
    2.7780425735e-03,  1.8582775956e-03,  2.9086196446e-04, -1.4235960552e-03,
   -2.6999015827e-03, -3.0612742994e-03, -2.3125547450e-03, -6.3365290407e-04,
    1.4458663063e-03,  3.1944846269e-03,  3.9118854329e-03,  3.1785175670e-03,
    1.0395868449e-03, -1.9460807089e-03, -4.8254416324e-03, -6.5208370797e-03,
   -6.1804908328e-03, -3.5056071356e-03,  1.0560825467e-03,  6.3321157359e-03,
    1.0686622933e-02,  1.2473019771e-02,  1.0563435033e-02,  4.8006759025e-03,
   -3.7790331990e-03, -1.3055616990e-02, -2.0284336060e-02, -2.2820075974e-02,
   -1.8932243809e-02, -8.4851626307e-03,  6.7335492931e-03,  2.3183923215e-02,
    3.6271259189e-02,  4.1456125677e-02,  3.5535667092e-02,  1.7779115587e-02,
   -9.3995966017e-03, -4.0466103703e-02, -6.7569032311e-02, -8.2017973065e-02,
   -7.6154530048e-02, -4.5213922858e-02,  1.1257279664e-02,  8.8789448142e-02,
    1.7838372290e-01,  2.6789104939e-01,  3.4413120151e-01,  3.9532911777e-01,
    4.1336068511e-01,  3.9532911777e-01,  3.4413120151e-01,  2.6789104939e-01,
    1.7838372290e-01,  8.8789448142e-02,  1.1257279664e-02, -4.5213922858e-02,
   -7.6154530048e-02, -8.2017973065e-02, -6.7569032311e-02, -4.0466103703e-02,
   -9.3995966017e-03,  1.7779115587e-02,  3.5535667092e-02,  4.1456125677e-02,
    3.6271259189e-02,  2.3183923215e-02,  6.7335492931e-03, -8.4851626307e-03,
   -1.8932243809e-02, -2.2820075974e-02, -2.0284336060e-02, -1.3055616990e-02,
   -3.7790331990e-03,  4.8006759025e-03,  1.0563435033e-02,  1.2473019771e-02,
    1.0686622933e-02,  6.3321157359e-03,  1.0560825467e-03, -3.5056071356e-03,
   -6.1804908328e-03, -6.5208370797e-03, -4.8254416324e-03, -1.9460807089e-03,
    1.0395868449e-03,  3.1785175670e-03,  3.9118854329e-03,  3.1944846269e-03,
    1.4458663063e-03, -6.3365290407e-04, -2.3125547450e-03, -3.0612742994e-03,
   -2.6999015827e-03, -1.4235960552e-03,  2.9086196446e-04,  1.8582775956e-03,
    2.7780425735e-03,  2.7859127149e-03,  1.9210274331e-03,  4.9391790526e-04,
   -1.0273903608e-03,
];

/// Decimate complex IQ by 2 (naive — drops the odd samples).
///
/// Safe here because the FPGA DDC's stage-3 FIR has already attenuated
/// everything outside ±8 kHz at 62.5 kSPS by 166+ dB; folding the upper
/// half into 0..8 kHz at 31.25 kSPS adds no measurable distortion to
/// the 6.25 kHz P25 passband.
pub fn decimate_by_2(input: &[Complex32]) -> Vec<Complex32> {
    input.iter().step_by(2).copied().collect()
}

/// Streaming real-FIR over Complex32, with per-stream tail-history so
/// successive `process()` calls produce a continuous output stream with
/// no boundary transients. The non-streaming `apply_real_fir_complex`
/// below is the equivalent batch entry point used by the unit tests.
///
/// The internal history buffer holds the last `taps.len()-1` input
/// samples; on the next call those samples are prepended (logically) to
/// the new input so output sample y[n] = Σ taps[k] * x[n-k] is exactly
/// the same as if all input had been concatenated and filtered in one
/// pass.
pub struct StreamingFir {
    taps: &'static [f32],
    history: Vec<Complex32>,
}

impl StreamingFir {
    pub fn new(taps: &'static [f32]) -> Self {
        let history_len = taps.len().saturating_sub(1);
        StreamingFir {
            taps,
            history: vec![Complex32::new(0.0, 0.0); history_len],
        }
    }

    /// Filter `input` and return an equal-length output. State is
    /// preserved across calls — feed back-to-back chunks for a continuous
    /// stream.
    pub fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let t = self.taps.len();
        let h = self.history.len(); // == t - 1
        let n = input.len();
        if n == 0 {
            return Vec::new();
        }
        // Build a temporary "extended" input = history || input.
        // This is the simplest correct implementation; an in-place ring
        // buffer would save the allocation but is harder to read.
        let mut ext: Vec<Complex32> = Vec::with_capacity(h + n);
        ext.extend_from_slice(&self.history);
        ext.extend_from_slice(input);

        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            // Output index `i` in the new stream corresponds to ext index
            // `h + i`. Compute y[i] = Σ_{k=0..t-1} taps[k] * ext[h+i-k].
            let mut acc_re = 0.0_f32;
            let mut acc_im = 0.0_f32;
            for k in 0..t {
                let h_k = self.taps[k];
                let x = ext[h + i - k];
                acc_re += h_k * x.re;
                acc_im += h_k * x.im;
            }
            out.push(Complex32::new(acc_re, acc_im));
        }

        // Update history: copy the last (t-1) samples of `ext` into history.
        // ext has length h + n; we need its last h elements.
        let total = h + n;
        for k in 0..h {
            self.history[k] = ext[total - h + k];
        }
        out
    }
}

/// Streaming /2 decimator that remembers the input phase across calls.
/// Without phase tracking, an odd-length chunk would shift the decimation
/// grid by one sample on the next call.
pub struct StreamingDecimator2 {
    /// Number of input samples we still need to "skip" from the next chunk
    /// before re-aligning to the even grid. 0 = next chunk starts on an
    /// even sample; 1 = drop the first sample and start there.
    skip: usize,
}

impl StreamingDecimator2 {
    pub fn new() -> Self {
        StreamingDecimator2 { skip: 0 }
    }

    pub fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        if input.len() <= self.skip {
            self.skip -= input.len();
            return Vec::new();
        }
        let start = self.skip;
        // After processing this chunk, the next chunk's first sample has
        // input-index `input.len()`. The next "even" sample relative to
        // this chunk's start is at index `start + 2*k`; the largest such
        // index <= input.len()-1 is `start + 2*((input.len()-1-start)/2)`,
        // and the next even sample after the chunk ends is at
        // `start + 2*ceil((input.len()-start)/2)`. The skip for the next
        // chunk is the offset of that next-even-sample relative to
        // input.len().
        let consumed = input.len() - start;
        let new_skip = if consumed % 2 == 0 { 0 } else { 1 };

        let mut out = Vec::with_capacity((consumed + 1) / 2);
        let mut i = start;
        while i < input.len() {
            out.push(input[i]);
            i += 2;
        }
        self.skip = new_skip;
        out
    }
}

impl Default for StreamingDecimator2 {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply a real-coefficient FIR independently to I and Q (causal,
/// length-N output where N == input length, no transient trimming).
///
/// Mirrors `apply_real_fir` in `tools/p25_lsm_demod.py`, which uses
/// `scipy.signal.lfilter(taps, [1.0], …)` — i.e. the standard direct-form
/// I FIR with zero history. Output sample y[n] = sum_{k=0}^{T-1} h[k] * x[n-k]
/// for n >= 0, with x[m] := 0 for m < 0.
pub fn apply_real_fir_complex(taps: &[f32], input: &[Complex32]) -> Vec<Complex32> {
    let n = input.len();
    let t = taps.len();
    let mut out = vec![Complex32::new(0.0, 0.0); n];
    for i in 0..n {
        let kmax = (i + 1).min(t);
        let mut acc_re = 0.0f32;
        let mut acc_im = 0.0f32;
        // y[i] = sum_{k=0..kmax-1} taps[k] * input[i-k]
        for k in 0..kmax {
            let h = taps[k];
            let x = input[i - k];
            acc_re += h * x.re;
            acc_im += h * x.im;
        }
        out[i] = Complex32::new(acc_re, acc_im);
    }
    out
}

#[cfg(test)]
mod tests {
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
}
