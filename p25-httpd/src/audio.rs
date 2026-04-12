//! Phase 7E: Audio distribution layer.
//!
//! The vocoder task produces 160 PCM samples (20 ms @ 8 kHz, 16-bit
//! signed) per IMBE frame. This module provides a broadcast channel
//! so multiple consumers (HTTP streaming, WebSocket, future WAV
//! recorder) can independently read the audio stream.

use tokio::sync::broadcast;

/// One chunk of decoded PCM audio (20 ms, one IMBE frame).
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// 160 samples @ 8 kHz, 16-bit signed = 320 bytes = 20 ms.
    pub pcm: [i16; 160],
    /// Monotonic sequence number (for gap detection).
    pub seq: u64,
    /// Talkgroup that produced this audio.
    pub talkgroup: u16,
}

pub type AudioTx = broadcast::Sender<AudioChunk>;

/// Create the audio broadcast channel.
/// Capacity 256 = ~5.1 seconds of audio, ~84 KB RAM.
pub fn audio_channel() -> AudioTx {
    let (tx, _rx) = broadcast::channel(256);
    tx
}

/// WAV header for a streaming 8 kHz 16-bit mono PCM stream.
/// Uses 0xFFFFFFFF for the data chunk size (indeterminate length).
pub fn wav_header_8k_16bit_mono() -> [u8; 44] {
    let sample_rate: u32 = 8000;
    let bits_per_sample: u16 = 16;
    let channels: u16 = 1;
    let byte_rate: u32 = sample_rate * (bits_per_sample as u32 / 8) * channels as u32;
    let block_align: u16 = channels * (bits_per_sample / 8);

    let mut h = [0u8; 44];
    // RIFF header
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // file size (unknown)
    h[8..12].copy_from_slice(b"WAVE");
    // fmt sub-chunk
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // sub-chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM format
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
    // data sub-chunk
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // data size (unknown)
    h
}
