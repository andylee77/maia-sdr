//! Dibit ring position math + dibit production clock (portable, host-tested).
//!
//! Change 054 (2026-09-26). Pure logic behind the low-latency dibit
//! reader in `app::dibit_readers`: no register access, no tokio, no
//! `cfg(linux)`, so every branch is unit-tested on the Windows host
//! (`dibit_ring_tests.rs`).
//!
//! ## What the HDL does (maia_hdl `DmaStreamRingWrite` + `DibitPacker`)
//!
//! - `DibitPacker` packs 32 dibits into one 64-bit word (dibit 0 in
//!   bits [1:0]); 4 dibits per byte, 4800 dibit/s = 1200 B/s.
//! - `DmaStreamRingWrite` writes fixed 16-beat bursts (128 B = 512
//!   dibits ≈ 107 ms) into a ring of `num_sub_buffers` × 4 KiB (8 × 4
//!   KiB = 32 KiB = 131072 dibits ≈ 27.3 s per lap for both P25 dibit
//!   rings). The ring base is aligned to the ring size.
//! - The AW channel runs up to **two bursts ahead** of the W data
//!   (`awvalid = enable & ~two_outstanding_bursts`), and `awaddr` is
//!   `base + aw_counter·128`. `*_dibit_next` (0xB0 / 0xD0) reads that
//!   `awaddr` (plain R, no side effect): the address of the NEXT burst
//!   to be issued.
//! - Therefore, at any instant with next-address `N` (absolute byte
//!   position, see below): the burst being filled starts at `N − 256`,
//!   the burst at `N − 128` is pre-issued but empty, and **every burst
//!   below `N − 256` has had all 16 W beats sent** (packet-mode HP1
//!   interconnect FIFO) and lands in DDR within microseconds.
//! - `last_buffer` (0xAC / 0xCC bits [18:16]) is the index of the most
//!   recently completed 4 KiB sub-buffer (B-channel side, init −1).
//!
//! ## Position math ([`RingTracker`])
//!
//! The reader keeps an **absolute byte position** `pos: u64` that never
//! wraps. Register values are ring offsets `off = next_address mod
//! ring_bytes` (valid because the base is ring-aligned). A new reading
//! is lifted to an absolute value by forward continuity:
//!
//! ```text
//! delta   = (off_new − off_prev) mod ring_bytes        (0 ≤ delta < ring)
//! abs_new = abs_prev + delta
//! ```
//!
//! That is exact as long as the writer advanced by less than one lap
//! between the two readings. The lap budget is 27.3 s and the reader
//! polls every ~40 ms, so the lap is resolved by continuity; the
//! guard below turns every case where it might not be into an explicit
//! resync instead of a silent lap slip:
//!
//! - `delta` larger than what the elapsed time allows (1200 B/s ×
//!   elapsed × 1.25 + 512 B) → the address jumped (or went backwards,
//!   which shows up as `delta ≈ ring`) → **resync**.
//! - elapsed time since the previous accepted reading ≥ 90 % of the lap
//!   budget → a lap may have been missed → **resync**.
//! - ring base (`next_address & !(ring−1)`) changed → **resync**.
//! - at delivery, anything older than `abs_next + 256 − ring_bytes` has
//!   been (or is being) overwritten → skipped as an **overrun resync**
//!   (only reachable after two consecutive long reader stalls, since
//!   delivery uses the previous reading's safe end);
//! - `last_buffer` provides an independent sub-buffer phase of the same
//!   lap: with `a_sub = off / sub_buffer_bytes` (the sub-buffer the next
//!   AW targets) it must satisfy `(last_buffer + 1) mod n ∈ {a_sub,
//!   a_sub − 1}` (the B response of the burst being filled can trail
//!   the AW pointer by at most a few bursts). A mismatch means the two
//!   registers disagree about which lap-phase the writer is in →
//!   **resync** (once per episode; see `phase_mismatch_resynced`).
//!
//! A resync re-anchors: the new absolute next-address is chosen as the
//! occurrence of `off_new` closest to `abs_prev + expected_advance`
//! (`expected_advance` = elapsed × 1200 B/s when the chain runs, else
//! 0), `pos` jumps to the new safe end, and the skipped byte count is
//! reported. The decoders get a framer reset at that point.
//!
//! **Start-up:** the first reading defines the absolute coordinate
//! `abs = off + ring_bytes` (one lap of headroom so `abs − 256` never
//! underflows) and `pos = abs − 256`: delivery starts at the first
//! burst that completes after the reader started, like the pre-054
//! reader started at the first sub-buffer completed after it started.
//! A fresh DMA engine (`last_buffer = 7`, `next = base + 256`) passes the
//! phase check (`a_sub = 0 = (7 + 1) mod 8`).
//!
//! **Safe end:** a poll at time `t_k` delivers `[pos, abs_next(t_{k−1})
//! − 256)`, i.e. it uses the PREVIOUS poll's reading, taken at least
//! one poll interval (and never less than `min_settle_us`) earlier, so
//! every delivered burst has certainly landed in DDR. The caller
//! invalidates the covering sub-buffer(s) before copying
//! ([`copy_plan`]).
//!
//! ## Production clock ([`DibitClock`])
//!
//! Absolute dibit index `i = 4 × absolute byte position`. Each register
//! reading at PS monotonic time `t` bounds the number of dibits the
//! HDL has produced so far:
//!
//! ```text
//! A = abs_next / 128                       (bursts issued)
//! D(t) ∈ [512·(A − 2), 512·(A − 1) + 32]   (+32: DibitPacker word in progress)
//! ```
//!
//! While the chain runs, `D(t) = D(t_ref) + R·(t − t_ref)` with
//! `R = 4800 dibit/s`. The clock keeps an interval `[lo, hi]` for
//! `D(t_ref)`, projects it forward (widened by ±`drift_ppm`), and
//! intersects it with every new reading. A burst boundary crossing
//! between two polls pins the phase to within one poll interval, and
//! successive crossings at varying poll phases shrink the interval
//! further. An empty intersection (symbol slip, missed pause) reseeds
//! from the latest reading. Pause/resume (`traffic_lsm_enable`) stop and
//! restart the linear model; completed running periods are kept as
//! frozen segments so dibits produced before a pause still map to the
//! right time.
//!
//! **Error bound** of the production time of dibit `i`:
//! `|t̂(i) − t(i)| ≤ (hi − lo)/(2R) + drift_ppm·|t − t_ref| + ε_hdl`
//! where `(hi − lo)` is reported as `uncertainty_dibits` (≤ 544 dibits
//! ≈ 113 ms right after start-up or a reseed, typically converging to
//! a few dibits within seconds of burst crossings) and `ε_hdl` is the
//! constant, un-modelled HDL pipeline delay between the antenna and
//! the slicer (DDC + LSM FIRs, ≈ 5–10 ms). All times are production
//! times at the slicer output ("air time" in the rest of the code).

