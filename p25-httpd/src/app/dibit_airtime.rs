//! Air-time attribution of traffic dibits + dibit delivery
//! instrumentation (change 054, 2026-09-26). Portable, host-tested.
//!
//! The dibit readers (`app::dibit_readers`, Linux) deliver dibits
//! ~50–200 ms after the HDL produced them (see
//! `hardware::dibit_ring`). Everything that changes what the traffic
//! chain's dibits *mean* — retune / NCO write, LSM reset, pause/resume,
//! framer reset, TG change, CallOpen, CallClose — happens in real time
//! in other tasks. This module records those actions as **chain epoch
//! cuts** on the absolute dibit index axis, so the traffic reader can
//! split each delivered chunk at the cuts and decode every piece under
//! the context that was in effect when those dibits were on the air.
//!
//! ## Epoch model
//!
//! - Every dibit has an absolute index (`4 × absolute ring byte
//!   position`, monotonic since the reader started; see
//!   `hardware::dibit_ring`).
//! - At action time `t_a` the recorder computes the **production cut
//!   index** = index of the next dibit the HDL will produce, from the
//!   production clock (`DibitClock`, fed by the reader's next-address
//!   readings). Hardware actions recorded from `IpCore` also feed the
//!   next-address register read *at the action* (under the IpCore
//!   lock), lifted onto the absolute axis with the reader's latest
//!   position, as an extra constraint.
//! - A cut is never placed below `claimed_end` (the end of what the
//!   reader has already taken for processing), so a cut can never be
//!   "in the past" of the decoder. Because the reader only ever claims
//!   up to the previous poll's safe end (≥ one burst behind
//!   production), the clamp only engages when the clock estimate is off
//!   by more than that margin; `clamped` counts it.
//! - Hardware discontinuities (retune, NCO write, LSM reset, resume
//!   after pause) are placed at the LOW end of the estimate interval and
//!   discard `(hi − lo) + settle_dibits` dibits after it (FIR flush +
//!   estimate uncertainty), with a framer reset at the cut. Software
//!   context changes are placed at the interval midpoint without discard.
//! - A context snapshot (TG, source, call_id, encrypted, freq) is taken
//!   from the forwarder's live atomics when the action is recorded.
//!   Frames are attributed by the index at which they COMPLETE: a frame
//!   straddling a pure context cut belongs to the new context (a new
//!   call's first HDU that began just before the grant was processed is
//!   kept); hardware cuts reset the framer so nothing straddles them.
//! - Gating: a segment whose context has `tg == 0` (no call) is not fed
//!   to the framer; every gate flip resets the framer, so a partial
//!   frame of one call is never completed with another call's dibits.
//!   A CallClose therefore closes the gate at its cut: the closing
//!   call's in-flight dibits (produced before the close, delivered
//!   after it) are still decoded under the closing call.
//!
//! ## Production timestamps
//!
//! `ClockView::time_of(index)` gives the PS monotonic production time;
//! [`unix_ms_of`] converts to wall-clock ms with the offset measured at
//! conversion time. Error bound: see `hardware::dibit_ring` (half the
//! reported `uncertainty_dibits` / 4800 s, plus drift, plus the fixed
//! HDL pipeline delay). IMBE batches carry this air time as
//! `captured_at_ms`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;

use serde::Serialize;

pub use crate::hardware::dibit_ring::{mono_us, HwAction};
#[cfg(test)]
use crate::hardware::dibit_ring::IndexEstimate;
use crate::hardware::dibit_ring::{
    lift_next_address, ChainEpochSink, ClockView, DibitClock, RingGeometry, DIBITS_PER_BYTE,
    NOMINAL_DIBIT_RATE_HZ,
};

// ── Time helpers ─────────────────────────────────────────────────────

/// Wall-clock unix milliseconds.
pub fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `unix_us − mono_us` right now. Re-measured at every conversion so an
/// NTP / `/api/set_time` step is picked up.
pub fn unix_offset_us() -> i64 {
    let unix_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0);
    unix_us - mono_us() as i64
}

/// Convert a PS monotonic time (µs) to unix ms with a given offset.
pub fn unix_ms_of(mono_us: f64, offset_us: i64) -> u64 {
    let v = mono_us + offset_us as f64;
    if v <= 0.0 {
        0
    } else {
        (v / 1000.0) as u64
    }
}

// ── Modes ────────────────────────────────────────────────────────────

