//! Dibit ring position tracking and the dibit production clock (pure, host-tested).
//!
//! The reader keeps an absolute byte position that never wraps. Register readings are ring
//! offsets (the base is ring-aligned), lifted onto the absolute coordinate by forward
//! continuity: `abs_new = abs_prev + (off_new - off_prev) mod ring`. That is exact while the
//! writer advances less than a lap (27.3 s) between readings; every case where it might not is
//! turned into an explicit resync instead of a silent lap slip: an advance larger than the
//! elapsed time allows, a reading gap near the lap budget, a changed ring base, a `last_buffer`
//! phase that disagrees, or undelivered bytes the writer is about to overwrite.
//!
//! Delivery hands out bytes up to the previous reading's safe end (`next - 256`): every burst
//! below it has had all its beats sent at least one poll earlier, so it has landed in DDR.
//!
//! `DibitClock` maps absolute dibit indices to monotonic time: each reading brackets the number
//! of dibits produced (`reading_interval`), and the brackets, carried forward at the nominal rate
//! with a drift allowance, narrow to a few dozen dibits. Pauses of the chain freeze the mapping.

use crate::hardware::p25core::rings::{
    phase_consistent, RingGeometry, RingSnapshot, BURST_BYTES, DIBITS_PER_BYTE, LEAD_BYTES,
    NOMINAL_BYTE_RATE_HZ, NOMINAL_DIBIT_RATE_HZ,
};

/// Dibits in a 64-bit word.
pub const DIBITS_PER_WORD: u64 = 32;
/// Dibits in a 128-byte burst.
pub const DIBITS_PER_BURST: u64 = BURST_BYTES * DIBITS_PER_BYTE;

/// Why the tracker re-anchored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncReason {
    /// Address advanced more than the elapsed time allows, or went
    /// backwards (shows up as an advance of almost one lap).
    Jump,
    /// Too long since the previous accepted reading: a lap may have
    /// been missed.
    Stall,
    /// The ring base changed (register glitch / different engine).
    BaseChanged,
    /// `last_buffer` disagrees with `next_address` about the lap phase.
    PhaseMismatch,
    /// The oldest undelivered byte is within one lap (minus 256 B) of
    /// the writer: it has been (or is about to be) overwritten. Only
    /// reachable after two consecutive long reader stalls.
    Overrun,
}

impl ResyncReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResyncReason::Jump => "jump",
            ResyncReason::Stall => "stall",
            ResyncReason::BaseChanged => "base_changed",
            ResyncReason::PhaseMismatch => "phase_mismatch",
            ResyncReason::Overrun => "overrun",
        }
    }
}

/// Details of a resync, for the event log and the counters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resync {
    pub reason: ResyncReason,
    /// Absolute byte position the reader jumped from.
    pub from_pos: u64,
    /// Absolute byte position the reader continues at.
    pub to_pos: u64,
    /// Raw forward delta (bytes, `< ring`) that triggered it.
    pub delta_bytes: u64,
    /// Seconds since the previous accepted reading.
    pub elapsed_secs: f64,
}

/// Result of one [`RingTracker::poll`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PollResult {
    /// Byte range `[start, end)` that may be copied now (empty when
    /// `start == end`). Always `None` when delivery was not requested.
    pub deliver: Option<(u64, u64)>,
    /// Absolute byte position of `next_address` in this reading (after
    /// any re-anchor). `None` when the reading was ignored because it
    /// came too soon after the previous one.
    pub abs_next: Option<u64>,
    /// Set when this reading triggered a re-anchor.
    pub resync: Option<Resync>,
    /// `last_buffer` phase check result for this reading.
    pub phase_ok: bool,
    /// True for the very first reading (start-up anchor).
    pub started: bool,
}

#[derive(Debug, Clone, Copy)]
struct Accepted {
    abs_next: u64,
    t_us: u64,
    base: u32,
    chain_enabled: bool,
}

