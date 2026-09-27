//! Unit tests for `app::dibit_airtime` (change 054). Included via
//! `#[path = "dibit_airtime_tests.rs"] mod tests;` in the production
//! file, so `use super::*;` reaches the private items.

use super::*;
use crate::hardware::dibit_ring::sim::{Rng, WriterModel};
use crate::hardware::dibit_ring::{RingTracker, BURST_BYTES};

fn ctx(tg: u16, call_id: u64) -> SegmentContext {
    SegmentContext {
        tg,
        source: 1000 + tg as u32,
        call_id,
        encrypted: false,
        freq_hz: 851_012_500,
    }
}

fn cut(index: u64, kind: EpochKind, c: Option<SegmentContext>, reset: bool, discard: u64) -> EpochCut {
    EpochCut {
        seq: index,
        index,
        est_lo: index,
        est_hi: index,
        kind,
        framer_reset: reset,
        discard_dibits: discard,
        ctx: c,
        call_id_only: false,
        recorded_unix_ms: 0,
        recorded_mono_us: 0,
        clamped: false,
        hw_reading: false,
    }
}

/// Steps without the informational `Applied` entries.
fn ops(steps: &[Step]) -> Vec<Step> {
    steps
        .iter()
        .filter(|s| !matches!(s, Step::Applied { .. }))
        .cloned()
        .collect()
}

// ── plan_chunk ───────────────────────────────────────────────────────

#[test]
fn no_cuts_feeds_or_gates_whole_chunk() {
    let mut st = AirtimeState::new(ctx(300, 7));
    let s = plan_chunk(&mut st, 1000, 1512, &[]);
    assert_eq!(s, vec![Step::Feed { start: 1000, end: 1512, ctx: ctx(300, 7) }]);
    let mut st = AirtimeState::new(ctx(0, 7));
    let s = plan_chunk(&mut st, 1000, 1512, &[]);
    assert_eq!(s, vec![Step::Gate { start: 1000, end: 1512 }]);
}

#[test]
fn call_close_mid_chunk_keeps_closing_calls_dibits() {
    // The closing call's in-flight dibits (before the cut) are decoded
    // under the closing call; the pre-054 reader dropped the whole batch.
    let mut st = AirtimeState::new(ctx(300, 7));
    let closed = SegmentContext { tg: 0, source: 0, call_id: 7, encrypted: false, freq_hz: 0 };
    let cuts = [cut(1200, EpochKind::CallClose, Some(closed), false, 0)];
    let s = plan_chunk(&mut st, 1000, 1512, &cuts);
    assert_eq!(
        ops(&s),
        vec![
            Step::Feed { start: 1000, end: 1200, ctx: ctx(300, 7) },
            Step::ResetFramer { at: 1200 },
            Step::Gate { start: 1200, end: 1512 },
        ]
    );
    assert!(matches!(s[1], Step::Applied { at: 1200, inside: true, .. }));
    assert!(st.ctx.gated());
}

#[test]
fn new_call_opens_gate_with_framer_reset() {
    let mut st = AirtimeState::new(SegmentContext::default());
    let cuts = [cut(1300, EpochKind::TgChange, Some(ctx(400, 9)), true, 0)];
    let s = plan_chunk(&mut st, 1000, 1512, &cuts);
    assert_eq!(
        ops(&s),
        vec![
            Step::Gate { start: 1000, end: 1300 },
            Step::ResetFramer { at: 1300 },
            Step::Feed { start: 1300, end: 1512, ctx: ctx(400, 9) },
        ]
    );
}

#[test]
fn retune_discards_presettle_then_new_context() {
    let mut st = AirtimeState::new(ctx(300, 7));
    let cuts = [
        cut(1100, EpochKind::Retune, None, true, 100),
        cut(1105, EpochKind::TgChange, Some(ctx(500, 8)), false, 0),
    ];
    let s = plan_chunk(&mut st, 1000, 1512, &cuts);
    assert_eq!(
        ops(&s),
        vec![
            Step::Feed { start: 1000, end: 1100, ctx: ctx(300, 7) },
            Step::ResetFramer { at: 1100 },
            Step::Discard { start: 1100, end: 1105 },
            Step::Discard { start: 1105, end: 1200 },
            Step::Feed { start: 1200, end: 1512, ctx: ctx(500, 8) },
        ]
    );
}