/// Dibit delivery mode (runtime switch, `--dibit-delivery` /
/// `POST /api/dibit_delivery?mode=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// Pre-054: whole 4 KiB sub-buffers on IRQ; traffic gating on the
    /// live TG sampled once per batch; framer resets in real time.
    Legacy,
    /// Position polling (low latency) with the legacy live gating.
    Poll,
    /// Position polling + air-time epoch attribution (traffic ring).
    /// Same as `Poll` on the control ring.
    Airtime,
}

impl DeliveryMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "legacy" | "irq" | "subbuffer" | "old" => Some(DeliveryMode::Legacy),
            "poll" | "lowlatency" | "low_latency" => Some(DeliveryMode::Poll),
            "airtime" | "air_time" | "epoch" | "default" => Some(DeliveryMode::Airtime),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            DeliveryMode::Legacy => "legacy",
            DeliveryMode::Poll => "poll",
            DeliveryMode::Airtime => "airtime",
        }
    }
    fn to_u8(self) -> u8 {
        match self {
            DeliveryMode::Legacy => 0,
            DeliveryMode::Poll => 1,
            DeliveryMode::Airtime => 2,
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            0 => DeliveryMode::Legacy,
            1 => DeliveryMode::Poll,
            _ => DeliveryMode::Airtime,
        }
    }
}

// ── Epoch types ──────────────────────────────────────────────────────

/// Decode context of a traffic segment (snapshot of the forwarder's
/// live atomics at the action that opened the epoch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct SegmentContext {
    pub tg: u16,
    pub source: u32,
    pub call_id: u64,
    pub encrypted: bool,
    pub freq_hz: u64,
}

impl SegmentContext {
    pub fn gated(&self) -> bool {
        self.tg == 0
    }
}

/// What opened an epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EpochKind {
    /// NCO write (+ optional LSM reset, + enable) via `retune_traffic_chain`.
    Retune,
    /// Bare NCO write / DDC reconfigure.
    NcoWrite,
    /// Bare `traffic_lsm_reset` pulse.
    LsmReset,
    /// `traffic_lsm_enable` 1 → 0.
    Pause,
    /// `traffic_lsm_enable` 0 → 1.
    Resume,
    /// Grant refresh changed source / encryption of the active call.
    CtxUpdate,
    /// Follower accepted a grant that needs a retune: the gate closes
    /// until the retune's hardware cut + new context, so old-frequency
    /// dibits never carry the new call's labels.
    GrantHold,
    /// Follower moved to a new TG (or released it: TG → 0).
    TgChange,
    /// Lifecycle assigned a new call_id.
    CallOpen,
    /// Follower released the chain on CallClose.
    CallClose,
}

impl EpochKind {
    pub fn is_discontinuity(&self) -> bool {
        matches!(
            self,
            EpochKind::Retune | EpochKind::NcoWrite | EpochKind::LsmReset | EpochKind::Resume
        )
    }
}

/// One recorded cut.
#[derive(Debug, Clone, PartialEq)]
pub struct EpochCut {
    pub seq: u64,
    /// Absolute dibit index of the first dibit of the new epoch.
    pub index: u64,
    /// Clock estimate interval at record time (diagnostic).
    pub est_lo: u64,
    pub est_hi: u64,
    pub kind: EpochKind,
    pub framer_reset: bool,
    /// Dibits after `index` to drop as not-yet-settled.
    pub discard_dibits: u64,
    /// New context (`None` = hardware-only cut, context unchanged).
    pub ctx: Option<SegmentContext>,
    /// Only `ctx.call_id` applies (lifecycle `CallOpen`: the call id is
    /// assigned asynchronously to the follower's TG / retune actions,
    /// so its snapshot of the other fields may be stale).
    pub call_id_only: bool,
    pub recorded_unix_ms: u64,
    pub recorded_mono_us: u64,
    /// True when the estimate was below `claimed_end` and got raised.
    pub clamped: bool,
    /// True when the index came with a register reading at the action.
    pub hw_reading: bool,
}

// ── Chunk planning (pure) ────────────────────────────────────────────

/// Traffic reader state carried across chunks.
#[derive(Debug, Clone, PartialEq)]
pub struct AirtimeState {
    pub ctx: SegmentContext,
    /// Absolute dibit index below which dibits are pre-settle garbage.
    pub discard_until: u64,
}

impl AirtimeState {
    pub fn new(ctx: SegmentContext) -> Self {
        AirtimeState { ctx, discard_until: 0 }
    }
}

