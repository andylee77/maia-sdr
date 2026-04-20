//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.


use super::*;

/// Golay-encode 12 data bits -> 24-bit codeword (data + 11 parity
/// bits from CHECKSUMS + overall parity at bit 23). Builds
/// known-good codewords for unit tests.
fn golay_encode(data: u16) -> [bool; 24] {
    let mut cw = [false; 24];
    for i in 0..12 {
        cw[i] = (data >> (11 - i)) & 1 != 0;
    }
    let mut syn: u16 = 0;
    for i in 0..12 {
        if cw[i] {
            syn ^= GOLAY24_CHECKSUMS[i];
        }
    }
    // Spread syn across positions 12..22: bit 10 -> cw[12],
    // bit 0 -> cw[22].
    for i in 0..11 {
        cw[12 + i] = (syn >> (10 - i)) & 1 != 0;
    }
    // Overall parity: bit 23 = XOR of all other bits.
    let mut parity = false;
    for i in 0..23 {
        if cw[i] {
            parity ^= true;
        }
    }
    cw[23] = parity;
    cw
}

#[test]
fn golay24_no_errors_syndrome_zero() {
    let cw = golay_encode(0x345);
    assert_eq!(golay24_syndrome(&cw), 0);
}

#[test]
fn golay24_corrects_single_bit_errors() {
    for flip in 0..23 {
        let mut cw = golay_encode(0x345);
        cw[flip] ^= true;
        let n = golay24_correct(&mut cw).expect("single-bit correctable");
        assert_eq!(n, 1, "flip@{}", flip);
        // Data bits should match the original 0x345.
        let mut got: u16 = 0;
        for i in 0..12 {
            got = (got << 1) | if cw[i] { 1 } else { 0 };
        }
        assert_eq!(got, 0x345, "flip@{} data corrupted", flip);
    }
}

#[test]
fn golay24_corrects_double_bit_errors() {
    for a in 0..23 {
        for b in (a + 1)..23 {
            let mut cw = golay_encode(0xABC);
            cw[a] ^= true;
            cw[b] ^= true;
            let n = golay24_correct(&mut cw)
                .expect("double-bit correctable");
            assert!(n <= 2, "weight should be 1 or 2 (got {})", n);
            let mut got: u16 = 0;
            for i in 0..12 {
                got = (got << 1) | if cw[i] { 1 } else { 0 };
            }
            assert_eq!(got, 0xABC, "flip@{},{} data corrupted", a, b);
        }
    }
}

/// 807 dibits with status dibits stripped -> 784 data dibits.
#[test]
fn strip_807_dibits_yields_784() {
    let raw: Vec<u8> = (0..807).map(|i| (i & 0x03) as u8).collect();
    let stripped = strip_body_status_dibits(&raw);
    assert_eq!(stripped.len(), 784);
    // Verify the first few removed positions are exactly the
    // body status positions {13, 49, 85, ...}.
    let removed: Vec<usize> = (0..807)
        .filter(|p| is_body_status_dibit(*p))
        .collect();
    assert_eq!(removed[0], 13);
    assert_eq!(removed[1], 49);
    assert_eq!(removed[2], 85);
    assert_eq!(removed[3], 121);
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
/// raw positions {13, 49}.
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
