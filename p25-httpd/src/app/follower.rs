//! Traffic-channel grant follower task.
//!
//! Consumes GroupVoiceChannelGrant TSBKs from the canonical LSM
//! control-channel decoder, applies the sticky-lock / monitor-list /
//! encryption policies, dispatches traffic-DDC retunes, and refreshes
//! the ImbeForwarder's current_talkgroup / current_source /
//! call_encrypted atomics.
//!
//! Linux-only: dispatches retunes via `fpga::IpCore`, so the module
//! is gated with `#![cfg(target_os = "linux")]` to keep dev-side
//! `cargo check` on Windows passing.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use tokio::sync::{Mutex, RwLock};
use tokio::sync::mpsc::Receiver;

use crate::app::imbe_forwarder::ImbeForwarder;
use crate::audio;
use crate::hardware::fpga;
use crate::protocol::p25::{self, control_channel::ControlChannelDecoder,
    traffic_manager::TrafficManager};
use crate::services::event_log::EventLog;
use crate::services::monitor::MonitorList;

/// Grant-follower timeout tick. The follower is event-driven (mpsc
/// receiver) for the happy path; this tick exists only so the
/// per-call timeout logic (sticky-lock decay, grant expiry) gets a
/// chance to fire even when no new grant events are arriving.
const FOLLOWER_TIMEOUT_TICK_MS: u64 = 200;