/// One processing step for a delivered chunk.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Feed dibits `[start, end)` to the framer under `ctx`.
    Feed { start: u64, end: u64, ctx: SegmentContext },
    /// Dibits `[start, end)` belong to no call (TG 0): not decoded.
    Gate { start: u64, end: u64 },
    /// Dibits `[start, end)` precede a discontinuity's settle point.
    Discard { start: u64, end: u64 },
    /// Reset the framer before processing dibit `at`.
    ResetFramer { at: u64 },
    /// A cut was applied at `at` (`inside` = strictly inside the chunk).
    Applied { at: u64, inside: bool, cut: EpochCut },
}

/// Split the chunk `[start, end)` (absolute dibit indices) at `cuts`
/// (sorted by index, then seq) and produce the processing steps.
/// Updates `state` to the context in effect at `end`.
pub fn plan_chunk(state: &mut AirtimeState, start: u64, end: u64, cuts: &[EpochCut]) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut cur = start;
    for cut in cuts {
        let at = cut.index.clamp(start, end.max(start));
        emit_range(state, cur, at, &mut steps);
        cur = cur.max(at);
        let was_gated = state.ctx.gated();
        if let Some(mut new) = cut.ctx {
            if cut.call_id_only {
                state.ctx.call_id = new.call_id;
            } else {
                // Encryption is sticky within one call: an in-band HDU /
                // LDU2 latch must survive a same-call refresh cut.
                if new.call_id != 0 && new.call_id == state.ctx.call_id {
                    new.encrypted |= state.ctx.encrypted;
                }
                state.ctx = new;
            }
        }
        let now_gated = state.ctx.gated();
        if cut.discard_dibits > 0 {
            state.discard_until = state.discard_until.max(cut.index + cut.discard_dibits);
        }
        steps.push(Step::Applied {
            at,
            inside: at > start && at < end,
            cut: cut.clone(),
        });
        if cut.framer_reset || was_gated != now_gated {
            steps.push(Step::ResetFramer { at });
        }
    }
    emit_range(state, cur, end, &mut steps);
    steps
}

fn emit_range(state: &AirtimeState, a: u64, b: u64, steps: &mut Vec<Step>) {
    if a >= b {
        return;
    }
    if state.ctx.gated() {
        steps.push(Step::Gate { start: a, end: b });
        return;
    }
    let du = state.discard_until;
    if a < du {
        let e = b.min(du);
        steps.push(Step::Discard { start: a, end: e });
        if e < b {
            steps.push(Step::Feed { start: e, end: b, ctx: state.ctx });
        }
    } else {
        steps.push(Step::Feed { start: a, end: b, ctx: state.ctx });
    }
}

// ── Age histogram ────────────────────────────────────────────────────

/// Bin upper edges (ms) of the delivery-age histogram. The last bin is
/// open-ended.
pub const AGE_EDGES_MS: &[f64] = &[
    10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0, 110.0, 120.0, 130.0, 140.0,
    150.0, 175.0, 200.0, 250.0, 300.0, 400.0, 500.0, 750.0, 1000.0, 1500.0, 2000.0, 2500.0,
    3000.0, 3500.0, 4000.0, 5000.0, 7500.0, 10000.0,
];

/// Weighted histogram of dibit age at delivery (poll time − estimated
/// production time), in ms.
#[derive(Debug, Clone, Serialize)]
pub struct AgeHistogram {
    pub counts: Vec<u64>,
    pub total: u64,
    pub sum_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
}

impl AgeHistogram {
    pub fn new() -> Self {
        AgeHistogram {
            counts: vec![0; AGE_EDGES_MS.len() + 1],
            total: 0,
            sum_ms: 0.0,
            min_ms: f64::INFINITY,
            max_ms: 0.0,
        }
    }

    pub fn record(&mut self, age_ms: f64, weight: u64) {
        if weight == 0 || !age_ms.is_finite() {
            return;
        }
        let a = age_ms.max(0.0);
        let bin = AGE_EDGES_MS.iter().position(|&e| a < e).unwrap_or(AGE_EDGES_MS.len());
        self.counts[bin] += weight;
        self.total += weight;
        self.sum_ms += a * weight as f64;
        self.min_ms = self.min_ms.min(a);
        self.max_ms = self.max_ms.max(a);
    }

    pub fn mean_ms(&self) -> Option<f64> {
        if self.total == 0 {
            None
        } else {
            Some(self.sum_ms / self.total as f64)
        }
    }

