//! Phase 7E: Audio distribution layer.
//!
//! The vocoder task produces 160 PCM samples (20 ms @ 8 kHz, 16-bit
//! signed) per IMBE frame. This module provides a broadcast channel
//! so multiple consumers (HTTP streaming, WebSocket, future WAV
//! recorder) can independently read the audio stream.

pub mod rec_storage;
pub mod recorder;

use tokio::sync::broadcast;

use crate::hardware::traffic_lane::Lane;

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
    /// Phase 2h (2026-04-25): the GrantFollower call_id active when
    /// the IMBE batch was submitted to the vocoder. Captured in
    /// `ImbeForwarder::forward_frames` (which reads
    /// `current_call_id`) and propagated through the vocoder thread
    /// onto every emitted chunk. The recorder routes by this
    /// directly — no more tg+source heuristic, no cross-call bleed
    /// from in-flight PCM. `0` = "no active call known at submit
    /// time" (e.g. follower idle, retune race); recorder still
    /// appends those to the active call defensively.
    pub call_id: u64,
    /// 2026-04-26 session-lifecycle refactor: wall-clock unix ms
    /// stamped at LDU dispatch in the framer (i.e. when the chain
    /// SAW these dibits, NOT when the vocoder produced PCM). Lets
    /// the recorder route by capture time vs the active session's
    /// `[open_at_ms, close_at_ms]` window — late chunks for a
    /// just-closed session still land in the right WAV regardless
    /// of vocoder/queue lag. All 9 IMBE frames in one batch share
    /// the same captured_at_ms (they came from one LDU dispatch).
    ///
    /// Change 054: in airtime mode this is the estimated production
    /// (air) time of the dibit that completed the LDU, not the dispatch
    /// time.
    pub captured_at_ms: u64,
    /// Change 054: `call_id` / `talkgroup` / `captured_at_ms` come from
    /// the air-time epoch of the dibits (traffic reader in airtime
    /// mode). The recorder then routes by `call_id` instead of the
    /// capture-time window, and the lifecycle only counts the chunk as
    /// activity of the call it belongs to.
    pub airtime: bool,
    /// Change 066: traffic chain that decoded the audio.
    pub lane: Lane,
}

pub type AudioTx = broadcast::Sender<AudioChunk>;

/// Create the audio broadcast channel.
/// Capacity 256 = ~5.1 seconds of audio, ~84 KB RAM.
pub fn audio_channel() -> AudioTx {
    audio_channel_for(1)
}

/// Change 066: 256 chunks per traffic chain (~5.1 s of each).
pub fn audio_channel_for(chains: usize) -> AudioTx {
    let (tx, _rx) = broadcast::channel(256 * chains.max(1));
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
    /// 2026-04-19 count-based close. Snapshot of
    /// `ImbeForwarder::frames_submitted` at the moment this boundary
    /// was dispatched. The recorder waits until the vocoder has
    /// *consumed* this many frames before actually calling
    /// `finalize()` — that way the tail PCM chunks corresponding to
    /// the LDUs submitted just before the boundary get appended to
    /// the closing recording instead of spawning a new 60-ms-and-
    /// discarded fragment. A 200 ms timer still guards the case
    /// where consumption stalls (e.g. encrypted skip streak).
    pub expected_submit_count: u64,
    /// Change 066: traffic chain the event belongs to. Voice events
    /// (HDU, NIDs, link control, end markers) carry their chain; a
    /// followed grant carries the chain chosen for it; not-followed
    /// grants and grant updates carry none.
    pub lane: Option<Lane>,
}

/// 2026-04-26 session-lifecycle refactor: the three terminator
/// flavours that can fire `CallBoundaryKind::SpeakerEnd`. Operator-
/// confirmed semantics (Clay County observation):
/// - `MotTalkComplete`: TDULC with Motorola MFID 0x90 TalkComplete
///   LCW. Carries a 7-digit field-radio SRC. Dispatcher consoles do
///   NOT emit this — only field radios. Reliable terminator.
/// - `CallTermination`: TDULC with standard CallTermination LCW
///   (opcode 0x0F MFID 0x00). The "BY:" field is a system-controller
///   address (FFFFFD/E/F), not a real radio ID. Reliable terminator.
/// - `BareTdu`: DUID=0x3 with no LCW payload. Heavily false-positive
///   on degenerate dibit streams (BCH false-pass on idle/noise);
///   the lifecycle layer treats this as informational only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminatorKind {
    BareTdu,
    MotTalkComplete,
    CallTermination,
}

