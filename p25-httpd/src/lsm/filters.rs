//! P25 LSM front-end filters: baseband LPF + RRC matched filter.
//!
//! 2026-05-03 refactor: operating rate dropped from 31.25 kSPS to **25 kSPS**
//! to match SDRTrunk's effective decimation. SDRTrunk's
//! `P25P1DecoderLSM.setSampleRate` decimates to ~25 kSPS (sps≈5.20); we used
//! to operate at 31.25 kSPS (sps≈6.51) because that was the natural /2 of the
//! HDL DDC's 62.5 kSPS output — but per-symbol dibit comparison vs SDRTrunk's
//! reference `.bits` showed 1% disagreement on the same RF, costing ~1 LDU1 +
//! 1 LDU2 per call. Matching SDRTrunk's rate closes the gap. See
//! `doc/diagnostics/2026-05-02/SESSION_LOG_SW_DEMOD.md` "2026-05-03 follow-up
//! — closing the gap to SDRTrunk" for the full investigation.
//!
//! Pipeline at runtime (post-refactor):
//!
//! ```text
//! 25 kSPS IQ direct from MultistageDdc (live: 8 MSPS / 320; offline: WAV/cs16
//!                                       resampled by ddc_to_25k)
//!   ↓ apply_real_fir_complex(LPF_TAPS_25K)   121-tap baseband LPF
//! 25 kSPS
//!   ↓ apply_real_fir_complex(RRC_TAPS_25K)   83-tap RRC matched filter
//! 25 kSPS, sps=5.208  →  demod loop in `lsm::demod`
//! ```
//!
//! The `decimate_by_2` / `StreamingDecimator2` helpers below are kept for
//! historical / test use but are no longer in the live pipeline path.

use super::Complex32;

/// LSM operating sample rate. The DDC output and the LsmPipeline input are
/// expected at this rate. SDRTrunk uses 25 kSPS (decim from input to ~25 kSPS
/// via half-band cascade), giving sps ≈ 5.208 for the 4800-baud LSM symbol
/// rate. Matches `P25P1DecoderLSM.setSampleRate`'s `decimatedSampleRate`.
pub const POST_DECIMATION_RATE_HZ: f32 = 25_000.0;

/// 121-tap Parks-McClellan equiripple baseband LPF.
/// Designed at 25 kSPS, passband 0..7250 Hz, stopband 8000..12500 Hz,
/// 0.01 ripple in both bands (≥60 dB stopband attenuation). Generated via
/// `scipy.signal.remez(121, [0, 7250, 8000, 12500], [1, 0], fs=25000)` to
/// match SDRTrunk's `getBasebandFilter` spec exactly.
#[rustfmt::skip]
pub const LPF_TAPS_25K: [f32; 121] = [
     3.1489354708e-04,  9.4100803167e-05, -3.4864745047e-04,  2.8733952275e-04,
     1.4598442216e-04, -4.2330150582e-04,  7.5354609900e-05,  5.4412800228e-04,
    -5.5153326415e-04, -2.5285023923e-04,  8.7836087133e-04, -3.3309520737e-04,
    -8.6276373379e-04,  1.0756557287e-03,  2.5364660158e-04, -1.5115646409e-03,
     8.1757980050e-04,  1.2351699052e-03, -1.9147007015e-03, -6.6888603800e-05,
     2.3516561872e-03, -1.6608625032e-03, -1.5799244455e-03,  3.1431997823e-03,
    -4.4352976373e-04, -3.3905461001e-03,  3.0150917900e-03,  1.7749606561e-03,
    -4.8419053734e-03,  1.4687537145e-03,  4.5866006599e-03, -5.0709843341e-03,
    -1.6401885257e-03,  7.1058949993e-03, -3.2832659402e-03, -5.8677536676e-03,
     8.1051770992e-03,  9.0300712529e-04, -1.0091290180e-02,  6.3273390780e-03,
     7.1365335824e-03, -1.2618028953e-02,  8.9959602222e-04,  1.4154066289e-02,
    -1.1478928704e-02, -8.2831411679e-03,  1.9793326615e-02, -4.7795351354e-03,
    -2.0346917946e-02,  2.1103192605e-02,  9.1979005061e-03, -3.3502001661e-02,
     1.4046184421e-02,  3.3016615064e-02, -4.5399459649e-02, -9.7883560573e-03,
     7.7344758134e-02, -5.3697395756e-02, -1.0117585498e-01,  2.9929656024e-01,
     6.0999253158e-01,  2.9929656024e-01, -1.0117585498e-01, -5.3697395756e-02,
     7.7344758134e-02, -9.7883560573e-03, -4.5399459649e-02,  3.3016615064e-02,
     1.4046184421e-02, -3.3502001661e-02,  9.1979005061e-03,  2.1103192605e-02,
    -2.0346917946e-02, -4.7795351354e-03,  1.9793326615e-02, -8.2831411679e-03,
    -1.1478928704e-02,  1.4154066289e-02,  8.9959602222e-04, -1.2618028953e-02,
     7.1365335824e-03,  6.3273390780e-03, -1.0091290180e-02,  9.0300712529e-04,
     8.1051770992e-03, -5.8677536676e-03, -3.2832659402e-03,  7.1058949993e-03,
    -1.6401885257e-03, -5.0709843341e-03,  4.5866006599e-03,  1.4687537145e-03,
    -4.8419053734e-03,  1.7749606561e-03,  3.0150917900e-03, -3.3905461001e-03,
    -4.4352976373e-04,  3.1431997823e-03, -1.5799244455e-03, -1.6608625032e-03,
     2.3516561872e-03, -6.6888603800e-05, -1.9147007015e-03,  1.2351699052e-03,
     8.1757980050e-04, -1.5115646409e-03,  2.5364660158e-04,  1.0756557287e-03,
    -8.6276373379e-04, -3.3309520737e-04,  8.7836087133e-04, -2.5285023923e-04,
    -5.5153326415e-04,  5.4412800228e-04,  7.5354609900e-05, -4.2330150582e-04,
     1.4598442216e-04,  2.8733952275e-04, -3.4864745047e-04,  9.4100803167e-05,
     3.1489354708e-04,
];