    /// Percentile `p` ∈ [0, 1], linearly interpolated inside the bin,
    /// clamped to the observed min/max.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.total == 0 {
            return None;
        }
        let target = (p.clamp(0.0, 1.0) * self.total as f64).max(1.0);
        let mut cum = 0.0;
        for (i, &c) in self.counts.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let next = cum + c as f64;
            if next >= target {
                let lo = if i == 0 { 0.0 } else { AGE_EDGES_MS[i - 1] };
                let hi = if i < AGE_EDGES_MS.len() { AGE_EDGES_MS[i] } else { self.max_ms };
                let frac = ((target - cum) / c as f64).clamp(0.0, 1.0);
                let v = lo + frac * (hi - lo);
                return Some(v.clamp(self.min_ms, self.max_ms));
            }
            cum = next;
        }
        Some(self.max_ms)
    }

    pub fn summary(&self) -> serde_json::Value {
        let r = |v: Option<f64>| v.map(|x| (x * 10.0).round() / 10.0);
        serde_json::json!({
            "samples_dibits": self.total,
            "mean_ms": r(self.mean_ms()),
            "min_ms":  if self.total == 0 { None } else { r(Some(self.min_ms)) },
            "p50_ms":  r(self.percentile(0.50)),
            "p90_ms":  r(self.percentile(0.90)),
            "p99_ms":  r(self.percentile(0.99)),
            "max_ms":  if self.total == 0 { None } else { r(Some(self.max_ms)) },
            "bin_edges_ms": AGE_EDGES_MS,
            "bin_counts": self.counts,
        })
    }
}

impl Default for AgeHistogram {
    fn default() -> Self {
        Self::new()
    }
}

// ── Shared per-ring state ────────────────────────────────────────────

/// Counters published by the reader + recorder paths.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RingCounters {
    pub polls: u64,
    pub polls_with_data: u64,
    pub irq_wakes: u64,
    pub legacy_batches: u64,
    pub bytes_delivered: u64,
    pub dibits_delivered: u64,
    pub resyncs: u64,
    pub resync_skipped_bytes: u64,
    pub phase_mismatches: u64,
    pub copy_errors: u64,
    pub cuts_recorded: u64,
    pub cuts_applied: u64,
    pub epoch_splits: u64,
    pub cuts_clamped: u64,
    /// Cuts raised to the previous cut's index to keep record order.
    pub cuts_reordered: u64,
    pub cuts_dropped_mode: u64,
    pub framer_resets: u64,
    pub dibits_fed: u64,
    pub dibits_gated: u64,
    pub dibits_discarded_presettle: u64,
    pub mode_switches: u64,
}

/// Applied-cut record for `/api/dibit_delivery`.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedCutInfo {
    pub seq: u64,
    pub kind: EpochKind,
    pub index: u64,
    pub est_width_dibits: u64,
    pub discard_dibits: u64,
    pub framer_reset: bool,
    pub inside_chunk: bool,
    pub clamped: bool,
    pub hw_reading: bool,
    pub tg: Option<u16>,
    pub call_id: Option<u64>,
    pub encrypted: Option<bool>,
    pub recorded_unix_ms: u64,
    /// Estimated production (air) time of the cut index.
    pub cut_air_unix_ms: Option<u64>,
    /// ms between recording and the reader applying it.
    pub apply_delay_ms: u64,
}

struct RingInner {
    geom: RingGeometry,
    clock: DibitClock,
    /// Latest accepted reading `(abs_next, t_us)` from the reader.
    reference: Option<(u64, u64)>,
    claimed_end: u64,
    pending: Vec<EpochCut>,
    next_seq: u64,
    /// Index of the last recorded cut: later cuts never go below it.
    last_cut_index: u64,
    counters: RingCounters,
    ages: AgeHistogram,
    /// Delivered-chunk backlog: production index estimate − delivered end
    /// at the last poll (dibits).
    last_backlog_dibits: Option<f64>,
    recent_cuts: VecDeque<AppliedCutInfo>,
    last_resync: Option<serde_json::Value>,
    started_unix_ms: u64,
}

/// Shared state for one dibit ring.
pub struct DibitRingShared {
    pub label: &'static str,
    requested_mode: AtomicU8,
    active_mode: AtomicU8,
    /// Post-discontinuity settle window (dibits).
    pub settle_dibits: AtomicU32,
    inner: Mutex<RingInner>,
}

