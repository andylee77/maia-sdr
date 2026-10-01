//! Host tests for `radio::streams::dibit_ring`.

use super::sim::{Rng, WriterModel};
use super::*;

const RING: u64 = 32768;
const BASE: u32 = 0x1B00_0000;

fn snap(off: u64, lb: u8, t_ms: f64) -> RingSnapshot {
    RingSnapshot {
        next_address: BASE + off as u32,
        last_buffer: lb,
        chain_enabled: true,
        t_us: (t_ms * 1000.0) as u64,
    }
}

/// Sub-buffer phase that the HDL would report for a next-address offset
/// in steady state (AW two bursts ahead, B response of the burst before
/// the one being filled returned).
fn lb_for(off: u64) -> u8 {
    let bursts_issued = off / 128;
    let done = bursts_issued as i64 - 2;
    ((done.div_euclid(32)) - 1).rem_euclid(8) as u8
}

// ── Tracker ──────────────────────────────────────────────────────────

#[test]
fn startup_anchor_and_previous_reading_rule() {
    // Fresh DMA engine: dibit 0 produced at t = 0.
    let m = WriterModel::new(0, 0.0);
    let mut tr = RingTracker::new(m.geom);

    let r0 = tr.poll(&m.snapshot(0), true);
    assert!(r0.started);
    assert!(r0.phase_ok);
    assert_eq!(r0.abs_next, Some(256 + RING));
    assert_eq!(tr.pos(), RING); // abs_next − 256
    assert_eq!(r0.deliver, Some((RING, RING)));

    // 40 ms: 192 dibits produced, first burst not complete.
    let r1 = tr.poll(&m.snapshot(40_000), true);
    assert_eq!(r1.deliver, Some((RING, RING)));
    // 110 ms: burst 0 done (at 106.667 ms) → AW moved to 384. The
    // PREVIOUS reading (40 ms) still says safe end = RING: nothing yet.
    let r2 = tr.poll(&m.snapshot(110_000), true);
    assert_eq!(r2.abs_next, Some(RING + 384));
    assert_eq!(r2.deliver, Some((RING, RING)));
    // 150 ms: now the 110 ms reading is one poll old → deliver burst 0.
    let r3 = tr.poll(&m.snapshot(150_000), true);
    assert_eq!(r3.deliver, Some((RING, RING + 128)));
    assert_eq!(tr.pos(), RING + 128);
    // Delivered bytes are exactly the first burst the model finished.
    assert_eq!(m.landed_bytes(110_000.0), 128);
}

#[test]
fn wraparound_is_continuous() {
    // Start 1 s before the ring end so the writer wraps during the test.
    let start_dibits = 4 * (RING - 1200); // bytes → dibits
    let m = WriterModel::new(start_dibits, 0.0);
    let mut tr = RingTracker::new(m.geom);
    let mut last_end: Option<u64> = None;
    let mut t = 0u64;
    let mut delivered = 0u64;
    while t < 3_000_000 {
        let r = tr.poll(&m.snapshot(t), true);
        assert!(r.resync.is_none(), "unexpected resync at t={t}: {:?}", r.resync);
        assert!(r.phase_ok);
        if let Some((s, e)) = r.deliver {
            if let Some(le) = last_end {
                assert_eq!(s, le, "gap/overlap at t={t}");
            }
            assert!(e >= s);
            delivered += e - s;
            last_end = Some(e);
        }
        t += 40_000;
    }
    // Crossed the ring end: absolute position beyond 2 laps of origin.
    assert!(tr.pos() > 2 * RING);
    // ~3 s × 1200 B/s minus the pipeline (≤ 3 bursts in flight).
    assert!(delivered >= 3600 - 4 * 128 && delivered <= 3600, "delivered={delivered}");
}

