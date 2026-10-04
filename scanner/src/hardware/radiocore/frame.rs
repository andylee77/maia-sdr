//! The spectrometer's frames (maia-sdr doc/changes/082, "The spectrum frame"): 4096 bins of 64
//! bits, bin 0 at -fs/2, each a 47-bit mantissa in [46:0] and an exponent in [58:56] (powers of
//! 4). The core puts a header in bins 0-7 and a check word in bin 4095, which lie outside the
//! ±0.45 × rate every user of the spectrum reads; `blank` gives them their neighbours' values
//! for the display and the floor.

use serde::Serialize;

/// Bins per frame.
pub const BINS: usize = 4096;
/// Bytes per frame.
pub const FRAME_BYTES: usize = BINS * 8;
/// Bins that hold the header.
pub const HEADER_BINS: usize = 8;

const MAGIC: u16 = 0x5346;
const FORMAT: u8 = 1;

/// Why a frame was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Not a frame (never written, or another format).
    Magic(u16),
    Format(u8),
    /// The XOR of the frame's 32-bit words is not zero: part of it is stale.
    Check(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct FrameHeader {
    /// Peak hold; else average power.
    pub peak: bool,
    /// An abort ended the frame before its integrations were done.
    pub aborted: bool,
    /// Transforms integrated.
    pub transforms: u16,
    /// The AD9361 sample index (since the core's reset) of the first transform's first sample.
    pub sample_index: u64,
    /// Frames since the core's reset (wraps).
    pub sequence: u32,
    /// The running count of AD9361 samples at full scale at that first sample.
    pub adc_clips: u32,
}

fn word(bytes: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(bytes[8 * i..8 * i + 8].try_into().unwrap())
}

/// Check a frame and read its header.
pub fn parse(bytes: &[u8]) -> Result<FrameHeader, Fault> {
    assert_eq!(bytes.len(), FRAME_BYTES);
    let w0 = word(bytes, 0);
    let magic = w0 as u16;
    if magic != MAGIC {
        return Err(Fault::Magic(magic));
    }
    let format = (w0 >> 16) as u8 & 0xF;
    if format != FORMAT {
        return Err(Fault::Format(format));
    }
    let check = bytes.chunks_exact(4).fold(0u32, |x, b| x ^ u32::from_le_bytes(b.try_into().unwrap()));
    if check != 0 {
        return Err(Fault::Check(check));
    }
    let flags = (w0 >> 24) as u8;
    let w2 = word(bytes, 2);
    Ok(FrameHeader {
        peak: flags & 1 != 0,
        aborted: flags & 2 != 0,
        transforms: (w0 >> 32) as u16 & 0x3FF,
        sample_index: word(bytes, 1),
        sequence: w2 as u32,
        adc_clips: (w2 >> 32) as u32,
    })
}

/// Give the header's bins their neighbours' values: bins 0-7 bin 8's, bin 4095 bin 4094's.
pub fn blank(bytes: &mut [u8]) {
    assert_eq!(bytes.len(), FRAME_BYTES);
    let first = HEADER_BINS * 8;
    let edge: [u8; 8] = bytes[first..first + 8].try_into().unwrap();
    for bin in bytes[..first].chunks_exact_mut(8) {
        bin.copy_from_slice(&edge);
    }
    let last = FRAME_BYTES - 8;
    bytes.copy_within(last - 8..last, last);
}

/// A frame as the core writes it (tests): `bins` with the header in bins 0-7 and the check in
/// bin 4095.
#[cfg(test)]
pub fn build(h: &FrameHeader, bins: &[u64]) -> Vec<u8> {
    assert_eq!(bins.len(), BINS);
    let flags = h.peak as u64 | (h.aborted as u64) << 1;
    let mut words = bins.to_vec();
    words[..HEADER_BINS].fill(0);
    words[0] = MAGIC as u64 | (FORMAT as u64) << 16 | flags << 24 | (h.transforms as u64) << 32;
    words[1] = h.sample_index;
    words[2] = h.sequence as u64 | (h.adc_clips as u64) << 32;
    let check = words[..BINS - 1].iter().fold(0u32, |x, w| x ^ *w as u32 ^ (*w >> 32) as u32);
    words[BINS - 1] = check as u64;
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> FrameHeader {
        FrameHeader { peak: true, aborted: false, transforms: 256, sample_index: 0x1_2345_6789, sequence: 77, adc_clips: 9 }
    }

    #[test]
    fn a_frame_reads_back_its_header() {
        let bins: Vec<u64> = (0..BINS as u64).map(|i| i * 1000 | 1 << 56).collect();
        let bytes = build(&header(), &bins);
        assert_eq!(parse(&bytes), Ok(header()));
    }

    #[test]
    fn a_stale_word_fails_the_check() {
        let bins = vec![5u64; BINS];
        let mut bytes = build(&header(), &bins);
        bytes[8 * 2000] ^= 0x10;
        assert_eq!(parse(&bytes), Err(Fault::Check(0x10)));
        let mut empty = vec![0u8; FRAME_BYTES];
        assert_eq!(parse(&empty), Err(Fault::Magic(0)));
        empty[2] = 0x2F;
        assert!(parse(&empty).is_err());
    }

    #[test]
    fn blanking_copies_the_neighbouring_bins() {
        let bins: Vec<u64> = (0..BINS as u64).collect();
        let mut bytes = build(&header(), &bins);
        blank(&mut bytes);
        let w = |i: usize| word(&bytes, i);
        assert!((0..HEADER_BINS).all(|i| w(i) == 8));
        assert_eq!(w(BINS - 1), (BINS - 2) as u64);
        assert_eq!(w(100), 100);
    }
}