/// Absolute-position tracker for one dibit ring. See the module doc.
#[derive(Debug, Clone)]
pub struct RingTracker {
    geom: RingGeometry,
    pos: u64,
    last: Option<Accepted>,
    /// Minimum age of the previous reading before its safe end may be
    /// delivered (landing margin for the final W beats).
    pub min_settle_us: u64,
    /// Fraction of the lap budget after which a gap between readings
    /// is treated as a possible lap slip.
    pub stall_fraction: f64,
    /// Plausibility slack: allowed advance = rate·elapsed·(1+rate_tol)
    /// + slack_bytes.
    pub rate_tol: f64,
    pub slack_bytes: u64,
    /// True while a phase mismatch has already caused one resync; the
    /// next mismatch is only counted (no resync loop).
    phase_mismatch_resynced: bool,
    pub phase_mismatches: u64,
    pub resyncs: u64,
    pub skipped_bytes: u64,
}

impl RingTracker {
    pub fn new(geom: RingGeometry) -> Self {
        RingTracker {
            geom,
            pos: 0,
            last: None,
            min_settle_us: 2_000,
            stall_fraction: 0.9,
            rate_tol: 0.25,
            slack_bytes: 4 * BURST_BYTES,
            phase_mismatch_resynced: false,
            phase_mismatches: 0,
            resyncs: 0,
            skipped_bytes: 0,
        }
    }

    #[cfg(test)]
    pub fn geometry(&self) -> RingGeometry {
        self.geom
    }

    #[cfg(test)]
    /// Absolute byte position of the next byte to deliver.
    pub fn pos(&self) -> u64 {
        self.pos
    }

    #[cfg(test)]
    /// Absolute next-address of the latest accepted reading.
    pub fn last_abs_next(&self) -> Option<u64> {
        self.last.map(|a| a.abs_next)
    }

    #[cfg(test)]
    /// Current safe end (`abs_next − 256`) of the latest accepted reading.
    pub fn last_safe_end(&self) -> Option<u64> {
        self.last.map(|a| a.abs_next.saturating_sub(LEAD_BYTES))
    }

    #[cfg(test)]
    pub fn started(&self) -> bool {
        self.last.is_some()
    }

    /// Maximum plausible forward advance for `elapsed_us` (bytes).
    fn max_advance(&self, elapsed_us: u64, running: bool) -> u64 {
        // `running` is true when either reading saw the chain enabled.
        // `slack_bytes` covers burst quantisation and a short enable
        // blip between two readings of a paused chain.
        let rate = if running { NOMINAL_BYTE_RATE_HZ } else { 0.0 };
        let secs = elapsed_us as f64 * 1e-6;
        (rate * secs * (1.0 + self.rate_tol)) as u64 + self.slack_bytes
    }