/// 42-tap unit-DC-gain root raised cosine matched filter — direct port of
/// SDRTrunk's `FilterFactory.getRootRaisedCosine(samplesPerSymbol, 16, 0.2)`.
///
/// Caller passes sps=5.2083 (= 25000/4800); SDRTrunk's implementation
/// internally halves it to 2.604 and uses tap_count = round(2.604 * 16) =
/// 42. Note: this is NOT the textbook closed-form RRC — SDRTrunk uses a
/// custom form. We port the exact formula so per-symbol filter response
/// matches SDRTrunk byte-for-byte.
#[rustfmt::skip]
pub const RRC_TAPS_25K: [f32; 42] = [
    -1.6339162077e-03,  1.8771090183e-03,  2.3670586161e-03, -1.3910498304e-03,
    -2.7590046598e-03,  1.4128109526e-03,  3.6505662576e-03, -1.9015895097e-03,
    -6.4881333830e-03,  1.0319384546e-03,  1.1715452921e-02,  4.6909228467e-03,
    -1.6712231421e-02, -1.8499414711e-02,  1.4733965371e-02,  4.0508358436e-02,
     4.9727593191e-03, -6.6024274046e-02, -6.2497814267e-02,  8.6759549690e-02,
     3.0141475239e-01,  4.0391045131e-01,  3.0141475239e-01,  8.6759549690e-02,
    -6.2497814267e-02, -6.6024274046e-02,  4.9727593191e-03,  4.0508358436e-02,
     1.4733965371e-02, -1.8499414711e-02, -1.6712231421e-02,  4.6909228467e-03,
     1.1715452921e-02,  1.0319384546e-03, -6.4881333830e-03, -1.9015895097e-03,
     3.6505662576e-03,  1.4128109526e-03, -2.7590046598e-03, -1.3910498304e-03,
     2.3670586161e-03,  1.8771090183e-03,
];

// ── Legacy 31.25 kSPS taps (kept for historical reference + tests) ──
// These were the operating-rate filters used 2026-04 to 2026-05-03 inclusive.
// No longer in the live pipeline path. See module docstring for context.
#[rustfmt::skip]
#[allow(dead_code)]
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
/// Legacy — kept for historical reference, no longer in live path.
#[rustfmt::skip]
#[allow(dead_code)]
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
#[path = "filters_tests.rs"]
mod tests;
