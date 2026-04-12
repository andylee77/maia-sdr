//! Phase 7D: IMBE vocoder.
//!
//! Converts raw 144-bit IMBE frames (`ImbeFrameRaw`) to 160-sample
//! PCM audio at 8 kHz (20 ms per frame).
//!
//! Two backends:
//! - **JMBE** (default): Pure Rust port of the Java MBE library.
//!   Better audio quality (spectral enhancement, adaptive smoothing).
//! - **mbelib** (fallback): C library via FFI. Available if JMBE
//!   has issues.

use std::os::raw::{c_char, c_int, c_short};

pub use mbelib_sys::{SAMPLES_PER_FRAME, SAMPLE_RATE};

use crate::p25::voice_frame::ImbeFrameRaw;

/// Persistent vocoder state. Not `Send`/`Sync` because the C structs
/// contain internal state that isn't thread-safe — use from a single
/// task only.
pub struct ImbeDecoder {
    cur_mp: mbelib_sys::mbe_parms,
    prev_mp: mbelib_sys::mbe_parms,
    prev_mp_enhanced: mbelib_sys::mbe_parms,
}

impl ImbeDecoder {
    /// Create a new decoder with initialized codec state.
    pub fn new() -> Self {
        unsafe {
            let mut cur = std::mem::zeroed::<mbelib_sys::mbe_parms>();
            let mut prev = std::mem::zeroed::<mbelib_sys::mbe_parms>();
            let mut prev_enh = std::mem::zeroed::<mbelib_sys::mbe_parms>();
            mbelib_sys::mbe_initMbeParms(&mut cur, &mut prev, &mut prev_enh);
            Self {
                cur_mp: cur,
                prev_mp: prev,
                prev_mp_enhanced: prev_enh,
            }
        }
    }

    /// Reset the decoder state (e.g. on call boundary).
    pub fn reset(&mut self) {
        unsafe {
            mbelib_sys::mbe_initMbeParms(
                &mut self.cur_mp,
                &mut self.prev_mp,
                &mut self.prev_mp_enhanced,
            );
        }
    }

    /// Decode one raw 144-bit IMBE frame to PCM.
    ///
    /// Returns `(pcm, errs, errs2)` where:
    /// - `pcm`: 160 signed 16-bit PCM samples at 8 kHz
    /// - `errs`: uncorrectable bit errors
    /// - `errs2`: correctable bit errors
    pub fn decode_frame(
        &mut self,
        frame: &ImbeFrameRaw,
    ) -> ([i16; SAMPLES_PER_FRAME], i32, i32) {
        let mut imbe_fr = [[0i8; 23]; 8];
        let mut imbe_d = [0i8; 88];
        let mut pcm = [0i16; SAMPLES_PER_FRAME];
        let mut errs: c_int = 0;
        let mut errs2: c_int = 0;
        let mut err_str = [0i8; 64];

        // Unpack 18 bytes (MSB-first) into imbe_fr[8][23]
        unpack_imbe_frame(&frame.bits, &mut imbe_fr);

        unsafe {
            mbelib_sys::mbe_processImbe7200x4400Frame(
                pcm.as_mut_ptr() as *mut c_short,
                &mut errs,
                &mut errs2,
                err_str.as_mut_ptr() as *mut c_char,
                imbe_fr.as_mut_ptr() as *mut [c_char; 23],
                imbe_d.as_mut_ptr() as *mut c_char,
                &mut self.cur_mp,
                &mut self.prev_mp,
                &mut self.prev_mp_enhanced,
                3, // uvquality: default
            );
        }

        (pcm, errs, errs2)
    }
}