    /// Process one register reading. `deliver = false` keeps the
    /// position tracking (and `pos`) current without handing out data.
    pub fn poll(&mut self, snap: &RingSnapshot, deliver: bool) -> PollResult {
        let ring = self.geom.ring_bytes();
        let off = self.geom.offset_of(snap.next_address);
        let base = self.geom.base_of(snap.next_address);
        let phase_ok = phase_consistent(&self.geom, off, snap.last_buffer);
        if !phase_ok {
            self.phase_mismatches += 1;
        }

        let prev = match self.last {
            None => {
                // Start-up anchor.
                let abs_next = off + ring;
                self.last = Some(Accepted {
                    abs_next,
                    t_us: snap.t_us,
                    base,
                    chain_enabled: snap.chain_enabled,
                });
                self.pos = abs_next - LEAD_BYTES;
                return PollResult {
                    deliver: if deliver { Some((self.pos, self.pos)) } else { None },
                    abs_next: Some(abs_next),
                    resync: None,
                    phase_ok,
                    started: true,
                };
            }
            Some(p) => p,
        };

        let elapsed_us = snap.t_us.saturating_sub(prev.t_us);
        if elapsed_us < self.min_settle_us {
            // Too soon (e.g. IRQ wake right after a poll): keep the
            // previous reading as the pending safe end, deliver nothing.
            return PollResult {
                deliver: if deliver { Some((self.pos, self.pos)) } else { None },
                abs_next: None,
                resync: None,
                phase_ok,
                started: false,
            };
        }

        let prev_off = prev.abs_next % ring;
        let delta = (off + ring - prev_off) % ring;
        let running = prev.chain_enabled || snap.chain_enabled;
        let stall_limit_us =
            (self.geom.lap_budget_secs() * self.stall_fraction * 1e6) as u64;

        let reason = if base != prev.base {
            Some(ResyncReason::BaseChanged)
        } else if elapsed_us >= stall_limit_us {
            Some(ResyncReason::Stall)
        } else if delta > self.max_advance(elapsed_us, running) {
            Some(ResyncReason::Jump)
        } else if !phase_ok && !self.phase_mismatch_resynced {
            Some(ResyncReason::PhaseMismatch)
        } else {
            None
        };
        if phase_ok {
            self.phase_mismatch_resynced = false;
        }

        match reason {
            None => {
                let abs_next = prev.abs_next + delta;
                // Deliverable end = PREVIOUS reading's safe end.
                let end = prev.abs_next.saturating_sub(LEAD_BYTES);
                let mut start = self.pos;
                // Overrun guard: the writer (at most at `abs_next`, plus
                // one burst while we copy) overwrites byte x when it
                // reaches x + ring. Skip whatever is that close.
                let floor = (abs_next + LEAD_BYTES).saturating_sub(ring);
                let mut resync = None;
                if start < floor {
                    resync = Some(Resync {
                        reason: ResyncReason::Overrun,
                        from_pos: start,
                        to_pos: floor,
                        delta_bytes: delta,
                        elapsed_secs: elapsed_us as f64 * 1e-6,
                    });
                    self.skipped_bytes += floor - start;
                    self.resyncs += 1;
                    start = floor;
                }
                let end = end.max(start);
                self.pos = end;
                if !deliver {
                    // Tracking only: follow the current safe end.
                    self.pos = self.pos.max(abs_next.saturating_sub(LEAD_BYTES));
                }
                self.last = Some(Accepted {
                    abs_next,
                    t_us: snap.t_us,
                    base,
                    chain_enabled: snap.chain_enabled,
                });
                PollResult {
                    deliver: if deliver { Some((start, end)) } else { None },
                    abs_next: Some(abs_next),
                    resync,
                    phase_ok,
                    started: false,
                }
            }
            Some(reason) => {
                if reason == ResyncReason::PhaseMismatch {
                    self.phase_mismatch_resynced = true;
                }
                // Re-anchor: choose the lap that best matches the time
                // that passed (0 when the chain was paused).
                let expected = if running {
                    NOMINAL_BYTE_RATE_HZ * elapsed_us as f64 * 1e-6
                } else {
                    0.0
                };
                let laps = if expected > delta as f64 {
                    ((expected - delta as f64) / ring as f64).round() as u64
                } else {
                    0
                };
                let abs_next = prev.abs_next + delta + laps * ring;
                let from = self.pos;
                let to = abs_next.saturating_sub(LEAD_BYTES).max(from);
                self.skipped_bytes += to - from;
                self.pos = to;
                self.resyncs += 1;
                self.last = Some(Accepted {
                    abs_next,
                    t_us: snap.t_us,
                    base,
                    chain_enabled: snap.chain_enabled,
                });
                PollResult {
                    deliver: if deliver { Some((to, to)) } else { None },
                    abs_next: Some(abs_next),
                    resync: Some(Resync {
                        reason,
                        from_pos: from,
                        to_pos: to,
                        delta_bytes: delta,
                        elapsed_secs: elapsed_us as f64 * 1e-6,
                    }),
                    phase_ok,
                    started: false,
                }
            }
        }
    }

}

/// Production-rate model mapping absolute dibit indices to monotonic time.
#[derive(Debug, Clone)]
pub struct DibitClock {
    /// Dibits per microsecond (4800 / 1e6).
    rate_per_us: f64,
    /// Rate uncertainty used to widen the interval between readings.
    pub drift_ppm: f64,
    /// Extra tolerance added to every reading interval (dibits).
    pub margin_dibits: f64,
    state: Option<ClockState>,
    /// Frozen mappings of completed running periods (oldest first).
    frozen: std::collections::VecDeque<Period>,
    pub observations: u64,
    pub reseeds: u64,
}

