//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.


use super::*;

#[test]
fn test_fundamental_frequency_default() {
    let (w0, l) = compute_fundamental(134);
    // w0 = 4*PI / (134 + 39.5) = 4*PI/173.5
    // L = floor(0.9254 * floor(PI/w0 + 0.25)) = floor(0.9254 * 43) = 39
    assert_eq!(l, 39);
    assert!((w0 - (4.0 * PI / 173.5)).abs() < 0.0001);
}

#[test]
fn test_deinterleave_roundtrip() {
    let mut frame = [false; 144];
    frame[0] = true;
    frame[5] = true;
    frame[143] = true;
    let original = frame;
    deinterleave(&mut frame);
    // After deinterleave, bits should have moved
    assert_ne!(frame, original);
}

#[test]
fn test_decode_frame_does_not_panic() {
    let mut decoder = ImbeDecoder::new();
    let frame: [u8; 18] = [
        0x7C, 0x57, 0xB7, 0x9E, 0x01, 0x6C, 0x72, 0x54, 0x26, 0x11, 0xA1, 0xE3, 0x29, 0xDD,
        0xE3, 0xA3, 0xDC, 0xFE,
    ];
    let samples = decoder.decode_frame(&frame);
    // Should produce 160 samples without panicking
    assert_eq!(samples.len(), 160);
}

#[test]
fn test_gain_table_size() {
    assert_eq!(GAIN_TABLE.len(), 64);
}

#[test]
fn test_synthesis_window_bounds() {
    assert_eq!(synthesis_window(-106), 0.0);
    assert_eq!(synthesis_window(106), 0.0);
    assert!(synthesis_window(0) > 0.0);
    assert_eq!(synthesis_window(0), 1.0); // center of flat region
}
