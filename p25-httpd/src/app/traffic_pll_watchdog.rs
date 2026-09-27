//! Traffic LSM PLL watchdog.
//!
//! The HDL Costas loop is decision-directed on the differential phase,
//! so its stable points are the bias plus multiples of π/2. Demodulating
//! noise walks it past π/4, the decisions then slip a quadrant and drive
//! it towards π/2, beyond the ±π/3 clamp: it pins at +8579 (Q2.13) and
//! every dibit comes out rotated. Nothing inside the chain brings it
//! back while the signal is present, so every later transmission on the
//! channel is lost until a retune pulses `traffic_lsm_reset`.
//!
//! The carrier drops between transmissions on one channel (a talker
//! change, the channel hang), so this hit whole exchanges. Bench
//! 2026-09-27 (Mode B replay, scene `B_20260503_084247_1695`): every
//! transmission after a carrier gap was lost, pll_dbg pinned at 8579; a
//! manual reset mid-transmission restored decoding within 0.4 s.
//!
//! While a call is followed this task resets the chain (cold start, the
//! seeds are zero) when:
//!   - the signal returns after at least `QUIET_ARM_MS` below the AGC
//!     idle gate (the PLL may have wandered while there was no signal),
//!   - or the PLL sits at the clamp for `PINNED_HOLD_MS` with signal.
//!
//! Change 059: P25 core 0.2.0 fixes this in the gateware: the LSM chains
//! hold their PLL and Gardner timing while the AGC gate reports no signal
//! and clamp the PLL at 0.65 rad, below π/4, so no clamp end traps. On that
//! gateware the task does not run: an onset reset would throw away the
//! held (correct) PLL value, and at the lower clamp "pinned" means a real
//! offset beyond ±497 Hz, which a reset cannot help.

#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;

#[cfg(target_os = "linux")]
use crate::httpd::AppState;

/// |pll_dbg| at or above this counts as pinned (the pre-0.2.0 clamp is
/// 8579, `core_version::PLL_CLAMP_Q213_LEGACY`).
pub const PINNED_Q13: i32 = 8000;
/// Pinned this long, with signal, before a reset.
pub const PINNED_HOLD_MS: u64 = 100;
/// Signal absent this long arms a reset at its return.
pub const QUIET_ARM_MS: u64 = 250;
/// Minimum spacing between resets.
pub const MIN_GAP_MS: u64 = 300;
/// Register poll period.
pub const POLL_MS: u64 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    Onset,
    Pinned,
}

#[derive(Debug, Default)]
pub struct PllWatchdog {
    pinned_since: Option<u64>,
    quiet_since: Option<u64>,
    last_reset: Option<u64>,
}

impl PllWatchdog {
    /// One sample. `active`: a call is followed and the chain enabled;
    /// `mag` / `gate`: the AGC input magnitude and idle gate (raw).
    pub fn step(&mut self, now_ms: u64, active: bool, pll_q13: i16, mag: u16, gate: u16)
        -> Option<ResetReason>
    {
        if !active {
            self.pinned_since = None;
            self.quiet_since = None;
            return None;
        }
        if mag < gate {
            self.quiet_since.get_or_insert(now_ms);
            self.pinned_since = None;
            return None;
        }
        let spaced = self.last_reset.map_or(true, |t| now_ms.saturating_sub(t) >= MIN_GAP_MS);
        if let Some(q) = self.quiet_since.take() {
            if now_ms.saturating_sub(q) >= QUIET_ARM_MS && spaced {
                return self.fire(now_ms, ResetReason::Onset);
            }
        }
        if (pll_q13 as i32).abs() >= PINNED_Q13 {
            let since = *self.pinned_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= PINNED_HOLD_MS && spaced {
                return self.fire(now_ms, ResetReason::Pinned);
            }
        } else {
            self.pinned_since = None;
        }
        None
    }

    fn fire(&mut self, now_ms: u64, why: ResetReason) -> Option<ResetReason> {
        self.last_reset = Some(now_ms);
        self.pinned_since = None;
        self.quiet_since = None;
        Some(why)
    }
}

