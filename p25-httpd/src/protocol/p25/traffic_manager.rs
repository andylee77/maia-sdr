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
//! Phase 7A.1 (2026-04-11): wired into main.rs as a singleton driven
//! by `app::follower::spawn_traffic_grant_follower` consuming typed
//! `GrantEvent` messages from the control-channel decoder's mpsc
//! broadcast. Phase 2e (2026-04-25): the prior `lsm_decoder.grants`
//! HashMap was removed; lifecycle now flows through `app::call_tracker`
//! (CallTrackerEvent) and the snapshot mirror in AppState.

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
    /// Last activity timestamp (HDU/LDU dispatch + retune). Diagnostic
    /// only after Phase 2c — release timing is owned by `app::call_tracker`
    /// and applied to the chain via `force_idle` from a CallTrackerEvent
    /// subscriber in the grant follower.
    last_activity: Instant,
    /// Total handle_grant() calls (Phase 7A.1: counts grant snapshots
    /// the polling task forwarded; some are duplicates that don't
    /// trigger a retune).
    pub grants_seen: u64,
    /// 2026-04-19: unique `(tg, freq)` grants first observed within the
    /// last `grant_dedup_window_ms`. SDRTrunk's per-call log typically
    /// shows ~3 `GRP_V_CH_GRANT` events per real call (one on first
    /// issue, two on rebroadcast over the ~12 s of voice) while our
    /// `grants_seen` counts every decode of any grant-family opcode,
    /// producing a ~20× over-count vs the SDRTrunk "unique grant"
    /// semantic. Splitting into `_new` / `_update` lets the dashboard
    /// reconcile the two semantics without losing the raw count.
    pub grants_seen_new: u64,
    /// 2026-04-19: refreshes of a `(tg, freq)` seen inside the dedup
    /// window. `grants_seen == grants_seen_new + grants_seen_update`
    /// modulo dedup-cache eviction.
    pub grants_seen_update: u64,
    /// LRU-ish cache: `(tg, freq_hz) -> last_seen Instant`. Entries
    /// older than `grant_dedup_window_ms` are evicted on the next
    /// `handle_grant` call to keep the map bounded.
    grant_dedup_last_seen:
        std::collections::HashMap<(u16, u64), Instant>,
    /// 2 s dedup window. Spec says "1-2 s"; we go with 2 s so a
    /// once-per-second GVCG rebroadcast still collapses into one new
    /// grant.
    grant_dedup_window_ms: u64,
    /// Total retunes triggered (handle_grant calls that returned true).
    pub retunes: u64,
    /// Wall-clock instant of the most recent retune.
    pub last_retune_at: Option<Instant>,
    /// Phase 2f (2026-04-25): grants that hit the NCO write-skip path
    /// (Idle + new grant freq matches currently-loaded NCO word). Each
    /// skip avoids a full PLL/AGC settle. Diagnostic surface in
    /// /api/traffic so the operator can see how often the optimisation
    /// fires on a busy site.
    pub nco_skips: u64,
    /// Grants the follower refused to lock onto because the call was
    /// flagged encrypted (either via `service_options.encrypted` on the
    /// TSBK or via the persistent encrypted-TG history).  Counts only
    /// the "Idle -> would-be-new-lock" rejects; refreshes for a TG the
    /// follower is already tracking fall through unchanged so the
    /// locked call doesn't get interrupted.  Added 2026-04-14 after the
    /// follower kept sticky-locking on TG 402 (encrypted) and starving
    /// the non-encrypted grants on the same site.
    pub grants_rejected_encrypted: u64,

    // ── NID/DUID stats from the traffic LSM HDL chain ─────────────
    //
    // The PS-side heartbeat dispatches HDU / LDU NID events to the
    // methods below. They update counters + last_duid/last_nac and
    // (for HDU/LDU) auto-promote Acquiring -> Active for the UI.
    //
    // Phase 2c (2026-04-25): TDU events are NO LONGER routed here.
    // Call lifecycle (start / TDU / timeout / release) is owned by
    // `app::call_tracker`, which broadcasts CallTrackerEvent::CallClose
    // to subscribers. The grant follower's CallClose subscriber
    // calls `force_idle()` on this manager to drive the state back
    // to Idle. The post-TDU hold window + `check_timeouts` /
    // `note_activity` / `tdu_received` machinery were retired — the
    // CallTracker timeout sweep + TDU detection cover the same
    // ground at the lifecycle layer with no parallel-judges bug.
    //
    /// Most recent DUID observed on the traffic LSM chain.
    pub last_duid: Option<u8>,
    /// Most recent NAC observed on the traffic LSM chain.
    pub last_nac: Option<u16>,
    /// Cumulative HDU count.
    pub hdus_seen: u64,
    /// Cumulative TDU count (TDU + TDU_LC combined).
    pub tdus_seen: u64,
    /// Cumulative LDU count (LDU1 + LDU2 combined).
    pub ldus_seen: u64,

    // ── Phase 10-prep: persistent grant-frequency map ────────────────
    //
    // Every grant observed (accepted or not) accumulates here, keyed
    // by (tg, frequency_hz). Used for:
    //   1. The scanner-mode UI's TG picker.
    //   2. Future auto-center-LO logic that picks an rx_lo to keep
    //      the most active traffic channels inside the AD9361 rf_bw
    //      window.
    // Populated in `tally_grant`, which `handle_grant` calls on every
    // incoming grant regardless of the follow decision.
    pub grant_map: std::collections::HashMap<(u16, u64), GrantMapEntry>,
}

