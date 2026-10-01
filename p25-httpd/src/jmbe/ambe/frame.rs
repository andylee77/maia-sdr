//! One 72-bit AMBE+2 frame: deinterleave, FEC and parameter bits.
//! Ports jmbe v1.0.9 `codec/ambe/AMBEFrame.java` and `edac/Golay24.java`;
//! Golay(23,12) reuses the IMBE port's `golay23_check_and_correct` (jmbe's
//! shared `edac/Golay23.java`).

use super::tables::TONES;
use super::FrameType;

// Bit positions of the four code vectors in the 72-bit frame (the DMR
// interleave), MSB first.
const VECTOR_C0: [usize; 24] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 64, 68, 1, 5, 9, 13, 17, 21,
];
const VECTOR_C1: [usize; 23] = [
    25, 29, 33, 37, 41, 45, 49, 53, 57, 61, 65, 69, 2, 6, 10, 14, 18, 22, 26, 30, 34, 38, 42,
];
const VECTOR_C2: [usize; 11] = [46, 50, 54, 58, 62, 66, 70, 3, 7, 11, 15];
const VECTOR_C3: [usize; 14] = [19, 23, 27, 31, 35, 39, 43, 47, 51, 55, 59, 63, 67, 71];

const GOLAY_CHECKSUMS: [u32; 12] = [
    0x63A, 0x31D, 0x7B4, 0x3DA, 0x1ED, 0x6CC, 0x366, 0x1B3, 0x6E3, 0x54B, 0x49F, 0x475,
];

/// Value of the bits `range` (inclusive, MSB first).
fn int(bits: &[bool], first: usize, last: usize) -> u32 {
    (first..=last).fold(0, |v, i| (v << 1) | bits[i] as u32)
}

/// Golay(23,12) syndrome of `bits[0..23]` (`Golay24.getSyndrome(message, 0)`).
fn syndrome(bits: &[bool]) -> u32 {
    let calculated = (0..12)
        .filter(|&i| bits[i])
        .fold(0, |c, i| c ^ GOLAY_CHECKSUMS[i]);
    int(bits, 12, 22) ^ calculated
}

fn rotate_left(bits: &mut [bool], start: usize, end: usize) {
    bits[start..=end].rotate_left(1);
}

fn rotate_right(bits: &mut [bool], places: usize, start: usize, end: usize) {
    for _ in 0..places {
        bits[start..=end].rotate_right(1);
    }
}

/// XOR `value` (`width` bits, MSB first) into `bits` at `offset`.
fn xor(bits: &mut [bool], offset: usize, width: usize, value: u32) {
    for x in 0..width {
        if value & (1 << (width - x - 1)) != 0 {
            bits[offset + x] = !bits[offset + x];
        }
    }
}

/// Ports `Golay24.checkAndCorrect(message, 0)`, which works in place on the
/// 24-bit vector (unlike Golay23 it keeps its trial bit flips and rotations
/// in `message`) and returns 0-3 corrected bits, 4 when the correction would
/// change more than 3 bits, or 2 when no correction was found.
pub(super) fn golay24_check_and_correct(message: &mut [bool; 24]) -> u32 {
    let parity_error = message.iter().filter(|&&b| b).count() % 2 != 0;
    let mut syndrome_value = syndrome(message);
    if syndrome_value == 0 {
        if parity_error {
            message[23] = !message[23];
            return 1;
        }
        return 0;
    }

    let original = int(message, 0, 22);
    let mut index: i32 = -1;
    let mut syndrome_weight = 3;
    while index < 23 {
        if index != -1 {
            // Restore the previous flipped bit, flip the next one.
            if index > 0 {
                let i = (index - 1) as usize;
                message[i] = !message[i];
            }
            message[index as usize] = !message[index as usize];
            syndrome_weight = 2;
        }
        syndrome_value = syndrome(message);
        // jmbe has no `else` here: a zero syndrome re-runs the same index.
        if syndrome_value > 0 {
            for i in 0..23 {
                let mut errors = syndrome_value.count_ones();
                if errors <= syndrome_weight {
                    xor(message, 12, 11, syndrome_value);
                    rotate_right(message, i, 0, 22);
                    if index >= 0 {
                        errors += 1;
                    }
                    let corrected = int(message, 0, 22);
                    if (original ^ corrected).count_ones() > 3 {
                        return 4;
                    }
                    return errors;
                }
                rotate_left(message, 0, 22);
                syndrome_value = syndrome(message);
            }
            index += 1;
        }
    }
    2
}

