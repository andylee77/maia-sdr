//! Unit tests for `synth.rs`.

use super::*;

#[test]
fn java_random_matches_java_util_random() {
    // new java.util.Random(42).nextFloat() x4, as float bits.
    let mut r = JavaRandom::new(42);
    let bits: Vec<u32> = (0..4).map(|_| r.next_float().to_bits()).collect();
    assert_eq!(bits, vec![1060782493, 1029695648, 1060038587, 1027890176]);
}

#[test]
fn white_noise_skips_the_constructor_draws() {
    // Java: new Random(20260930), 257 nextFloat(), then the first sample
    // of getSamples(160, 0.003f).
    let mut noise = WhiteNoiseGenerator::new(20260930);
    assert_eq!(noise.samples()[0].to_bits() as i32, -1174831763);
}

#[test]
fn noise_sequence_starts_zeroed() {
    let mut noise = NoiseSequence::new();
    // jmbe never primes the buffer: the first frame's noise is all zero
    // (which is what makes its first unvoiced frame NaN).
    assert!(noise.next_buffer().iter().all(|&u| u == 0.0));
    let second = noise.next_buffer();
    assert!(second[..96].iter().all(|&u| u == 0.0));
    assert_eq!(second[96], 3147.0);
    assert_eq!(second[97], ((171.0f32 * 3147.0 + 11213.0) % 53125.0));
    let third = noise.next_buffer();
    assert_eq!(third[..96], second[160..]);
}

#[test]
fn oscillator_keeps_unit_magnitude() {
    let mut o = Oscillator::new();
    o.set_frequency(1000.0);
    let mut peak = 0.0f32;
    for _ in 0..50 {
        for s in o.generate(1.0) {
            peak = peak.max(s.abs());
        }
    }
    let (i, q) = o.angle;
    assert!(((i * i + q * q) - 1.0).abs() < 1e-3);
    assert!((peak - 1.0).abs() < 1e-3, "peak {peak}");
}

#[test]
fn zero_frequency_oscillator_is_silent() {
    let mut o = Oscillator::new();
    assert!(o.generate(1.0).iter().all(|&s| s == 0.0));
}