/// Single row in the grant frequency map.
#[derive(Debug, Clone, Default)]
pub struct GrantMapEntry {
    /// Number of times this (tg, freq) pair has been granted.
    pub count: u64,
    /// Number of times the grant was flagged encrypted.
    pub encrypted_count: u64,
    /// Unix milliseconds of the first grant on this (tg, freq).
    pub first_seen_unix_ms: u64,
    /// Unix milliseconds of the most recent grant on this (tg, freq).
    pub last_seen_unix_ms: u64,
}

// ── Concurrency contract ─────────────────────────────────────────────
//
// All `&mut self` methods on `TrafficManager` (`handle_grant`,
// `hdu_received`, `ldu_received`, `force_idle`, `tally_grant`)
// assume the caller holds exclusive access. The singleton is owned
// as `Arc<tokio::sync::Mutex<TrafficManager>>` constructed in
// `main.rs`, and every call site (grant-follower task, traffic-LSM
// NID dispatcher, all `/api/traffic*` HTTP handlers) acquires the
// lock with `.lock().await` before touching the manager. Do not
// add a new caller that bypasses that lock.
impl TrafficManager {
    pub fn new(rx_lo_hz: u64, sample_rate_hz: u64) -> Self {
        TrafficManager {
            state: TrafficState::Idle,
            rx_lo_hz,
            sample_rate_hz,
            nco_word: 0,
            last_offset_hz: 0,
            last_activity: Instant::now(),
            grants_seen: 0,
            grants_seen_new: 0,
            grants_seen_update: 0,
            grant_dedup_last_seen: std::collections::HashMap::new(),
            grant_dedup_window_ms: 2_000,
            retunes: 0,
            last_retune_at: None,
            nco_skips: 0,
            grants_rejected_encrypted: 0,
            last_duid: None,
            last_nac: None,
            hdus_seen: 0,
            tdus_seen: 0,
            ldus_seen: 0,
            // Phase 10-prep: grant frequency map. Populated on every
            // observed TSBK grant regardless of follow decision, so
            // scanner-mode UI / LO auto-center can see the full view
            // even when `/api/monitor` is filtering most grants out.
            grant_map: std::collections::HashMap::new(),
        }
    }

