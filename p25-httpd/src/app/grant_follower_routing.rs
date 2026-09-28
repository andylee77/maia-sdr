//! Routing + chain control (Linux-only): the grant follower. Child
//! module of `app::grant_follower` (change 066: moved to its own file and
//! extended to several traffic chains).
//!
//! Consumes `GrantEvent`s from the CC decoder, applies the gates
//! (monitor list, speaker groups, channel reuse, encryption, chain choice
//! with sticky lock / pre-emption, `traffic_lock_freq`), dispatches FPGA
//! retunes through `fpga::IpCore::lane`, and refreshes each chain's
//! `ImbeForwarder` atomics. Subscribes to `CallTrackerEvent::CallClose`
//! to release a chain.
//!
//! Change 066: one follower drives every traffic chain ("lane"). The
//! chain for a grant comes from `app::lane_policy::choose_lane`; with one
//! chain the decisions are exactly the pre-066 ones.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use tokio::sync::{Mutex, RwLock};
use tokio::sync::mpsc::Receiver;

use super::{now_unix_ms, resume_needs_reset, Arc, CallTrackerEventKind, CallTrackerEventTx, CloseReason};
use crate::app::dibit_airtime::EpochKind;
use crate::app::imbe_forwarder::ImbeForwarder;
use crate::app::lane_policy::{choose_lane, LaneChoice, LaneView};
use crate::audio;
use crate::hardware::fpga;
use crate::hardware::traffic_lane::Lane;
use crate::protocol::p25::{self, control_channel::ControlChannelDecoder,
    traffic_chain::TrafficChain};
use crate::services::event_log::{EventLog, LogCategory};
use crate::services::monitor::MonitorList;

// 2026-05-03 dual-DDC pivot: the polyphase-channelizer-specific
// helpers (`CHANNELIZER_M`, `CHANNELIZER_FFT_LAG`, `bit_reverse_6`,
// `offset_to_bin_and_nco`, plus their unit tests) have been retired.
// Retunes now write a frequency offset directly to the dedicated
// traffic DDC NCO, mirroring the control-side DDC. See `doc/changes/`
// for the dual-DDC pivot.

/// Change 066: one traffic chain as the follower drives it.
pub struct FollowerLane {
    pub mgr: Arc<Mutex<TrafficChain>>,
    pub imbe: Arc<ImbeForwarder>,
    pub decoder: Arc<RwLock<ControlChannelDecoder>>,
}

impl FollowerLane {
    fn lane(&self) -> Lane {
        self.imbe.lane
    }
}

/// 2026-05-02 quality gate on state preservation. Skipping the LSM
/// reset pulse on a grant inherits the chain's end-of-call register
/// state. If the prior call ended cleanly (healthy IMBE rate, low silent
/// ratio), that state was a good steady-state lock and is worth keeping.
/// If it ended with the chain mid-fade, its state is degenerate and
/// should be cleared. Captured at CallClose; consumed at the next retune.
#[derive(Clone, Copy)]
struct LastCallQuality {
    freq_hz: u64,
    imbe_extracted: u64,
    silent_frames: u64,
    close_reason: CloseReason,
}

impl LastCallQuality {
    fn was_clean(&self) -> bool {
        // 2026-05-03 (build `quality-coast-no-los` follow-up): dropped the
        // `close_reason == TgChange` requirement. On a trunked system calls
        // almost always close with `Timeout`, not TgChange. The IMBE +
        // silent ratio characterise call quality on their own; close
        // reason only matters as a "did the chain crash" signal which
        // `StreamLag` already flags.
        //
        // Healthy: ≥1.5 s of decoded audio (≥30 IMBE @ 20 ms), < 5 %
        // silent, did NOT close due to broadcast lag.
        self.imbe_extracted >= 30
            && self.silent_frames * 20 < self.imbe_extracted
            && !matches!(self.close_reason, CloseReason::StreamLag)
    }
}

