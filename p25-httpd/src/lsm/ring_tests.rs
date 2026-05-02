//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.

use super::*;

/// One known IQ pair: re=0x1234, im=-0x5678 (little-endian on the wire).
#[test]
fn decode_single_pair() {
    // re = 0x1234 = 4660,  im = -0x5678 = -22136
    let bytes = [0x34, 0x12, 0x88, 0xA9];
    let out = sub_buffer_to_complex(&bytes);
    assert_eq!(out.len(), 1);
    assert!((out[0].re - 4660.0 / 32768.0).abs() < 1e-6);
    assert!((out[0].im - (-22136.0_f32) / 32768.0).abs() < 1e-6);
}

/// Sample 0 must come before sample 1 (preserves time order).
#[test]
fn preserves_sample_order() {
    let bytes = [
        0x01, 0x00, 0x02, 0x00, // sample 0: re=1, im=2
        0x03, 0x00, 0x04, 0x00, // sample 1: re=3, im=4
    ];
    let out = sub_buffer_to_complex(&bytes);
    assert_eq!(out.len(), 2);
    assert!((out[0].re - 1.0 / 32768.0).abs() < 1e-9);
    assert!((out[0].im - 2.0 / 32768.0).abs() < 1e-9);
    assert!((out[1].re - 3.0 / 32768.0).abs() < 1e-9);
    assert!((out[1].im - 4.0 / 32768.0).abs() < 1e-9);
}

#[test]
fn concatenate_two_subbuffers() {
    let a = [0x00, 0x00, 0x00, 0x00];
    let b = [0xFF, 0x7F, 0x00, 0x80]; // re=+max, im=-max
    let bufs: [&[u8]; 2] = [&a, &b];
    let out = sub_buffers_to_complex(&bufs);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], Complex32::new(0.0, 0.0));
    assert!((out[1].re - 32767.0 / 32768.0).abs() < 1e-6);
    assert!((out[1].im - (-32768.0_f32) / 32768.0).abs() < 1e-6);
}