#[test]
fn discard_window_spans_chunks() {
    let mut st = AirtimeState::new(ctx(300, 7));
    let cuts = [cut(1400, EpochKind::Resume, None, true, 300)];
    let s = plan_chunk(&mut st, 1000, 1512, &cuts);
    assert_eq!(
        ops(&s),
        vec![
            Step::Feed { start: 1000, end: 1400, ctx: ctx(300, 7) },
            Step::ResetFramer { at: 1400 },
            Step::Discard { start: 1400, end: 1512 },
        ]
    );
    let s = plan_chunk(&mut st, 1512, 2024, &[]);
    assert_eq!(
        s,
        vec![
            Step::Discard { start: 1512, end: 1700 },
            Step::Feed { start: 1700, end: 2024, ctx: ctx(300, 7) },
        ]
    );
}

#[test]
fn late_cut_applies_at_chunk_start() {
    let mut st = AirtimeState::new(ctx(300, 7));
    let cuts = [cut(900, EpochKind::CtxUpdate, Some(ctx(300, 7)), false, 0)];
    let s = plan_chunk(&mut st, 1000, 1512, &cuts);
    assert!(matches!(s[0], Step::Applied { at: 1000, inside: false, .. }));
    assert_eq!(ops(&s), vec![Step::Feed { start: 1000, end: 1512, ctx: ctx(300, 7) }]);
}

#[test]
fn same_tg_regrant_switches_call_id_without_reset() {
    // Back-to-back PTTs on one TG: frames completing after the cut are
    // the new call's, no framer reset (a new HDU that began just before
    // the grant was processed is kept).
    let mut st = AirtimeState::new(ctx(300, 7));
    let cuts = [cut(1256, EpochKind::CallOpen, Some(ctx(300, 8)), false, 0)];
    let s = plan_chunk(&mut st, 1000, 1512, &cuts);
    assert_eq!(
        ops(&s),
        vec![
            Step::Feed { start: 1000, end: 1256, ctx: ctx(300, 7) },
            Step::Feed { start: 1256, end: 1512, ctx: ctx(300, 8) },
        ]
    );
}

#[test]
fn call_open_updates_only_the_call_id() {
    // Lifecycle CallOpen while gated (grant hold / after CallClose):
    // the gate stays closed, only the id changes.
    let mut st = AirtimeState::new(SegmentContext { tg: 0, ..ctx(300, 7) });
    let mut c = cut(1200, EpochKind::CallOpen, Some(SegmentContext { call_id: 8, ..SegmentContext::default() }), false, 0);
    c.call_id_only = true;
    let s = plan_chunk(&mut st, 1000, 1512, &[c.clone()]);
    assert_eq!(ops(&s), vec![Step::Gate { start: 1000, end: 1200 }, Step::Gate { start: 1200, end: 1512 }]);
    assert_eq!(st.ctx.call_id, 8);
    assert_eq!(st.ctx.tg, 0);
    // While live on a TG: TG / source / encryption are kept.
    let mut a = ctx(300, 7);
    a.encrypted = true;
    let mut st = AirtimeState::new(a);
    let s = plan_chunk(&mut st, 1000, 1512, &[c]);
    assert_eq!(
        ops(&s),
        vec![
            Step::Feed { start: 1000, end: 1200, ctx: a },
            Step::Feed { start: 1200, end: 1512, ctx: SegmentContext { call_id: 8, ..a } },
        ]
    );
}

