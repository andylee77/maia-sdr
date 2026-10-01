//! Unit tests for `frame.rs`: deinterleave, Golay(24,12) / PN / Golay(23,12)
//! and the b-vector against jmbe 1.0.9 on the capture, plus a test-only
//! encoder to inject errors and build tone frames.

use super::*;

/// Golay(23,12) parity of 12 data bits (`calculateChecksum`).
fn golay_parity(data: u32) -> u32 {
    (0..12)
        .filter(|&i| data & (1 << (11 - i)) != 0)
        .fold(0, |c, i| c ^ GOLAY_CHECKSUMS[i])
}

fn put(frame: &mut [u8; 9], positions: &[usize], value: u32) {
    let width = positions.len();
    for (i, &p) in positions.iter().enumerate() {
        if value & (1 << (width - 1 - i)) != 0 {
            frame[p / 8] |= 0x80 >> (p % 8);
        }
    }
}

/// Encodes u0, u1 (12 bits each), C2 (11 bits) and C3 (14 bits) as jmbe
/// decodes them: Golay(24,12) with even parity, scrambled Golay(23,12),
/// interleaved.
pub(crate) fn encode_vectors(u0: u32, u1: u32, c2: u32, c3: u32) -> [u8; 9] {
    let c0 = (u0 << 11) | golay_parity(u0);
    let c0 = (c0 << 1) | (c0.count_ones() & 1);
    let pn = modulation_vector(u0)
        .iter()
        .fold(0u32, |v, &b| (v << 1) | b as u32);
    let c1 = ((u1 << 11) | golay_parity(u1)) ^ pn;
    let mut frame = [0u8; 9];
    put(&mut frame, &VECTOR_C0, c0);
    put(&mut frame, &VECTOR_C1, c1);
    put(&mut frame, &VECTOR_C2, c2);
    put(&mut frame, &VECTOR_C3, c3);
    frame
}

/// Encodes a voice/silence/erasure b-vector.
pub(crate) fn encode_b(b: &[u32; 9]) -> [u8; 9] {
    let u0 = ((b[0] >> 3) << 8) | ((b[1] >> 1) << 4) | (b[2] >> 1);
    let u1 = ((b[3] >> 1) << 4) | (b[4] >> 3);
    let c2 = ((b[5] >> 1) << 7) | ((b[6] >> 1) << 4) | ((b[7] >> 1) << 1) | (b[8] >> 2);
    let c3 = ((b[1] & 1) << 13)
        | ((b[2] & 1) << 12)
        | ((b[0] & 7) << 9)
        | ((b[3] & 1) << 8)
        | ((b[4] & 7) << 5)
        | ((b[5] & 1) << 4)
        | ((b[6] & 1) << 3)
        | ((b[7] & 1) << 2)
        | (b[8] & 3);
    encode_vectors(u0, u1, c2, c3)
}

/// Encodes a tone frame: u0 = 111111 + amplitude high bits, u1 = id and the
/// id's high nibble repeated, C3's tone check bits zero.
pub(crate) fn encode_tone(id: u8, amplitude: u8) -> [u8; 9] {
    let u0 = (63 << 6) | (amplitude as u32 >> 1);
    let u1 = ((id as u32) << 4) | (id as u32 >> 4);
    let c3 = (4 << 9) | ((amplitude as u32 & 1) << 5);
    encode_vectors(u0, u1, 0, c3)
}

/// Flips bit `index` of code vector `vector` (0-3) in the frame.
pub(crate) fn flip(frame: &mut [u8; 9], vector: usize, index: usize) {
    let p = [
        &VECTOR_C0[..],
        &VECTOR_C1[..],
        &VECTOR_C2[..],
        &VECTOR_C3[..],
    ][vector][index];
    frame[p / 8] ^= 0x80 >> (p % 8);
}

pub(crate) fn capture_frames() -> Vec<[u8; 9]> {
    include_bytes!("test_frames_clay_ts2.bin")
        .chunks_exact(9)
        .map(|c| c.try_into().unwrap())
        .collect()
}