#[derive(Debug, Clone, Copy)]
struct ClockState {
    t_ref_us: u64,
    lo: f64,
    hi: f64,
    running: bool,
    /// First dibit index of the current running period (−∞ for the
    /// initial period).
    period_start: f64,
}

/// A frozen running period: dibits `[d_start, next.d_start)` were
/// produced at `t_anchor + (d − d_anchor)/R`.
#[derive(Debug, Clone, Copy)]
struct Period {
    d_start: f64,
    d_anchor: f64,
    t_anchor_us: f64,
}

/// Cheap copyable view of the clock for per-word time lookups outside
/// the shared lock.
#[derive(Debug, Clone)]
pub struct ClockView {
    rate_per_us: f64,
    current: Option<(f64, f64, f64, bool, u64)>, // (period_start, d_anchor, t_anchor_us, running, t_ref)
    frozen: Vec<Period>,
}

/// Interval returned by [`DibitClock::index_at`].
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndexEstimate {
    pub lo: f64,
    pub mid: f64,
    pub hi: f64,
}

/// Observation interval implied by an absolute next-address reading.
pub fn reading_interval(abs_next: u64) -> (f64, f64) {
    let a = abs_next / BURST_BYTES;
    let lo = (a.saturating_sub(2) * DIBITS_PER_BURST) as f64;
    let hi = (a.saturating_sub(1) * DIBITS_PER_BURST + DIBITS_PER_WORD) as f64;
    (lo, hi)
}

impl DibitClock {
    pub fn new() -> Self {
        DibitClock {
            rate_per_us: NOMINAL_DIBIT_RATE_HZ * 1e-6,
            drift_ppm: 200.0,
            margin_dibits: 4.0,
            state: None,
            frozen: std::collections::VecDeque::new(),
            observations: 0,
            reseeds: 0,
        }
    }

    /// Project the stored interval to time `t_us` (no mutation).
    fn projected(&self, t_us: u64) -> Option<(f64, f64)> {
        let s = self.state?;
        if !s.running {
            return Some((s.lo, s.hi));
        }
        let dt = t_us as f64 - s.t_ref_us as f64;
        if dt >= 0.0 {
            let adv = self.rate_per_us * dt;
            let widen = adv * self.drift_ppm * 1e-6;
            Some((s.lo + adv - widen, s.hi + adv + widen))
        } else {
            // Query in the past of the reference: move back, never
            // before the start of the running period.
            let adv = self.rate_per_us * dt; // negative
            let widen = -adv * self.drift_ppm * 1e-6;
            let lo = (s.lo + adv - widen).max(s.period_start.min(s.lo));
            let hi = (s.hi + adv + widen).max(lo);
            Some((lo, hi))
        }
    }

    fn move_to(&mut self, t_us: u64) {
        if let Some((lo, hi)) = self.projected(t_us) {
            if let Some(s) = self.state.as_mut() {
                if t_us >= s.t_ref_us {
                    s.lo = lo;
                    s.hi = hi;
                    s.t_ref_us = t_us;
                }
            }
        }
    }

    /// Add one register reading (absolute next-address) taken at `t_us`.
    pub fn observe_next(&mut self, t_us: u64, abs_next: u64, running_hint: bool) {
        let (lo, hi) = reading_interval(abs_next);
        self.observe_interval(t_us, lo - self.margin_dibits, hi + self.margin_dibits, running_hint);
    }