#[test]
fn encryption_latch_is_sticky_within_a_call_only() {
    let mut a = ctx(300, 7);
    a.encrypted = true;
    let mut st = AirtimeState::new(a);
    // Grant refresh for the same call carries encrypted=false.
    let cuts = [cut(1100, EpochKind::CtxUpdate, Some(ctx(300, 7)), false, 0)];
    plan_chunk(&mut st, 1000, 1512, &cuts);
    assert!(st.ctx.encrypted);
    // A new call does not inherit it.
    let cuts = [cut(1600, EpochKind::CallOpen, Some(ctx(300, 8)), false, 0)];
    plan_chunk(&mut st, 1512, 2024, &cuts);
    assert!(!st.ctx.encrypted);
}

#[test]
fn every_dibit_accounted_exactly_once() {
    let mut st = AirtimeState::new(ctx(300, 7));
    let closed = SegmentContext { tg: 0, ..ctx(300, 7) };
    let cuts = [
        cut(1010, EpochKind::CtxUpdate, Some(ctx(300, 7)), false, 0),
        cut(1100, EpochKind::Retune, None, true, 64),
        cut(1110, EpochKind::TgChange, Some(ctx(301, 8)), false, 0),
        cut(1300, EpochKind::CallClose, Some(closed), false, 0),
        cut(1300, EpochKind::TgChange, Some(ctx(302, 9)), true, 0),
    ];
    let s = plan_chunk(&mut st, 1000, 1512, &cuts);
    let mut covered = 0;
    let mut next = 1000;
    for step in &s {
        match step {
            Step::Feed { start, end, .. } | Step::Gate { start, end } | Step::Discard { start, end } => {
                assert_eq!(*start, next, "gap/overlap in {s:?}");
                covered += end - start;
                next = *end;
            }
            _ => {}
        }
    }
    assert_eq!(covered, 512);
    assert_eq!(st.ctx, ctx(302, 9));
}

// ── Shared recorder ─────────────────────────────────────────────────

fn shared() -> DibitRingShared {
    DibitRingShared::new("traffic", RingGeometry::P25_DIBIT, DeliveryMode::Airtime)
}

/// Drain pending cuts without moving the claim point.
fn take(sh: &DibitRingShared) -> Vec<EpochCut> {
    sh.lock().pending.drain(..).collect()
}

#[test]
fn recording_is_a_noop_until_airtime_is_active() {
    let sh = shared();
    assert!(!sh.epochs_active()); // reader has not activated yet
    sh.record_sw_at(EpochKind::TgChange, ctx(300, 1), true, 1000);
    assert!(sh.claim(u64::MAX).is_empty());
    sh.activate_mode(DeliveryMode::Airtime, 4000);
    sh.record_sw_at(EpochKind::TgChange, ctx(300, 1), true, 2000);
    let c = sh.claim(u64::MAX);
    assert_eq!(c.len(), 1);
    // Clock unseeded: placed at the claim point.
    assert_eq!(c[0].index, 4000);
    sh.activate_mode(DeliveryMode::Poll, 4000);
    sh.record_sw_at(EpochKind::TgChange, ctx(300, 1), true, 3000);
    assert!(sh.claim(u64::MAX).is_empty());
}

#[test]
fn cuts_are_clamped_to_claimed_end_and_claimed_in_order() {
    let sh = shared();
    sh.activate_mode(DeliveryMode::Airtime, 0);
    // Seed the clock: D(1 s) ∈ [10000, 10050].
    {
        let mut g = sh.lock();
        g.clock.observe_interval(1_000_000, 10_000.0, 10_050.0, true);
    }
    // Reader already claimed up to 12000 (> estimate at 1.1 s ≈ 10505).
    assert!(sh.claim(12_000).is_empty());
    sh.record_sw_at(EpochKind::CallOpen, ctx(300, 2), false, 1_100_000);
    // Later actions land after the claim point, in time order.
    sh.record_sw_at(EpochKind::CtxUpdate, ctx(300, 2), false, 1_600_000);
    sh.record_sw_at(EpochKind::CallClose, ctx(0, 2), false, 1_800_000);
    let c = sh.claim(20_000);
    assert_eq!(c.len(), 3);
    assert_eq!(c[0].index, 12_000);
    assert!(c[0].clamped);
    assert!(c[1].index > c[0].index && c[2].index > c[1].index);
    assert!(!c[1].clamped);
    // ≈ 10025 + 4800·0.6 = 12905
    assert!((c[1].index as i64 - 12_905).abs() <= 2, "{}", c[1].index);
    assert_eq!(sh.counters().cuts_clamped, 1);
    // A cut exactly at a claim end stays pending for the next chunk.
    sh.record_sw_at(EpochKind::CtxUpdate, ctx(300, 2), false, 1_800_000);
    let pending_idx = sh.lock().pending[0].index;
    assert!(sh.claim(pending_idx).is_empty());
    assert_eq!(sh.claim(pending_idx + 1).len(), 1);
}