impl DibitRingShared {
    pub fn new(label: &'static str, geom: RingGeometry, mode: DeliveryMode) -> Self {
        DibitRingShared {
            label,
            requested_mode: AtomicU8::new(mode.to_u8()),
            active_mode: AtomicU8::new(DeliveryMode::Legacy.to_u8()),
            settle_dibits: AtomicU32::new(DEFAULT_SETTLE_DIBITS),
            inner: Mutex::new(RingInner {
                geom,
                clock: DibitClock::new(),
                reference: None,
                claimed_end: 0,
                pending: Vec::new(),
                next_seq: 1,
                last_cut_index: 0,
                counters: RingCounters::default(),
                ages: AgeHistogram::new(),
                last_backlog_dibits: None,
                recent_cuts: VecDeque::with_capacity(RECENT_CUTS),
                last_resync: None,
                started_unix_ms: unix_ms_now(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RingInner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    pub fn requested_mode(&self) -> DeliveryMode {
        DeliveryMode::from_u8(self.requested_mode.load(Ordering::Relaxed))
    }
    pub fn set_requested_mode(&self, m: DeliveryMode) {
        self.requested_mode.store(m.to_u8(), Ordering::Relaxed);
    }
    /// Mode the reader is actually running (set by the reader after a
    /// hand-over). Epoch recording keys off this.
    pub fn active_mode(&self) -> DeliveryMode {
        DeliveryMode::from_u8(self.active_mode.load(Ordering::Relaxed))
    }
    pub fn epochs_active(&self) -> bool {
        self.active_mode() == DeliveryMode::Airtime
    }

    /// Reader hand-over: publish the new mode, drop pending cuts and set
    /// the claim point to the reader's position (absolute dibit index).
    pub fn activate_mode(&self, m: DeliveryMode, claim_from: u64) {
        let mut g = self.lock();
        g.pending.clear();
        g.claimed_end = claim_from;
        g.last_cut_index = 0;
        g.counters.mode_switches += 1;
        self.active_mode.store(m.to_u8(), Ordering::Relaxed);
    }

    /// Reader: accepted register reading.
    pub fn observe(&self, t_us: u64, abs_next: u64, running: bool) {
        let mut g = self.lock();
        g.reference = Some((abs_next, t_us));
        g.clock.observe_next(t_us, abs_next, running);
    }

    /// Reader: take every pending cut with `index < end` (dibit index).
    /// Cuts recorded afterwards are clamped to `≥ end`.
    pub fn claim(&self, end: u64) -> Vec<EpochCut> {
        let mut g = self.lock();
        if end > g.claimed_end {
            g.claimed_end = end;
        }
        let split = g.pending.partition_point(|c| c.index < end);
        g.pending.drain(..split).collect()
    }

    /// Clock view for per-word time lookups outside the lock.
    pub fn clock_view(&self) -> ClockView {
        self.lock().clock.view()
    }

    /// Current production-index estimate (dibits) at `t_us`.
    #[cfg(test)]
    pub fn index_estimate(&self, t_us: u64) -> Option<IndexEstimate> {
        self.lock().clock.index_at(t_us)
    }

    /// Queue a cut. `cut.index` is the raw estimate; it is raised to
    /// `claimed_end` when the reader already took those dibits.
    /// `discard_until` (absolute) is preserved across the raise.
    fn push_cut(g: &mut RingInner, mut cut: EpochCut, discard_until: Option<u64>) {
        if cut.index < g.claimed_end {
            cut.index = g.claimed_end;
            cut.clamped = true;
            g.counters.cuts_clamped += 1;
        }
        // Cuts are recorded in program order (all callers stamp "now"),
        // so a later cut cannot have happened earlier on air. Estimate
        // jitter can still invert them: a hardware cut sits at the low
        // edge, a software cut at the midpoint, and an LSM reset widens
        // the estimate. Bench 2026-09-26: a grant hold (TG 0) recorded
        // before the retune / TG change / CallOpen of the new call got
        // an index 2 dibits later than theirs, so the gate closed after
        // the new call opened and its whole first transmission was
        // gated (1 call in ~150).
        if cut.index < g.last_cut_index {
            cut.index = g.last_cut_index;
            g.counters.cuts_reordered += 1;
        }
        g.last_cut_index = cut.index;
        if let Some(du) = discard_until {
            cut.discard_dibits = du.saturating_sub(cut.index);
        }
        cut.seq = g.next_seq;
        g.next_seq += 1;
        let pos = g.pending.partition_point(|c| (c.index, c.seq) <= (cut.index, cut.seq));
        g.pending.insert(pos, cut);
        g.counters.cuts_recorded += 1;
        if g.pending.len() > MAX_PENDING_CUTS {
            // Reader not consuming (should not happen in airtime mode):
            // keep the newest.
            let excess = g.pending.len() - MAX_PENDING_CUTS;
            g.pending.drain(..excess);
        }
    }

    /// Record a hardware chain-control action (called from `IpCore`
    /// under the IpCore lock, with the next-address register read right
    /// after the action). Updates the production clock in every mode;
    /// pushes a cut only in airtime mode.
    pub fn record_hw(&self, action: HwAction, t_us: u64, next_address: u32, enabled_before: bool) {
        let settle = self.settle_dibits.load(Ordering::Relaxed) as u64;
        let epochs = self.epochs_active();
        let mut g = self.lock();
        let geom = g.geom;
        let lifted = lift_next_address(&geom, g.reference, next_address, t_us);
        if let Some(abs) = lifted {
            // The reading describes the state right after the write; the
            // dibit count itself does not jump at the write.
            g.clock.observe_next(t_us, abs, enabled_before);
        }
        let (kind, enabled_after) = match action {
            HwAction::Retune { lsm_reset } => {
                if lsm_reset {
                    g.clock.perturb(t_us, 8.0);
                }
                (EpochKind::Retune, true)
            }
            HwAction::NcoWrite => (EpochKind::NcoWrite, enabled_before),
            HwAction::LsmReset => {
                g.clock.perturb(t_us, 8.0);
                (EpochKind::LsmReset, enabled_before)
            }
            HwAction::Enable(true) => (EpochKind::Resume, true),
            HwAction::Enable(false) => (EpochKind::Pause, false),
        };
        // A write that does not change the enable state is not an epoch.
        if matches!(action, HwAction::Enable(_)) && enabled_after == enabled_before {
            return;
        }
        let est = g.clock.index_at(t_us);
        if enabled_after != enabled_before {
            g.clock.set_running(t_us, enabled_after);
        }
        if !epochs {
            g.counters.cuts_dropped_mode += 1;
            return;
        }
        let claimed = g.claimed_end;
        let (lo, mid, hi) = match est {
            Some(e) => (e.lo.max(0.0) as u64, e.mid.max(0.0) as u64, e.hi.max(0.0).ceil() as u64),
            None => (claimed, claimed, claimed),
        };
        let resumed = enabled_after && !enabled_before;
        let discontinuity = kind.is_discontinuity() || resumed;
        // Discontinuity: cut at the LOW edge, discard through the HIGH
        // edge plus the settle window (absolute, survives clamping).
        let (index, discard_until) = if discontinuity {
            (lo, Some(hi.max(claimed) + settle))
        } else {
            (mid, None)
        };
        let cut = EpochCut {
            seq: 0,
            index,
            est_lo: lo,
            est_hi: hi,
            kind,
            framer_reset: discontinuity,
            discard_dibits: 0,
            ctx: None,
            call_id_only: false,
            recorded_unix_ms: unix_ms_now(),
            recorded_mono_us: t_us,
            clamped: false,
            hw_reading: lifted.is_some(),
        };
        Self::push_cut(&mut g, cut, discard_until);
    }

    /// Record a software context change (`ctx` = live snapshot after the
    /// change). No-op unless the ring runs in airtime mode.
    pub fn record_sw(&self, kind: EpochKind, ctx: SegmentContext, framer_reset: bool) {
        self.record_sw_at(kind, ctx, framer_reset, mono_us());
    }

    /// [`record_sw`](Self::record_sw) with an explicit action time.
    pub fn record_sw_at(&self, kind: EpochKind, ctx: SegmentContext, framer_reset: bool, t_us: u64) {
        self.record_ctx(kind, ctx, framer_reset, false, t_us);
    }

    /// Record a call_id change only (lifecycle `CallOpen`).
    pub fn record_call_id_at(&self, call_id: u64, t_us: u64) {
        let ctx = SegmentContext { call_id, ..SegmentContext::default() };
        self.record_ctx(EpochKind::CallOpen, ctx, false, true, t_us);
    }

    fn record_ctx(
        &self,
        kind: EpochKind,
        ctx: SegmentContext,
        framer_reset: bool,
        call_id_only: bool,
        t_us: u64,
    ) {
        if !self.epochs_active() {
            return;
        }
        let mut g = self.lock();
        let claimed = g.claimed_end;
        let est = g.clock.index_at(t_us);
        let (lo, mid, hi) = match est {
            Some(e) => (e.lo.max(0.0) as u64, e.mid.max(0.0).round() as u64, e.hi.max(0.0).ceil() as u64),
            None => (claimed, claimed, claimed),
        };
        let cut = EpochCut {
            seq: 0,
            index: mid,
            est_lo: lo,
            est_hi: hi,
            kind,
            framer_reset,
            discard_dibits: 0,
            ctx: Some(ctx),
            call_id_only,
            recorded_unix_ms: unix_ms_now(),
            recorded_mono_us: t_us,
            clamped: false,
            hw_reading: false,
        };
        Self::push_cut(&mut g, cut, None);
    }

    /// Reader: a resync happened; publish it.
    pub fn note_resync(&self, info: serde_json::Value, skipped_bytes: u64) {
        let mut g = self.lock();
        g.counters.resyncs += 1;
        g.counters.resync_skipped_bytes += skipped_bytes;
        g.last_cut_index = 0;
        g.last_resync = Some(info);
    }

    /// Reader: per-poll bookkeeping.
    pub fn note_poll(&self, irq_wake: bool, phase_ok: bool) {
        let mut g = self.lock();
        g.counters.polls += 1;
        if irq_wake {
            g.counters.irq_wakes += 1;
        }
        if !phase_ok {
            g.counters.phase_mismatches += 1;
        }
    }

    pub fn note_copy_error(&self) {
        self.lock().counters.copy_errors += 1;
    }

    /// Reader: dibits `[start, end)` delivered at `now_us`. Records the
    /// age of every 32-dibit word (weighted) and the backlog.
    pub fn record_delivery(&self, now_us: u64, start: u64, end: u64, legacy: bool) {
        if end <= start {
            return;
        }
        let mut g = self.lock();
        g.counters.bytes_delivered += (end - start) / DIBITS_PER_BYTE;
        g.counters.dibits_delivered += end - start;
        if legacy {
            g.counters.legacy_batches += 1;
        } else {
            g.counters.polls_with_data += 1;
        }
        let view = g.clock.view();
        let mut i = start;
        while i < end {
            let w_end = (i + 32).min(end);
            if let Some(t) = view.time_of(w_end - 1) {
                let age_ms = (now_us as f64 - t) / 1000.0;
                g.ages.record(age_ms, w_end - i);
            }
            i = w_end;
        }
        if let Some(est) = g.clock.index_at(now_us) {
            g.last_backlog_dibits = Some(est.mid - end as f64);
        }
    }

    /// Reader: steps executed for one chunk.
    pub fn record_steps(&self, steps: &[Step], applied_unix_ms: u64, view: &ClockView, offset_us: i64) {
        let mut g = self.lock();
        for s in steps {
            match s {
                Step::Feed { start, end, .. } => g.counters.dibits_fed += end - start,
                Step::Gate { start, end } => g.counters.dibits_gated += end - start,
                Step::Discard { start, end } => {
                    g.counters.dibits_discarded_presettle += end - start
                }
                Step::ResetFramer { .. } => g.counters.framer_resets += 1,
                Step::Applied { inside, cut, .. } => {
                    g.counters.cuts_applied += 1;
                    if *inside {
                        g.counters.epoch_splits += 1;
                    }
                    let info = AppliedCutInfo {
                        seq: cut.seq,
                        kind: cut.kind,
                        index: cut.index,
                        est_width_dibits: cut.est_hi.saturating_sub(cut.est_lo),
                        discard_dibits: cut.discard_dibits,
                        framer_reset: cut.framer_reset,
                        inside_chunk: *inside,
                        clamped: cut.clamped,
                        hw_reading: cut.hw_reading,
                        tg: cut.ctx.filter(|_| !cut.call_id_only).map(|c| c.tg),
                        call_id: cut.ctx.map(|c| c.call_id),
                        encrypted: cut.ctx.filter(|_| !cut.call_id_only).map(|c| c.encrypted),
                        recorded_unix_ms: cut.recorded_unix_ms,
                        cut_air_unix_ms: view.time_of(cut.index).map(|t| unix_ms_of(t, offset_us)),
                        apply_delay_ms: applied_unix_ms.saturating_sub(cut.recorded_unix_ms),
                    };
                    if g.recent_cuts.len() >= RECENT_CUTS {
                        g.recent_cuts.pop_front();
                    }
                    g.recent_cuts.push_back(info);
                }
            }
        }
    }

    /// Reader: dibits fed / gated outside the epoch planner (poll mode).
    pub fn note_fed(&self, fed: u64, gated: u64) {
        let mut g = self.lock();
        g.counters.dibits_fed += fed;
        g.counters.dibits_gated += gated;
    }

    pub fn note_framer_reset(&self) {
        self.lock().counters.framer_resets += 1;
    }

    pub fn reset_stats(&self) {
        let mut g = self.lock();
        g.counters = RingCounters::default();
        g.ages = AgeHistogram::new();
        g.recent_cuts.clear();
        g.last_resync = None;
        g.started_unix_ms = unix_ms_now();
    }

    /// Age summary only (for periodic EventLog lines).
    pub fn age_summary(&self) -> serde_json::Value {
        self.lock().ages.summary()
    }

    pub fn counters(&self) -> RingCounters {
        self.lock().counters.clone()
    }

    /// JSON for `/api/dibit_delivery`.
    pub fn status_json(&self, with_cuts: bool) -> serde_json::Value {
        let now = mono_us();
        let g = self.lock();
        let est = g.clock.index_at(now);
        let mut v = serde_json::json!({
            "ring": self.label,
            "requested_mode": self.requested_mode().as_str(),
            "active_mode": self.active_mode().as_str(),
            "settle_dibits": self.settle_dibits.load(Ordering::Relaxed),
            "stats_since_unix_ms": g.started_unix_ms,
            "age": g.ages.summary(),
            "counters": g.counters,
            "backlog_dibits_at_last_delivery": g.last_backlog_dibits.map(|b| b.round()),
            "clock": {
                "seeded": g.clock.is_seeded(),
                "running": g.clock.running(),
                "uncertainty_dibits": g.clock.uncertainty_dibits().map(|u| u.round()),
                "uncertainty_ms": g.clock.uncertainty_dibits()
                    .map(|u| ((u / 2.0) / NOMINAL_DIBIT_RATE_HZ * 1e4).round() / 10.0),
                "observations": g.clock.observations,
                "reseeds": g.clock.reseeds,
                "production_index_now": est.map(|e| e.mid.round()),
            },
            "claimed_end": g.claimed_end,
            "pending_cuts": g.pending.len(),
            "last_resync": g.last_resync,
        });
        if with_cuts {
            v["recent_cuts"] = serde_json::to_value(g.recent_cuts.iter().collect::<Vec<_>>())
                .unwrap_or(serde_json::Value::Null);
        }
        v
    }
}

impl ChainEpochSink for DibitRingShared {
    fn record_hw(&self, action: HwAction, t_us: u64, next_address: u32, enabled_before: bool) {
        DibitRingShared::record_hw(self, action, t_us, next_address, enabled_before);
    }
}

/// Default post-discontinuity settle window: 48 dibits = 10 ms (traffic
/// DDC + LSM LPF/RRC flush ≈ 7–8 ms).
pub const DEFAULT_SETTLE_DIBITS: u32 = 48;
/// Default poll interval of the low-latency readers.
pub const DEFAULT_POLL_MS: u32 = 40;
const RECENT_CUTS: usize = 64;
const MAX_PENDING_CUTS: usize = 1024;

/// Both rings + global knobs. Held in `AppState` and cloned into the
/// readers, the forwarder and `IpCore`.
pub struct DibitDelivery {
    pub control: std::sync::Arc<DibitRingShared>,
    pub traffic: std::sync::Arc<DibitRingShared>,
    pub poll_ms: AtomicU32,
}

impl DibitDelivery {
    pub fn new(mode: DeliveryMode, poll_ms: u32) -> Self {
        DibitDelivery {
            control: std::sync::Arc::new(DibitRingShared::new(
                "control",
                RingGeometry::P25_DIBIT,
                mode,
            )),
            traffic: std::sync::Arc::new(DibitRingShared::new(
                "traffic",
                RingGeometry::P25_DIBIT,
                mode,
            )),
            poll_ms: AtomicU32::new(poll_ms.clamp(MIN_POLL_MS, MAX_POLL_MS)),
        }
    }

    pub fn poll_ms(&self) -> u32 {
        self.poll_ms.load(Ordering::Relaxed)
    }

    pub fn set_poll_ms(&self, ms: u32) {
        self.poll_ms.store(ms.clamp(MIN_POLL_MS, MAX_POLL_MS), Ordering::Relaxed);
    }

    pub fn status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "poll_ms": self.poll_ms(),
            "control": self.control.status_json(false),
            "traffic": self.traffic.status_json(true),
        })
    }
}

pub const MIN_POLL_MS: u32 = 5;
pub const MAX_POLL_MS: u32 = 1000;

#[cfg(test)]
#[path = "dibit_airtime_tests.rs"]
mod tests;