/// `AMBEFrame` errors, type and b-vector from jmbe 1.0.9 for each frame of
/// `test_frames_clay_ts2.bin` (`reference/AmbeReference.java` frames.txt).
#[rustfmt::skip]
const JMBE_FRAMES: [([u32; 2], FrameType, [u32; 9]); 108] = [
    ([0, 0], FrameType::Voice, [119, 16, 0, 87, 33, 2, 14, 13, 7]),
    ([0, 0], FrameType::Voice, [97, 16, 0, 87, 77, 8, 14, 1, 1]),
    ([0, 0], FrameType::Voice, [119, 20, 1, 60, 5, 18, 1, 13, 3]),
    ([0, 0], FrameType::Voice, [83, 28, 29, 394, 56, 21, 4, 15, 0]),
    ([0, 0], FrameType::Voice, [78, 23, 9, 347, 46, 13, 11, 0, 1]),
    ([0, 0], FrameType::Voice, [0, 21, 1, 257, 59, 1, 0, 13, 5]),
    ([0, 0], FrameType::Voice, [73, 15, 18, 283, 46, 22, 2, 1, 2]),
    ([0, 0], FrameType::Voice, [75, 12, 6, 410, 32, 13, 12, 2, 3]),
    ([0, 0], FrameType::Voice, [76, 14, 7, 330, 27, 14, 7, 12, 7]),
    ([0, 0], FrameType::Voice, [75, 12, 6, 283, 110, 22, 2, 1, 7]),
    ([0, 0], FrameType::Voice, [74, 12, 6, 261, 36, 18, 0, 14, 6]),
    ([0, 0], FrameType::Voice, [90, 14, 6, 328, 40, 10, 7, 13, 2]),
    ([0, 0], FrameType::Voice, [90, 16, 6, 277, 64, 20, 8, 8, 0]),
    ([0, 0], FrameType::Voice, [90, 16, 6, 287, 43, 29, 14, 1, 2]),
    ([0, 0], FrameType::Voice, [44, 16, 6, 140, 95, 13, 14, 14, 4]),
    ([0, 0], FrameType::Voice, [19, 24, 23, 41, 10, 10, 5, 5, 6]),
    ([0, 0], FrameType::Voice, [106, 16, 13, 160, 7, 10, 1, 3, 7]),
    ([0, 0], FrameType::Voice, [17, 24, 14, 17, 15, 21, 5, 13, 1]),
    ([0, 0], FrameType::Voice, [106, 16, 11, 43, 14, 10, 8, 1, 2]),
    ([0, 0], FrameType::Voice, [17, 24, 14, 16, 15, 8, 5, 1, 1]),
    ([0, 0], FrameType::Voice, [96, 16, 11, 11, 15, 10, 8, 7, 7]),
    ([0, 0], FrameType::Voice, [2, 21, 5, 456, 74, 2, 5, 11, 5]),
    ([0, 0], FrameType::Voice, [100, 16, 5, 224, 35, 19, 13, 15, 1]),
    ([0, 0], FrameType::Voice, [100, 16, 7, 186, 77, 12, 8, 12, 2]),
    ([0, 0], FrameType::Voice, [91, 16, 5, 277, 10, 18, 8, 13, 7]),
    ([0, 0], FrameType::Voice, [91, 16, 6, 321, 63, 22, 13, 0, 0]),
    ([0, 0], FrameType::Voice, [100, 16, 6, 99, 106, 10, 7, 11, 0]),
    ([0, 0], FrameType::Voice, [91, 16, 6, 276, 102, 12, 2, 13, 1]),
    ([0, 0], FrameType::Voice, [91, 16, 6, 230, 8, 1, 1, 10, 5]),
    ([0, 0], FrameType::Voice, [91, 16, 6, 176, 4, 26, 15, 12, 1]),
    ([0, 0], FrameType::Voice, [94, 16, 6, 370, 77, 11, 12, 10, 7]),
    ([0, 0], FrameType::Voice, [76, 16, 6, 352, 62, 22, 11, 11, 4]),
    ([0, 0], FrameType::Voice, [76, 16, 8, 204, 39, 5, 1, 13, 0]),
    ([0, 0], FrameType::Voice, [48, 30, 5, 225, 30, 25, 0, 12, 7]),
    ([0, 0], FrameType::Voice, [105, 16, 6, 286, 43, 5, 15, 10, 3]),
    ([0, 0], FrameType::Voice, [105, 16, 6, 261, 41, 10, 5, 12, 1]),
    ([0, 0], FrameType::Voice, [105, 16, 6, 140, 74, 26, 10, 13, 2]),
    ([0, 0], FrameType::Voice, [75, 16, 22, 59, 50, 10, 5, 9, 7]),
    ([0, 0], FrameType::Voice, [75, 16, 11, 315, 4, 29, 2, 15, 0]),
    ([0, 0], FrameType::Voice, [75, 16, 8, 373, 41, 2, 9, 15, 1]),
    ([0, 0], FrameType::Voice, [105, 16, 6, 334, 46, 25, 10, 0, 7]),
    ([0, 0], FrameType::Voice, [105, 14, 18, 53, 26, 14, 4, 3, 3]),
    ([0, 0], FrameType::Voice, [105, 16, 16, 316, 105, 21, 9, 4, 6]),
    ([0, 0], FrameType::Voice, [62, 12, 18, 95, 33, 0, 6, 15, 7]),
    ([0, 0], FrameType::Voice, [60, 13, 24, 387, 83, 31, 4, 0, 4]),
    ([0, 0], FrameType::Voice, [60, 8, 24, 129, 22, 10, 5, 9, 2]),
    ([0, 0], FrameType::Voice, [60, 0, 22, 400, 15, 6, 12, 11, 3]),
    ([0, 0], FrameType::Voice, [61, 0, 23, 408, 41, 21, 8, 7, 6]),
    ([0, 0], FrameType::Voice, [62, 2, 22, 273, 56, 22, 9, 13, 2]),
    ([0, 0], FrameType::Voice, [64, 2, 19, 211, 27, 14, 1, 3, 5]),
    ([0, 0], FrameType::Voice, [68, 2, 13, 210, 89, 13, 8, 14, 3]),
    ([0, 0], FrameType::Voice, [71, 10, 7, 138, 0, 25, 9, 11, 7]),
    ([0, 0], FrameType::Voice, [72, 8, 8, 266, 58, 22, 5, 12, 7]),
    ([0, 0], FrameType::Voice, [70, 2, 17, 209, 22, 14, 5, 11, 1]),
    ([0, 0], FrameType::Voice, [70, 2, 22, 392, 62, 13, 0, 3, 5]),
    ([0, 0], FrameType::Voice, [71, 2, 23, 272, 63, 13, 5, 10, 3]),
    ([0, 0], FrameType::Voice, [72, 0, 25, 177, 30, 22, 4, 10, 1]),
    ([0, 0], FrameType::Voice, [74, 6, 28, 5, 55, 13, 4, 3, 2]),
    ([0, 0], FrameType::Voice, [75, 12, 25, 165, 54, 6, 2, 9, 3]),
    ([0, 0], FrameType::Voice, [73, 2, 18, 271, 33, 23, 11, 10, 0]),
    ([0, 0], FrameType::Voice, [71, 2, 28, 235, 32, 21, 14, 8, 5]),
    ([0, 0], FrameType::Voice, [70, 2, 27, 74, 107, 22, 14, 11, 0]),
    ([0, 0], FrameType::Voice, [70, 1, 24, 496, 66, 14, 14, 11, 5]),
    ([0, 0], FrameType::Voice, [70, 0, 24, 340, 35, 6, 1, 8, 2]),
    ([0, 0], FrameType::Voice, [72, 0, 20, 215, 35, 13, 3, 15, 2]),
    ([0, 0], FrameType::Voice, [73, 0, 5, 483, 93, 21, 9, 13, 3]),
    ([0, 0], FrameType::Voice, [74, 12, 7, 199, 34, 10, 2, 1, 6]),
    ([0, 0], FrameType::Voice, [75, 12, 9, 343, 0, 0, 4, 5, 4]),
    ([0, 0], FrameType::Voice, [79, 16, 11, 312, 20, 25, 1, 15, 6]),
    ([0, 0], FrameType::Voice, [74, 15, 28, 257, 52, 14, 3, 9, 2]),
    ([0, 0], FrameType::Voice, [73, 0, 26, 137, 6, 13, 4, 10, 4]),
    ([0, 0], FrameType::Voice, [73, 0, 24, 409, 10, 4, 5, 11, 0]),
    ([0, 0], FrameType::Voice, [73, 0, 23, 144, 3, 22, 8, 2, 5]),
    ([0, 0], FrameType::Voice, [73, 0, 22, 404, 10, 21, 10, 8, 5]),
    ([0, 0], FrameType::Voice, [71, 0, 19, 144, 42, 19, 14, 1, 3]),
    ([0, 0], FrameType::Voice, [69, 0, 21, 136, 58, 8, 12, 3, 0]),
    ([0, 0], FrameType::Voice, [63, 14, 18, 281, 79, 31, 1, 11, 0]),
    ([0, 0], FrameType::Voice, [64, 7, 11, 307, 23, 18, 5, 3, 6]),
    ([0, 0], FrameType::Voice, [68, 8, 16, 306, 16, 8, 5, 13, 2]),
    ([0, 0], FrameType::Voice, [69, 8, 11, 256, 54, 29, 4, 9, 2]),
    ([0, 0], FrameType::Voice, [69, 9, 16, 427, 87, 13, 7, 15, 1]),
    ([0, 0], FrameType::Voice, [71, 9, 9, 151, 66, 10, 1, 10, 4]),
    ([0, 0], FrameType::Voice, [72, 16, 12, 197, 47, 4, 7, 1, 3]),
    ([0, 0], FrameType::Voice, [71, 16, 11, 421, 38, 13, 11, 9, 3]),
    ([0, 0], FrameType::Voice, [71, 14, 9, 339, 52, 15, 10, 2, 5]),
    ([0, 0], FrameType::Voice, [71, 14, 8, 389, 12, 9, 7, 1, 5]),
    ([0, 0], FrameType::Voice, [71, 16, 10, 143, 36, 20, 8, 10, 7]),
    ([0, 0], FrameType::Voice, [103, 16, 12, 235, 5, 31, 3, 12, 4]),
    ([0, 0], FrameType::Voice, [103, 16, 14, 421, 51, 27, 8, 5, 6]),
    ([0, 0], FrameType::Voice, [50, 15, 11, 199, 50, 13, 7, 3, 5]),
    ([0, 0], FrameType::Voice, [68, 12, 10, 227, 102, 31, 0, 7, 7]),
    ([0, 0], FrameType::Voice, [95, 14, 13, 334, 57, 27, 14, 12, 5]),
    ([0, 0], FrameType::Voice, [94, 14, 9, 481, 68, 28, 0, 13, 6]),
    ([0, 0], FrameType::Voice, [51, 12, 11, 278, 23, 21, 15, 3, 5]),
    ([0, 0], FrameType::Voice, [54, 12, 7, 324, 7, 3, 15, 4, 6]),
    ([0, 0], FrameType::Voice, [57, 12, 7, 430, 94, 27, 14, 13, 1]),
    ([0, 0], FrameType::Voice, [57, 16, 7, 69, 104, 4, 6, 3, 1]),
    ([0, 0], FrameType::Voice, [69, 15, 7, 488, 55, 19, 15, 9, 7]),
    ([0, 0], FrameType::Voice, [64, 14, 9, 279, 68, 0, 10, 0, 5]),
    ([0, 0], FrameType::Voice, [56, 12, 8, 498, 50, 24, 8, 2, 6]),
    ([0, 0], FrameType::Voice, [52, 12, 12, 355, 86, 28, 13, 10, 5]),
    ([0, 0], FrameType::Voice, [52, 12, 17, 439, 5, 25, 10, 0, 4]),
    ([3, 3], FrameType::Voice, [13, 1, 13, 160, 5, 10, 0, 2, 4]),
    ([3, 3], FrameType::Voice, [72, 20, 30, 138, 119, 8, 0, 14, 2]),
    ([3, 3], FrameType::Voice, [2, 0, 0, 439, 83, 29, 4, 3, 7]),
    ([3, 3], FrameType::Voice, [63, 4, 5, 490, 21, 17, 4, 6, 0]),
    ([3, 3], FrameType::Erasure, [120, 16, 17, 153, 103, 4, 1, 13, 2]),
    ([3, 3], FrameType::Voice, [10, 0, 7, 372, 99, 2, 10, 5, 0]),
];