/// Change 066: the follower's memory of one chain.
#[derive(Default)]
struct LaneMem {
    /// 2026-05-02: the frequency last programmed into this chain's DDC
    /// (the chain stays parked there after a call).
    last_traffic_freq_hz: Option<u64>,
    last_call_quality: Option<LastCallQuality>,
    /// Change 057: (TG, frequency, unix ms) of the live call the
    /// lifecycle last closed by `Timeout`, for `refollow_on_update`.
    last_timeout_close: Option<(u16, u64, u64)>,
    /// Change 059: (TG, frequency, unix ms) of the newest clear grant the
    /// sticky gate rejected while this chain was a candidate.
    last_sticky_reject: Option<(u16, u64, u64)>,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_grant_follower(
    // Change 066: every traffic chain, lane One first.
    lanes: Vec<FollowerLane>,
    follower_core: Arc<Mutex<fpga::IpCore>>,
    follower_current_sample_rate_hz: Arc<std::sync::atomic::AtomicU32>,
    follower_current_rx_lo: Arc<AtomicI64>,
    // 2026-04-30: live DDC NCO crystal-trim shift, mirroring the control
    // chain's NCO programming. Read on every retune so the traffic
    // chain's offset includes the same PPM correction the control chain
    // bakes in via `tuning.rs` (auto-PPM, `PUT /api/ppm`, boot-loaded
    // shifts). A static `lo_ppm` left the traffic Costas loop absorbing
    // the full residual (`pll_dbg ≈ -5200` on traffic vs -2658 on
    // control in `2026-04-30-sync-trace`).
    follower_current_lo_shift_hz: Arc<AtomicI64>,
    follower_enabled: Arc<AtomicBool>,
    follower_monitor: Arc<RwLock<MonitorList>>,
    // Change 063: talkgroup groups -> speakers, priority pre-emption.
    follower_routing: Arc<crate::services::ui_settings::RoutingPolicy>,
    // Change 070: grants per frequency for the window planner.
    follower_plans: Arc<crate::services::lo_plan::PlanStore>,
    follower_event_log: Arc<EventLog>,
    mut grant_event_rx: Receiver<p25::events::P25Event>,
    follower_lock_freq: Arc<AtomicBool>,
    follower_boundary_tx: audio::CallBoundaryTx,
    follower_tracker_tx: CallTrackerEventTx,
    // 2026-05-03 seeding bake: shared converged-seed snapshot published
    // by the control-chain heartbeat (currently unused by the retune,
    // see `IpCore::retune_traffic_chain`).
    follower_converged_seeds: crate::app::seed_snapshot::ConvergedSeedsShared,
) {
    tokio::spawn(async move {
        assert!(!lanes.is_empty(), "grant follower needs a traffic chain");
        tracing::info!(
            "traffic grant follower task started ({} chain{})",
            lanes.len(),
            if lanes.len() == 1 { "" } else { "s" },
        );
        // Phase 2c (2026-04-25): CallTrackerEvent subscription. CallClose
        // drives release; the lifecycle's timeout sweep is the upstream
        // timer.
        let mut tracker_rx = follower_tracker_tx.subscribe();
        let mut mem: Vec<LaneMem> = lanes.iter().map(|_| LaneMem::default()).collect();
        // Site-wide state lives on lane One's objects: the encrypted TG
        // history (shared by every forwarder anyway), the grant map and
        // the encrypted-reject counter.
        let site = &lanes[0];

        // Process a grant on a chain; returns true if a retune is needed.
        let handle_grant_event =
            |g: &p25::events::GrantEvent,
             mgr: &mut TrafficChain,
             imbe: &ImbeForwarder| -> bool
        {
            let freq_hz = match g.frequency_hz {
                Some(f) => f,
                None => return false,
            };
            let retune = mgr.handle_grant(g.channel, g.talkgroup, freq_hz);

            // Only update the ImbeForwarder's active-call atomics when
            // this grant is for the call the chain follows. Otherwise a
            // grant for TG 700 [ENC] arriving while locked on clear TG
            // 300 would set call_encrypted=true on the TG 300 path.
            let grant_is_for_active = retune
                || mgr.current_talkgroup() == Some(g.talkgroup);

            // Encryption: the grant flag, then the TG history (updated
            // on every grant, followed or not).
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
                imbe.current_talkgroup.store(g.talkgroup.0, Ordering::Relaxed);
                // The grant's FM:<source> stamps recordings from the
                // CONTROL channel. Only on a genuine active-call grant.
                if let Some(src) = g.source {
                    if src.0 != 0 {
                        imbe.current_source.store(src.0, Ordering::Relaxed);
                    }
                }
                // 2026-04-24: per-channel attribution of calls.
                if let Some(f) = g.frequency_hz {
                    imbe.current_frequency_hz.store(f, Ordering::Relaxed);
                }
                if let Ok(mut s) = imbe.current_channel.lock() {
                    *s = format!("{}", g.channel);
                }
                if retune {
                    // New call: set encryption and reset vocoder.
                    imbe.call_encrypted.store(is_enc, Ordering::Relaxed);
                    imbe.vocoder_reset_pending.store(true, Ordering::Relaxed);
                } else if is_enc {
                    // Sticky-true within an active call.
                    imbe.call_encrypted.store(true, Ordering::Relaxed);
                }
                // call_encrypted is cleared only on Idle.
            }
            retune
        };

        // 2026-04-24 CC-grant-centric refactor: one
        // `CallBoundaryKind::CcGrantArrival` (or `CcGrantUpdate`) per
        // grant event. `not_followed` = the rejection reason (None =
        // accepted). `nac` is 0 (the HDU enriches it). Change 066: a
        // followed grant names its chain.
        let send_cc_boundary =
            |g: &p25::events::GrantEvent,
             not_followed: Option<&'static str>,
             lane: Option<Lane>| {
            let kind = if g.is_update {
                audio::CallBoundaryKind::CcGrantUpdate {
                    tg: g.talkgroup.0,
                    freq_hz: g.frequency_hz,
                    channel: g.channel.0,
                }
            } else {
                audio::CallBoundaryKind::CcGrantArrival {
                    tg: g.talkgroup.0,
                    // 2026-04-25: RadioId(0) → None ("CC announced the
                    // call but didn't tell us who").
                    source: g.source.and_then(|r| if r.0 != 0 { Some(r.0) } else { None }),
                    freq_hz: g.frequency_hz,
                    channel: g.channel.0,
                    encrypted: g.encrypted,
                    not_followed,
                }
            };
            let lane = if g.is_update || not_followed.is_some() { None } else { lane };
            let _ = follower_boundary_tx.send(audio::CallBoundary {
                kind,
                nac: 0,
                talkgroup: Some(g.talkgroup.0),
                expected_submit_count: 0,
                lane,
            });
        };

        // NCO offset of an RF frequency: rx_lo and sample rate read fresh
        // (POST /api/preset / /api/tune move them), plus the live DDC
        // crystal-trim shift.
        let nco_offset = |freq_hz: u64| -> (f64, f64) {
            let rx_lo_now = follower_current_rx_lo.load(Ordering::Relaxed);
            let sample_rate_now =
                follower_current_sample_rate_hz.load(Ordering::Relaxed) as f64;
            let shift = follower_current_lo_shift_hz.load(Ordering::Relaxed) as f64;
            (freq_hz as f64 - rx_lo_now as f64 + shift, sample_rate_now)
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
                    let p25::events::P25Event::Grant(g) = event;
                    let freq_mhz = g.frequency_hz.map(|f| f as f64 / 1e6).unwrap_or(0.0);

                    // Eager history populate: a first encrypted=true
                    // observation for a TG permanently blocks it, before
                    // any gate runs.
                    if g.encrypted {
                        if let Ok(mut hist) = site.imbe.encrypted_tg_history.lock() {
                            hist.insert(g.talkgroup.0);
                        }
                    }

                    // 2026-04-25: only log NEW grants, not GVCG_UPDATE
                    // keep-alives (~30 entries/sec pushed real activity
                    // out of the log ring).
                    if !g.is_update {
                        follower_event_log.push(
                            LogCategory::Grant,
                            format!(
                                "grant TG={} ch={} {:.4} MHz src={}{}",
                                g.talkgroup.0,
                                g.channel.0,
                                freq_mhz,
                                g.source.map(|r| r.0).unwrap_or(0),
                                if g.encrypted { " [ENC]" } else { "" },
                            ),
                            serde_json::json!({
                                "tg":        g.talkgroup.0,
                                "channel":   g.channel.0,
                                "frequency": g.frequency_hz,
                                "src":       g.source.map(|r| r.0),
                                "encrypted": g.encrypted,
                                "emergency": g.emergency,
                                "is_update": false,
                            }),
                        );
                    }

                    // Every chain's lock, read once per grant.
                    let mut locked: Vec<Option<u16>> = Vec::with_capacity(lanes.len());
                    for fl in &lanes {
                        locked.push(fl.mgr.lock().await.current_talkgroup().map(|t| t.0));
                    }

                    // 2026-04-26 GVCG_UPD fast-path: an update for a TG a
                    // chain follows is a keep-alive; no heavy locks.
                    let g = if g.is_update {
                        if locked.contains(&Some(g.talkgroup.0)) {
                            send_cc_boundary(&g, None, None);
                            continue;
                        }
                        // Change 057: the CC still announces the call we
                        // closed for lack of keep-alive: follow it again,
                        // as a (source-less) grant through every gate.
                        // Change 059: likewise a clear grant the sticky
                        // gate rejected, once a chain it was refused on is
                        // idle or its call has ended.
                        let now_ms = now_unix_ms();
                        let mut refollow: Option<(usize, bool)> = None;
                        for (i, fl) in lanes.iter().enumerate() {
                            let idle = locked[i].is_none();
                            if super::refollow_on_update(
                                mem[i].last_timeout_close, g.talkgroup.0, g.frequency_hz,
                                idle, now_ms, super::REFOLLOW_WINDOW_MS,
                            ) {
                                refollow = Some((i, true));
                                break;
                            }
                            let chain_free = locked[i].map_or(true, |t| {
                                super::end_marker_frees_chain(
                                    t,
                                    super::unpack_end_marker(
                                        fl.imbe.active_end_marker.load(Ordering::Relaxed)),
                                    now_ms,
                                )
                            });
                            if super::refollow_on_update(
                                mem[i].last_sticky_reject, g.talkgroup.0, g.frequency_hz,
                                chain_free, now_ms, super::REFOLLOW_STICKY_MS,
                            ) {
                                refollow = Some((i, false));
                                break;
                            }
                        }
                        let Some((i, by_timeout)) = refollow else {
                            // Updates are keep-alives. They must NEVER
                            // bring a chain out of Idle or pull it onto a
                            // different TG: UPD opcodes carry no service
                            // options, so a TG whose initial grant we
                            // missed could slip past the encrypted check
                            // (2026-04-30: TG 700 update pulled the chain
                            // off Idle).
                            send_cc_boundary(&g, Some("update_no_lock"), None);
                            continue;
                        };
                        let (why, reason) = if by_timeout {
                            mem[i].last_timeout_close = None;
                            ("call closed by timeout", "refollow_on_update")
                        } else {
                            for m in mem.iter_mut() {
                                m.last_sticky_reject = None;
                            }
                            ("rejected while the chain was busy", "refollow_after_sticky")
                        };
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!(
                                "re-follow TG={} on grant update {:.4} MHz ({}, still announced)",
                                g.talkgroup.0, freq_mhz, why,
                            ),
                            serde_json::json!({
                                "tg":        g.talkgroup.0,
                                "channel":   g.channel.0,
                                "frequency": g.frequency_hz,
                                "reason":    reason,
                                "chain":     lanes[i].lane().label(),
                            }),
                        );
                        let mut regrant = g.clone();
                        regrant.is_update = false;
                        regrant
                    } else {
                        g
                    };

                    // Tally every observed grant into the persistent
                    // frequency map (/api/grant_map), followed or not.
                    if let Some(freq) = g.frequency_hz {
                        site.mgr.lock().await.tally_grant(g.talkgroup.0, freq, g.encrypted);
                        follower_plans.note_grant(freq);
                    }

                    // Change 068: the ignore list wins over the monitor
                    // list and the speaker groups.
                    let routing = follower_routing.snapshot();
                    if routing.ignored(g.talkgroup.0) {
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!("reject: TG={} on the ignore list", g.talkgroup.0),
                            serde_json::json!({
                                "tg":     g.talkgroup.0,
                                "reason": "ignored",
                            }),
                        );
                        send_cc_boundary(&g, Some("ignored"), None);
                        continue;
                    }

                    // Monitor list gate.
                    let monitored = {
                        let monitor = follower_monitor.read().await;
                        monitor.is_empty() || monitor.contains(g.talkgroup.0)
                    };
                    if !monitored {
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!("reject: TG={} not in monitor list", g.talkgroup.0),
                            serde_json::json!({
                                "tg":     g.talkgroup.0,
                                "reason": "monitor_list",
                            }),
                        );
                        send_cc_boundary(&g, Some("monitor_list"), None);
                        continue;
                    }