/// Golay(23,12) on the descrambled C1, through the IMBE port's decoder
/// (jmbe's shared `Golay23.checkAndCorrect`). Returns the bit errors, 4
/// when uncorrectable (the vector is then left as received).
fn golay23_check_and_correct(c1: &mut [bool; 23]) -> u32 {
    let mut frame = [false; 144];
    frame[..23].copy_from_slice(c1);
    let errors = crate::jmbe::golay23_check_and_correct(&mut frame, 0);
    c1.copy_from_slice(&frame[..23]);
    errors
}

/// C1's PN scrambling sequence, seeded with C0's 12 data bits (Algs 52-54,
/// `AMBEFrame.getModulationVector`).
fn modulation_vector(seed: u32) -> [bool; 23] {
    let mut vector = [false; 23];
    let mut pr = 16 * seed;
    for bit in vector.iter_mut() {
        pr = (173 * pr + 13849) % 65536;
        *bit = pr >= 32768;
    }
    vector
}

/// A tone frame's tone and amplitude (`ToneParameters`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmbeTone {
    /// Tone index (`Tone.mValue`); `None` when it is not in jmbe's table
    /// (`Tone.INVALID`).
    pub id: Option<u8>,
    /// Tone amplitude, 0-127.
    pub amplitude: u8,
}

/// jmbe's tone groups, which `getAudioWithMetadata` reports under these names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneKind {
    /// Single tones 5-122, metadata key `TONE`.
    Tone,
    /// DTMF 128-143, key `DTMF`.
    Dtmf,
    /// KNOX 144-159, key `KNOX`.
    Knox,
    /// Dial, ringing, busy, call progress 160-163, key `CALL PROGRESS`.
    CallProgress,
}

impl AmbeTone {
    fn entry(&self) -> Option<&'static (u8, &'static str, f64, f64)> {
        let id = self.id?;
        TONES.iter().find(|t| t.0 == id)
    }

    /// The tone's label (`Tone.toString()`), e.g. `"1000.00"`, `"5"`, `"BUSY TONE"`.
    pub fn label(&self) -> &'static str {
        self.entry().map_or("INVALID", |t| t.1)
    }

    /// Frequencies in Hz; the second is 0 for single tones.
    pub fn frequencies(&self) -> (f64, f64) {
        self.entry().map_or((0.0, 0.0), |t| (t.2, t.3))
    }

    pub fn kind(&self) -> Option<ToneKind> {
        match self.id? {
            5..=122 => Some(ToneKind::Tone),
            128..=143 => Some(ToneKind::Dtmf),
            144..=159 => Some(ToneKind::Knox),
            160..=163 => Some(ToneKind::CallProgress),
            _ => None,
        }
    }

    /// The metadata pair `getAudioWithMetadata` attaches, e.g. `("DTMF", "5")`.
    pub fn metadata(&self) -> Option<(&'static str, &'static str)> {
        let key = match self.kind()? {
            ToneKind::Tone => "TONE",
            ToneKind::Dtmf => "DTMF",
            ToneKind::Knox => "KNOX",
            ToneKind::CallProgress => "CALL PROGRESS",
        };
        Some((key, self.label()))
    }
}