#[test]
fn matches_jmbe_fec_and_parameters_on_capture() {
    let frames = capture_frames();
    assert_eq!(frames.len(), JMBE_FRAMES.len());
    for (i, (bytes, (errors, frame_type, b))) in frames.iter().zip(JMBE_FRAMES.iter()).enumerate() {
        let frame = AmbeFrame::decode(bytes);
        assert_eq!(frame.errors, *errors, "frame {i} errors");
        assert_eq!(frame.frame_type, *frame_type, "frame {i} type");
        assert_eq!(frame.b, *b, "frame {i} b-vector");
        assert_eq!(frame.b0 as u32, b[0], "frame {i} b0");
    }
}

/// Strong signal: the 34 voice bursts decode with no bit errors. The last
/// two bursts (E, F: EMB failed, the transmission had ended) are noise, which
/// the perfect Golay(23) code "corrects" with 3 bit errors per vector.
#[test]
fn error_counts_on_capture() {
    let frames = capture_frames();
    let errors: Vec<[u32; 2]> = frames.iter().map(|f| AmbeFrame::decode(f).errors).collect();
    let clean = errors.iter().filter(|e| **e == [0, 0]).count();
    eprintln!(
        "clean {clean}/{}; last six {:?}",
        errors.len(),
        &errors[102..]
    );
    assert!(errors[..102].iter().all(|e| *e == [0, 0]));
    assert!(errors[102..].iter().all(|e| e[0] + e[1] >= 4));
}

