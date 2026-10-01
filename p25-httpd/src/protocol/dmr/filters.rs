//! Filters of the DMR front end (SDRTrunk `DMRDecoder.setSampleRate`).
//! The demodulator works on phase, so their gain does not matter, only
//! their shape.

/// Baseband low-pass at the 50 kSPS input rate, before the half-band:
/// pass 0-5100 Hz, stop 6500 Hz, 0.01 ripple (SDRTrunk `getBasebandFilter`;
/// its order estimate gives 71, an even-length type 2 filter):
/// `scipy.signal.remez(72, [0, 5100, 6500, 25000], [1, 0], fs=50000)`.
/// Pass ripple ±0.09 dB, stop band below -39.9 dB.
#[rustfmt::skip]
pub const LPF_DMR_50K: [f32; 72] = [
    4.283080749e-03, -1.996115585e-03, -3.185241016e-03, -4.028413931e-03, -3.522043520e-03, -1.357488109e-03,
    1.764245102e-03, 4.359665328e-03, 4.873917722e-03, 2.602565679e-03, -1.716935431e-03, -6.010633467e-03,
    -7.800822938e-03, -5.537291901e-03, 3.101245638e-04, 7.143350417e-03, 1.128336236e-02, 9.872574425e-03,
    2.586567236e-03, -7.634734555e-03, -1.561899631e-02, -1.641823702e-02, -8.053201618e-03, 6.754329928e-03,
    2.111910207e-02, 2.682537485e-02, 1.846875388e-02, -3.026440868e-03, -2.933958593e-02, -4.708855250e-02,
    -4.295222041e-02, -9.620424243e-03, 5.003692365e-02, 1.226080253e-01, 1.881068842e-01, 2.268979764e-01,
    2.268979764e-01, 1.881068842e-01, 1.226080253e-01, 5.003692365e-02, -9.620424243e-03, -4.295222041e-02,
    -4.708855250e-02, -2.933958593e-02, -3.026440868e-03, 1.846875388e-02, 2.682537485e-02, 2.111910207e-02,
    6.754329928e-03, -8.053201618e-03, -1.641823702e-02, -1.561899631e-02, -7.634734555e-03, 2.586567236e-03,
    9.872574425e-03, 1.128336236e-02, 7.143350417e-03, 3.101245638e-04, -5.537291901e-03, -7.800822938e-03,
    -6.010633467e-03, -1.716935431e-03, 2.602565679e-03, 4.873917722e-03, 4.359665328e-03, 1.764245102e-03,
    -1.357488109e-03, -3.522043520e-03, -4.028413931e-03, -3.185241016e-03, -1.996115585e-03, 4.283080749e-03,
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