    /// Add a constraint `D(t_us) ∈ [lo, hi]`. `running_hint` is the chain
    /// enable bit read with the reading; a change not already recorded by
    /// [`set_running`](Self::set_running) is applied at `t_us` (poll
    /// precision).
    pub fn observe_interval(&mut self, t_us: u64, lo: f64, hi: f64, running_hint: bool) {
        self.observations += 1;
        match self.state {
            None => {
                self.state = Some(ClockState {
                    t_ref_us: t_us,
                    lo,
                    hi,
                    running: running_hint,
                    period_start: f64::NEG_INFINITY,
                });
            }
            Some(s) => {
                if t_us < s.t_ref_us {
                    // Out-of-order reading (another task's snapshot that
                    // lost the race for the lock). Ignore — never move
                    // the reference backwards.
                    return;
                }
                if s.running != running_hint {
                    self.set_running(t_us, running_hint);
                }
                self.move_to(t_us);
                let s = self.state.as_mut().unwrap();
                let nlo = s.lo.max(lo);
                let nhi = s.hi.min(hi);
                if nlo <= nhi {
                    s.lo = nlo;
                    s.hi = nhi;
                } else {
                    // Inconsistent: trust the register.
                    s.lo = lo;
                    s.hi = hi;
                    self.reseeds += 1;
                }
            }
        }
    }

    /// Record a chain start/stop at `t_us` (`traffic_lsm_enable`).
    pub fn set_running(&mut self, t_us: u64, running: bool) {
        let Some(s) = self.state else {
            return;
        };
        if s.running == running {
            return;
        }
        self.move_to(t_us.max(s.t_ref_us));
        let s = self.state.as_mut().unwrap();
        let mid = 0.5 * (s.lo + s.hi);
        if !running {
            // Freeze the running period that just ended: dibits below
            // `mid` were produced by it.
            self.frozen.push_back(Period {
                d_start: s.period_start,
                d_anchor: mid,
                t_anchor_us: s.t_ref_us as f64,
            });
            while self.frozen.len() > 64 {
                self.frozen.pop_front();
            }
            s.running = false;
        } else {
            s.running = true;
        }
        // Dibits from `mid` on belong to the next running period.
        s.period_start = mid;
    }

    /// Estimated number of dibits produced by `t_us` (= index of the next
    /// dibit the HDL will produce).
    #[cfg(test)]
    pub fn index_at(&self, t_us: u64) -> Option<IndexEstimate> {
        let (lo, hi) = self.projected(t_us)?;
        Some(IndexEstimate { lo, mid: 0.5 * (lo + hi), hi })
    }

    /// Current interval width (dibits).
    #[cfg(test)]
    pub fn uncertainty_dibits(&self) -> Option<f64> {
        self.state.map(|s| s.hi - s.lo)
    }

    pub fn view(&self) -> ClockView {
        ClockView {
            rate_per_us: self.rate_per_us,
            current: self.state.map(|s| {
                (s.period_start, 0.5 * (s.lo + s.hi), s.t_ref_us as f64, s.running, s.t_ref_us)
            }),
            frozen: self.frozen.iter().copied().collect(),
        }
    }
}

impl Default for DibitClock {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockView {
    /// PS monotonic time (µs, may be fractional) at which dibit `index`
    /// was produced. For a paused chain, dibits at/after the freeze point
    /// have not been produced; they map to the pause reference time.
    pub fn time_of(&self, index: u64) -> Option<f64> {
        let (period_start, d_anchor, t_anchor, running, t_ref) = self.current?;
        let d = index as f64;
        if d >= period_start || self.frozen.is_empty() {
            if !running {
                // Paused: nothing at/after the freeze point exists yet.
                // (Before any running period was seen, map linearly.)
                if self.frozen.is_empty() && period_start == f64::NEG_INFINITY {
                    return Some(t_anchor + (d - d_anchor) / self.rate_per_us);
                }
                return Some(t_ref as f64);
            }
            return Some(t_anchor + (d - d_anchor) / self.rate_per_us);
        }
        // Older period: latest frozen period whose start ≤ d.
        let p = self
            .frozen
            .iter()
            .rev()
            .find(|p| p.d_start <= d)
            .or_else(|| self.frozen.first())?;
        Some(p.t_anchor_us + (d - p.d_anchor) / self.rate_per_us)
    }
}

#[cfg(test)]
#[path = "dibit_ring_sim.rs"]
pub(crate) mod sim;

#[cfg(test)]
#[path = "dibit_ring_tests.rs"]
mod tests;
