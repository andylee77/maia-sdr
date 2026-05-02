//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.

use super::*;

#[test]
fn to_dibit_quadrants() {
    // Match Dibit.java mapping:
    //   +1 (   0..π/2) -> 00
    //   +3 ( π/2..π  ) -> 01
    //   -1 (-π/2..0  ) -> 10
    //   -3 (-π..-π/2 ) -> 11
    assert_eq!(to_dibit(PI / 4.0), 0b00);
    assert_eq!(to_dibit(3.0 * PI / 4.0), 0b01);
    assert_eq!(to_dibit(-PI / 4.0), 0b10);
    assert_eq!(to_dibit(-3.0 * PI / 4.0), 0b11);
}

#[test]
fn dibit_phase_inverse_of_to_dibit() {
    for d in 0..4u8 {
        assert_eq!(to_dibit(dibit_phase(d)), d);
    }
}

/// Smoke test: feed a constant DC carrier and verify the demod runs to
/// completion without panicking, produces the expected number of
/// symbols (approximately len/sps), and returns finite values.
/// Doesn't validate algorithmic correctness — that's covered by the
/// integration test against the Python reference output.
#[test]
fn demod_runs_on_constant_input() {
    let sample_rate = 31_250.0_f32;
    let n = 4096;
    // Constant rotating carrier so AGC has something to track.
    let iq: Vec<Complex32> = (0..n)
        .map(|i| {
            let t = i as f32 / sample_rate;
            let phi = 2.0 * PI * 1000.0 * t; // 1 kHz tone
            Complex32::new(phi.cos(), phi.sin())
        })
        .collect();
    let result = demod_lsm(&iq, sample_rate);
    let sps = sample_rate / P25_SYMBOL_RATE;
    let expected = (n as f32 / sps) as usize;
    // Allow ±2 symbol slop for the loop's edge handling.
    assert!(
        result.n_symbols() >= expected - 2 && result.n_symbols() <= expected + 2,
        "expected ~{expected} symbols, got {}",
        result.n_symbols()
    );
    for &p in &result.pll_trace {
        assert!(p.is_finite() && p.abs() <= MAX_PLL_ABS + 1e-6);
    }
    for &s in &result.soft_phases {
        assert!(s.is_finite());
    }
}