#[test]
fn encoder_round_trips_capture_frames() {
    for (bytes, (_, frame_type, b)) in capture_frames().iter().zip(JMBE_FRAMES.iter()).take(102) {
        if *frame_type == FrameType::Voice {
            assert_eq!(&encode_b(b), bytes);
        }
    }
}

#[test]
fn golay24_corrects_up_to_three_errors_in_c0() {
    let clean = capture_frames()[20];
    let reference = AmbeFrame::decode(&clean);
    // Data and parity positions, several patterns per weight.
    for bits in [
        &[0usize][..],
        &[11],
        &[17],
        &[22],
        &[0, 5],
        &[3, 19],
        &[12, 22],
        &[1, 2, 3],
        &[0, 11, 22],
        &[4, 13, 21],
    ] {
        let mut frame = clean;
        for &i in bits {
            flip(&mut frame, 0, i);
        }
        let decoded = AmbeFrame::decode(&frame);
        assert_eq!(decoded.errors, [bits.len() as u32, 0], "C0 bits {bits:?}");
        assert_eq!(decoded.b, reference.b, "C0 bits {bits:?}");
    }
}

#[test]
fn golay24_parity_bit_alone_counts_one_error() {
    let mut frame = capture_frames()[20];
    flip(&mut frame, 0, 23);
    let decoded = AmbeFrame::decode(&frame);
    assert_eq!(decoded.errors, [1, 0]);
    assert_eq!(decoded.b, AmbeFrame::decode(&capture_frames()[20]).b);
}