#[allow(clippy::too_many_arguments)]
pub fn spawn_traffic_grant_follower(
    follower_mgr: Arc<Mutex<TrafficManager>>,
    follower_core: Arc<Mutex<fpga::IpCore>>,
    follower_current_sample_rate_hz: Arc<std::sync::atomic::AtomicU32>,
    follower_current_rx_lo: Arc<AtomicI64>,
    follower_lo_ppm: f64,
    follower_enabled: Arc<AtomicBool>,
    follower_imbe: Arc<ImbeForwarder>,
    follower_monitor: Arc<RwLock<MonitorList>>,
    follower_event_log: Arc<EventLog>,
    follower_traffic_decoder: Arc<RwLock<ControlChannelDecoder>>,
    mut grant_event_rx: Receiver<p25::events::P25Event>,
    follower_lock_freq: Arc<AtomicBool>,
    follower_boundary_tx: audio::CallBoundaryTx,
) {
        tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            tracing::info!(
                "traffic grant follower task started \
                 (event-driven via mpsc + 200 ms timeout tick)"
            );
            let mut timeout_tick =
                tokio::time::interval(std::time::Duration::from_millis(FOLLOWER_TIMEOUT_TICK_MS));
            timeout_tick.tick().await; // discard immediate first tick

            // Process a grant event; returns true if a retune was performed.
            //
            // Sticky-lock policy (from SDRTrunk PR #2010):
            // - If locked on a TG, only accept grants for that TG.
            // - If Idle, accept according to monitor list priority
            //   (or newest if monitor list is empty).
            let handle_grant_event =
                |g: &p25::events::GrantEvent,
                 mgr: &mut p25::traffic_manager::TrafficManager,
                 imbe: &ImbeForwarder| -> bool
            {
                let freq_hz = match g.frequency_hz {
                    Some(f) => f,
                    None => return false,
                };
                let retune = mgr.handle_grant(g.channel, g.talkgroup, freq_hz);

                // Only update the ImbeForwarder's active-call atomics
                // (current_talkgroup, current_source, call_encrypted)
                // when this grant is for the call we're following.
                // Otherwise a grant for TG 700 [ENC] arriving while
                // sticky-locked on unencrypted TG 300 would set
                // call_encrypted=true on the TG 300 path and IMBE
                // frames would be encryption-skipped. Gate on:
                //   - retune=true: we just switched to this TG.
                //   - mgr.current_talkgroup() == Some(g.tg): grant is
                //     a refresh for the already-active call.
                let grant_is_for_active = retune
                    || mgr.current_talkgroup() == Some(g.talkgroup);

                // Determine encryption: check the grant flag, then
                // fall back to TG history. History is updated on every
                // grant regardless of whether we follow it, so the
                // encrypted_tgs blocklist learns about TG 700 being
                // encrypted even while we stay locked on TG 300.
                let is_enc = if g.encrypted {
                    if let Ok(mut hist) = imbe.encrypted_tg_history.lock() {
                        hist.insert(g.talkgroup.0);
                    }
                    true
                } else {
                    imbe.encrypted_tg_history.lock()
                        .map(|h| h.contains(&g.talkgroup.0))
                        .unwrap_or(false)
                };

                if grant_is_for_active {
                    imbe.current_talkgroup
                        .store(g.talkgroup.0, Ordering::Relaxed);
                    // Stash the grant's FM:<source> so the recorder
                    // can stamp filenames from the CONTROL channel.
                    // Only overwrite on a genuine active-call grant.
                    if let Some(src) = g.source {
                        if src.0 != 0 {
                            imbe.current_source
                                .store(src.0, Ordering::Relaxed);
                        }
                    }
                    // 2026-04-24: tag traffic-channel freq/channel
                    // so RecordingEntry + GrantDecodeSummary can
                    // attribute calls per-channel for cross-channel
                    // quality diffs.
                    if let Some(f) = g.frequency_hz {
                        imbe.current_frequency_hz
                            .store(f, Ordering::Relaxed);
                    }
                    if let Ok(mut s) = imbe.current_channel.lock() {
                        *s = format!("{}", g.channel);
                    }
                    if retune {
                        // New call: set encryption and reset vocoder
                        imbe.call_encrypted.store(is_enc, Ordering::Relaxed);
                        imbe.vocoder_reset_pending
                            .store(true, Ordering::Relaxed);
                    } else if is_enc {
                        // Sticky-true within an active call
                        imbe.call_encrypted.store(true, Ordering::Relaxed);
                    }
                    // Note: we deliberately do NOT set call_encrypted
                    // = false on a grant refresh where g.encrypted ==
                    // false. The flag is cleared only on Idle.
                }
                retune
            };

            // Activity log is DELIBERATELY NOT deduped: every
            // `P25Event::Grant` arrival gets its own line, even
            // back-to-back grants on the same (TG, channel, freq)
            // packed into one 3-TSBK TSDU. The correct-handling
            // invariant lives in `TrafficManager::handle_grant`:
            // the `same_tg_same_freq` branch at
            // traffic_manager.rs:258 short-circuits with
            // `return false` (no retune, no second transition)
            // whenever the grant matches the current lock, and
            // Acquiring->Active auto-promotes on that same branch.

            // 2026-04-24 CC-grant-centric refactor: fire one
            // `CallBoundaryKind::CcGrantArrival` (or `CcGrantUpdate`)
            // per incoming grant event. Full grants carry the
            // `not_followed` rejection reason (None = accepted);
            // Updates are keep-alives that only refresh ttl on an
            // existing OpenGrant and ignore not_followed. grant_stats
            // owns the OpenGrant lifecycle — follower just reports
            // facts. `nac` is stubbed to 0 (CC-side NAC isn't
            // plumbed into GrantEvent; traffic heartbeat's HduStart
            // enriches OpenGrant with the real NAC once the chain
            // locks).
            let send_cc_boundary =
                |g: &p25::events::GrantEvent,
                 not_followed: Option<&'static str>| {
                let kind = if g.is_update {
                    audio::CallBoundaryKind::CcGrantUpdate {
                        tg: g.talkgroup.0,
                        freq_hz: g.frequency_hz,
                        channel: g.channel.0,
                    }
                } else {
                    audio::CallBoundaryKind::CcGrantArrival {
                        tg: g.talkgroup.0,
                        // 2026-04-25: map RadioId(0) → None. Some sites
                        // emit `GRP_VCH_GRANT TG:NNN SRC:00000` on the
                        // wire (observed for TG 302 on Clay County
                        // 8A1) — these are "CC announced the call but
                        // didn't tell us who", semantically equivalent
                        // to a GRP_VCH_GRNT_UPD that doesn't carry a
                        // source. Treat as None so downstream LDU1 LC
                        // FM: enrichment can fill in (when we follow
                        // the call) and the dashboard renders a clean
                        // "--" instead of misleading "0".
                        source: g.source.and_then(|r| {
                            if r.0 != 0 { Some(r.0) } else { None }
                        }),
                        freq_hz: g.frequency_hz,
                        channel: g.channel.0,
                        encrypted: g.encrypted,
                        not_followed,
                    }
                };
                let _ = follower_boundary_tx.send(audio::CallBoundary {
                    kind,
                    nac: 0,
                    talkgroup: Some(g.talkgroup.0),
                    expected_submit_count: 0,
                });
            };

            loop {
                tokio::select! {
                    event = grant_event_rx.recv() => {
                        let event = match event {
                            Some(e) => e,
                            None => break, // channel closed
                        };

                        if !follower_enabled.load(Ordering::Relaxed) {
                            continue;
                        }

                        match event {
                            p25::events::P25Event::Grant(g) => {
                                use crate::services::event_log::LogCategory;
                                let freq_mhz = g.frequency_hz
                                    .map(|f| f as f64 / 1e6)
                                    .unwrap_or(0.0);

                                // Eager history populate: any
                                // encrypted=true grant adds the TG to
                                // the persistent history before any
                                // gate check runs. Otherwise a site
                                // that sometimes-sets / sometimes-
                                // doesn't set service_options would
                                // let us retune to the same encrypted
                                // TG multiple times before history
                                // caught up. First encrypted=true
                                // observation for a TG permanently
                                // blocks all subsequent grants for it.
                                if g.encrypted {
                                    if let Ok(mut hist) =
                                        follower_imbe
                                            .encrypted_tg_history
                                            .lock()
                                    {
                                        hist.insert(g.talkgroup.0);
                                    }
                                }

                                // Raw grant receipt (pre-filter).
                                // Logged unconditionally; double-retune
                                // protection lives in
                                // TrafficManager::handle_grant's
                                // `same_tg_same_freq` branch
                                // (traffic_manager.rs:258).
                                follower_event_log.push(
                                    LogCategory::Grant,
                                    format!(
                                        "grant TG={} ch={} {:.4} MHz{}",
                                        g.talkgroup.0,
                                        g.channel.0,
                                        freq_mhz,
                                        if g.encrypted { " [ENC]" } else { "" },
                                    ),
                                    serde_json::json!({
                                        "tg":        g.talkgroup.0,
                                        "channel":   g.channel.0,
                                        "frequency": g.frequency_hz,
                                        "src":       g.source.map(|r| r.0),
                                        "encrypted": g.encrypted,
                                        "emergency": g.emergency,
                                    }),
                                );

                                // Tally every observed grant into the
                                // persistent frequency map regardless
                                // of follow decision; populates
                                // /api/grant_map for scanner-mode UI
                                // and future LO auto-center.
                                if let Some(freq) = g.frequency_hz {
                                    let mut mgr = follower_mgr.lock().await;
                                    mgr.tally_grant(
                                        g.talkgroup.0, freq, g.encrypted);
                                }

                                // Monitor list gate
                                let dominated = {
                                    let monitor = follower_monitor.read().await;
                                    if monitor.is_empty() {
                                        true // accept all
                                    } else {
                                        monitor.contains(g.talkgroup.0)
                                    }
                                };
                                if !dominated {
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "reject: TG={} not in monitor list",
                                            g.talkgroup.0,
                                        ),
                                        serde_json::json!({
                                            "tg":     g.talkgroup.0,
                                            "reason": "monitor_list",
                                        }),
                                    );
                                    send_cc_boundary(&g, Some("monitor_list"));
                                    continue;
                                }

                                let mut mgr = follower_mgr.lock().await;
                                let locked_tg = mgr.current_talkgroup();
                                let locked_ch = mgr.current_channel();

                                // Channel-reuse detection: if the
                                // grant's channel matches our current
                                // lock but the TG differs, the trunking
                                // system has reassigned our voice
                                // channel to a different TG and the old
                                // call is done. Tear down so we can
                                // follow the new TG (subject to the
                                // gates below). Without this the
                                // sticky-lock reject would keep us on
                                // a dead channel for up to
                                // call_timeout_ms (2 s).
                                // 2026-04-25 Fix B: extend channel-reuse
                                // detection to also fire on same-FREQ-
                                // different-channel-id. Different TGs
                                // can be assigned to the same physical
                                // freq via different channel-id values
                                // depending on which IDEN_UPDATE band
                                // record the trunking system was using
                                // — observed on Clay County NAC 8A1
                                // where TG 302 and TG 1210 both transmit
                                // on 857.9875 MHz but the chain might
                                // be sticky-locked on TG 302's channel-
                                // id (e.g., 0-1189 from a prior call on
                                // 858.4375 MHz) when a TG 1210 grant
                                // for 0-1117 on 857.9875 arrives.
                                //
                                // Original channel-id-only check would
                                // miss this case → sticky_lock rejects
                                // the new grant → chain doesn't follow
                                // TG 1210 → no recordings, but the chain
                                // continues to decode whatever audio is
                                // on its current freq.
                                //
                                // Adding the same-freq fallback closes
                                // the gap. When the chain's actual freq
                                // (current_frequency_hz) matches the new
                                // grant's freq_hz, treat it as channel-
                                // reuse: tear down old, accept new. No
                                // retune happens because freq is already
                                // correct; just TG/source attribution
                                // updates.
                                let chan_id_match = locked_ch
                                    .map(|c| c == g.channel)
                                    .unwrap_or(false);
                                let parked_freq = follower_imbe
                                    .current_frequency_hz
                                    .load(Ordering::Relaxed);
                                let same_freq_diff_chan = parked_freq != 0
                                    && g.frequency_hz == Some(parked_freq);
                                let is_channel_reuse =
                                    (chan_id_match || same_freq_diff_chan)
                                    && locked_tg.map(|t| t.0 != g.talkgroup.0).unwrap_or(false);
                                if is_channel_reuse {
                                    let prev_tg = locked_tg.map(|t| t.0).unwrap_or(0);
                                    let reuse_reason = if chan_id_match {
                                        "channel_reuse"
                                    } else {
                                        "same_freq_reuse"
                                    };
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "channel reuse: ch={} was TG={}, now TG={} ({}) — teardown",
                                            g.channel, prev_tg, g.talkgroup.0, reuse_reason,
                                        ),
                                        serde_json::json!({
                                            "channel":     format!("{}", g.channel),
                                            "prev_tg":     prev_tg,
                                            "new_tg":      g.talkgroup.0,
                                            "parked_freq": parked_freq,
                                            "grant_freq":  g.frequency_hz,
                                            "reason":      reuse_reason,
                                        }),
                                    );
                                    mgr.force_idle();
                                    follower_imbe.current_talkgroup
                                        .store(0, Ordering::Relaxed);
                                    // Fall through — re-evaluate the
                                    // grant as if Idle. Encrypted +
                                    // sticky-lock checks below now see
                                    // locked_tg = None and proceed.
                                }
                                // Re-read after the possible force_idle.
                                let locked_tg = mgr.current_talkgroup();

                                // Encrypted check runs BEFORE the
                                // sticky-lock check. Order matters: a
                                // TG 406 [ENC] grant arriving while
                                // locked on TG 301 must reach the
                                // encrypted gate so encrypted_tg_history
                                // learns TG 406; otherwise if we later
                                // went Idle and TG 406 re-emitted with
                                // a flipped service-options byte
                                // (FEC-marginal), we'd accept it. This
                                // ordering also keeps the
                                // grants_rejected_encrypted stat
                                // accurate and names the correct reason
                                // in the log line.
                                let tg_known_enc = follower_imbe
                                    .encrypted_tg_history
                                    .lock()
                                    .map(|h| h.contains(&g.talkgroup.0))
                                    .unwrap_or(false);
                                if g.encrypted || tg_known_enc {
                                    if g.encrypted {
                                        if let Ok(mut hist) =
                                            follower_imbe
                                                .encrypted_tg_history
                                                .lock()
                                        {
                                            hist.insert(g.talkgroup.0);
                                        }
                                    }
                                    mgr.grants_rejected_encrypted += 1;

                                    // If the encrypted TG is our
                                    // current lock, tear down
                                    // synchronously — otherwise the
                                    // sticky 2 s timeout holds the
                                    // slot until the call ends
                                    // naturally.
                                    let was_locked = locked_tg
                                        .map(|t| t.0 == g.talkgroup.0)
                                        .unwrap_or(false);
                                    if was_locked {
                                        mgr.force_idle();
                                        drop(mgr);
                                        // Zero current_talkgroup so
                                        // the vocoder task's TG-change
                                        // auto-flush fires and closes
                                        // the call summary.
                                        follower_imbe.current_talkgroup
                                            .store(0, Ordering::Relaxed);
                                        // DO NOT clear call_encrypted
                                        // here. Buffered LDU dibits
                                        // from the previous channel
                                        // are still in flight in the
                                        // DMA ring + mpsc channel;
                                        // clearing the flag would let
                                        // the vocoder DECODE those
                                        // encrypted LDUs as clear and
                                        // produce garbled output.
                                        // Leaving it `true` keeps the
                                        // skip path active until the
                                        // next retune (which
                                        // unconditionally writes
                                        // call_encrypted =
                                        // new_grant.is_enc).
                                        #[cfg(target_os = "linux")]
                                        {
                                            let core = follower_core
                                                .lock().await;
                                            // Quiesce both LSM + C4FM
                                            // chains on encrypted
                                            // teardown so the traffic
                                            // LSM demod stops emitting
                                            // phantom NID events until
                                            // the next grant.
                                            core.pause_traffic_chain();
                                        }
                                        // Reset the traffic framer --
                                        // it's mid-frame on encrypted
                                        // data and would carry bogus
                                        // state into the next lock.
                                        {
                                            let mut dec = follower_traffic_decoder
                                                .write().await;
                                            dec.reset_framer_state();
                                        }
                                    }

                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "reject TG={} encrypted{}",
                                            g.talkgroup.0,
                                            if was_locked {
                                                " (tore down active lock)"
                                            } else { "" },
                                        ),
                                        serde_json::json!({
                                            "tg":         g.talkgroup.0,
                                            "enc_flag":   g.encrypted,
                                            "in_history": tg_known_enc,
                                            "was_locked": was_locked,
                                            "reason":     "encrypted",
                                        }),
                                    );
                                    send_cc_boundary(&g, Some("encrypted"));
                                    continue;
                                }

                                // Sticky-lock check runs AFTER the
                                // channel-reuse + encrypted gates. A
                                // grant for a different TG on a
                                // different channel is an unrelated
                                // call; stay on the current lock.
                                let locked_tg_final = mgr.current_talkgroup();
                                if let Some(tg) = locked_tg_final {
                                    if tg.0 != g.talkgroup.0 {
                                        follower_event_log.push(
                                            LogCategory::Traffic,
                                            format!(
                                                "reject: TG={} (sticky-locked on TG={})",
                                                g.talkgroup.0, tg.0,
                                            ),
                                            serde_json::json!({
                                                "tg":        g.talkgroup.0,
                                                "locked_tg": tg.0,
                                                "reason":    "sticky_lock",
                                            }),
                                        );
                                        send_cc_boundary(&g, Some("sticky_lock"));
                                        continue;
                                    }
                                }

                                // Diagnostic lock: when on, suppress
                                // retunes so the chain stays parked
                                // on a known-active channel, but
                                // STILL process grants whose freq
                                // matches the parked freq so the
                                // call state machine fires (TG/source
                                // get stamped, IMBE flows into the
                                // recorder, audio gets decoded).
                                //
                                // 2026-04-25 fix: previously this
                                // gate skipped EVERY grant
                                // unconditionally — meaning lock=on
                                // also stopped following the very
                                // calls the operator wanted to
                                // measure on the parked freq. Now we
                                // only skip grants that would require
                                // a retune (different freq from
                                // current_frequency_hz). Same-freq
                                // grants fall through and process
                                // normally; handle_grant_event sees
                                // the matching freq and returns
                                // retune=false so no actual retune
                                // dispatch happens.
                                if follower_lock_freq.load(Ordering::Relaxed) {
                                    let parked_freq = follower_imbe
                                        .current_frequency_hz
                                        .load(Ordering::Relaxed);
                                    let grant_freq = g.frequency_hz
                                        .unwrap_or(0);
                                    let same_freq = parked_freq != 0
                                        && grant_freq != 0
                                        && grant_freq == parked_freq;
                                    if !same_freq {
                                        follower_event_log.push(
                                            LogCategory::Traffic,
                                            format!(
                                                "lock: skip grant TG={} SRC={} \
                                                 ch={} freq={} (parked at {})",
                                                g.talkgroup.0,
                                                g.source.map(|s| s.0).unwrap_or(0),
                                                g.channel,
                                                grant_freq,
                                                parked_freq,
                                            ),
                                            serde_json::json!({
                                                "tg":          g.talkgroup.0,
                                                "channel":     format!("{}", g.channel),
                                                "grant_freq":  grant_freq,
                                                "parked_freq": parked_freq,
                                                "reason":      "traffic_lock",
                                            }),
                                        );
                                        send_cc_boundary(&g, Some("traffic_lock"));
                                        continue;
                                    }
                                    // Same freq as parked — fall through.
                                    // handle_grant_event will return
                                    // retune=false (TrafficManager sees
                                    // matching channel/freq), the imbe
                                    // atomics get updated, the call state
                                    // advances, and the recorder /
                                    // grant_stats see proper events.
                                }
                                // Accepted — about to evaluate handle_grant_event.
                                // Emit CcGrantArrival (or Update) with not_followed=None
                                // so grant_stats can open / refresh the OpenGrant. Kept
                                // here instead of pre-gates so we only fire for grants
                                // that actually proceed to state-machine evaluation.
                                send_cc_boundary(&g, None);
                                let pre_state = mgr.state_label();
                                let retune = handle_grant_event(
                                    &g, &mut mgr, &follower_imbe
                                );
                                let post_state = mgr.state_label();
                                drop(mgr);

                                if pre_state != post_state {
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "state {} -> {} TG={}",
                                            pre_state, post_state, g.talkgroup.0,
                                        ),
                                        serde_json::json!({
                                            "from": pre_state,
                                            "to":   post_state,
                                            "tg":   g.talkgroup.0,
                                        }),
                                    );
                                }

                                if retune {
                                    let freq_hz = g.frequency_hz.unwrap();
                                    // PPM correction matching the control
                                    // DDC path in get_reinit(). Cancels
                                    // the Pluto crystal trim error (a
                                    // few hundred Hz depending on lo_ppm
                                    // and rx_lo) so the traffic PLL
                                    // doesn't sit at a residual steady-
                                    // state phase error on every call.
                                    //
                                    // rx_lo + sample rate read fresh
                                    // (not captured at spawn) so offset
                                    // math follows live LO or preset
                                    // changes (POST /api/preset moves
                                    // sample rate; POST /api/tune moves
                                    // rx_lo in Auto mode).
                                    let rx_lo_now = follower_current_rx_lo
                                        .load(std::sync::atomic::Ordering::Relaxed);
                                    let sample_rate_now =
                                        follower_current_sample_rate_hz
                                            .load(std::sync::atomic::Ordering::Relaxed)
                                            as f64;
                                    let nco_lo_shift_hz =
                                        -follower_lo_ppm * 1e-6
                                            * rx_lo_now as f64;
                                    let offset_hz = (freq_hz as f64
                                        - rx_lo_now as f64
                                        + nco_lo_shift_hz)
                                        as i64;

                                    // Reset the traffic-side framer
                                    // BEFORE the DDC retune so dibits
                                    // from the new frequency aren't
                                    // consumed while the framer is
                                    // mid-state on stale data.
                                    // Preserves cumulative counters.
                                    {
                                        let mut dec = follower_traffic_decoder
                                            .write().await;
                                        dec.reset_framer_state();
                                    }

                                    let core = follower_core.lock().await;
                                    // Atomic freeze-reset-thaw:
                                    // `retune_traffic_chain` disables
                                    // both LSM and C4FM chains, writes
                                    // the new DDC frequency, pulses
                                    // `traffic_lsm_reset` (clearing the
                                    // PLL accumulator and upstream
                                    // state), then re-enables. Post-
                                    // retune PLL starts from 0 and
                                    // converges in ~50-100 ms instead
                                    // of carrying stale phase from the
                                    // previous carrier. See
                                    // doc/changes/038.
                                    match core.retune_traffic_chain(
                                        offset_hz as f64,
                                        sample_rate_now,
                                    ) {
                                        Ok(()) => {
                                            tracing::info!(
                                                target: "p25_traffic",
                                                "retune: TG={} channel={:?} \
                                                 freq={} Hz offset={:+} Hz \
                                                 (LSM freeze-reset-thaw, framer reset)",
                                                g.talkgroup.0, g.channel,
                                                freq_hz, offset_hz,
                                            );
                                            follower_event_log.push(
                                                LogCategory::Traffic,
                                                format!(
                                                    "retune TG={} -> {:.4} MHz (offset {:+} Hz)",
                                                    g.talkgroup.0,
                                                    freq_hz as f64 / 1e6,
                                                    offset_hz,
                                                ),
                                                serde_json::json!({
                                                    "tg":          g.talkgroup.0,
                                                    "channel":     g.channel.0,
                                                    "frequency":   freq_hz,
                                                    "offset_hz":   offset_hz,
                                                    "framer_reset": true,
                                                    "lsm_reset":   true,
                                                }),
                                            );
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                target: "p25_traffic",
                                                "traffic DDC retune failed: \
                                                 TG={} freq={} Hz \
                                                 offset={:+} Hz: {}",
                                                g.talkgroup.0, freq_hz,
                                                offset_hz, e,
                                            );
                                            follower_event_log.push(
                                                LogCategory::Traffic,
                                                format!(
                                                    "retune FAILED TG={}: {}",
                                                    g.talkgroup.0, e,
                                                ),
                                                serde_json::json!({
                                                    "tg":     g.talkgroup.0,
                                                    "error":  e.to_string(),
                                                }),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ = timeout_tick.tick() => {
                        if !follower_enabled.load(Ordering::Relaxed) {
                            continue;
                        }
                        // Diagnostic lock also suppresses idle timeout
                        // so the chain stays Active on the parked freq
                        // and the demod keeps running between calls.
                        if follower_lock_freq.load(Ordering::Relaxed) {
                            continue;
                        }
                        let mut mgr = follower_mgr.lock().await;
                        let pre_timeout_tg = mgr.current_talkgroup();
                        if mgr.check_timeouts() {
                            drop(mgr);
                            let core = follower_core.lock().await;
                            // Pause both LSM + C4FM chains between
                            // calls so the traffic demod is quiescent
                            // during Idle — no phantom NID events and
                            // no PLL drift against noise.
                            core.pause_traffic_chain();
                            if let Some(tg) = pre_timeout_tg {
                                // Phase 2e (2026-04-25): the per-decoder
                                // grant store is gone — call lifecycle
                                // lives in `app::call_tracker`. Just
                                // log the Idle transition; CallTracker's
                                // own timeout sweep already closed the
                                // matching call seconds earlier.
                                tracing::info!(
                                    target: "p25_traffic",
                                    "traffic Idle (timeout) TG {} -- \
                                     demod_enable=off",
                                    tg.0,
                                );
                                follower_event_log.push(
                                    crate::services::event_log::LogCategory::Traffic,
                                    format!(
                                        "state -> Idle (timeout) TG={}",
                                        tg.0,
                                    ),
                                    serde_json::json!({
                                        "tg":       tg.0,
                                        "to":       "Idle",
                                        "reason":   "call_timeout",
                                    }),
                                );
                            } else {
                                tracing::info!(
                                    target: "p25_traffic",
                                    "traffic Idle (timeout) -- \
                                     demod_enable=off"
                                );
                            }
                            follower_imbe.call_encrypted.store(
                                false, Ordering::Relaxed,
                            );
                            follower_imbe.current_talkgroup.store(
                                0, Ordering::Relaxed,
                            );
                            // Clear stashed source on Idle so a
                            // subsequent call with no FM: in its grant
                            // doesn't inherit the previous speaker's
                            // ID.
                            follower_imbe.current_source.store(
                                0, Ordering::Relaxed,
                            );
                            follower_imbe.current_frequency_hz.store(
                                0, Ordering::Relaxed,
                            );
                            if let Ok(mut s) = follower_imbe.current_channel.lock() {
                                s.clear();
                            }
                        }
                    }
                }
            }
            tracing::warn!("grant follower task exiting (channel closed)");
        });
}