/// P25 IMBE de-interleave tables from DSD (p25p1_const.h).
///
/// Each on-air dibit position `d` (0..71) scatters its two bits:
///   MSB → imbe_fr[IW[d]][IX[d]]
///   LSB → imbe_fr[IY[d]][IZ[d]]
///
/// These are the canonical tables used by DSD, DSD-fme, and OP25.
#[rustfmt::skip]
const IW: [usize; 72] = [
    0, 2, 4, 1, 3, 5,    0, 2, 4, 1, 3, 6,
    0, 2, 4, 1, 3, 6,    0, 2, 4, 1, 3, 6,
    0, 2, 4, 1, 3, 6,    0, 2, 4, 1, 3, 6,
    0, 2, 5, 1, 3, 6,    0, 2, 5, 1, 3, 6,
    0, 2, 5, 1, 3, 7,    0, 2, 5, 1, 3, 7,
    0, 2, 5, 1, 4, 7,    0, 3, 5, 2, 4, 7,
];
#[rustfmt::skip]
const IX: [usize; 72] = [
    22, 20, 10, 20, 18,  0,   20, 18,  8, 18, 16, 13,
    18, 16,  6, 16, 14, 11,   16, 14,  4, 14, 12,  9,
    14, 12,  2, 12, 10,  7,   12, 10,  0, 10,  8,  5,
    10,  8, 13,  8,  6,  3,    8,  6, 11,  6,  4,  1,
     6,  4,  9,  4,  2,  6,    4,  2,  7,  2,  0,  4,
     2,  0,  5,  0, 13,  2,    0, 21,  3, 21, 11,  0,
];
#[rustfmt::skip]
const IY: [usize; 72] = [
    1, 3, 5, 0, 2, 4,    1, 3, 6, 0, 2, 4,
    1, 3, 6, 0, 2, 4,    1, 3, 6, 0, 2, 4,
    1, 3, 6, 0, 2, 4,    1, 3, 6, 0, 2, 5,
    1, 3, 6, 0, 2, 5,    1, 3, 6, 0, 2, 5,
    1, 3, 6, 0, 2, 5,    1, 3, 7, 0, 2, 5,
    1, 4, 7, 0, 3, 5,    2, 4, 7, 1, 3, 5,
];
#[rustfmt::skip]
const IZ: [usize; 72] = [
    21, 19,  1, 21, 19,  9,   19, 17, 14, 19, 17,  7,
    17, 15, 12, 17, 15,  5,   15, 13, 10, 15, 13,  3,
    13, 11,  8, 13, 11,  1,   11,  9,  6, 11,  9, 14,
     9,  7,  4,  9,  7, 12,    7,  5,  2,  7,  5, 10,
     5,  3,  0,  5,  3,  8,    3,  1,  5,  3,  1,  6,
     1, 14,  3,  1, 22,  4,   22, 12,  1, 22, 20,  2,
];

/// Unpack 18 bytes (144 bits = 72 dibits, MSB-first) into mbelib's
/// `imbe_fr[8][23]` using the DSD de-interleave tables.
///
/// Our `ImbeFrameRaw` stores 72 dibits in transmission order,
/// packed MSB-first (dibit 0 = byte 0 bits 7:6). Each dibit's
/// MSB goes to `imbe_fr[IW[d]][IX[d]]` and LSB to
/// `imbe_fr[IY[d]][IZ[d]]`, scattering bits across the 8 FEC
/// code words for mbelib.
fn unpack_imbe_frame(packed: &[u8; 18], imbe_fr: &mut [[i8; 23]; 8]) {
    *imbe_fr = [[0i8; 23]; 8];

    for d in 0..72 {
        let bit0_idx = d * 2;     // MSB of dibit
        let bit1_idx = d * 2 + 1; // LSB of dibit

        let byte0 = bit0_idx / 8;
        let shift0 = 7 - (bit0_idx % 8);
        let msb = ((packed[byte0] >> shift0) & 1) as i8;

        let byte1 = bit1_idx / 8;
        let shift1 = 7 - (bit1_idx % 8);
        let lsb = ((packed[byte1] >> shift1) & 1) as i8;

        imbe_fr[IW[d]][IX[d]] = msb;
        imbe_fr[IY[d]][IZ[d]] = lsb;
    }
}

// ── JMBE decoder (primary) ─────────────────────────────────────────────

