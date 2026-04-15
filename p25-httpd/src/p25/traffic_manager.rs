//! Traffic Channel Manager
//!
//! When a voice channel grant is detected on the control channel:
//! 1. Map logical channel number to RF frequency (via IDEN_UP table)
//! 2. Compute DDC NCO offset for the traffic channel frequency
//! 3. Command the traffic DDC to retune (write NCO frequency register)
//! 4. Start traffic DMA and monitor dibit stream for voice frames
//! 5. Manage call lifecycle (grant -> active -> teardown)
//!
//! Grant following latency budget (from DEVPLAN):
//!   TSBK received: ~20ms
//!   PS processes grant: ~1ms
//!   DDC retune (register write): ~1µs
//!   FIR flush + sync acquisition: ~40ms
//!   Total: ~60ms (P25 allows ~200ms)
//!
//! Phase 7A.1 (2026-04-11): wired into main.rs as a singleton driven by
//! a 50 ms polling task that snapshots the canonical `lsm_decoder.grants`
//! HashMap and forwards the newest entry. Polling rather than typed
//! events because the existing broadcast channel is `Sender<String>` --
//! see doc/changes/033 for the rationale and the upgrade path to typed
//! events in Phase 7B.

use std::time::Instant;

use super::types::*;

/// Traffic channel state
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrafficState {
    /// No active call, waiting for grant
    Idle,
    /// DDC retuned, acquiring sync on traffic channel
    Acquiring {
        channel: Channel,
        talkgroup: Talkgroup,
        frequency_hz: u64,
        started: Instant,
    },
    /// Locked on traffic channel, receiving voice frames
    Active {
        channel: Channel,
        talkgroup: Talkgroup,
        frequency_hz: u64,
        started: Instant,
    },
}

/// Traffic channel manager
pub struct TrafficManager {
    /// Current state
    pub state: TrafficState,
    /// RX LO frequency (center of AD9361 capture band)
    rx_lo_hz: u64,
    /// ADC sample rate
    sample_rate_hz: u64,
    /// NCO word for the traffic DDC (28-bit, computed from frequency offset)
    pub nco_word: u32,
    /// Last NCO offset (Hz, signed) -- diagnostic surface for /api/traffic.
    pub last_offset_hz: i64,
    /// Timeout for sync acquisition (ms)
    acquire_timeout_ms: u64,
    /// Timeout for call inactivity before returning to idle (ms)
    call_timeout_ms: u64,
    /// Last dibit activity timestamp
    last_activity: Instant,
    /// Total handle_grant() calls (Phase 7A.1: counts grant snapshots
    /// the polling task forwarded; some are duplicates that don't
    /// trigger a retune).
    pub grants_seen: u64,
    /// Total retunes triggered (handle_grant calls that returned true).
    pub retunes: u64,
    /// Wall-clock instant of the most recent retune.
    pub last_retune_at: Option<Instant>,
    /// Grants the follower refused to lock onto because the call was
    /// flagged encrypted (either via `service_options.encrypted` on the
    /// TSBK or via the persistent encrypted-TG history).  Counts only
    /// the "Idle -> would-be-new-lock" rejects; refreshes for a TG the
    /// follower is already tracking fall through unchanged so the
    /// locked call doesn't get interrupted.  Added 2026-04-14 after the
    /// follower kept sticky-locking on TG 402 (encrypted) and starving
    /// the non-encrypted grants on the same site.
    pub grants_rejected_encrypted: u64,

