use super::*;
use crate::lsm::filters::RRC_TAPS_25K;

#[test]
fn rrc_reproduces_the_p25_taps() {
    // RRC_TAPS_25K was ported from SDRTrunk's getRootRaisedCosine(25000/4800, 16, 0.2).
    let taps = root_raised_cosine(25_000.0 / 4800.0, 16, 0.2);
    assert_eq!(taps.len(), RRC_TAPS_25K.len());
    for (a, b) in taps.iter().zip(RRC_TAPS_25K.iter()) {
        assert!((a - b).abs() < 1e-6, "{a} vs {b}");
    }
}

#[test]
fn dmr_rrc_shape() {
    // DMRDecoder: alpha = 5760 / 25000, symbol length floor(-44 alpha + 33) = 22 (even).
    let alpha = 5760.0f32 / 25_000.0;
    let symbols = ((-44.0 * alpha) + 33.0).floor() as usize;
    assert_eq!(symbols, 22);
    let taps = root_raised_cosine(25_000.0 / 4800.0, symbols, alpha);
    assert_eq!(taps.len(), 57);
    let sum: f32 = taps.iter().sum();
    assert!((sum - 1.0).abs() < 1e-5);
    // Peak at the centre.
    let peak = taps.iter().cloned().fold(f32::MIN, f32::max);
    assert_eq!(peak, taps[28]);
}

#[test]
fn lowpass_is_symmetric() {
    for i in 0..18 {
        assert_eq!(LPF_DMR_25K[i], LPF_DMR_25K[36 - i]);
    }
}