/// Spawns the watchdog task (Linux: reads the traffic LSM registers), on
/// gateware without the LSM signal hold only.
#[cfg(target_os = "linux")]
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let ver = state.ip_core.lock().await.core_version();
        if ver.has_lsm_signal_hold() {
            tracing::info!(target: "p25_traffic",
                           "P25 core v{ver} holds the LSM PLL in gaps: PLL watchdog off");
            return;
        }
        state.imbe_forwarder.pll_wd_enabled.store(true, Ordering::Relaxed);
        tracing::info!(target: "p25_traffic",
                       "P25 core v{ver} has no LSM signal hold: PLL watchdog on");
        let mut wd = PllWatchdog::default();
        let t0 = std::time::Instant::now();
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(POLL_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let fwd = &state.imbe_forwarder;
            let active = fwd.current_talkgroup.load(Ordering::Relaxed) != 0
                && !fwd.traffic_paused_by_teardown.load(Ordering::Relaxed);
            let now_ms = t0.elapsed().as_millis() as u64;
            let why = {
                let core = state.ip_core.lock().await;
                if !active {
                    wd.step(now_ms, false, 0, 0, 0)
                } else {
                    let (pll, _) = core.traffic_lsm_debug();
                    let (_, mag) = core.traffic_lsm_agc_debug();
                    let gate = core.traffic_lsm_agc_threshold();
                    let why = wd.step(now_ms, true, pll, mag, gate);
                    if why.is_some() {
                        // Records the LsmReset epoch cut (054 IpCore hook).
                        core.pulse_traffic_lsm_reset();
                    }
                    why
                }
            };
            match why {
                Some(ResetReason::Onset) => {
                    fwd.pll_wd_resets_onset.fetch_add(1, Ordering::Relaxed);
                }
                Some(ResetReason::Pinned) => {
                    fwd.pll_wd_resets_pinned.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(target: "p25_traffic",
                                   "traffic PLL pinned at the clamp: reset");
                }
                None => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATE: u16 = 256;

    #[test]
    fn idle_or_inactive_never_resets() {
        let mut wd = PllWatchdog::default();
        for t in (0..2000).step_by(25) {
            assert_eq!(wd.step(t, false, 8579, 2000, GATE), None);
        }
        // Noise only (below the gate): the PLL may walk, nothing to do yet.
        for t in (2000..4000).step_by(25) {
            assert_eq!(wd.step(t, true, 8579, 20, GATE), None);
        }
    }

    #[test]
    fn signal_return_after_a_gap_resets_once() {
        let mut wd = PllWatchdog::default();
        // Locked signal, then a 400 ms carrier gap, then signal again.
        for t in (0..500).step_by(25) {
            assert_eq!(wd.step(t, true, 120, 1500, GATE), None);
        }
        for t in (500..900).step_by(25) {
            assert_eq!(wd.step(t, true, 7000, 10, GATE), None);
        }
        assert_eq!(wd.step(900, true, 7000, 1500, GATE), Some(ResetReason::Onset));
        for t in (925..1500).step_by(25) {
            assert_eq!(wd.step(t, true, 100, 1500, GATE), None);
        }
    }

    #[test]
    fn short_gap_keeps_the_lock() {
        let mut wd = PllWatchdog::default();
        wd.step(0, true, 100, 1500, GATE);
        for t in (25..200).step_by(25) {
            wd.step(t, true, 150, 10, GATE);
        }
        assert_eq!(wd.step(200, true, 150, 1500, GATE), None);
    }

    #[test]
    fn pinned_with_signal_resets_after_hold_and_spacing() {
        let mut wd = PllWatchdog::default();
        assert_eq!(wd.step(0, true, 8579, 1500, GATE), None);
        assert_eq!(wd.step(75, true, 8579, 1500, GATE), None);
        assert_eq!(wd.step(100, true, 8579, 1500, GATE), Some(ResetReason::Pinned));
        // Still pinned right after: spacing holds the next reset off.
        for t in (125..400).step_by(25) {
            assert_eq!(wd.step(t, true, -8579, 1500, GATE), None);
        }
        assert_eq!(wd.step(400, true, -8579, 1500, GATE), Some(ResetReason::Pinned));
    }

    #[test]
    fn legitimate_offset_below_the_threshold_is_left_alone() {
        let mut wd = PllWatchdog::default();
        for t in (0..3000).step_by(25) {
            // ≈ 700 Hz residual: large, but tracking.
            assert_eq!(wd.step(t, true, 7400, 1500, GATE), None);
        }
    }
}
