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
    /// Talkgroup that produced this audio.
    pub talkgroup: u16,
    /// 2026-04-19: source radio ID (FM:<n>) known at the time the
    /// vocoder produced this chunk, resolved from the PRIMARY control-
    /// channel `GRP_VCH_GRANT.FM` → fallback traffic-channel LDU1 LC
    /// FM → fallback Motorola TDULC BY: order. `0` = unknown (e.g.
    /// only a GRP_VCH_GRNT_UPD has arrived and no LC/TDULC has been
    /// decoded yet). Lets the recorder stamp the source into the WAV
    /// filename from the very first audio chunk of the call without
    /// waiting for a boundary event to race in.
    pub source: u32,
}

pub type AudioTx = broadcast::Sender<AudioChunk>;

/// Create the audio broadcast channel.
/// Capacity 256 = ~5.1 seconds of audio, ~84 KB RAM.
pub fn audio_channel() -> AudioTx {
    let (tx, _rx) = broadcast::channel(256);
    tx
}

/// 2026-04-19: per-call boundary event for the recorder.
///
/// The PCM audio path only has (`pcm`, `talkgroup`) — it can't tell
/// when a *new speaker* starts talking on the *same* TG (dispatcher ↔
/// unit back-and-forth). P25 marks each PTT-down with a fresh HDU, and
/// end-of-speaker with a TDULC. By broadcasting HDU / TDULC events on
/// a second channel the recorder can split per-speaker WAVs without
/// having to sniff the vocoder state machine.
///
/// See also [`reference_sdrtrunk_call_model_and_opcode_coverage.md`]
/// memory — this mirrors SDRTrunk's per-call event model where every
/// HDU starts a new `.mp3` file under `TO_<TG>_FROM_<source>`.
#[derive(Debug, Clone)]
pub struct CallBoundary {
    pub kind: CallBoundaryKind,
    pub nac: u16,
    /// Talkgroup the traffic manager was locked on when the boundary
    /// event fired. Useful so the recorder doesn't have to rejoin the
    /// manager lock to resolve context.
    pub talkgroup: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
pub enum CallBoundaryKind {
    /// DUID 0x0 — header arrived. A fresh speaker is keying up: the
    /// recorder should close out the previous `ActiveCall` (if any)
    /// and start a new one for the next PCM chunks.
    HduStart,
    /// DUID 0xF — TDULC arrived. `source` is the Motorola vendor
    /// `TALK_COMPLETE` BY: field when the LCW parser was able to
    /// recover it, otherwise `None`. The recorder stamps
    /// `ActiveCall.source` when Some, but does NOT finalize on this
    /// event — finalize still waits for either an HDU or the grace
    /// window, matching SDRTrunk's "close on new speaker or on
    /// sync-loss" model.
    TdulcComplete { source: Option<u32> },
}

pub type CallBoundaryTx = broadcast::Sender<CallBoundary>;

/// Create the call-boundary broadcast channel. Capacity 64 — boundary
/// events fire at most ~1/call, not per-frame, so a small ring is
/// plenty and lag just means the recorder missed a PTT split (we'd
/// fall back to the audio grace-window finaliser anyway).
pub fn call_boundary_channel() -> CallBoundaryTx {
    let (tx, _rx) = broadcast::channel(64);
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