#[test]
fn hw_discontinuity_brackets_truth_with_discard() {
    let m = WriterModel::new(50_000, 0.0);
    let sh = shared();
    sh.activate_mode(DeliveryMode::Airtime, 0);
    let mut tr = RingTracker::new(m.geom);
    let mut rng = Rng(7);
    let mut t = 100_000.0f64;
    let mut k = 0i64;
    while t < 8_000_000.0 {
        let r = tr.poll(&m.snapshot(t as u64), true);
        if let Some(abs) = r.abs_next {
            if r.started {
                k = abs as i64 - m.abs_next_bytes(t) as i64;
            }
            sh.observe(t as u64, abs, true);
            sh.claim(tr.pos() * 4);
        }
        t += 35_000.0 + 10_000.0 * rng.unit();
    }
    // Retune at an arbitrary time with the register read at the action.
    let ta = t + 13_000.0;
    let settle = sh.settle_dibits.load(Ordering::Relaxed) as u64;
    sh.record_hw(HwAction::Retune { lsm_reset: true }, ta as u64, m.next_address(ta), true);
    let c = sh.claim(u64::MAX);
    assert_eq!(c.len(), 1);
    let c = &c[0];
    assert_eq!(c.kind, EpochKind::Retune);
    assert!(c.framer_reset && c.hw_reading);
    let truth = (m.produced(ta) as i64 + 4 * k) as u64;
    assert!(c.index <= truth, "cut {} after truth {}", c.index, truth);
    assert!(c.index + c.discard_dibits >= truth + settle);
    // Converged clock: the discard window stays small (≤ ~40 ms).
    assert!(c.discard_dibits < settle + 150, "discard {}", c.discard_dibits);
}

#[test]
fn enable_changes_pause_and_resume() {
    let sh = shared();
    sh.activate_mode(DeliveryMode::Airtime, 0);
    sh.observe(1_000_000, 64 * BURST_BYTES, true);
    let next = 0x1B00_0000 + (64 * BURST_BYTES) as u32;
    // Writing the same state is not an epoch.
    sh.record_hw(HwAction::Enable(true), 1_010_000, next, true);
    assert!(take(&sh).is_empty());
    sh.record_hw(HwAction::Enable(false), 1_020_000, next, true);
    let c = take(&sh);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].kind, EpochKind::Pause);
    assert!(!c[0].framer_reset);
    assert_eq!(sh.lock().clock.running(), Some(false));
    sh.record_hw(HwAction::Enable(true), 3_000_000, next, false);
    let c = take(&sh);
    assert_eq!(c[0].kind, EpochKind::Resume);
    assert!(c[0].framer_reset && c[0].discard_dibits >= DEFAULT_SETTLE_DIBITS as u64);
    assert_eq!(sh.lock().clock.running(), Some(true));
}

#[test]
fn legacy_mode_still_tracks_chain_state_but_records_no_cuts() {
    let sh = shared();
    sh.activate_mode(DeliveryMode::Legacy, 0);
    sh.observe(1_000_000, 64 * BURST_BYTES, true);
    let next = 0x1B00_0000 + (64 * BURST_BYTES) as u32;
    sh.record_hw(HwAction::Enable(false), 1_020_000, next, true);
    assert!(take(&sh).is_empty());
    assert_eq!(sh.lock().clock.running(), Some(false));
    assert_eq!(sh.counters().cuts_dropped_mode, 1);
}