                    // Change 063: speaker groups. A talkgroup whose group
                    // is on neither speaker, or an ungrouped one with
                    // "other talkgroups" off, is not followed.
                    let Some(route) = routing.route(g.talkgroup.0) else {
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!("reject: TG={} not on a speaker", g.talkgroup.0),
                            serde_json::json!({
                                "tg":     g.talkgroup.0,
                                "reason": "speaker_off",
                            }),
                        );
                        send_cc_boundary(&g, Some("speaker_off"), None);
                        continue;
                    };

                    // Change 070: a channel outside the usable receive
                    // window cannot be decoded (it would alias). Not
                    // followed; the planner counts it (above) and the
                    // recentre task moves the window when that pays.
                    if let Some(freq) = g.frequency_hz {
                        let (offset_hz, sample_rate_now) = nco_offset(freq);
                        let half = crate::services::lo_plan::usable_half_hz(sample_rate_now as u32) as f64;
                        if offset_hz.abs() > half {
                            follower_event_log.push(
                                LogCategory::Traffic,
                                format!(
                                    "reject: TG={} on {:.5} MHz, outside the receive window ({:+.0} kHz of +-{:.0})",
                                    g.talkgroup.0, freq as f64 / 1e6, offset_hz / 1e3, half / 1e3,
                                ),
                                serde_json::json!({
                                    "tg":        g.talkgroup.0,
                                    "freq_hz":   freq,
                                    "offset_hz": offset_hz,
                                    "reason":    "out_of_band",
                                }),
                            );
                            send_cc_boundary(&g, Some("out_of_band"), None);
                            continue;
                        }
                    }

                    // Channel-reuse detection, on every chain: the grant's
                    // channel (or frequency: 2026-04-25 Fix B, different
                    // channel ids for one frequency across IDEN records)
                    // matches a chain's lock but the TG differs — the
                    // trunking system reassigned that voice channel and
                    // the old call is done. Tear it down so the grant can
                    // be followed (subject to the gates below).
                    for (i, fl) in lanes.iter().enumerate() {
                        let mut mgr = fl.mgr.lock().await;
                        let locked_tg = mgr.current_talkgroup();
                        let chan_id_match = mgr.current_channel()
                            .map(|c| c == g.channel)
                            .unwrap_or(false);
                        let parked_freq = fl.imbe.current_frequency_hz.load(Ordering::Relaxed);
                        let same_freq_diff_chan = parked_freq != 0
                            && g.frequency_hz == Some(parked_freq);
                        let is_channel_reuse = (chan_id_match || same_freq_diff_chan)
                            && locked_tg.map(|t| t.0 != g.talkgroup.0).unwrap_or(false);
                        if !is_channel_reuse {
                            continue;
                        }
                        let prev_tg = locked_tg.map(|t| t.0).unwrap_or(0);
                        let reuse_reason = if chan_id_match { "channel_reuse" } else { "same_freq_reuse" };
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
                                "chain":       fl.lane().label(),
                            }),
                        );
                        mgr.force_idle();
                        locked[i] = None;
                        fl.imbe.current_talkgroup.store(0, Ordering::Relaxed);
                        // Change 054: the old TG's lock ends here on the
                        // air-time axis.
                        fl.imbe.mark_epoch(EpochKind::TgChange, false);
                    }

                    // Encrypted check runs BEFORE the chain choice so the
                    // history learns every encrypted TG and the log names
                    // the right reason.
                    let tg_known_enc = site.imbe.encrypted_tg_history.lock()
                        .map(|h| h.contains(&g.talkgroup.0))
                        .unwrap_or(false);
                    // Forensics override (2026-05-03 Track 2): follow
                    // encrypted grants when armed with follow_encrypted=1.
                    let forensics_override = crate::app::forensics::follow_encrypted_enabled();
                    if (g.encrypted || tg_known_enc) && !forensics_override {
                        site.mgr.lock().await.grants_rejected_encrypted += 1;
                        // A chain locked on this TG tears down now instead
                        // of holding the slot until the call ends.
                        let mut was_locked = false;
                        for (i, fl) in lanes.iter().enumerate() {
                            if locked[i] != Some(g.talkgroup.0) {
                                continue;
                            }
                            was_locked = true;
                            fl.mgr.lock().await.force_idle();
                            locked[i] = None;
                            fl.imbe.current_talkgroup.store(0, Ordering::Relaxed);
                            // Change 054: gate closes at this air-time cut.
                            // call_encrypted stays true: buffered LDUs of
                            // the channel are still in flight and must not
                            // be decoded as clear; the next retune writes
                            // the new grant's flag.
                            fl.imbe.mark_epoch(EpochKind::TgChange, true);
                            {
                                let core = follower_core.lock().await;
                                if let Some(l) = core.lane(fl.lane()) {
                                    l.pause();
                                }
                                // Change 057: the same-freq resume re-enables it.
                                fl.imbe.traffic_paused_by_teardown.store(true, Ordering::Relaxed);
                            }
                            // Change 054: in airtime mode the reader resets
                            // the framer at the cut recorded above.
                            if !fl.imbe.epochs_active() {
                                fl.decoder.write().await.reset_framer_state();
                            }
                        }
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!(
                                "reject TG={} encrypted{}",
                                g.talkgroup.0,
                                if was_locked { " (tore down active lock)" } else { "" },
                            ),
                            serde_json::json!({
                                "tg":         g.talkgroup.0,
                                "enc_flag":   g.encrypted,
                                "in_history": tg_known_enc,
                                "was_locked": was_locked,
                                "reason":     "encrypted",
                            }),
                        );
                        send_cc_boundary(&g, Some("encrypted"), None);
                        continue;
                    }

                    // Change 066: which chain follows the grant. Sticky
                    // lock (a chain stays with its call), pre-emption at
                    // the locked call's end marker (059) or for a
                    // higher-priority group (063), within the grant's
                    // candidate chains.
                    let now_ms = now_unix_ms();
                    let views: Vec<LaneView> = lanes.iter().enumerate().map(|(i, fl)| LaneView {
                        lane: fl.lane(),
                        locked_tg: locked[i],
                        parked_freq: mem[i].last_traffic_freq_hz,
                        end_marker: super::unpack_end_marker(
                            fl.imbe.active_end_marker.load(Ordering::Relaxed)),
                    }).collect();
                    let choice = choose_lane(
                        g.talkgroup.0, route.side, g.frequency_hz, &views, &routing,
                        |t, m| super::end_marker_frees_chain(t, m, now_ms),
                    );
                    let Some(lane) = choice.lane() else {
                        // Every candidate chain is busy. Remember the clear
                        // grant (the encrypted gate is above) for
                        // `refollow_on_update` once one frees up.
                        let cands = crate::app::lane_policy::candidates(
                            route.side, g.frequency_hz, &views, &routing);
                        if let Some(f) = g.frequency_hz {
                            for (i, fl) in lanes.iter().enumerate() {
                                if cands.contains(&fl.lane()) {
                                    mem[i].last_sticky_reject = Some((g.talkgroup.0, f, now_ms));
                                }
                            }
                        }
                        let ci = cands.first()
                            .and_then(|l| lanes.iter().position(|fl| fl.lane() == *l))
                            .unwrap_or(0);
                        let locked_tg = locked[ci].unwrap_or(0);
                        let parked_freq = lanes[ci].imbe.current_frequency_hz.load(Ordering::Relaxed);
                        let same_freq = parked_freq != 0 && g.frequency_hz == Some(parked_freq);
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!(
                                "reject: TG={} src={} freq={} \
                                 (sticky-locked on TG={} parked_freq={}{})",
                                g.talkgroup.0,
                                g.source.map(|r| r.0).unwrap_or(0),
                                g.frequency_hz.unwrap_or(0),
                                locked_tg, parked_freq,
                                if same_freq { " — SAME FREQ, channel-reuse miss?" } else { "" },
                            ),
                            serde_json::json!({
                                "tg":          g.talkgroup.0,
                                "src":         g.source.map(|r| r.0),
                                "grant_freq":  g.frequency_hz,
                                "locked_tg":   locked_tg,
                                "parked_freq": parked_freq,
                                "same_freq":   same_freq,
                                "reason":      "sticky_lock",
                                "chain":       lanes[ci].lane().label(),
                            }),
                        );
                        send_cc_boundary(&g, Some("sticky_lock"), None);
                        continue;
                    };
                    let li = lanes.iter().position(|fl| fl.lane() == lane).unwrap_or(0);
                    let fl = &lanes[li];
                    if let LaneChoice::Preempt(_, reason) = choice {
                        let prev_tg = locked[li].unwrap_or(0);
                        let end_marker = super::unpack_end_marker(
                            fl.imbe.active_end_marker.load(Ordering::Relaxed));
                        let ended_ms = end_marker.map_or(0, |(_, at)| now_ms.saturating_sub(at));
                        let why = if reason == "priority_preempt" {
                            "higher-priority group".to_string()
                        } else {
                            format!("ended {ended_ms} ms ago (end marker)")
                        };
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!("pre-empt: TG={} {}, follow TG={}", prev_tg, why, g.talkgroup.0),
                            serde_json::json!({
                                "prev_tg":  prev_tg,
                                "new_tg":   g.talkgroup.0,
                                "ended_ms": ended_ms,
                                "reason":   reason,
                                "chain":    lane.label(),
                            }),
                        );
                        fl.mgr.lock().await.force_idle();
                        locked[li] = None;
                        fl.imbe.current_talkgroup.store(0, Ordering::Relaxed);
                        // Change 054: the old TG's lock ends here on the
                        // air-time axis.
                        fl.imbe.mark_epoch(EpochKind::TgChange, false);
                    }

                    // Diagnostic lock: when on, suppress retunes so the
                    // chain stays parked, but still process grants whose
                    // freq matches the parked freq so the call state
                    // machine fires (2026-04-25 fix).
                    if follower_lock_freq.load(Ordering::Relaxed) {
                        let parked_freq = fl.imbe.current_frequency_hz.load(Ordering::Relaxed);
                        let grant_freq = g.frequency_hz.unwrap_or(0);
                        let same_freq = parked_freq != 0 && grant_freq != 0 && grant_freq == parked_freq;
                        if !same_freq {
                            follower_event_log.push(
                                LogCategory::Traffic,
                                format!(
                                    "lock: skip grant TG={} SRC={} ch={} freq={} (parked at {})",
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
                            send_cc_boundary(&g, Some("traffic_lock"), None);
                            continue;
                        }
                    }

                    // Accepted. Change 054: the boundary goes out AFTER
                    // handle_grant_event, so that when a retune follows,
                    // the grant-hold cut is recorded before the lifecycle
                    // can publish the new call_id.
                    let mut mgr = fl.mgr.lock().await;
                    let pre_state = mgr.state_label();
                    let ctx_before = fl.imbe.live_context();
                    let retune = handle_grant_event(&g, &mut mgr, &fl.imbe);
                    let post_state = mgr.state_label();
                    drop(mgr);
                    let epochs = fl.imbe.epochs_active();
                    if retune && epochs {
                        fl.imbe.mark_epoch_ctx(
                            EpochKind::GrantHold,
                            crate::app::dibit_airtime::SegmentContext { tg: 0, ..ctx_before },
                            true,
                        );
                    }
                    send_cc_boundary(&g, None, Some(lane));

                    if pre_state != post_state {
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!("state {} -> {} TG={}", pre_state, post_state, g.talkgroup.0),
                            serde_json::json!({
                                "from":  pre_state,
                                "to":    post_state,
                                "tg":    g.talkgroup.0,
                                "chain": lane.label(),
                            }),
                        );
                    }

                    if retune {
                        let freq_hz = g.frequency_hz.unwrap();
                        let (offset_hz, sample_rate_now) = nco_offset(freq_hz);
                        let offset_hz = offset_hz as i64;
                        // Reset the framer before the retune (pre-054
                        // modes); airtime mode resets at the retune's
                        // epoch cut, so in-flight dibits of the old call
                        // still decode to their end.
                        if !epochs {
                            fl.decoder.write().await.reset_framer_state();
                        }
                        let _ = fl.imbe.agc_seed_for_freq(freq_hz).unwrap_or(0);
                        // 2026-05-03 quality-gated coast (Option C): coast
                        // if the chain's previous call was clean, reset if
                        // it was degenerate.
                        let same_freq = mem[li].last_traffic_freq_hz == Some(freq_hz);
                        let prev_clean = mem[li].last_call_quality
                            .as_ref()
                            .map(|q| q.was_clean())
                            .unwrap_or(false);
                        let freq_changed = !prev_clean;
                        // PLL-only warm-start seeds (unused by the retune).
                        let seed_tuple: Option<(u32, i16, i32)> = {
                            let slot = follower_converged_seeds.read().await;
                            slot.as_ref().map(|s| (0, s.pll_seed, 0))
                        };
                        let retune_result = {
                            let core = follower_core.lock().await;
                            match core.lane(lane) {
                                Some(l) => l.retune(offset_hz as f64, sample_rate_now, freq_changed, seed_tuple),
                                None => Err(anyhow::anyhow!("{lane} chain is not present")),
                            }
                        };
                        if let Err(ref e) = retune_result {
                            tracing::warn!(target: "p25_traffic", "{lane} retune failed: {e}");
                        } else {
                            mem[li].last_traffic_freq_hz = Some(freq_hz);
                            // The retune enabled the chain.
                            fl.imbe.traffic_paused_by_teardown.store(false, Ordering::Relaxed);
                            // Change 054: the new call's context starts
                            // right after the retune's hardware cut. A
                            // failed retune never left the old frequency:
                            // the grant hold keeps the gate closed there.
                            let ctx_after = fl.imbe.live_context();
                            let kind = if ctx_after.tg != ctx_before.tg {
                                EpochKind::TgChange
                            } else {
                                EpochKind::CtxUpdate
                            };
                            fl.imbe.mark_epoch(kind, false);
                        }
                        tracing::info!(
                            target: "p25_traffic",
                            "retune {lane} (dual-DDC): TG={} channel={:?} freq={} Hz offset={:+} Hz \
                             same_freq={} prev_clean={} freq_changed={}",
                            g.talkgroup.0, g.channel, freq_hz, offset_hz,
                            same_freq, prev_clean, freq_changed,
                        );
                        let policy = if freq_changed { "reset" } else { "coast" };
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!(
                                "retune TG={} -> {:.4} MHz (offset {:+} Hz) {}",
                                g.talkgroup.0, freq_hz as f64 / 1e6, offset_hz, policy,
                            ),
                            serde_json::json!({
                                "tg":           g.talkgroup.0,
                                "channel":      g.channel.0,
                                "frequency":    freq_hz,
                                "offset_hz":    offset_hz,
                                "framer_reset": freq_changed,
                                "policy":       policy,
                                "prev_clean_q": prev_clean,
                                "same_freq_q":  same_freq,
                                "chain":        lane.label(),
                            }),
                        );
                    } else if pre_state == "Idle" && post_state != "Idle" {
                        // M2B 2026-05-02: same-freq new call (Idle ->
                        // Active on the frequency the chain is parked on).
                        // The chain stayed parked + enabled, so PLL/AGC
                        // carry across the gap; the PS framer resets. No
                        // NCO re-write unless the parked state went stale
                        // (`resume_needs_reset`), then the same reset
                        // retune as a frequency change.
                        let last_imbe = fl.imbe.last_imbe_at_millis.load(Ordering::Relaxed);
                        let ms_since_voice = (last_imbe != 0).then(|| now_ms.saturating_sub(last_imbe));
                        let (pll_pre, clamp) = {
                            let core = follower_core.lock().await;
                            let pre = core.lane(lane).map_or(0, |l| l.debug().0);
                            (pre, core.core_version().pll_clamp_q213())
                        };
                        let reset = resume_needs_reset(pll_pre, ms_since_voice, clamp);
                        // Change 057: an encrypted teardown paused the
                        // chain and the coast path writes no register:
                        // re-enable it.
                        let reenable = fl.imbe.traffic_paused_by_teardown.swap(false, Ordering::Relaxed);
                        let mut reset_ok = false;
                        if reset {
                            let freq_hz = g.frequency_hz.unwrap_or(0);
                            let (offset_hz, sample_rate_now) = nco_offset(freq_hz);
                            let seed_tuple: Option<(u32, i16, i32)> = {
                                let slot = follower_converged_seeds.read().await;
                                slot.as_ref().map(|s| (0, s.pll_seed, 0))
                            };
                            if !epochs {
                                fl.decoder.write().await.reset_framer_state();
                            }
                            // Records the Retune epoch cut with its settle
                            // discard (054 IpCore hook).
                            let res = {
                                let core = follower_core.lock().await;
                                match core.lane(lane) {
                                    Some(l) => l.retune(offset_hz, sample_rate_now, true, seed_tuple),
                                    None => Err(anyhow::anyhow!("{lane} chain is not present")),
                                }
                            };
                            match res {
                                Ok(_) => reset_ok = true,
                                Err(e) => tracing::warn!(
                                    target: "p25_traffic",
                                    "{lane} same-freq reset retune failed: {e}"
                                ),
                            }
                            if epochs {
                                fl.imbe.mark_epoch(EpochKind::TgChange, !reset_ok);
                            }
                        } else if epochs {
                            // Change 054: airtime mode applies the framer
                            // reset + new context at this air-time cut.
                            fl.imbe.mark_epoch(EpochKind::TgChange, true);
                        } else {
                            fl.decoder.write().await.reset_framer_state();
                        }
                        if reenable && !reset_ok {
                            // Records the Resume epoch cut (054 IpCore hook).
                            let core = follower_core.lock().await;
                            if let Some(l) = core.lane(lane) {
                                l.set_enable(true);
                            }
                        }
                        let idle_s = ms_since_voice
                            .map(|ms| format!("{:.1} s", ms as f64 / 1000.0))
                            .unwrap_or_else(|| "never".into());
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!(
                                "same-freq resume TG={} ({}; pll {} idle {}{})",
                                g.talkgroup.0,
                                if reset_ok { "reset" } else { "coast" },
                                pll_pre, idle_s,
                                if reenable { "; re-enabled after encrypted pause" } else { "" },
                            ),
                            serde_json::json!({
                                "tg":             g.talkgroup.0,
                                "channel":        g.channel.0,
                                "frequency":      g.frequency_hz,
                                "framer_reset":   true,
                                "lsm_reset":      reset_ok,
                                "nco_write":      reset_ok,
                                "policy":         if reset_ok { "reset" } else { "coast" },
                                "lsm_enable":     if reenable { "re-enabled" } else { "unchanged" },
                                "pll_pre_resume": pll_pre,
                                "ms_since_voice": ms_since_voice,
                                "chain":          lane.label(),
                            }),
                        );
                    } else if fl.imbe.live_context() != ctx_before {
                        // Change 054: a grant refresh for the active call
                        // changed its source / encryption: frames completing
                        // after this point carry the update.
                        fl.imbe.mark_epoch(EpochKind::CtxUpdate, false);
                    }
                }
                tracker_event = tracker_rx.recv() => {
                    let event = match tracker_event {
                        Ok(e) => e,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                target: "p25_traffic",
                                "follower tracker_rx lagged by {n} events; skipping",
                            );
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    // Phase 2c (2026-04-25): release a chain on the
                    // lifecycle's CallClose.
                    let CallTrackerEventKind::CallClose { reason, .. } = &event.kind else {
                        continue;
                    };
                    let reason = *reason;
                    // Change 066: the chain of the closed call; a call that
                    // was not followed has none.
                    let Some(li) = event.lane.and_then(|l| lanes.iter().position(|fl| fl.lane() == l)) else {
                        continue;
                    };
                    let fl = &lanes[li];

                    // Change 054 gating fix: only a CallClose of the chain's
                    // LIVE call releases it (the lifecycle publishes a
                    // successor before closing its predecessor; between
                    // calls it holds the last call's id).
                    let live_call_id = fl.imbe.current_call_id.load(Ordering::Relaxed);
                    if event.call_id != live_call_id {
                        tracing::debug!(
                            target: "p25_traffic",
                            "CallClose {:?} for call_id={} ignored ({} live call_id={})",
                            reason, event.call_id, fl.lane(), live_call_id,
                        );
                        continue;
                    }

                    // The closed call's quality for the chain's next retune
                    // (change 057: its own counts by call_id).
                    if let Some(freq_hz) = mem[li].last_traffic_freq_hz {
                        let counts = fl.imbe.call_counts.get(event.call_id).unwrap_or_default();
                        let q = LastCallQuality {
                            freq_hz,
                            imbe_extracted: counts.imbe_extracted,
                            silent_frames: counts.vocoder_silent,
                            close_reason: reason,
                        };
                        mem[li].last_call_quality = Some(q);
                        tracing::info!(
                            target: "p25_traffic",
                            "{} call quality captured for next retune: \
                             freq={} Hz imbe={} silent={} reason={:?} clean={}",
                            fl.lane(), q.freq_hz, q.imbe_extracted, q.silent_frames,
                            reason, q.was_clean(),
                        );
                    }

                    // Diagnostic lock keeps the chain on the parked freq.
                    if follower_lock_freq.load(Ordering::Relaxed) {
                        continue;
                    }

                    let mut mgr = fl.mgr.lock().await;
                    let pre_close_tg = mgr.current_talkgroup();
                    // Change 057: remember a timeout close, so a grant
                    // update that still announces this call re-follows it.
                    let parked_freq = fl.imbe.current_frequency_hz.load(Ordering::Relaxed);
                    mem[li].last_timeout_close = match (reason, pre_close_tg) {
                        (CloseReason::Timeout, Some(tg)) if parked_freq != 0 => {
                            Some((tg.0, parked_freq, now_unix_ms()))
                        }
                        _ => None,
                    };
                    // Soft release only — TrafficChain goes Idle; the LSM
                    // chain stays ENABLED on the last freq so the PLL keeps
                    // its lock for the next same-freq call.
                    mgr.force_idle();
                    drop(mgr);

                    if let Some(tg) = pre_close_tg {
                        tracing::info!(
                            target: "p25_traffic",
                            "{} Idle (CallClose {:?}) TG {} -- chain stays parked on last freq",
                            fl.lane(), reason, tg.0,
                        );
                        follower_event_log.push(
                            LogCategory::Traffic,
                            format!("state -> Idle ({:?}) TG={} (chain parked)", reason, tg.0),
                            serde_json::json!({
                                "tg":           tg.0,
                                "to":           "Idle",
                                "reason":       format!("{:?}", reason),
                                "chain_parked": true,
                                "chain":        fl.lane().label(),
                            }),
                        );
                    }
                    fl.imbe.call_encrypted.store(false, Ordering::Relaxed);
                    fl.imbe.current_talkgroup.store(0, Ordering::Relaxed);
                    // A later call with no FM: must not inherit this source.
                    fl.imbe.current_source.store(0, Ordering::Relaxed);
                    fl.imbe.current_frequency_hz.store(0, Ordering::Relaxed);
                    if let Ok(mut s) = fl.imbe.current_channel.lock() {
                        s.clear();
                    }
                    // Change 054: the gate closes at this air-time cut.
                    fl.imbe.mark_epoch(EpochKind::CallClose, false);
                }
            }
        }
        tracing::warn!("grant follower task exiting (channel closed)");
    });
}