// ── Time base + chain-control hook ───────────────────────────────────

static MONO_ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Process-wide PS monotonic microseconds (the time base of every
/// [`RingSnapshot`] and of the production clock).
pub fn mono_us() -> u64 {
    let origin = MONO_ORIGIN.get_or_init(std::time::Instant::now);
    origin.elapsed().as_micros() as u64
}

/// Hardware chain-control action on the traffic chain, reported by the
/// `IpCore` methods that perform it (so every caller — grant follower,
/// `/api/traffic`, `/api/encrypted_tgs`, sw_demod — is covered).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwAction {
    /// `retune_traffic_chain`: NCO write, optional LSM reset, enable.
    Retune { lsm_reset: bool },
    /// Bare NCO write / DDC reconfigure.
    NcoWrite,
    /// Bare LSM reset pulse.
    LsmReset,
    /// `traffic_lsm_enable` written (only reported when it changes).
    Enable(bool),
}

/// Receiver of hardware chain-control actions. Called under the IpCore
/// lock with the chain's next-address register read right after the
/// action (`next_address`) and the chain enable state before it.
pub trait ChainEpochSink: Send + Sync {
    fn record_hw(&self, action: HwAction, t_us: u64, next_address: u32, enabled_before: bool);
}

/// Bytes per AXI burst (16 beats × 8 B).
pub const BURST_BYTES: u64 = 128;
/// AW runs at most two bursts ahead of W: the burst being filled starts
/// at `next_address − LEAD_BYTES`.
pub const LEAD_BYTES: u64 = 2 * BURST_BYTES;
/// Four 2-bit dibits per byte (32 per 64-bit word).
pub const DIBITS_PER_BYTE: u64 = 4;
/// 32 dibits per 64-bit DMA word.
pub const DIBITS_PER_WORD: u64 = 32;
/// Dibits per 128-byte burst.
pub const DIBITS_PER_BURST: u64 = BURST_BYTES * DIBITS_PER_BYTE;
/// Nominal P25 symbol rate (dibits per second).
pub const NOMINAL_DIBIT_RATE_HZ: f64 = 4800.0;
/// Nominal ring fill rate (bytes per second) = 4800 / 4.
pub const NOMINAL_BYTE_RATE_HZ: f64 = NOMINAL_DIBIT_RATE_HZ / DIBITS_PER_BYTE as f64;