/// JMBE-based IMBE decoder. Pure Rust, no FFI. Better audio quality
/// than mbelib due to spectral enhancement and adaptive smoothing.
pub struct JmbeDecoder {
    inner: crate::jmbe::ImbeDecoder,
}

impl JmbeDecoder {
    pub fn new() -> Self {
        Self {
            inner: crate::jmbe::ImbeDecoder::new(),
        }
    }

    pub fn reset(&mut self) {
        self.inner = crate::jmbe::ImbeDecoder::new();
    }

    /// Decode one raw 144-bit IMBE frame to PCM.
    /// Returns 160 signed 16-bit PCM samples at 8 kHz.
    pub fn decode_frame(&mut self, frame: &ImbeFrameRaw) -> [i16; SAMPLES_PER_FRAME] {
        let floats = self.inner.decode_frame(&frame.bits);
        let mut pcm = [0i16; SAMPLES_PER_FRAME];
        for (i, &f) in floats.iter().enumerate() {
            // JMBE outputs float in roughly [-1.0, 1.0] range scaled
            // for 16-bit. Clamp and convert.
            let scaled = (f * 32767.0).round();
            pcm[i] = scaled.clamp(-32768.0, 32767.0) as i16;
        }
        pcm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpack_all_ones() {
        // All-ones input: every de-interleaved position should be 1
        let packed = [0xFFu8; 18];
        let mut imbe_fr = [[0i8; 23]; 8];
        unpack_imbe_frame(&packed, &mut imbe_fr);

        // Count total ones placed by de-interleave (should be 144)
        let total: i32 = imbe_fr.iter().flat_map(|r| r.iter()).map(|&b| b as i32).sum();
        assert_eq!(total, 144, "all-ones input should produce 144 ones");
    }

    #[test]
    fn unpack_all_zeros() {
        let packed = [0x00u8; 18];
        let mut imbe_fr = [[1i8; 23]; 8]; // pre-fill with ones
        unpack_imbe_frame(&packed, &mut imbe_fr);

        let total: i32 = imbe_fr.iter().flat_map(|r| r.iter()).map(|&b| b as i32).sum();
        assert_eq!(total, 0, "all-zeros input should produce all zeros");
    }

    #[test]
    fn unpack_first_dibit_goes_to_dsd_positions() {
        // Dibit 0 (bits 0,1) maps via DSD tables:
        //   MSB → imbe_fr[IW[0]][IX[0]] = imbe_fr[0][22]
        //   LSB → imbe_fr[IY[0]][IZ[0]] = imbe_fr[1][21]
        let mut packed = [0x00u8; 18];
        packed[0] = 0b11_000000; // first dibit = 0b11
        let mut imbe_fr = [[0i8; 23]; 8];
        unpack_imbe_frame(&packed, &mut imbe_fr);

        assert_eq!(imbe_fr[0][22], 1, "MSB of dibit 0 -> IW[0]=0, IX[0]=22");
        assert_eq!(imbe_fr[1][21], 1, "LSB of dibit 0 -> IY[0]=1, IZ[0]=21");
        let total: i32 = imbe_fr.iter().flat_map(|r| r.iter()).map(|&b| b as i32).sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn decode_silence_frame() {
        let mut dec = ImbeDecoder::new();
        let frame = ImbeFrameRaw { bits: [0u8; 18] };
        let (pcm, _errs, _errs2) = dec.decode_frame(&frame);
        // We just verify it doesn't crash and produces 160 samples
        assert_eq!(pcm.len(), SAMPLES_PER_FRAME);
    }

    #[test]
    fn decode_multiple_frames_no_crash() {
        let mut dec = ImbeDecoder::new();
        for i in 0..9 {
            let mut bits = [0u8; 18];
            bits[0] = i as u8;
            let frame = ImbeFrameRaw { bits };
            let (_pcm, _errs, _errs2) = dec.decode_frame(&frame);
        }
    }

    /// Alt de-interleave: LSB-first within code words.
    #[rustfmt::skip]
    const DEINTERLEAVE_ALT: [(usize, usize); 144] = [
        (0, 0), (0,12), (1, 1), (1,13), (2, 2), (2,14),
        (3, 3), (3,15), (4, 4), (5, 1), (5,13), (6,10),
        (0, 1), (0,13), (1, 2), (1,14), (2, 3), (2,15),
        (3, 4), (3,16), (4, 5), (5, 2), (5,14), (6,11),
        (0, 2), (0,14), (1, 3), (1,15), (2, 4), (2,16),
        (3, 5), (3,17), (4, 6), (5, 3), (6, 0), (6,12),
        (0, 3), (0,15), (1, 4), (1,16), (2, 5), (2,17),
        (3, 6), (3,18), (4, 7), (5, 4), (6, 1), (6,13),
        (0, 4), (0,16), (1, 5), (1,17), (2, 6), (2,18),
        (3, 7), (3,19), (4, 8), (5, 5), (6, 2), (6,14),
        (0, 5), (0,17), (1, 6), (1,18), (2, 7), (2,19),
        (3, 8), (3,20), (4, 9), (5, 6), (6, 3), (7, 0),
        (0, 6), (0,18), (1, 7), (1,19), (2, 8), (2,20),
        (3, 9), (3,21), (4,10), (5, 7), (6, 4), (7, 1),
        (0, 7), (0,19), (1, 8), (1,20), (2, 9), (2,21),
        (3,10), (3,22), (4,11), (5, 8), (6, 5), (7, 2),
        (0, 8), (0,20), (1, 9), (1,21), (2,10), (2,22),
        (3,11), (4, 0), (4,12), (5, 9), (6, 6), (7, 3),
        (0, 9), (0,21), (1,10), (1,22), (2,11), (3, 0),
        (3,12), (4, 1), (4,13), (5,10), (6, 7), (7, 4),
        (0,10), (0,22), (1,11), (2, 0), (2,12), (3, 1),
        (3,13), (4, 2), (4,14), (5,11), (6, 8), (7, 5),
        (0,11), (1, 0), (1,12), (2, 1), (2,13), (3, 2),
        (3,14), (4, 3), (5, 0), (5,12), (6, 9), (7, 6),
    ];

    fn unpack_alt(packed: &[u8; 18], imbe_fr: &mut [[i8; 23]; 8]) {
        *imbe_fr = [[0i8; 23]; 8];
        for (bit_idx, &(row, col)) in DEINTERLEAVE_ALT.iter().enumerate() {
            let byte_pos = bit_idx / 8;
            let shift = 7 - (bit_idx % 8);
            imbe_fr[row][col] = ((packed[byte_pos] >> shift) & 1) as i8;
        }
    }

    /// Sequential (non-de-interleaved) unpack for comparison testing.
    fn unpack_sequential(packed: &[u8; 18], imbe_fr: &mut [[i8; 23]; 8]) {
        let row_sizes = [23, 23, 23, 23, 15, 15, 15, 7];
        *imbe_fr = [[0i8; 23]; 8];
        let mut bit_idx = 0usize;
        for (row, &size) in row_sizes.iter().enumerate() {
            for col in (0..size).rev() {
                let byte_pos = bit_idx / 8;
                let bit_pos = 7 - (bit_idx % 8);
                imbe_fr[row][col] = ((packed[byte_pos] >> bit_pos) & 1) as i8;
                bit_idx += 1;
            }
        }
    }

    /// Decode captured frames with BOTH unpackings and compare.
    #[test]
    #[ignore]
    fn decode_captured_compare_unpackings() {
        let hex_frames = [
            "ee54972c2201c44dfa51099dbdf3dd0809a7",
            "c03d1bc0e1008453cef445fbfecc46f692cb",
            "0eb3b6c8868795c5589e4bbac728b235157a",
            "9a9601d6b30bc993822c104af2c5686f495e",
            "0af5d3db801a2b9bffe9106d9ba2a6ca1f60",
            "aaf9f33c9b0aca37953944044436a7aa8bfc",
            "3f2935f65944c1c27d33348249c30884ef34",
            "f4c5e32e4a4ea70cb75164ff46980b443e47",
            "ea20a65ebd0aed61ad8db1b391b8a3a40fe7",
        ];

        eprintln!("\n=== DE-INTERLEAVED (TIA-102.BAHA table) ===");
        let mut dec1 = ImbeDecoder::new();
        for (i, hex) in hex_frames.iter().enumerate() {
            let bytes: Vec<u8> = (0..hex.len()).step_by(2)
                .map(|j| u8::from_str_radix(&hex[j..j+2], 16).unwrap()).collect();
            let mut bits = [0u8; 18];
            bits.copy_from_slice(&bytes[..18]);
            let frame = ImbeFrameRaw { bits };
            let (pcm, errs, errs2) = dec1.decode_frame(&frame);
            let max_s = pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
            eprintln!("  frame {i}: errs={errs} errs2={errs2} max={max_s}");
        }

        eprintln!("\n=== ALT DE-INTERLEAVE (LSB-first code words) ===");
        let mut dec3 = ImbeDecoder::new();
        for (i, hex) in hex_frames.iter().enumerate() {
            let bytes: Vec<u8> = (0..hex.len()).step_by(2)
                .map(|j| u8::from_str_radix(&hex[j..j+2], 16).unwrap()).collect();
            let mut bits = [0u8; 18];
            bits.copy_from_slice(&bytes[..18]);
            let mut imbe_fr = [[0i8; 23]; 8];
            let mut imbe_d = [0i8; 88];
            let mut pcm = [0i16; 160];
            let mut errs: i32 = 0;
            let mut errs2: i32 = 0;
            let mut err_str = [0i8; 64];
            unpack_alt(&bits, &mut imbe_fr);
            unsafe {
                mbelib_sys::mbe_processImbe7200x4400Frame(
                    pcm.as_mut_ptr(), &mut errs, &mut errs2,
                    err_str.as_mut_ptr(),
                    imbe_fr.as_mut_ptr() as *mut [i8; 23],
                    imbe_d.as_mut_ptr(),
                    &mut dec3.cur_mp, &mut dec3.prev_mp, &mut dec3.prev_mp_enhanced,
                    3,
                );
            }
            let max_s = pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
            eprintln!("  frame {i}: errs={errs} errs2={errs2} max={max_s}");
        }

        eprintln!("\n=== SEQUENTIAL (no de-interleave) ===");
        let mut dec2 = ImbeDecoder::new();
        for (i, hex) in hex_frames.iter().enumerate() {
            let bytes: Vec<u8> = (0..hex.len()).step_by(2)
                .map(|j| u8::from_str_radix(&hex[j..j+2], 16).unwrap()).collect();
            let mut bits = [0u8; 18];
            bits.copy_from_slice(&bytes[..18]);
            // Use sequential unpacking
            let mut imbe_fr = [[0i8; 23]; 8];
            let mut imbe_d = [0i8; 88];
            let mut pcm = [0i16; 160];
            let mut errs: i32 = 0;
            let mut errs2: i32 = 0;
            let mut err_str = [0i8; 64];
            unpack_sequential(&bits, &mut imbe_fr);
            unsafe {
                mbelib_sys::mbe_processImbe7200x4400Frame(
                    pcm.as_mut_ptr(),
                    &mut errs,
                    &mut errs2,
                    err_str.as_mut_ptr(),
                    imbe_fr.as_mut_ptr() as *mut [i8; 23],
                    imbe_d.as_mut_ptr(),
                    &mut dec2.cur_mp,
                    &mut dec2.prev_mp,
                    &mut dec2.prev_mp_enhanced,
                    3,
                );
            }
            let max_s = pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
            eprintln!("  frame {i}: errs={errs} errs2={errs2} max={max_s}");
        }
    }

    /// Decode real captured IMBE frames from TG 300 (clear voice)
    /// and write a WAV file for offline listening.
    /// Run with: cargo test -- --ignored decode_captured
    #[test]
    #[ignore]
    fn decode_captured_frames_to_wav() {
        let hex_frames = [
            "ee54972c2201c44dfa51099dbdf3dd0809a7",
            "c03d1bc0e1008453cef445fbfecc46f692cb",
            "0eb3b6c8868795c5589e4bbac728b235157a",
            "9a9601d6b30bc993822c104af2c5686f495e",
            "0af5d3db801a2b9bffe9106d9ba2a6ca1f60",
            "aaf9f33c9b0aca37953944044436a7aa8bfc",
            "3f2935f65944c1c27d33348249c30884ef34",
            "f4c5e32e4a4ea70cb75164ff46980b443e47",
            "ea20a65ebd0aed61ad8db1b391b8a3a40fe7",
            "c9ef78a76a02d787ad0264fe94bd78a0af23",
            "3fbc77bf8bab6535bbb2629821fcf22a0260",
            "2b09b8294e6da347b9bea8e058c24afc94ee",
            "290a07148ac7f8d79f0fcde678b789a4b408",
            "590d53e371d98fb0f34460f6a9ddc562530f",
            "c6c1935a6145c095f9c129a0e349d56e11f0",
            "040a41803406c1c2b4fefe74edc6f67d13fb",
            "5c2113f867a789fbff4938da2219a6bb9bdc",
            "6e5b33e01062cec70994135ad4186df286ef",
            "2baf0d6c5419b231fabbfdbc1e47b5fa8308",
            "4fa9a2281a03f2ea985c2cd892e086ad236a",
            "395c09f24207aadb9a46e6844555385b781d",
            "398cc1202a07ab31805bd0112bfeb6bc8fa3",
            "4b6fb65b7d21ac9feb0ec05619c5c99aaae1",
            "7dce4c42606ac4850a2034f336b8bee2c68a",
            "093bb3d74cea3b36a4af350c85fb78c51ec4",
            "7d7c54dad6db4940034d6601b79a0ef5239f",
            "6d68557b873a3bc4318c182d5b384a39455f",
        ];

        let mut decoder = ImbeDecoder::new();
        let mut all_pcm: Vec<i16> = Vec::new();
        let mut total_errs = 0i32;
        let mut total_errs2 = 0i32;

        for (i, hex) in hex_frames.iter().enumerate() {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|j| u8::from_str_radix(&hex[j..j + 2], 16).unwrap())
                .collect();
            let mut bits = [0u8; 18];
            bits.copy_from_slice(&bytes[..18]);
            let frame = ImbeFrameRaw { bits };
            let (pcm, errs, errs2) = decoder.decode_frame(&frame);
            all_pcm.extend_from_slice(&pcm);
            total_errs += errs;
            total_errs2 += errs2;
            let max_s = pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
            let rms = (pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
                / pcm.len() as f64)
                .sqrt();
            eprintln!(
                "frame {i:2}: errs={errs} errs2={errs2} max={max_s:6} rms={rms:7.1}"
            );
        }

        let max_abs = all_pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
        let rms = (all_pcm
            .iter()
            .map(|&s| (s as f64).powi(2))
            .sum::<f64>()
            / all_pcm.len() as f64)
            .sqrt();
        eprintln!(
            "\nTotal: {} frames, errs={total_errs}, errs2={total_errs2}",
            hex_frames.len()
        );
        eprintln!(
            "PCM: {} samples ({:.1}s), max_abs={max_abs}, rms={rms:.1}",
            all_pcm.len(),
            all_pcm.len() as f64 / 8000.0
        );

        // Write WAV
        use std::io::Write;
        let wav_path = "imbe_decoded.wav";
        let mut f = std::fs::File::create(wav_path).unwrap();
        let data_size = (all_pcm.len() * 2) as u32;
        let file_size = 36 + data_size;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&file_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&8000u32.to_le_bytes()).unwrap();
        f.write_all(&16000u32.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_size.to_le_bytes()).unwrap();
        for &sample in &all_pcm {
            f.write_all(&sample.to_le_bytes()).unwrap();
        }
        eprintln!("Wrote {wav_path}");
    }