#[test]
fn golay23_corrects_descrambled_c1() {
    let clean = capture_frames()[30];
    let reference = AmbeFrame::decode(&clean);
    for bits in [&[0usize][..], &[22], &[7, 15], &[0, 1, 2], &[8, 12, 20]] {
        let mut frame = clean;
        for &i in bits {
            flip(&mut frame, 1, i);
        }
        let decoded = AmbeFrame::decode(&frame);
        assert_eq!(decoded.errors, [0, bits.len() as u32], "C1 bits {bits:?}");
        assert_eq!(decoded.b, reference.b, "C1 bits {bits:?}");
    }
}

#[test]
fn c0_errors_change_the_c1_descrambling() {
    // A miscorrected C0 seeds the wrong PN sequence, so C1 fails too.
    let mut frame = capture_frames()[40];
    for i in [0, 1, 2, 3] {
        flip(&mut frame, 0, i);
    }
    let decoded = AmbeFrame::decode(&frame);
    assert!(
        decoded.errors[0] >= 2 && decoded.errors[1] >= 2,
        "{:?}",
        decoded.errors
    );
}

#[test]
fn tone_frames() {
    let frame = AmbeFrame::decode(&encode_tone(133, 77));
    assert_eq!(frame.frame_type, FrameType::Tone);
    assert_eq!(frame.errors, [0, 0]);
    let tone = frame.tone.unwrap();
    assert_eq!((tone.id, tone.amplitude), (Some(133), 77));
    assert_eq!(tone.kind(), Some(ToneKind::Dtmf));
    assert_eq!(tone.metadata(), Some(("DTMF", "5")));
    assert_eq!(tone.frequencies(), (1336.0, 770.0));

    let single = AmbeFrame::decode(&encode_tone(32, 127)).tone.unwrap();
    assert_eq!(single.metadata(), Some(("TONE", "1000.00")));
    assert_eq!(single.frequencies(), (1000.0, 0.0));
    let busy = AmbeFrame::decode(&encode_tone(162, 10)).tone.unwrap();
    assert_eq!(busy.metadata(), Some(("CALL PROGRESS", "BUSY TONE")));
    let knox = AmbeFrame::decode(&encode_tone(150, 10)).tone.unwrap();
    assert_eq!(knox.metadata(), Some(("KNOX", "6")));

    // Not in jmbe's table: Tone.INVALID.
    let invalid = AmbeFrame::decode(&encode_tone(200, 50)).tone.unwrap();
    assert_eq!(
        (invalid.id, invalid.label(), invalid.metadata()),
        (None, "INVALID", None)
    );
}

#[test]
fn tone_b0_without_the_tone_pattern_is_an_erasure() {
    // b0 = 126 but u0's bits 4-5 are not 11.
    let frame = AmbeFrame::decode(&encode_b(&[126, 0, 4, 0, 0, 0, 0, 0, 0]));
    assert_eq!(frame.frame_type, FrameType::Erasure);
    assert_eq!(frame.b0, 120);
}

#[test]
fn silence_with_the_tone_pattern_reads_as_a_tone() {
    // jmbe's tone test only looks at u0 bits 0-5 and C3 bits 10-13: a
    // silence frame (b0 124) with b1 >= 24 and those bits zero is a tone.
    let frame = AmbeFrame::decode(&encode_b(&[124, 24, 3, 10, 9, 3, 0, 0, 0]));
    assert_eq!(frame.frame_type, FrameType::Tone);
    let silence = AmbeFrame::decode(&encode_b(&[124, 23, 3, 10, 9, 3, 0, 0, 0]));
    assert_eq!(silence.frame_type, FrameType::Silence);
}