#[test]
fn backwards_address_resyncs() {
    let mut tr = RingTracker::new(RingGeometry::P25_DIBIT);
    let off = 10 * 128;
    tr.poll(&snap(off, lb_for(off), 0.0), true);
    tr.poll(&snap(off + 128, lb_for(off + 128), 110.0), true);
    let before = tr.pos();
    // Address goes back by one burst.
    let r = tr.poll(&snap(off, lb_for(off), 150.0), true);
    let rs = r.resync.expect("resync");
    assert_eq!(rs.reason, ResyncReason::Jump);
    assert_eq!(rs.delta_bytes, RING - 128);
    assert!(rs.to_pos >= before);
    assert_eq!(tr.resyncs, 1);
    // Tracking continues normally afterwards.
    let r = tr.poll(&snap(off + 128, lb_for(off + 128), 260.0), true);
    assert!(r.resync.is_none());
}

#[test]
fn implausible_jump_resyncs() {
    let mut tr = RingTracker::new(RingGeometry::P25_DIBIT);
    tr.poll(&snap(1024, lb_for(1024), 0.0), true);
    // +8 KiB in 40 ms (rate allows ~60 B + 512 B slack).
    let off = 1024 + 8192;
    let r = tr.poll(&snap(off, lb_for(off), 40.0), true);
    assert_eq!(r.resync.map(|x| x.reason), Some(ResyncReason::Jump));
}

#[test]
fn stall_resyncs_with_lap_estimate() {
    let m = WriterModel::new(0, 0.0);
    let mut tr = RingTracker::new(m.geom);
    tr.poll(&m.snapshot(1_000_000), true);
    tr.poll(&m.snapshot(1_040_000), true);
    let k = tr.last_abs_next().unwrap() as i64 - m.abs_next_bytes(1_040_000.0) as i64;
    // Reader stalls 60 s (2.2 laps).
    let t = 61_040_000u64;
    let r = tr.poll(&m.snapshot(t), true);
    let rs = r.resync.expect("resync");
    assert_eq!(rs.reason, ResyncReason::Stall);
    // Re-anchor picked the right lap (rate is exact in the model).
    let truth = m.abs_next_bytes(t as f64) as i64 + k;
    assert_eq!(r.abs_next.unwrap() as i64, truth);
    assert_eq!(tr.pos() as i64, truth - LEAD_BYTES as i64);
    assert!(tr.skipped_bytes > 60 * 1100);
}

#[test]
fn two_long_stalls_overrun_is_skipped_not_delivered() {
    // Two consecutive 16 s gaps: each is under the stall limit (24.6 s),
    // but the range handed out on the third reading would start 32 s
    // behind the writer — more than one lap. It must be clipped.
    let m = WriterModel::new(4 * 1000, 0.0);
    let mut tr = RingTracker::new(m.geom);
    tr.poll(&m.snapshot(0), true);
    tr.poll(&m.snapshot(40_000), true);
    let r = tr.poll(&m.snapshot(16_040_000), true);
    assert!(r.resync.is_none(), "{:?}", r.resync);
    let t = 32_040_000u64;
    let r = tr.poll(&m.snapshot(t), true);
    let rs = r.resync.expect("overrun resync");
    assert_eq!(rs.reason, ResyncReason::Overrun);
    let (a, b) = r.deliver.unwrap();
    let abs_next = r.abs_next.unwrap();
    assert!(a + RING >= abs_next + LEAD_BYTES, "delivers lapped data");
    assert!(b > a);
    assert_eq!(a, rs.to_pos);
    assert!(tr.skipped_bytes > 0);
}

#[test]
fn base_change_resyncs() {
    let mut tr = RingTracker::new(RingGeometry::P25_DIBIT);
    tr.poll(&snap(1024, lb_for(1024), 0.0), true);
    let mut s = snap(1024, lb_for(1024), 40.0);
    s.next_address = 0x1A00_0000 + 1024;
    let r = tr.poll(&s, true);
    assert_eq!(r.resync.map(|x| x.reason), Some(ResyncReason::BaseChanged));
}