// ── End-to-end attribution (writer model + tracker + recorder) ──────

struct Harness {
    m: WriterModel,
    tr: RingTracker,
    sh: DibitRingShared,
    st: AirtimeState,
    k: i64,
    fed: Vec<(u64, u64, SegmentContext)>,
    gated: Vec<(u64, u64)>,
    discarded: Vec<(u64, u64)>,
    resets: Vec<u64>,
}

impl Harness {
    fn new(m: WriterModel) -> Self {
        let geom = m.geom;
        let sh = shared();
        sh.activate_mode(DeliveryMode::Airtime, 0);
        Harness {
            m,
            tr: RingTracker::new(geom),
            sh,
            st: AirtimeState::new(SegmentContext::default()),
            k: 0,
            fed: Vec::new(),
            gated: Vec::new(),
            discarded: Vec::new(),
            resets: Vec::new(),
        }
    }

    fn poll(&mut self, t: f64) {
        let s = self.m.snapshot(t as u64);
        let r = self.tr.poll(&s, true);
        assert!(r.resync.is_none());
        if let Some(abs) = r.abs_next {
            if r.started {
                self.k = abs as i64 - self.m.abs_next_bytes(t) as i64;
                self.sh.activate_mode(DeliveryMode::Airtime, self.tr.pos() * 4);
            }
            self.sh.observe(t as u64, abs, s.chain_enabled);
        }
        if let Some((a, b)) = r.deliver {
            let cuts = self.sh.claim(b * 4);
            if b > a || !cuts.is_empty() {
                let steps = plan_chunk(&mut self.st, a * 4, b * 4, &cuts);
                for s in steps {
                    match s {
                        Step::Feed { start, end, ctx } => self.fed.push((start, end, ctx)),
                        Step::Gate { start, end } => self.gated.push((start, end)),
                        Step::Discard { start, end } => self.discarded.push((start, end)),
                        Step::ResetFramer { at } => self.resets.push(at),
                        Step::Applied { .. } => {}
                    }
                }
            }
        }
    }

    /// Production time (µs) of tracker-coordinate dibit `idx`.
    fn t_of(&self, idx: u64) -> f64 {
        self.m.time_of((idx as i64 - 4 * self.k) as u64)
    }
}

