//! IQ ring buffer adapter for the LSM pipeline.
//!
//! Phase 6D plumbing on top of the Phase 6C `iq_dma` ring exposed by
//! `crate::fpga::IpCore`. The FPGA writes 64-bit DMA words in the layout:
//!
//! ```text
//! bit 63                                                              bit 0
//! +-----------------+-----------------+-----------------+-----------------+
//! |    im[1] s16    |    re[1] s16    |    im[0] s16    |    re[0] s16    |
//! +-----------------+-----------------+-----------------+-----------------+
//!        63..48            47..32            31..16            15..0
//!               sample 1                          sample 0
//! ```
//!
//! Equivalently: each 32 KB sub-buffer is 8192 little-endian `i16` pairs in
//! `(re, im, re, im, ...)` order — the natural byte order of an
//! interleaved-IQ buffer. We convert to `Complex32` with the standard
//! `i16 → f32 / 32768` normalisation so the dynamic range matches the
//! Python reference's `load_wav_iq`, which scales 16-bit PCM to ±1.0.

use super::Complex32;

/// Sample rate of the post-DDC IQ stream coming out of the iq_dma ring.
/// Defined by the FPGA DDC chain (`fpga.rs::P25_DEC1*P25_DEC2*P25_DEC3 =
/// 128x` from 8 MSPS).
pub const IQ_DMA_SAMPLE_RATE_HZ: f32 = 62_500.0;

/// Convert a single iq_dma sub-buffer (`&[u8]`) to a `Vec<Complex32>`.
///
/// The input slice must be a multiple of 4 bytes (one I + one Q sample per
/// pair). Any trailing bytes are silently dropped — they cannot occur in
/// practice because the kernel-allocated sub-buffer is 32 KB = exact
/// multiple of 4.
pub fn sub_buffer_to_complex(buf: &[u8]) -> Vec<Complex32> {
    let n_samples = buf.len() / 4; // 4 bytes = 1 complex sample
    let mut out = Vec::with_capacity(n_samples);
    for k in 0..n_samples {
        // Little-endian i16 pair: bytes [4k..4k+2] = re, [4k+2..4k+4] = im
        let re = i16::from_le_bytes([buf[4 * k], buf[4 * k + 1]]) as f32;
        let im = i16::from_le_bytes([buf[4 * k + 2], buf[4 * k + 3]]) as f32;
        out.push(Complex32::new(re / 32768.0, im / 32768.0));
    }
    out
}

/// Concatenate several sub-buffers into a single contiguous Complex32
/// stream, applying `sub_buffer_to_complex` to each in turn. Used to glue
/// together the per-IRQ batch of newly-completed sub-buffers.
pub fn sub_buffers_to_complex(buffers: &[&[u8]]) -> Vec<Complex32> {
    let total: usize = buffers.iter().map(|b| b.len() / 4).sum();
    let mut out = Vec::with_capacity(total);
    for buf in buffers {
        out.extend(sub_buffer_to_complex(buf));
    }
    out
}

#[cfg(test)]
mod tests {
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
}
