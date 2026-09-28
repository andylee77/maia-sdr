//! Phase 7B: Typed P25 events pushed by the control channel decoder.
//!
//! Replaces the 50 ms polling loop in the grant follower with an
//! event-driven mpsc channel. The decoder pushes `P25Event::Grant`
//! on every `GroupVoiceChannelGrant` or `GroupVoiceChannelGrantUpdate`
//! TSBK, and the follower task receives them via `select!` alongside
//! a timeout tick for call-end detection.

use super::types::*;

/// Typed event from the control channel decoder.
#[derive(Debug, Clone)]
pub enum P25Event {
    /// A voice channel grant was observed (new or refreshed).
    Grant(GrantEvent),
}

/// Voice channel grant details, mirroring the fields from
/// `ChannelGrant` / `GrantInfo` that the follower needs.
#[derive(Debug, Clone)]
pub struct GrantEvent {
    pub channel: Channel,
    pub talkgroup: Talkgroup,
    pub source: Option<RadioId>,
    pub frequency_hz: Option<u64>,
    pub encrypted: bool,
    pub emergency: bool,
    /// 2026-04-24 CC-grant-centric refactor: distinguishes
    /// `GroupVoiceChannelGrantUpdate` (0x02, keep-alive refresh)
    /// from the full `GroupVoiceChannelGrant` (0x00) and
    /// `GroupVoiceChannelGrantUpdateExplicit` (0x03) TSBKs. Set by
    /// the control-channel decoder when emitting the event. The
    /// follower uses this to dispatch `CallBoundaryKind::CcGrantUpdate`
    /// (refresh only) vs `CcGrantArrival` (can open a new OpenGrant).
    pub is_update: bool,
    /// Change 071a: the channel is on a TDMA (Phase 2) band; this
    /// receiver decodes Phase 1 only.
    pub tdma: bool,
}