/// Geometry of one dibit ring (from the maia-kmod sysfs attributes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingGeometry {
    pub sub_buffer_bytes: u64,
    pub num_sub_buffers: u64,
}

impl RingGeometry {
    /// Both P25 dibit rings: 8 × 4 KiB (`p25_hdl/config.py`).
    pub const P25_DIBIT: RingGeometry = RingGeometry {
        sub_buffer_bytes: 4096,
        num_sub_buffers: 8,
    };

    pub fn ring_bytes(&self) -> u64 {
        self.sub_buffer_bytes * self.num_sub_buffers
    }

    /// True when the geometry satisfies the HDL's constraints (power-of-
    /// two ring made of whole bursts, at least two sub-buffers).
    pub fn is_valid(&self) -> bool {
        let r = self.ring_bytes();
        self.num_sub_buffers >= 2
            && self.sub_buffer_bytes >= BURST_BYTES
            && self.sub_buffer_bytes % BURST_BYTES == 0
            && r.is_power_of_two()
    }

    /// Ring offset of an AXI address (base is ring-aligned).
    pub fn offset_of(&self, address: u32) -> u64 {
        address as u64 & (self.ring_bytes() - 1)
    }

    /// Ring base of an AXI address.
    pub fn base_of(&self, address: u32) -> u32 {
        (address as u64 & !(self.ring_bytes() - 1)) as u32
    }

    /// Sub-buffer index containing a ring offset.
    pub fn sub_buffer_of(&self, offset: u64) -> u64 {
        (offset % self.ring_bytes()) / self.sub_buffer_bytes
    }

    /// Seconds for the writer to lap the ring at the nominal rate.
    pub fn lap_budget_secs(&self) -> f64 {
        self.ring_bytes() as f64 / NOMINAL_BYTE_RATE_HZ
    }

    /// Most recent absolute position `≤ reference` whose ring offset is
    /// `offset`. Used to map a sub-buffer index (legacy path) or a
    /// hand-over offset onto the absolute coordinate.
    pub fn abs_at_or_before(&self, reference: u64, offset: u64) -> u64 {
        let ring = self.ring_bytes();
        let back = (reference % ring + ring - offset % ring) % ring;
        reference.saturating_sub(back)
    }
}

/// `last_buffer` ↔ `next_address` lap-phase consistency check (see the
/// module doc). `next_offset` is the ring offset of `next_address`.
pub fn phase_consistent(geom: &RingGeometry, next_offset: u64, last_buffer: u8) -> bool {
    let n = geom.num_sub_buffers;
    let a_sub = geom.sub_buffer_of(next_offset);
    let completed_next = (last_buffer as u64 + 1) % n;
    completed_next == a_sub || completed_next == (a_sub + n - 1) % n
}

/// One register reading of a dibit ring. `t_us` is PS monotonic time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingSnapshot {
    pub next_address: u32,
    pub last_buffer: u8,
    /// Chain master enable (`lsm_enable` / `traffic_lsm_enable`) as read
    /// back from the control register in the same snapshot.
    pub chain_enabled: bool,
    pub t_us: u64,
}

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

