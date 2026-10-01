//! Filters of the DMR front end (SDRTrunk `DMRDecoder.setSampleRate`).
//! The demodulator works on phase, so their gain does not matter, only
//! their shape.

/// Baseband low-pass at 25 kSPS, after the half-band: pass 0-5100 Hz, stop
/// 6500 Hz, 0.01 ripple (SDRTrunk `getBasebandFilter`'s spec; its order
/// estimate at this rate gives 36):
/// `scipy.signal.remez(37, [0, 5100, 6500, 12500], [1, 0], fs=25000)`.
/// Pass ripple +-0.08 dB, stop band below -40.8 dB.
///
/// SDRTrunk runs this filter at the input rate before decimating (72 taps at
/// 50 kSPS here); decimating first, as the P25 C4FM port does, gives the
/// same response for a quarter of the work (half the rate, half the taps).
#[rustfmt::skip]
pub const LPF_DMR_25K: [f32; 37] = [
    4.784963153e-03, -3.982484845e-03, -8.026469275e-03, 1.399433568e-04, 9.833205647e-03, 9.850466585e-04,
    -1.504427216e-02, -6.055285378e-03, 1.961610411e-02, 1.373337795e-02, -2.465542375e-02, -2.676167133e-02,
    2.910466362e-02, 4.912275371e-02, -3.274697953e-02, -9.689253066e-02, 3.508398699e-02, 3.151472145e-01,
    4.641004424e-01, 3.151472145e-01, 3.508398699e-02, -9.689253066e-02, -3.274697953e-02, 4.912275371e-02,
    2.910466362e-02, -2.676167133e-02, -2.465542375e-02, 1.373337795e-02, 1.961610411e-02, -6.055285378e-03,
    -1.504427216e-02, 9.850466585e-04, 9.833205647e-03, 1.399433568e-04, -8.026469275e-03, -3.982484845e-03,
    4.784963153e-03,
];

/// SDRTrunk `FilterFactory.getRootRaisedCosine`: its own RRC form (it halves
/// the samples per symbol and uses `round(sps / 2 * symbols)` taps), scaled
/// to unit DC gain.
pub fn root_raised_cosine(samples_per_symbol: f64, symbol_count: usize, alpha: f32) -> Vec<f32> {
    use std::f64::consts::PI;
    let sps = samples_per_symbol / 2.0;
    let taps = (sps * symbol_count as f64).round() as usize;
    let alpha = alpha as f64;
    let mut c = vec![0.0f32; taps];
    let mut scale = 0.0f32;
    for (x, tap) in c.iter_mut().enumerate() {
        let index = x as f64 - (taps as f32 / 2.0) as f64;
        let x1 = PI * index / sps;
        let x2 = 4.0 * alpha * index / sps;
        let x3 = x2 * x2 - 1.0;
        let (numerator, denominator);
        if x3.abs() >= 0.000001 {
            numerator = if x != taps / 2 {
                ((1.0 + alpha) * x1).cos() + ((1.0 - alpha) * x1).sin() / (4.0 * alpha * index / sps)
            } else {
                ((1.0 + alpha) * x1).cos() + (1.0 - alpha) * PI / (4.0 * alpha)
            };
            denominator = x3 * PI;
        } else {
            if alpha == 1.0 {
                *tap = -1.0;
                continue;
            }
            let x3 = (1.0 - alpha) * x1;
            let x2 = (1.0 + alpha) * x1;
            numerator = x2.sin() * (1.0 + alpha) * PI - x3.cos() * ((1.0 - alpha) * PI * sps) / (4.0 * alpha * index)
                + x3.sin() * sps * sps / (4.0 * alpha * index * index);
            denominator = -32.0 * PI * alpha * alpha * index / sps;
        }
        *tap = (4.0 * alpha * numerator / denominator) as f32;
        scale += *tap;
    }
    for tap in c.iter_mut() {
        *tap /= scale;
    }
    c
}

#[cfg(test)]
#[path = "filters_tests.rs"]
mod tests;