#[test]
fn end_to_end_air_time_attribution() {
    // Timeline (µs):
    //  3.0 s  same-freq resume: TG 300 call 1 (framer reset)
    //  6.0 s  CallClose of call 1 (gate closes)
    //  6.5 s  retune + TG 400 call 2
    //  9.0 s  same-TG re-grant: call 3 (TG 400)
    // 11.0 s  encrypted teardown: TG 0 + pause; resume 12.0 s with TG 500 call 4
    let mut m = WriterModel::new(9_999, 0.0);
    m.pauses.push((11_000_000.0, 12_000_000.0));
    let mut h = Harness::new(m);
    let mut rng = Rng(0xabcdef);
    let actions: Vec<(f64, u8)> = vec![
        (3_000_000.0, 1),
        (6_000_000.0, 2),
        (6_500_000.0, 3),
        (9_000_000.0, 4),
        (11_000_000.0, 5),
        (12_000_000.0, 6),
        (14_000_000.0, 7),
    ];
    let mut ai = 0;
    let mut t = 200_000.0f64;
    while t < 17_000_000.0 {
        while ai < actions.len() && actions[ai].0 <= t {
            let ta = actions[ai].0 as u64;
            let next = h.m.next_address(ta as f64);
            match actions[ai].1 {
                1 => h.sh.record_sw_at(EpochKind::TgChange, ctx(300, 1), true, ta),
                2 => h.sh.record_sw_at(
                    EpochKind::CallClose,
                    SegmentContext { tg: 0, ..ctx(300, 1) },
                    false,
                    ta,
                ),
                3 => {
                    h.sh.record_hw(HwAction::Retune { lsm_reset: true }, ta, next, true);
                    h.sh.record_sw_at(EpochKind::TgChange, ctx(400, 2), false, ta + 200);
                }
                4 => h.sh.record_call_id_at(3, ta),
                5 => {
                    h.sh.record_sw_at(
                        EpochKind::TgChange,
                        SegmentContext { tg: 0, ..ctx(400, 3) },
                        true,
                        ta,
                    );
                    h.sh.record_hw(HwAction::Enable(false), ta + 100, next, true);
                }
                6 => {
                    h.sh.record_hw(HwAction::Retune { lsm_reset: true }, ta, next, false);
                    h.sh.record_sw_at(EpochKind::TgChange, ctx(500, 4), false, ta + 200);
                }
                7 => {
                    // Same TG moves to a new frequency while live; the
                    // lifecycle publishes the new call_id BEFORE the
                    // retune lands (async task won the race).
                    h.sh.record_sw_at(
                        EpochKind::GrantHold,
                        SegmentContext { tg: 0, ..ctx(500, 4) },
                        true,
                        ta - 4_000,
                    );
                    h.sh.record_call_id_at(5, ta - 2_000);
                    h.sh.record_hw(HwAction::Retune { lsm_reset: false }, ta, next, true);
                    h.sh.record_sw_at(EpochKind::TgChange, ctx(500, 5), false, ta + 200);
                }
                _ => unreachable!(),
            }
            ai += 1;
        }
        h.poll(t);
        t += 35_000.0 + 10_000.0 * rng.unit();
    }

    // Ground truth: context of a dibit produced at time `tp`.
    let truth = |tp: f64| -> Option<(u16, u64)> {
        if tp < 3_000_000.0 {
            None
        } else if tp < 6_000_000.0 {
            Some((300, 1))
        } else if tp < 6_500_000.0 {
            None
        } else if tp < 9_000_000.0 {
            Some((400, 2))
        } else if tp < 11_000_000.0 {
            Some((400, 3))
        } else if tp < 14_000_000.0 {
            Some((500, 4))
        } else {
            Some((500, 5))
        }
    };
    let near_action = |tp: f64, tol: f64| actions.iter().any(|(ta, _)| (tp - ta).abs() < tol);
    // Tolerance = clock uncertainty (converges to ≲ 10 ms) + settle.
    let tol_us = 25_000.0;

    let mut fed_total = 0u64;
    let mut worst_ms: f64 = 0.0;
    for &(a, b, c) in &h.fed {
        for idx in (a..b).step_by(7) {
            let tp = h.t_of(idx);
            fed_total += 1;
            let want = truth(tp);
            if want != Some((c.tg, c.call_id)) {
                assert!(
                    near_action(tp, tol_us),
                    "dibit at {:.3} s fed as TG {} call {} (want {:?})",
                    tp / 1e6, c.tg, c.call_id, want
                );
                let d = actions.iter().map(|(ta, _)| (tp - ta).abs()).fold(f64::MAX, f64::min);
                worst_ms = worst_ms.max(d / 1000.0);
            }
        }
    }
    assert!(fed_total > 0);
    // Gated dibits are never inside a live call (away from its edges).
    for &(a, b) in &h.gated {
        for idx in (a..b).step_by(7) {
            let tp = h.t_of(idx);
            if truth(tp).is_some() {
                assert!(near_action(tp, tol_us), "live dibit at {:.3} s gated", tp / 1e6);
            }
        }
    }
    // Discards sit right at the two discontinuities: 6.5 s (retune) and
    // the 11.0 s pause / 12.0 s resume (one point on the dibit axis: the
    // dibits just below it were produced before the pause, those above
    // after the resume).
    for &(a, b) in &h.discarded {
        let (ta, tb) = (h.t_of(a), h.t_of(b - 1));
        let ok = |x: f64| {
            [6_500_000.0, 11_000_000.0, 12_000_000.0, 14_000_000.0]
                .iter()
                .any(|&p| x >= p - tol_us && x <= p + tol_us)
        };
        assert!(ok(ta) && ok(tb), "discard {:.4}..{:.4} s", ta / 1e6, tb / 1e6);
    }
    // The closing call's tail (produced before 6.0 s but delivered
    // after the close was recorded) was decoded; the new call never got
    // the old call's dibits.
    let fed_call1_last = h
        .fed
        .iter()
        .filter(|f| f.2.call_id == 1)
        .map(|f| h.t_of(f.1 - 1))
        .fold(0.0, f64::max);
    assert!(fed_call1_last > 6_000_000.0 - tol_us, "call 1 tail cut at {fed_call1_last}");
    let call2_first = h
        .fed
        .iter()
        .filter(|f| f.2.call_id == 2)
        .map(|f| h.t_of(f.0))
        .fold(f64::MAX, f64::min);
    assert!(call2_first >= 6_500_000.0, "call 2 got pre-retune dibits at {call2_first}");
    // Early CallOpen before a retune: no old-frequency dibit is labelled
    // with the new call.
    let call5_first = h
        .fed
        .iter()
        .filter(|f| f.2.call_id == 5)
        .map(|f| h.t_of(f.0))
        .fold(f64::MAX, f64::min);
    assert!(call5_first >= 14_000_000.0, "call 5 got pre-retune dibits at {call5_first}");
    // Framer resets at: 3.0 (resume), 6.0 (gate close), 6.5 (retune),
    // 11.0 (gate close), 12.0 (retune/resume).
    assert!(h.resets.len() >= 5, "resets {:?}", h.resets);
    eprintln!("attribution: worst mismatch {worst_ms:.1} ms from an action");
}

