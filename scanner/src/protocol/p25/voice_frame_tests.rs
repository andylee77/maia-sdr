//! Host tests for `protocol::p25::voice_frame` (its FEC is tested in `protocol::fec`).

use super::*;

/// 807 dibits with status dibits stripped -> 784 data dibits.
#[test]
fn strip_807_dibits_yields_784() {
    let raw: Vec<u8> = (0..807).map(|i| (i & 0x03) as u8).collect();
    let stripped = strip_body_status_dibits(&raw);
    assert_eq!(stripped.len(), 784);
    // Verify the first few removed positions are exactly the
    // body status positions {14, 50, 86, ...}.
    let removed: Vec<usize> = (0..807)
        .filter(|p| is_body_status_dibit(*p))
        .collect();
    assert_eq!(removed[0], 14);
    assert_eq!(removed[1], 50);
    assert_eq!(removed[2], 86);
    assert_eq!(removed[3], 122);
    // The last body dibit is a status dibit (the frame's every 36th).
    assert_eq!(*removed.last().unwrap(), 806);
    assert_eq!(removed.len(), 23);
}

/// dibits_to_bits packs dibit MSB first: 0b10 -> [true, false],
/// 0b01 -> [false, true], 0b11 -> [true, true], 0b00 -> [false, false].
#[test]
fn dibits_to_bits_msb_first() {
    let dibits = vec![0b10, 0b01, 0b11, 0b00];
    let bits = dibits_to_bits(&dibits);
    assert_eq!(
        bits,
        vec![true, false, false, true, true, true, false, false]
    );
}

/// All-ones body: every IMBE frame should be 18 bytes of 0xFF.
#[test]
fn extract_all_ones_body_yields_all_ones_frames() {
    let raw = vec![0b11_u8; LDU_RAW_DIBITS];
    let frames = extract_imbe_frames(&raw).expect("len matches");
    for (i, frame) in frames.iter().enumerate() {
        for (j, &b) in frame.bits.iter().enumerate() {
            assert_eq!(b, 0xFF, "frame {} byte {}", i, j);
        }
    }
}

/// Marker bit at IMBE frame 0 bit 0 should land at frame 0
/// byte 0 bit 7.
#[test]
fn extract_first_bit_position() {
    // Set body data dibit 0 to 0b10 -> bit 0 = true, bit 1 = false.
    // Frame 0 starts at bit 0 so byte 0 bit 7 is the marker.
    let mut raw = vec![0b00_u8; LDU_RAW_DIBITS];
    raw[0] = 0b10; // body dibit 0
    let frames = extract_imbe_frames(&raw).expect("len matches");
    assert_eq!(
        frames[0].bits[0], 0x80,
        "frame 0 byte 0 should have bit 7 set (= IMBE bit 0)"
    );
    for &b in &frames[0].bits[1..] {
        assert_eq!(b, 0x00);
    }
}

/// Frame 1 starts at LDU data bit 144 (body data dibit 72).
/// Body data 72 = body raw 74 after skipping status dibits at
/// raw positions {14, 50}.
#[test]
fn extract_frame_1_marker_bit() {
    let mut raw = vec![0b00_u8; LDU_RAW_DIBITS];
    raw[74] = 0b11;
    let frames = extract_imbe_frames(&raw).expect("len matches");
    // All frame 0 bytes should be zero.
    for &b in &frames[0].bits {
        assert_eq!(b, 0x00);
    }
    // Frame 1 byte 0 should have bits 7 and 6 set = 0xC0.
    assert_eq!(frames[1].bits[0], 0xC0);
    for &b in &frames[1].bits[1..] {
        assert_eq!(b, 0x00);
    }
}