/// A decoded frame (`AMBEFrame`): FEC results and the quantised parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AmbeFrame {
    /// Bit errors corrected in C0 (Golay 24,12) and C1 (Golay 23,12), as
    /// jmbe counts them (`getErrors()`): 0-3, or 4 when uncorrectable.
    pub errors: [u32; 2],
    pub frame_type: FrameType,
    /// Fundamental frequency index b0 (0-127) after jmbe's tone override.
    pub b0: u8,
    /// Quantiser values b0..b8 (`mB`); all 0 for tone frames.
    pub b: [u32; 9],
    /// The tone of a tone frame.
    pub tone: Option<AmbeTone>,
}

impl AmbeFrame {
    /// Decodes a 9-byte frame (72 bits, MSB first, as SDRTrunk's
    /// `VoiceMessage.getAMBEFrames()` hands them to jmbe).
    pub fn decode(bytes: &[u8; 9]) -> AmbeFrame {
        let bit = |i: usize| bytes[i / 8] & (0x80 >> (i % 8)) != 0;
        let mut c0 = [false; 24];
        let mut c1 = [false; 23];
        let mut c2 = [false; 11];
        let mut c3 = [false; 14];
        for (v, &i) in c0.iter_mut().zip(VECTOR_C0.iter()) {
            *v = bit(i);
        }
        for (v, &i) in c1.iter_mut().zip(VECTOR_C1.iter()) {
            *v = bit(i);
        }
        for (v, &i) in c2.iter_mut().zip(VECTOR_C2.iter()) {
            *v = bit(i);
        }
        for (v, &i) in c3.iter_mut().zip(VECTOR_C3.iter()) {
            *v = bit(i);
        }

        // Correct C0, then descramble and correct C1.
        let e0 = golay24_check_and_correct(&mut c0);
        let pn = modulation_vector(int(&c0, 0, 11));
        for (v, p) in c1.iter_mut().zip(pn.iter()) {
            *v ^= *p;
        }
        let e1 = golay23_check_and_correct(&mut c1);

        let mut b0 = (int(&c0, 0, 3) << 3) + int(&c3, 2, 4);
        let error_count = e0 + e1;

        let tone_frame = error_count < 6
            && int(&c0, 0, 5) == 63
            && (int(&c3, 10, 13) == 0 || int(&c1, 0, 3) == int(&c1, 8, 11));

        let mut b = [0u32; 9];
        let frame_type;
        let mut tone = None;
        if tone_frame {
            frame_type = FrameType::Tone;
            let id = int(&c1, 0, 7) as u8;
            let known = TONES.iter().any(|t| t.0 == id);
            tone = Some(AmbeTone {
                id: if known { Some(id) } else { None },
                amplitude: ((int(&c0, 6, 11) << 1) + c3[8] as u32) as u8,
            });
        } else {
            let mut ft = super::tables::FUNDAMENTAL[b0 as usize].2;
            // A tone b0 without the tone pattern is a bit error: jmbe makes
            // it an erasure (W120), which repeats the previous frame.
            if ft == FrameType::Tone {
                b0 = 120;
                ft = FrameType::Erasure;
            }
            frame_type = ft;
            b[0] = b0;
            b[1] = (int(&c0, 4, 7) << 1) + c3[0] as u32;
            b[2] = (int(&c0, 8, 11) << 1) + c3[1] as u32;
            b[3] = (int(&c1, 0, 7) << 1) + c3[5] as u32;
            b[4] = (int(&c1, 8, 11) << 3) + int(&c3, 6, 8);
            b[5] = (int(&c2, 0, 3) << 1) + c3[9] as u32;
            b[6] = (int(&c2, 4, 6) << 1) + c3[10] as u32;
            b[7] = (int(&c2, 7, 9) << 1) + c3[11] as u32;
            b[8] = (c2[10] as u32 * 4) + int(&c3, 12, 13);
        }

        AmbeFrame {
            errors: [e0, e1],
            frame_type,
            b0: b0 as u8,
            b,
            tone,
        }
    }
}

#[cfg(test)]
#[path = "frame_tests.rs"]
pub(super) mod tests;
