//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.

use super::*;

// Shared sanity: the all-zero vector is a valid codeword for every
// linear code, regardless of kk.
fn zero_roundtrip(kk: usize) {
    let input = [0u32; NN];
    let out = decode(&input, kk).expect("zero is a valid codeword");
    assert_eq!(out, input);
}

#[test]
fn rs_24_12_13_zero() {
    zero_roundtrip(51);
}

#[test]
fn rs_24_16_9_zero() {
    zero_roundtrip(55);
}

#[test]
fn rs_63_47_17_zero() {
    zero_roundtrip(47);
}

// Single-symbol error is correctable by every variant (t>=4).
fn single_error_correct(kk: usize) {
    let mut input = [0u32; NN];
    input[7] = 0x1F;
    let out = decode(&input, kk).expect("single-symbol correctable");
    assert_eq!(out[7], 0);
}

#[test]
fn rs_24_12_13_single() {
    single_error_correct(51);
}

#[test]
fn rs_24_16_9_single() {
    single_error_correct(55);
}

#[test]
fn rs_63_47_17_single() {
    single_error_correct(47);
}

// RS(24,16,9) has t=4, so 4 symbol errors are still at the edge of
// recoverability. Inject 3 to stay safely inside.
#[test]
fn rs_24_16_9_three_errors() {
    let mut input = [0u32; NN];
    input[3] = 0x2A;
    input[11] = 0x15;
    input[29] = 0x3F;
    let out = decode(&input, 55).expect("3 symbols correctable");
    for (i, &s) in out.iter().enumerate() {
        assert_eq!(s, 0, "position {} should be corrected to zero", i);
    }
}

// RS(63,47,17) has t=8, so we can inject more errors.
#[test]
fn rs_63_47_17_six_errors() {
    let mut input = [0u32; NN];
    for (i, pos) in [3, 11, 17, 29, 41, 53].iter().enumerate() {
        input[*pos] = ((i as u32) * 7 + 1) & 0x3F;
    }
    let out = decode(&input, 47).expect("6 symbols correctable");
    for (i, &s) in out.iter().enumerate() {
        assert_eq!(s, 0, "position {} should be corrected", i);
    }
}