// ── Stats ───────────────────────────────────────────────────────────

#[test]
fn age_histogram_percentiles() {
    let mut h = AgeHistogram::new();
    assert!(h.percentile(0.5).is_none());
    for i in 0..1000 {
        h.record(40.0 + (i % 100) as f64, 32);
    }
    let p50 = h.percentile(0.5).unwrap();
    let p99 = h.percentile(0.99).unwrap();
    assert!(p50 > 80.0 && p50 < 100.0, "p50 {p50}");
    assert!(p99 >= 130.0 && p99 <= 139.0, "p99 {p99}");
    assert_eq!(h.total, 32_000);
    assert!((h.mean_ms().unwrap() - 89.5).abs() < 1e-6);
    h.record(3_500.0, 16_384);
    assert!(h.percentile(0.999).unwrap() >= 3_000.0);
    assert_eq!(h.max_ms, 3_500.0);
    let j = h.summary();
    assert!(j["p99_ms"].is_number());
}

#[test]
fn delivery_mode_parse_roundtrip() {
    for m in [DeliveryMode::Legacy, DeliveryMode::Poll, DeliveryMode::Airtime] {
        assert_eq!(DeliveryMode::parse(m.as_str()), Some(m));
        assert_eq!(DeliveryMode::from_u8(m.to_u8()), m);
    }
    assert_eq!(DeliveryMode::parse("LEGACY"), Some(DeliveryMode::Legacy));
    assert_eq!(DeliveryMode::parse("nope"), None);
}

#[test]
fn status_json_shape() {
    let d = DibitDelivery::new(DeliveryMode::Airtime, 2);
    assert_eq!(d.poll_ms(), MIN_POLL_MS);
    d.traffic.record_delivery(1_000_000, 4000, 4512, false);
    let j = d.status_json();
    assert_eq!(j["traffic"]["requested_mode"], "airtime");
    assert!(j["traffic"]["counters"]["dibits_delivered"].as_u64().unwrap() == 512);
    assert!(j["traffic"]["recent_cuts"].is_array());
    assert!(j["control"]["age"]["bin_edges_ms"].is_array());
}