#[test]
fn phase_mismatch_resyncs_once_per_episode() {
    let mut tr = RingTracker::new(RingGeometry::P25_DIBIT);
    let off = 3 * 4096 + 512;
    tr.poll(&snap(off, lb_for(off), 0.0), true);
    let r = tr.poll(&snap(off, 6, 40.0), true);
    assert!(!r.phase_ok);
    assert_eq!(r.resync.map(|x| x.reason), Some(ResyncReason::PhaseMismatch));
    // Persistent mismatch: counted, no resync loop.
    let r = tr.poll(&snap(off, 6, 80.0), true);
    assert!(!r.phase_ok);
    assert!(r.resync.is_none());
    assert_eq!(tr.phase_mismatches, 2);
    // Consistent again, then a fresh mismatch → one more resync.
    let r = tr.poll(&snap(off, lb_for(off), 120.0), true);
    assert!(r.phase_ok && r.resync.is_none());
    let r = tr.poll(&snap(off, 6, 160.0), true);
    assert_eq!(r.resync.map(|x| x.reason), Some(ResyncReason::PhaseMismatch));
    assert_eq!(tr.resyncs, 2);
}

#[test]
fn irq_wake_too_soon_keeps_previous_reading() {
    let m = WriterModel::new(0, 0.0);
    let mut tr = RingTracker::new(m.geom);
    tr.poll(&m.snapshot(0), true);
    tr.poll(&m.snapshot(106_000), true); // burst 0 not yet done
    let r = tr.poll(&m.snapshot(107_000), true); // done at 106.667 ms
    let pending = r.abs_next; // accepted (1 ms ≥ min_settle 2 ms? no)
    assert!(pending.is_none(), "1 ms after a reading must be ignored");
    let r = tr.poll(&m.snapshot(107_500), true);
    assert!(r.abs_next.is_none());
    // 2.5 ms after the 106 ms reading: accepted, delivers the 106 ms
    // reading's safe end (still nothing: burst 0 wasn't done then).
    let r = tr.poll(&m.snapshot(108_500), true);
    assert!(r.abs_next.is_some());
    assert_eq!(r.deliver, Some((RING, RING)));
    let r = tr.poll(&m.snapshot(148_500), true);
    assert_eq!(r.deliver, Some((RING, RING + 128)));
}

#[test]
fn lift_next_address_cases() {
    let g = RingGeometry::P25_DIBIT;
    let refr = Some((5 * RING + 1024, 1_000_000u64));
    // Forward one burst 50 ms later.
    assert_eq!(
        lift_next_address(&g, refr, BASE + 1024 + 128, 1_050_000),
        Some(5 * RING + 1152)
    );
    // Across the ring end.
    let refr2 = Some((5 * RING + RING - 128, 1_000_000u64));
    assert_eq!(
        lift_next_address(&g, refr2, BASE + 0, 1_120_000),
        Some(6 * RING)
    );
    // Slightly older than the reference (small backwards step).
    assert_eq!(
        lift_next_address(&g, refr, BASE + 1024 - 128, 1_000_000),
        Some(5 * RING + 896)
    );
    // Implausible.
    assert_eq!(lift_next_address(&g, refr, BASE + 1024 + 16384, 1_010_000), None);
    assert_eq!(lift_next_address(&g, None, BASE, 0), None);
}

// ── Full simulation: delivery safety / latency / clock accuracy ─────