    /// Phase 10-prep: record this grant in the accumulated
    /// frequency map, regardless of whether it will be followed.
    /// Called on every TSBK-driven grant before the follow decision.
    pub fn tally_grant(
        &mut self,
        talkgroup: u16,
        frequency_hz: u64,
        encrypted: bool,
    ) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let entry = self.grant_map
            .entry((talkgroup, frequency_hz))
            .or_insert_with(|| GrantMapEntry {
                count: 0,
                encrypted_count: 0,
                first_seen_unix_ms: now_ms,
                last_seen_unix_ms: now_ms,
            });
        entry.count += 1;
        if encrypted {
            entry.encrypted_count += 1;
        }
        entry.last_seen_unix_ms = now_ms;
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
    /// Phase 2c (2026-04-25): the Idle->next-grant transition is
    /// driven externally by `app::call_tracker` — when CallTracker
    /// closes a call (TDU / timeout / speaker-change), the grant
    /// follower's CallTrackerEvent subscriber calls `force_idle()`
    /// here. Once Idle, any new grant is accepted via the same code
    /// path.
    pub fn handle_grant(
        &mut self,
        channel: Channel,
        talkgroup: Talkgroup,
        frequency_hz: u64,
    ) -> bool {
        self.grants_seen += 1;

        // 2026-04-19: split the raw count into `_new` and `_update`
        // buckets so the dashboard can reconcile Fishball's
        // every-decode counter with SDRTrunk's one-per-unique-grant
        // semantics. A `(tg, freq)` pair seen within the dedup window
        // counts as an update; otherwise it's a new grant event and
        // we remember its timestamp. Evict stale entries on each call
        // so the map stays O(active calls) rather than growing with
        // every historical grant.
        let now = Instant::now();
        let window =
            std::time::Duration::from_millis(self.grant_dedup_window_ms);
        self.grant_dedup_last_seen
            .retain(|_, &mut last| now.duration_since(last) <= window);
        let key = (talkgroup.0, frequency_hz);
        match self.grant_dedup_last_seen.get(&key) {
            Some(&last) if now.duration_since(last) <= window => {
                self.grants_seen_update += 1;
            }
            _ => {
                self.grants_seen_new += 1;
            }
        }
        self.grant_dedup_last_seen.insert(key, now);

        // Same call (same TG)? Refresh activity. If the network
        // moved the TG to a new frequency, fall through to the
        // retune path so we follow it.
        //
        // Auto-promote Acquiring -> Active when a same-TG grant
        // arrives. Originally a workaround for the absence of a real
        // sync detector — kept post-Phase 2c because the LSM
        // heartbeat's hdu_received/ldu_received also promote, so this
        // covers the rare case of a CC keep-alive landing before the
        // first voice-frame dispatch.
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
        // Both paths require a retune (modulo the Phase 2f skip below).
        let offset_hz = frequency_hz as i64 - self.rx_lo_hz as i64;
        let nco_frac = offset_hz as f64 / self.sample_rate_hz as f64;
        // Convert to 28-bit unsigned (two's complement wrapping)
        let new_nco_word =
            (nco_frac * (1u64 << 28) as f64) as i32 as u32 & 0x0FFF_FFFF;

        // Phase 2f (2026-04-25): NCO write-skip. If the chain is Idle
        // and the new grant's NCO word matches what's already loaded
        // (the call before this one was on the same freq), the DDC is
        // physically on the right freq. Promote directly to Active —
        // no FPGA NCO write, no PLL/AGC settle penalty. Common on
        // sites with a small number of voice channels in heavy use.
        if new_nco_word == self.nco_word
            && matches!(self.state, TrafficState::Idle)
        {
            self.last_offset_hz = offset_hz;
            self.state = TrafficState::Active {
                channel,
                talkgroup,
                frequency_hz,
                started: Instant::now(),
            };
            let now = Instant::now();
            self.last_activity = now;
            self.nco_skips = self.nco_skips.saturating_add(1);
            return false; // No FPGA retune needed
        }

        self.nco_word = new_nco_word;
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

    /// Synchronous release path. Phase 2c (2026-04-25): the only
    /// release driver — called by the grant follower's
    /// `CallTrackerEvent::CallClose` subscriber and by
    /// `/api/talkgroups` when an operator force-clears a slot.
    /// Counters preserved. Caller pauses the FPGA traffic chain.
    pub fn force_idle(&mut self) {
        self.state = TrafficState::Idle;
    }

    /// Called when we detect frame sync on the traffic channel.
    /// Test-only today; the production path moves Acquiring->Active
    /// via `handle_grant` + the LDU/HDU dispatch.
    #[cfg(test)]
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

    // ── NID/DUID dispatch methods ─────────────────────────────────

    /// HDU (Header Data Unit, DUID 0x0) dispatched from the traffic
    /// LSM heartbeat. Bumps counters and fast-promotes
    /// Acquiring -> Active so the dashboard label flips immediately
    /// once the chain has decoded a frame.
    pub fn hdu_received(&mut self, now: Instant, nac: u16) {
        self.hdus_seen += 1;
        self.last_duid = Some(0x0);
        self.last_nac = Some(nac);
        self.last_activity = now;
        self.promote_acquiring_to_active();
    }

    /// LDU1 (DUID 0x5) / LDU2 (DUID 0xA) — voice frames. Bumps
    /// counters, refreshes activity, and fast-promotes if Acquiring.
    pub fn ldu_received(&mut self, now: Instant, nac: u16, is_ldu2: bool) {
        self.ldus_seen += 1;
        self.last_duid = Some(if is_ldu2 { 0xA } else { 0x5 });
        self.last_nac = Some(nac);
        self.last_activity = now;
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

    /// Check if we're currently following a call. Test-only today;
    /// production reads `current_talkgroup().is_some()` directly.
    #[cfg(test)]
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
#[path = "traffic_manager_tests.rs"]
mod tests;