#[derive(Debug, Clone, Copy)]
pub enum CallBoundaryKind {
    /// DUID 0x0 — header arrived. Logged as an informational
    /// timeline marker only. 2026-04-22 fragmentation fix: HDU no
    /// longer closes the active recording — it fires at DUID=0
    /// before the LC is decoded, so the source is unknown, and
    /// using it as a split trigger fragmented same-speaker PTT
    /// re-keys. The new-speaker split now happens in the
    /// audio_chunk arm of the recorder when `chunk.source`
    /// changes to a different non-zero ID on the same TG.
    ///
    /// 2026-04-24 CC-grant-centric refactor: HDU is now ENRICHMENT
    /// ONLY. It stamps `first_audio_at_unix_ms` on an existing
    /// `OpenGrant` (created by `CcGrantArrival`) but never opens a
    /// new one on its own. An HDU with no matching OpenGrant is a
    /// silent no-op — heartbeat false-positives from BCH t=11 noise
    /// during PLL/AGC settle can't pollute the calls ring.
    HduStart,
    /// Mid-call source stamp. Fires from `imbe_forwarder.on_ldu1`
    /// when the rolling 3-of-4 LDU1 LC FM: vote agrees on a new
    /// plausibility-passed RID (see `LDU1_FM_VOTE_M` /
    /// `LDU1_FM_VOTE_N`). Stamps `ActiveCall.source` but does NOT
    /// finalise — speaker is still talking.
    ///
    /// 2026-04-24: wired live. The N-of-M voting + plausibility gate
    /// in the emitter means every `TdulcComplete` represents a stable
    /// FEC-corroborated source observation, not a single-packet
    /// decode. `grant_stats` uses fill-in-only semantics — never
    /// overrides a CC-derived OpenGrant.source.
    TdulcComplete { source: Option<u32> },
    /// End-of-speaker / end-of-call LCW. Fires on Motorola
    /// `TALK_COMPLETE` (opcode 0x0F MFID 0x90), standard
    /// `CALL_TERMINATION` (opcode 0x0F MFID 0x00), and bare TDU.
    /// 2026-04-26 session-lifecycle refactor: `kind` distinguishes
    /// the source (operator-confirmed: dispatcher consoles never
    /// emit Motorola TALK_COMPLETE; only field radios do, so kind
    /// = MotTalkComplete carries the field-radio RID). The
    /// lifecycle layer closes the session on `MotTalkComplete` or
    /// `CallTermination`; `BareTdu` is informational only (bare
    /// TDUs are heavily false-positive on degenerate dibit
    /// streams; the 5 s no-audio timeout is the backstop).
    SpeakerEnd {
        source: Option<u32>,
        kind: TerminatorKind,
    },
    /// 2026-04-24 CC-grant-centric refactor: every control-channel
    /// `GRP_VCH_GRANT` (opcode 0x00) and `GVCG_UPDT_EXP` (0x03) fires
    /// one of these. Carries everything the CC announced about the
    /// call. `not_followed = Some(reason)` means the follower
    /// declined to retune (monitor-list miss, encrypted, sticky-lock,
    /// diagnostic traffic-lock) — the call still gets a `grant_stats`
    /// entry so dashboards show ALL P25 activity even when we only
    /// decode a subset. `not_followed = None` means the call was
    /// accepted for normal follow-and-decode.
    ///
    /// `channel` is the P25 channel identifier (low 12 bits of the
    /// ChannelId tuple); pair with NetworkStatus.band to reconstruct
    /// frequency if `freq_hz` is absent. Copy-able: downstream
    /// formatters render `{band}-{ch}` if needed.
    CcGrantArrival {
        tg: u16,
        source: Option<u32>,
        freq_hz: Option<u64>,
        channel: u16,
        encrypted: bool,
        not_followed: Option<&'static str>,
    },
    /// 2026-04-24 CC-grant-centric refactor: plain
    /// `GRP_VCH_GRNT_UPD` (opcode 0x02) keep-alive. Carries neither
    /// SRC nor service_options; only refreshes the ttl on an
    /// existing `OpenGrant` for the same TG. If no OpenGrant is
    /// open this event is a silent no-op (came-up-mid-call edge case
    /// — the design doc accepts that we'd miss a call started before
    /// we booted).
    CcGrantUpdate {
        tg: u16,
        freq_hz: Option<u64>,
        channel: u16,
    },
    /// 2026-05-03 loss-of-sync detector: traffic-LSM heartbeat saw an
    /// `nid_event` strobe (any DUID, any nid_valid). The lifecycle
    /// uses this purely as a framer-level liveness signal — it stamps
    /// `ActiveCall.last_nid_at_ms` so the periodic tick can close
    /// the call with `CloseReason::SyncLost` when no NIDs have fired
    /// for `LOS_TIMEOUT_MS`. Mirrors SDRTrunk's per-channel
    /// loss-of-sync flag without adding a new HDL counter.
    ///
    /// The timestamp is the receipt time at the lifecycle task.
    /// NAC/TG are intentionally omitted — LoS is a pure framer-state
    /// metric, independent of which TG / NAC the chain is decoding.
    ///
    /// Change 057: `voice` = a valid LDU1 / LDU2 NID. The heartbeat
    /// reads the HDL in real time (16 ms poll), ahead of the PS
    /// decode, so a voice NID seen after an end-of-transmission marker
    /// means the channel is carrying voice again; the lifecycle then
    /// cancels the pending end close.
    TrafficNidObserved { voice: bool },
    /// Change 057: end of a voice transmission of `call_id` — the first
    /// LC-FEC-valid TDULC decoded after voice of that call (Motorola
    /// TALK COMPLETE, CALL TERMINATION, or the GROUP VOICE CHANNEL USER
    /// the system repeats through its channel hang). Emitted by the
    /// forwarder once per transmission (`CallCounts::note_end_marker`),
    /// never for a TDULC before the call's first LDU. `call_id` and
    /// `air_ms` come from the air-time segment in airtime mode, so a
    /// previous call's late terminator never ends the current call.
    /// SDRTrunk ends its call event at the same frame.
    VoiceEnd {
        call_id: u64,
        /// Air time (unix ms) of the terminator (airtime mode), else
        /// its decode time.
        air_ms: u64,
        /// "talk_complete" | "call_termination" | "network_teardown" |
        /// "channel_user" | "link_control".
        lc: &'static str,
    },
}

pub type CallBoundaryTx = broadcast::Sender<CallBoundary>;

/// Create the call-boundary broadcast channel. Change 071a: every grant
/// TSBK and every traffic voice NID goes through it (a few hundred per
/// second at most), so it holds several seconds of them.
pub const CALL_BOUNDARY_CAPACITY: usize = 4096;

pub fn call_boundary_channel() -> CallBoundaryTx {
    let (tx, _rx) = broadcast::channel(CALL_BOUNDARY_CAPACITY);
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