#[test]
fn simulated_delivery_is_gapless_safe_and_timely() {
    // Writer has been running for a while (arbitrary phase), reader
    // polls every 40 ms with ±5 ms jitter and occasional IRQ wakes.
    let m = WriterModel::new(123_457, 0.0);
    let mut tr = RingTracker::new(m.geom);
    let mut clock = DibitClock::new();
    let mut rng = Rng(0x5eed_1234);
    let mut t = 2_000_000.0f64;
    let mut k: Option<i64> = None; // tracker abs − model abs
    let mut last_end: Option<u64> = None;
    let mut prev_reading_t: Option<f64> = None;
    let mut max_age_ms: f64 = 0.0;
    let mut sum_age = 0.0;
    let mut n_age = 0.0;
    let mut delivered = 0u64;
    let mut worst_clock_err = 0.0f64;
    let mut width_after_10s = f64::MAX;
    while t < 72_000_000.0 {
        let s = m.snapshot(t as u64);
        let r = tr.poll(&s, true);
        assert!(r.resync.is_none(), "resync at {t}: {:?}", r.resync);
        assert!(r.phase_ok, "phase mismatch at {t}");
        if let Some(abs) = r.abs_next {
            let model_abs = m.abs_next_bytes(t) as i64;
            let kk = *k.get_or_insert(abs as i64 - model_abs);
            assert_eq!(abs as i64 - model_abs, kk, "absolute drift at {t}");
            clock.observe_next(t as u64, abs, true);
        }
        if let Some((a, b)) = r.deliver {
            if let Some(le) = last_end {
                assert_eq!(a, le, "gap/overlap");
            }
            last_end = Some(b);
            if b > a {
                let kk = k.unwrap();
                // Safety: everything delivered had landed by the previous
                // accepted reading (≥ min_settle before now).
                let tp = prev_reading_t.expect("delivery needs a previous reading");
                assert!(t - tp >= 2_000.0);
                assert!((b as i64 - kk) as u64 <= m.landed_bytes(tp));
                // Ages of first / last dibit.
                let first = 4 * (a as i64 - kk) as u64;
                let last = 4 * (b as i64 - kk) as u64 - 1;
                let age_old = (t - m.time_of(first)) / 1000.0;
                let age_new = (t - m.time_of(last)) / 1000.0;
                assert!(age_new > 0.0);
                max_age_ms = max_age_ms.max(age_old);
                sum_age += 0.5 * (age_old + age_new) * (b - a) as f64;
                n_age += (b - a) as f64;
                delivered += b - a;
            }
        }
        if r.abs_next.is_some() {
            prev_reading_t = Some(t);
        }
        // Clock accuracy against the truth (tracker coordinate).
        if let (Some(kk), Some(est)) = (k, clock.index_at(t as u64)) {
            let truth = m.produced(t) as f64 + 4.0 * kk as f64;
            let err = if truth < est.lo {
                est.lo - truth
            } else if truth > est.hi {
                truth - est.hi
            } else {
                0.0
            };
            worst_clock_err = worst_clock_err.max(err);
            if t > 12_000_000.0 {
                width_after_10s = width_after_10s.min(est.hi - est.lo);
                assert!(est.hi - est.lo < 160.0, "clock width {} at {t}", est.hi - est.lo);
            }
        }
        // Next wake: poll ± jitter, sometimes an extra IRQ wake 0.3 ms later.
        let jitter = (rng.unit() - 0.5) * 10_000.0;
        t += if rng.unit() < 0.05 { 300.0 } else { 40_000.0 + jitter };
    }
    let mean_age = sum_age / n_age;
    eprintln!(
        "sim: delivered={delivered} B mean_age={mean_age:.1} ms max_age={max_age_ms:.1} ms          clock_width_min_after_10s={width_after_10s:.0} dibits worst_excl={worst_clock_err:.2}"
    );
    // 70 s × 1200 B/s, minus what is still in flight.
    assert!(delivered > 83_000, "delivered {delivered}");
    // Burst fill (≤ 106.7 ms) + up to two jittered poll intervals.
    assert!(max_age_ms < 106.7 + 2.0 * 45.0 + 1.0, "max age {max_age_ms} ms");
    assert!(mean_age > 40.0 && mean_age < 140.0, "mean age {mean_age} ms");
    assert!(worst_clock_err <= 1.0, "clock excluded truth by {worst_clock_err} dibits");
    assert!(width_after_10s < 80.0, "clock never converged: {width_after_10s}");
}

#[test]
fn clock_time_of_brackets_production_time() {
    let m = WriterModel::new(40_000, 0.0);
    let mut tr = RingTracker::new(m.geom);
    let mut clock = DibitClock::new();
    let mut rng = Rng(42);
    let mut t = 1_000_000.0f64;
    let mut k = 0i64;
    while t < 20_000_000.0 {
        let r = tr.poll(&m.snapshot(t as u64), true);
        if let Some(abs) = r.abs_next {
            if r.started {
                k = abs as i64 - m.abs_next_bytes(t) as i64;
            }
            clock.observe_next(t as u64, abs, true);
        }
        t += 35_000.0 + rng.unit() * 10_000.0;
    }
    let width = clock.uncertainty_dibits().unwrap();
    let view = clock.view();
    // Dibits produced in the last 10 s: time error within half the
    // interval width (+ drift allowance + 1 ms rounding).
    for n in (m.produced(10_000_000.0)..m.produced(19_000_000.0)).step_by(997) {
        let idx = (n as i64 + 4 * k) as u64;
        let est = view.time_of(idx).unwrap();
        let truth = m.time_of(n);
        let bound_us = width / 2.0 / 4800.0e-6 + 2_000.0;
        assert!(
            (est - truth).abs() <= bound_us,
            "time_of error {} us > {} us",
            (est - truth).abs(),
            bound_us
        );
    }
}