/// Details of a resync, for EventLog + stats.
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

    #[allow(dead_code)]
    pub fn geometry(&self) -> RingGeometry {
        self.geom
    }

    /// Absolute byte position of the next byte to deliver.
    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// Absolute next-address of the latest accepted reading.
    #[allow(dead_code)]
    pub fn last_abs_next(&self) -> Option<u64> {
        self.last.map(|a| a.abs_next)
    }

    /// Current safe end (`abs_next − 256`) of the latest accepted reading.
    pub fn last_safe_end(&self) -> Option<u64> {
        self.last.map(|a| a.abs_next.saturating_sub(LEAD_BYTES))
    }

    pub fn started(&self) -> bool {
        self.last.is_some()
    }

    /// Move the delivery position to the most recent occurrence of
    /// `offset` at or before the current safe end (legacy → poll
    /// hand-over: `offset` = start of the first sub-buffer the legacy
    /// path has not delivered yet). No-op before start-up.
    pub fn rewind_to_offset(&mut self, offset: u64) {
        if let Some(safe) = self.last_safe_end() {
            let p = self.geom.abs_at_or_before(safe, offset);
            // Never rewind by a lap or more (data would be overwritten).
            let floor = safe.saturating_sub(self.geom.ring_bytes() - 2 * self.geom.sub_buffer_bytes);
            self.pos = p.max(floor);
        }
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
    /// position tracking (and `pos`) current without handing out data —
    /// used by the legacy whole-sub-buffer path so a later switch to the
    /// poll path starts from a known point.
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
                    // Legacy tracking: follow the current safe end.
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

/// Lift a next-address value read OUTSIDE the poll loop (e.g. by an
/// IpCore chain-control hook at action time) onto the absolute
/// coordinate, relative to the latest accepted reading `(abs_next,
/// t_us)`. Returns `None` before start-up or when implausible.
pub fn lift_next_address(
    geom: &RingGeometry,
    reference: Option<(u64, u64)>,
    next_address: u32,
    t_us: u64,
) -> Option<u64> {
    let (abs_ref, t_ref) = reference?;
    let ring = geom.ring_bytes();
    let off = geom.offset_of(next_address);
    let delta = (off + ring - abs_ref % ring) % ring;
    let elapsed = t_us.saturating_sub(t_ref) as f64 * 1e-6;
    let limit = NOMINAL_BYTE_RATE_HZ * elapsed * 1.25 + (4 * BURST_BYTES) as f64;
    if (delta as f64) <= limit {
        Some(abs_ref + delta)
    } else if ring - delta <= 2 * BURST_BYTES {
        // Reading slightly older than the reference (both taken within
        // the same burst period by different tasks): allow a small
        // backwards step.
        Some(abs_ref - (ring - delta))
    } else {
        None
    }
}

/// One contiguous piece of a ring copy: `len` bytes starting at byte
/// `offset` of sub-buffer `sub_buffer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyPiece {
    pub sub_buffer: usize,
    pub offset: usize,
    pub len: usize,
}

/// Split the absolute byte range `[start, end)` into per-sub-buffer
/// pieces in ring order. The caller invalidates each distinct
/// `sub_buffer` once before copying. `end − start` must not exceed one
/// lap (the caller never asks for more).
pub fn copy_plan(geom: &RingGeometry, start: u64, end: u64) -> Vec<CopyPiece> {
    let mut out = Vec::new();
    if end <= start {
        return out;
    }
    let ring = geom.ring_bytes();
    let mut cur = start;
    let end = end.min(start + ring);
    while cur < end {
        let off = cur % ring;
        let sb = off / geom.sub_buffer_bytes;
        let in_sb = off % geom.sub_buffer_bytes;
        let room = geom.sub_buffer_bytes - in_sb;
        let len = room.min(end - cur);
        out.push(CopyPiece {
            sub_buffer: sb as usize,
            offset: in_sb as usize,
            len: len as usize,
        });
        cur += len;
    }
    out
}

/// Production-rate model mapping absolute dibit indices to PS monotonic
/// time. See the module doc for the model and its error bound.
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

    pub fn is_seeded(&self) -> bool {
        self.state.is_some()
    }

    pub fn running(&self) -> Option<bool> {
        self.state.map(|s| s.running)
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

    /// Widen the interval by ±`dibits` (LSM reset may slip the symbol
    /// phase / warm-up by a few dibits).
    pub fn perturb(&mut self, t_us: u64, dibits: f64) {
        if self.state.is_none() {
            return;
        }
        self.move_to(t_us);
        if let Some(s) = self.state.as_mut() {
            s.lo -= dibits;
            s.hi += dibits;
        }
    }

    /// Estimated number of dibits produced by `t_us` (= index of the next
    /// dibit the HDL will produce).
    pub fn index_at(&self, t_us: u64) -> Option<IndexEstimate> {
        let (lo, hi) = self.projected(t_us)?;
        Some(IndexEstimate { lo, mid: 0.5 * (lo + hi), hi })
    }

    /// Current interval width (dibits).
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