    // ── Phase 7A.2: NID/DUID dispatch + post-TDU hold window ──────────
    //
    // The traffic-side LSM HDL chain produces a stream of NID events
    // via the new traffic_lsm register bank (bank 6 at 0xC0). PS-side
    // dispatcher classifies each NID by DUID and feeds it to one of
    // these methods:
    //
    //   - `hdu_received(now)`  -- DUID 0x0 = HDU = call start
    //   - `tdu_received(now)`  -- DUID 0x3 / 0xF = TDU / TDU_LC = end
    //   - `note_activity()`    -- DUID 0x5 / 0xA = LDU1 / LDU2 = voice
    //
    // The post-TDU hold window matches SDRTrunk upstream PR #2010
    // semantics: a TDU does NOT immediately deallocate the slot. The
    // slot stays bound to the same TG for `post_tdu_hold_ms` after the
    // TDU so that back-to-back transmissions from another speaker on
    // the same TG (PTT release between speakers in a conversation)
    // reuse the same slot instead of looking like two separate calls.
    // SDRTrunk's `STALE_EVENT_THRESHOLD_MS = 2000` is the same value
    // we use for `call_timeout_ms` -- the post-TDU hold runs in
    // parallel and serves a different purpose: the timeout is
    // "release if NOTHING is happening", the hold is "stay bound even
    // if dibits stop, in case TG resumes within the window".
    //
    // The two semantics differ subtly:
    //
    //   - With NO TDU events arriving (Phase 7A.1 fallback):
    //     `call_timeout_ms` does the work and we release on 2 s of
    //     no activity.
    //
    //   - With TDU events arriving (Phase 7A.2 onward):
    //     A TDU is a strong "this transmission ended" signal. We
    //     start the post-TDU hold immediately. If the TG resumes
    //     within the hold, we cancel it and stay bound. Otherwise
    //     the hold expires and we release.
    //
    /// 2 s post-TDU hold window matching SDRTrunk PR #2010.
    post_tdu_hold_ms: u64,
    /// When `Some(t)`, the call is in the post-TDU hold window and
    /// will release at instant `t` if no further activity arrives.
    /// Cleared by `note_activity()` and `hdu_received()`.
    pub post_tdu_hold_until: Option<Instant>,
    /// Most recent DUID observed on the traffic LSM chain
    /// (Phase 7A.2 dispatch surface).
    pub last_duid: Option<u8>,
    /// Most recent NAC observed on the traffic LSM chain.
    pub last_nac: Option<u16>,
    /// Cumulative HDU count.
    pub hdus_seen: u64,
    /// Cumulative TDU count (TDU + TDU_LC combined).
    pub tdus_seen: u64,
    /// Cumulative LDU count (LDU1 + LDU2 combined).
    pub ldus_seen: u64,
}

impl TrafficManager {
    pub fn new(rx_lo_hz: u64, sample_rate_hz: u64) -> Self {
        TrafficManager {
            state: TrafficState::Idle,
            rx_lo_hz,
            sample_rate_hz,
            nco_word: 0,
            last_offset_hz: 0,
            acquire_timeout_ms: 200,
            // Phase 7A.1 sticky-lock: 2000 ms matches SDRTrunk
            // upstream PR #2010 / commit 1b3ce431's
            // STALE_EVENT_THRESHOLD_MS = 2000 in
            // P25TrafficChannelEventTracker.java. Years of P25
            // monitoring on the SDRTrunk codebase have settled on
            // 2 s as the right "this call is really over" gap; any
            // longer and we hold a singleton DDC slot through real
            // call ends, any shorter and we drop calls during PTT
            // releases between speakers in the same conversation.
            // Phase 7C will replace this with a TDU-based release
            // (with a 2 s post-TDU hold window, also from PR #2010)
            // once we have LDU/TDU sync detection.
            call_timeout_ms: 2000,
            last_activity: Instant::now(),
            grants_seen: 0,
            retunes: 0,
            last_retune_at: None,
            grants_rejected_encrypted: 0,
            // Phase 7A.2: post-TDU hold window. 2000 ms matches
            // SDRTrunk's STALE_EVENT_THRESHOLD_MS / the post-TDU
            // hold timer in P25TrafficChannelManager.processP1TrafficCallEnd().
            post_tdu_hold_ms: 2000,
            post_tdu_hold_until: None,
            last_duid: None,
            last_nac: None,
            hdus_seen: 0,
            tdus_seen: 0,
            ldus_seen: 0,
        }
    }