    /// Decode captured frames with JMBE and save WAV for comparison.
    #[test]
    #[ignore]
    fn decode_captured_jmbe_wav() {
        let hex_frames = [
            "ee54972c2201c44dfa51099dbdf3dd0809a7",
            "c03d1bc0e1008453cef445fbfecc46f692cb",
            "0eb3b6c8868795c5589e4bbac728b235157a",
            "9a9601d6b30bc993822c104af2c5686f495e",
            "0af5d3db801a2b9bffe9106d9ba2a6ca1f60",
            "aaf9f33c9b0aca37953944044436a7aa8bfc",
            "3f2935f65944c1c27d33348249c30884ef34",
            "f4c5e32e4a4ea70cb75164ff46980b443e47",
            "ea20a65ebd0aed61ad8db1b391b8a3a40fe7",
            "c9ef78a76a02d787ad0264fe94bd78a0af23",
            "3fbc77bf8bab6535bbb2629821fcf22a0260",
            "2b09b8294e6da347b9bea8e058c24afc94ee",
            "290a07148ac7f8d79f0fcde678b789a4b408",
            "590d53e371d98fb0f34460f6a9ddc562530f",
            "c6c1935a6145c095f9c129a0e349d56e11f0",
            "040a41803406c1c2b4fefe74edc6f67d13fb",
            "5c2113f867a789fbff4938da2219a6bb9bdc",
            "6e5b33e01062cec70994135ad4186df286ef",
            "2baf0d6c5419b231fabbfdbc1e47b5fa8308",
            "4fa9a2281a03f2ea985c2cd892e086ad236a",
            "395c09f24207aadb9a46e6844555385b781d",
            "398cc1202a07ab31805bd0112bfeb6bc8fa3",
            "4b6fb65b7d21ac9feb0ec05619c5c99aaae1",
            "7dce4c42606ac4850a2034f336b8bee2c68a",
            "093bb3d74cea3b36a4af350c85fb78c51ec4",
            "7d7c54dad6db4940034d6601b79a0ef5239f",
            "6d68557b873a3bc4318c182d5b384a39455f",
        ];

        let mut decoder = JmbeDecoder::new();
        let mut all_pcm: Vec<i16> = Vec::new();

        for (i, hex) in hex_frames.iter().enumerate() {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|j| u8::from_str_radix(&hex[j..j + 2], 16).unwrap())
                .collect();
            let mut bits = [0u8; 18];
            bits.copy_from_slice(&bytes[..18]);
            let frame = ImbeFrameRaw { bits };
            let pcm = decoder.decode_frame(&frame);
            let max_s = pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
            let rms = (pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
                / pcm.len() as f64).sqrt();
            eprintln!("JMBE frame {i:2}: max={max_s:6} rms={rms:7.1}");
            all_pcm.extend_from_slice(&pcm);
        }

        let max_abs = all_pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
        let rms = (all_pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
            / all_pcm.len() as f64).sqrt();
        eprintln!("\nJMBE Total: {} frames, max={max_abs}, rms={rms:.1}",
            hex_frames.len());

        // Write WAV
        use std::io::Write;
        let wav_path = "imbe_decoded_jmbe.wav";
        let mut f = std::fs::File::create(wav_path).unwrap();
        let data_size = (all_pcm.len() * 2) as u32;
        let file_size = 36 + data_size;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&file_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&8000u32.to_le_bytes()).unwrap();
        f.write_all(&16000u32.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_size.to_le_bytes()).unwrap();
        for &sample in &all_pcm {
            f.write_all(&sample.to_le_bytes()).unwrap();
        }
        eprintln!("Wrote {wav_path} ({:.1}s)", all_pcm.len() as f64 / 8000.0);
    }
}