#[test]
fn clock_pause_resume_mapping() {
    // Chain paused 5 s .. 8 s; pause/resume reported at the exact time
    // (chain-control hook precision).
    let mut m = WriterModel::new(8_000, 0.0);
    m.pauses.push((5_000_000.0, 8_000_000.0));
    let mut tr = RingTracker::new(m.geom);
    let mut clock = DibitClock::new();
    let mut k = 0i64;
    let mut t = 500_000.0f64;
    let mut paused_reported = false;
    let mut resumed_reported = false;
    while t < 12_000_000.0 {
        if !paused_reported && t >= 5_000_000.0 {
            clock.set_running(5_000_000, false);
            paused_reported = true;
        }
        if !resumed_reported && t >= 8_000_000.0 {
            clock.set_running(8_000_000, true);
            resumed_reported = true;
        }
        let s = m.snapshot(t as u64);
        let r = tr.poll(&s, true);
        assert!(r.resync.is_none(), "resync {:?}", r.resync);
        if let Some(abs) = r.abs_next {
            if r.started {
                k = abs as i64 - m.abs_next_bytes(t) as i64;
            }
            clock.observe_next(t as u64, abs, s.chain_enabled);
        }
        t += 40_000.0 + ((t as u64 * 7919) % 5_000) as f64;
    }
    let view = clock.view();
    let width = clock.uncertainty_dibits().unwrap();
    let bound = |w: f64| w / 2.0 / 4800.0e-6 + 25_000.0;
    // A dibit produced at 4.5 s (before the pause) …
    let n_before = m.produced(4_500_000.0);
    let est = view.time_of((n_before as i64 + 4 * k) as u64).unwrap();
    assert!((est - 4_500_000.0).abs() < bound(width), "before-pause {est}");
    // … and one produced at 9 s (after the resume).
    let n_after = m.produced(9_000_000.0);
    let est = view.time_of((n_after as i64 + 4 * k) as u64).unwrap();
    assert!((est - 9_000_000.0).abs() < bound(width), "after-resume {est}");
    // The paused 3 s are NOT counted as dibits.
    assert_eq!(m.produced(7_900_000.0), m.produced(5_000_000.0));
}

#[test]
fn clock_reseeds_on_inconsistent_reading() {
    let mut c = DibitClock::new();
    c.observe_interval(0, 1000.0, 1100.0, true);
    // 1 s later the truth should be ~5800 ± drift; claim 20000 instead.
    c.observe_interval(1_000_000, 20_000.0, 20_100.0, true);
    assert_eq!(c.reseeds, 1);
    let e = c.index_at(1_000_000).unwrap();
    assert!((e.lo - 20_000.0).abs() < 1e-6 && (e.hi - 20_100.0).abs() < 1e-6);
    // Consistent narrowing does not reseed.
    c.observe_interval(1_000_000, 20_050.0, 20_500.0, true);
    assert_eq!(c.reseeds, 1);
    let e = c.index_at(1_000_000).unwrap();
    assert!((e.lo - 20_050.0).abs() < 1e-6 && (e.hi - 20_100.0).abs() < 1e-6);
}

#[test]
fn reading_interval_matches_dma_model() {
    // For every time, the true dibit count lies inside the interval
    // implied by the next-address register.
    let m = WriterModel::new(77, 0.0);
    let mut t = 0.0;
    while t < 5_000_000.0 {
        let (lo, hi) = reading_interval(m.abs_next_bytes(t));
        let d = m.produced(t) as f64;
        assert!(d >= lo && d <= hi, "t={t} d={d} [{lo},{hi}]");
        t += 1_234.0;
    }
}