    /// Single-character state label for /api/traffic JSON.
    pub fn state_label(&self) -> &'static str {
        match self.state {
            TrafficState::Idle => "Idle",
            TrafficState::Acquiring { .. } => "Acquiring",
            TrafficState::Active { .. } => "Active",
        }
    }

    /// Channel currently being followed (if any).
    pub fn current_channel(&self) -> Option<Channel> {
        match &self.state {
            TrafficState::Acquiring { channel, .. }
            | TrafficState::Active { channel, .. } => Some(*channel),
            TrafficState::Idle => None,
        }
    }

    /// Handle a voice grant from the control channel.
    /// Returns true if the traffic DDC should be retuned.
    ///
    /// **Sticky-lock policy (Phase 7A.1, derived from SDRTrunk
    /// upstream PR #2010 / commit 1b3ce431):**
    ///
    /// Call identity is determined by **talkgroup ID only** (the
    /// "TO" identifier in P25 parlance), NOT by channel ID. This
    /// matches `isSameCallCheckingToOnly()` in
    /// `P25TrafficChannelEventTracker.java` from upstream
    /// `1b3ce431`. The reason: P25 networks routinely reassign an
    /// active call from one channel to another mid-conversation
    /// (network rebalancing, channel-add via
    /// `GroupVoiceChannelGrantUpdate`). Channel-based matching
    /// would treat a reassignment as a different call and break
    /// audio continuity.
    ///
    /// Behaviour:
    ///
    /// - Same TG, same frequency -> refresh activity, no retune.
    /// - Same TG, different frequency -> retune to new frequency
    ///   (TG was reassigned by the network), keep call alive.
    /// - Different TG (any frequency) -> caller is responsible for
    ///   filtering this out via the polling task's sticky-lock
    ///   policy (only call handle_grant on a different TG when
    ///   state is Idle). If the caller violates that contract,
    ///   handle_grant will accept the new TG and start a new
    ///   call -- the manager itself does not enforce stickiness.
    ///
    /// The Idle->next-grant transition is gated entirely by the
    /// `call_timeout_ms` inactivity timer (2000 ms, matching
    /// SDRTrunk's STALE_EVENT_THRESHOLD_MS). Once Idle, any new
    /// grant is accepted via the same code path.
    pub fn handle_grant(
        &mut self,
        channel: Channel,
        talkgroup: Talkgroup,
        frequency_hz: u64,
    ) -> bool {
        self.grants_seen += 1;

        // Same call (same TG)? Refresh activity. If the network
        // moved the TG to a new frequency, fall through to the
        // retune path so we follow it.
        //
        // Phase 7A.1 bug-fix (same commit, post-on-target observation):
        // also auto-promote Acquiring -> Active here. The original
        // design had `sync_acquired()` as the only way to promote out
        // of Acquiring, but Phase 7A.1 has no sync detector (Phase 7C
        // will add LDU sync extraction). Without auto-promotion the
        // state stayed in Acquiring forever, and `check_timeouts`'s
        // Acquiring branch uses the 200 ms `acquire_timeout_ms`
        // against `started`, not `last_activity` -- so the call
        // unconditionally timed out 200 ms after the retune and was
        // immediately re-acquired by the next poll, producing a
        // ~4 retunes/sec thrashing cycle even with sticky lock
        // working correctly. Promoting on the very next matching
        // poll (50 ms after the retune) puts us in the Active
        // branch's 2 s `call_timeout_ms` window, which is the right
        // semantics for the Phase 7A.1 "no real sync detection yet"
        // state.
        let same_tg_same_freq = match &self.state {
            TrafficState::Active {
                talkgroup: t,
                frequency_hz: f,
                ..
            } if t.0 == talkgroup.0 => {
                self.last_activity = Instant::now();
                *f == frequency_hz
            }
            TrafficState::Acquiring {
                channel: c,
                talkgroup: t,
                frequency_hz: f,
                started: s,
            } if t.0 == talkgroup.0 => {
                let same_freq = *f == frequency_hz;
                if same_freq {
                    // Auto-promote: we have at least one same-TG
                    // poll matching, treat the chain as locked.
                    let promoted = TrafficState::Active {
                        channel: *c,
                        talkgroup: *t,
                        frequency_hz: *f,
                        started: *s,
                    };
                    self.state = promoted;
                }
                self.last_activity = Instant::now();
                same_freq
            }
            _ => false,
        };

        // Same TG, same frequency -> nothing to do.
        if same_tg_same_freq {
            return false;
        }

        // Either a fresh call (different TG, or no current call)
        // OR same TG that has been reassigned to a new frequency.
        // Both paths require a retune.
        let offset_hz = frequency_hz as i64 - self.rx_lo_hz as i64;
        let nco_frac = offset_hz as f64 / self.sample_rate_hz as f64;
        // Convert to 28-bit unsigned (two's complement wrapping)
        self.nco_word = (nco_frac * (1u64 << 28) as f64) as i32 as u32 & 0x0FFF_FFFF;
        self.last_offset_hz = offset_hz;

        self.state = TrafficState::Acquiring {
            channel,
            talkgroup,
            frequency_hz,
            started: Instant::now(),
        };
        let now = Instant::now();
        self.last_activity = now;
        self.retunes += 1;
        self.last_retune_at = Some(now);

        true // DDC retune needed
    }

    /// Phase 7F.2 (2026-04-14): synchronous release path that bypasses
    /// the `call_timeout_ms` / post-TDU-hold timers. Used by the
    /// follower when a mid-call encryption detection forces us to
    /// drop the lock immediately -- waiting the full 2 s call-timeout
    /// lets encrypted LDUs keep flowing through the vocoder (where
    /// they get skipped as garbage, not audio).
    ///
    /// Behaviour: state -> Idle, post-TDU hold cleared. Counters are
    /// preserved. Caller is responsible for turning off demod_enable
    /// on the FPGA core.
    pub fn force_idle(&mut self) {
        self.state = TrafficState::Idle;
        self.post_tdu_hold_until = None;
    }

    /// Called when we detect frame sync on the traffic channel
    pub fn sync_acquired(&mut self) {
        if let TrafficState::Acquiring {
            channel,
            talkgroup,
            frequency_hz,
            started,
        } = self.state.clone()
        {
            self.state = TrafficState::Active {
                channel,
                talkgroup,
                frequency_hz,
                started,
            };
            self.last_activity = Instant::now();
        }
    }

    /// Called on each traffic channel dibit (or LDU NID event) to
    /// refresh activity timer. Also clears any in-flight post-TDU
    /// hold window -- if dibits are flowing again, the call is alive.
    pub fn note_activity(&mut self) {
        self.last_activity = Instant::now();
        self.post_tdu_hold_until = None;
    }

    // ── Phase 7A.2: NID/DUID dispatch methods ─────────────────────

    /// Phase 7A.2: HDU (Header Data Unit, DUID 0x0) dispatched from
    /// the traffic LSM heartbeat task. Marks the call start. Refreshes
    /// activity (and clears any post-TDU hold from the previous call).
    /// Phase 7C will extend this to extract the HDU payload (algorithm
    /// ID, key ID, source RadioID, MFID); for 7A.2 we just count and
    /// timestamp.
    pub fn hdu_received(&mut self, now: Instant, nac: u16) {
        self.hdus_seen += 1;
        self.last_duid = Some(0x0);
        self.last_nac = Some(nac);
        self.last_activity = now;
        self.post_tdu_hold_until = None;
        // Phase 7F.4 (2026-04-14): fast promote Acquiring -> Active.
        // Before this, promotion relied on a second same-TG grant
        // arriving from the control channel -- on slow sites that
        // could take 1-2 s after the retune, and the dashboard
        // would show "Acquiring" for the entire lead-in of the call
        // even though LDUs were already flowing. An HDU is
        // unambiguous positive proof that the call is live on the
        // traffic channel, so promote immediately.
        self.promote_acquiring_to_active();
    }

    /// Phase 7A.2: TDU (Terminator, DUID 0x3) or TDU_LC (DUID 0xF)
    /// dispatched from the traffic LSM heartbeat task. Starts the
    /// post-TDU hold window. The slot stays bound to the same TG
    /// for `post_tdu_hold_ms` after the TDU so that PTT releases
    /// between speakers in a multi-speaker conversation reuse the
    /// same slot. SDRTrunk PR #2010 semantics.
    ///
    /// `is_lc` is true for TDU_LC (DUID 0xF) -- carries final Link
    /// Control payload. Phase 7C will extract the LC for end-of-call
    /// logging; for 7A.2 we just track the count.
    ///
    /// 2026-04-15 fix: idempotent TDU handling matching SDRTrunk
    /// `P25TrafficChannelEventTracker.completeTraffic()`. Only the
    /// FIRST TDU after the slot became active starts the hold
    /// window; subsequent TDU/TDU_LC events inside the same call
    /// are no-ops (other than bumping the stats counter and
    /// refreshing last_duid/last_nac). Previously we overwrote
    /// `post_tdu_hold_until` on every TDU, so the HDL framer's
    /// phantom TDU_LC burst (20-70 copies of the same end-of-call
    /// marker at ~80 ms intervals, Phase 10 TODO) kept extending
    /// the hold and pinned the traffic DDC on dead channels for
    /// 5-10 s instead of the intended 2 s. Live on-target
    /// measurement at Duval County NAC 3BA on 2026-04-15 showed
    /// LDU1+LDU2 = 411 vs TDU_LC = 835 (2:1), with short calls
    /// missing voice capture entirely because the previous call's
    /// hold hadn't released yet. SDRTrunk reference (see
    /// P25TrafficChannelEventTracker.java:272-283): once
    /// `mComplete = true`, subsequent `completeTraffic()` calls
    /// return false without touching state.
    pub fn tdu_received(&mut self, now: Instant, nac: u16, is_lc: bool) {
        self.tdus_seen += 1;
        self.last_duid = Some(if is_lc { 0xF } else { 0x3 });
        self.last_nac = Some(nac);
        // Idempotent: only start the hold window if we are not
        // already in one. Matches SDRTrunk
        // P25TrafficChannelEventTracker.completeTraffic()'s
        // mComplete flag semantics. LDU arrival inside the hold
        // clears post_tdu_hold_until back to None (see
        // ldu_received), so a multi-speaker conversation with a
        // real LDU resume after PTT release still re-arms the
        // hold correctly on the NEXT real TDU. Phantom TDU_LC
        // bursts between the first TDU and the hold expiry now
        // do nothing, capping the dead-channel dwell at exactly
        // post_tdu_hold_ms.
        if self.post_tdu_hold_until.is_none() {
            self.post_tdu_hold_until = Some(
                now + std::time::Duration::from_millis(self.post_tdu_hold_ms));
        }
    }

    /// Phase 7A.2: LDU1 (DUID 0x5) or LDU2 (DUID 0xA) dispatched from
    /// the traffic LSM heartbeat task. These are the voice frames --
    /// each carries 9 IMBE frames (88 bits each, 20 ms of audio per
    /// frame, 180 ms of audio per LDU). Phase 7C will extract the
    /// IMBE bits and feed them to the vocoder; for 7A.2 we just
    /// refresh activity.
    pub fn ldu_received(&mut self, now: Instant, nac: u16, is_ldu2: bool) {
        self.ldus_seen += 1;
        self.last_duid = Some(if is_ldu2 { 0xA } else { 0x5 });
        self.last_nac = Some(nac);
        self.last_activity = now;
        // LDU resumes the conversation -- cancel the post-TDU hold.
        self.post_tdu_hold_until = None;
        // Phase 7F.4: same fast promote as `hdu_received`. If we
        // missed the HDU (short lead-in, HDU BCH rejected, or call
        // mid-speech) the first LDU is still proof the call is live.
        self.promote_acquiring_to_active();
    }

    /// Phase 7F.4 (2026-04-14): if we're in Acquiring, transition
    /// to Active, preserving channel/talkgroup/frequency/started.
    /// No-op if already Active or Idle. Called from the traffic-
    /// side heartbeat's HDU/LDU dispatch so the UI doesn't sit on
    /// "Acquiring" for the first 1-2 seconds of every call.
    fn promote_acquiring_to_active(&mut self) {
        if let TrafficState::Acquiring {
            channel,
            talkgroup,
            frequency_hz,
            started,
        } = self.state.clone()
        {
            self.state = TrafficState::Active {
                channel,
                talkgroup,
                frequency_hz,
                started,
            };
        }
    }

    /// Returns the remaining post-TDU hold time in milliseconds, or
    /// None if no hold is active. Surfaced via /api/traffic for
    /// debugging.
    pub fn post_tdu_hold_remaining_ms(&self) -> Option<u64> {
        self.post_tdu_hold_until.map(|t| {
            let now = Instant::now();
            if t > now {
                t.duration_since(now).as_millis() as u64
            } else {
                0
            }
        })
    }

    /// Check for timeouts and return to idle if needed.
    /// Returns true if state changed to Idle.
    ///
    /// Phase 7A.2: now also honours the post-TDU hold window. If a
    /// hold is active and has expired (and we're still in
    /// Active/Acquiring on the same call), release. The
    /// `call_timeout_ms` fallback continues to run in parallel for
    /// the case where TDUs are not arriving (e.g. before the LSM
    /// chain has locked, or for non-LSM voice channels).
    pub fn check_timeouts(&mut self) -> bool {
        let now = Instant::now();
        let elapsed_ms = now.duration_since(self.last_activity).as_millis() as u64;

        // Phase 7A.2: post-TDU hold window has priority. If a hold
        // is set and has expired, release immediately.
        if let Some(hold_until) = self.post_tdu_hold_until {
            if now >= hold_until {
                self.state = TrafficState::Idle;
                self.post_tdu_hold_until = None;
                return true;
            }
            // Hold window still active -- don't release on the
            // timeout below either, even if call_timeout_ms has
            // elapsed. The hold is the authoritative release
            // signal when TDUs are arriving.
            return false;
        }

        match &self.state {
            TrafficState::Acquiring { .. } => {
                // Phase 7A.1 fix: use last_activity, not `started`,
                // and apply the same call_timeout_ms as Active. The
                // 200 ms acquire_timeout_ms was a Phase 7C concern
                // (real sync detection) -- without sync detection
                // we can't distinguish "haven't locked yet" from
                // "locked and decoding". The Acquiring auto-promote
                // in handle_grant pushes us into Active within
                // 50 ms anyway, so this branch rarely runs in
                // practice.
                if elapsed_ms > self.call_timeout_ms {
                    self.state = TrafficState::Idle;
                    return true;
                }
            }
            TrafficState::Active { .. } => {
                if elapsed_ms > self.call_timeout_ms {
                    self.state = TrafficState::Idle;
                    return true;
                }
            }
            TrafficState::Idle => {}
        }

        false
    }

    /// Check if we're currently following a call
    pub fn is_active(&self) -> bool {
        !matches!(self.state, TrafficState::Idle)
    }

    /// Get the current talkgroup being followed (if any)
    pub fn current_talkgroup(&self) -> Option<Talkgroup> {
        match &self.state {
            TrafficState::Acquiring { talkgroup, .. }
            | TrafficState::Active { talkgroup, .. } => Some(*talkgroup),
            TrafficState::Idle => None,
        }
    }

    /// Get the current traffic channel frequency (if any)
    pub fn current_frequency(&self) -> Option<u64> {
        match &self.state {
            TrafficState::Acquiring { frequency_hz, .. }
            | TrafficState::Active { frequency_hz, .. } => Some(*frequency_hz),
            TrafficState::Idle => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nco_calculation() {
        // RX LO at 858.1 MHz, sample rate 8 MSPS
        let mut mgr = TrafficManager::new(858_100_000, 8_000_000);

        // Retune to 860.9625 MHz (control channel, +2.8625 MHz offset)
        let retune = mgr.handle_grant(
            Channel(0x0639),
            Talkgroup(300),
            860_962_500,
        );
        assert!(retune);

        // NCO word: 2862500 / 8000000 * 2^28 = 0x05_B3D4A0... let me compute
        // 2862500 / 8000000 = 0.35781250
        // 0.35781250 * 2^28 = 0.35781250 * 268435456 = 96050176 = 0x05B9_0000
        // The exact value depends on floating point, but should be in this range
        assert!(mgr.nco_word > 0x05B0_0000 && mgr.nco_word < 0x05C0_0000,
                "NCO word {} should be near 0x05B9_0000", mgr.nco_word);
    }

    #[test]
    fn test_negative_offset() {
        // RX LO at 858.1 MHz, target 855.2375 MHz (-2.8625 MHz)
        let mut mgr = TrafficManager::new(858_100_000, 8_000_000);

        let retune = mgr.handle_grant(
            Channel(0x0001),
            Talkgroup(100),
            855_237_500,
        );
        assert!(retune);
        // Negative offset should produce a large unsigned NCO word (two's complement)
        assert!(mgr.nco_word > 0x0A00_0000, "Negative offset should wrap: {:#010X}", mgr.nco_word);
    }

    #[test]
    fn test_grant_lifecycle() {
        let mut mgr = TrafficManager::new(858_100_000, 8_000_000);

        // Start idle
        assert!(!mgr.is_active());
        assert_eq!(mgr.current_talkgroup(), None);

        // Handle grant -> acquiring
        mgr.handle_grant(Channel(0x045D), Talkgroup(300), 857_987_500);
        assert!(mgr.is_active());
        assert_eq!(mgr.current_talkgroup(), Some(Talkgroup(300)));

        // Sync acquired -> active
        mgr.sync_acquired();
        assert!(matches!(mgr.state, TrafficState::Active { .. }));

        // Same channel grant just refreshes
        let retune = mgr.handle_grant(Channel(0x045D), Talkgroup(300), 857_987_500);
        assert!(!retune); // no retune needed
    }
}
