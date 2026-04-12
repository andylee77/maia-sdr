//! Raw FFI bindings to mbelib (IMBE/AMBE vocoder library).
//!
//! This crate vendors the mbelib C source and compiles it via `cc`.
//! Only the functions needed for P25 Phase 1 IMBE (7200x4400) are
//! exposed, plus the init/parms management functions.

#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_float, c_short};

/// mbelib voice codec parameters (persists across frames).
#[repr(C)]
pub struct mbe_parms {
    pub w0: c_float,
    pub l: c_int,        // L
    pub k: c_int,        // K
    pub vl: [c_int; 57],
    pub ml: [c_float; 57],
    pub log2ml: [c_float; 57],
    pub phil: [c_float; 57],
    pub psil: [c_float; 57],
    pub gamma: c_float,
    pub un: c_int,
    pub repeat: c_int,
}

extern "C" {
    /// Initialize three mbe_parms structs (current, previous,
    /// previous-enhanced) to their default state. Must be called
    /// before the first frame decode.
    pub fn mbe_initMbeParms(
        cur_mp: *mut mbe_parms,
        prev_mp: *mut mbe_parms,
        prev_mp_enhanced: *mut mbe_parms,
    );

    /// Decode one IMBE 7200x4400 frame (P25 Phase 1).
    ///
    /// - `aout_buf`: output buffer for 160 PCM samples (16-bit signed)
    /// - `errs`: output — number of uncorrectable bit errors
    /// - `errs2`: output — number of correctable bit errors
    /// - `err_str`: output — 64-byte error description string
    /// - `imbe_fr`: input — raw 144 bits as `[8][23]` char array
    ///   (each char is 0 or 1). Layout: rows 0-3 are Golay(23,12),
    ///   rows 4-6 are Hamming(15,11) in [0..14], row 7 is 7 uncoded
    ///   bits in [0..6].
    /// - `imbe_d`: scratch — 88-byte buffer for decoded data bits
    /// - `cur_mp`, `prev_mp`, `prev_mp_enhanced`: codec state
    /// - `uvquality`: unvoiced quality (3 = default)
    pub fn mbe_processImbe7200x4400Frame(
        aout_buf: *mut c_short,
        errs: *mut c_int,
        errs2: *mut c_int,
        err_str: *mut c_char,
        imbe_fr: *mut [c_char; 23],  // [8][23]
        imbe_d: *mut c_char,         // [88]
        cur_mp: *mut mbe_parms,
        prev_mp: *mut mbe_parms,
        prev_mp_enhanced: *mut mbe_parms,
        uvquality: c_int,
    );

    /// Synthesize 160 samples of silence into `aout_buf`.
    pub fn mbe_synthesizeSilence(aout_buf: *mut c_short);
}

/// Number of PCM samples produced per IMBE frame decode.
pub const SAMPLES_PER_FRAME: usize = 160;

/// PCM sample rate (Hz).
pub const SAMPLE_RATE: u32 = 8000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_and_silence() {
        unsafe {
            let mut cur = std::mem::zeroed::<mbe_parms>();
            let mut prev = std::mem::zeroed::<mbe_parms>();
            let mut prev_enh = std::mem::zeroed::<mbe_parms>();
            mbe_initMbeParms(&mut cur, &mut prev, &mut prev_enh);

            let mut buf = [0i16; SAMPLES_PER_FRAME];
            mbe_synthesizeSilence(buf.as_mut_ptr());
            // Silence should be all zeros
            assert!(buf.iter().all(|&s| s == 0));
        }
    }
}
