//! Host tests for `app::grant_stats` (change 057: per-call counters by
//! call_id). Attached via
//! `#[cfg(test)] #[path = "grant_stats_tests.rs"] mod tests;`.
//!
//! Numbers follow the bench replay: 81- and 72-frame PTTs back to back
//! on one channel (src 3436046, then the reply from 1014).

use super::*;
use crate::app::grant_follower::OpenReason;

fn forwarder() -> Arc<ImbeForwarder> {
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    Arc::new(ImbeForwarder::new(tx))
}

fn open(call_id: u64, source: u32, t: u64) -> CallTrackerEvent {
    CallTrackerEvent {
        call_id,
        timestamp_unix_ms: t,
        kind: CallTrackerEventKind::CallOpen {
            tg: 300,
            nac: 0,
            source: Some(source),
            freq_hz: Some(857_987_500),
            channel: Some("1117".into()),
            encrypted: false,
            not_followed: None,
            opened_via: OpenReason::CcGrant,
            baseline_frames_submitted: 0,
        },
        lane: Some(Lane::One),
    }
}

fn close(call_id: u64, reason: CloseReason, t: u64, open_ms: u64) -> CallTrackerEvent {
    CallTrackerEvent {
        call_id,
        timestamp_unix_ms: t,
        kind: CallTrackerEventKind::CallClose {
            reason,
            final_source: None,
            final_actual_speaker: None,
            started_unix_ms: t - open_ms,
            ended_unix_ms: t,
            first_audio_at_unix_ms: None,
            first_hdu_at_unix_ms: None,
            expected_submit_count: 0,
            sources_observed: vec![],
            last_upd_at_unix_ms: 0,
            open_ms,
            end_lc: None,
        },
        lane: Some(Lane::One),
    }
}

/// `n` IMBE frames of `call_id` as the forwarder + vocoder count them.
fn frames(f: &ImbeForwarder, call_id: u64, n: u64) {
    f.call_counts.update(call_id, |c| {
        c.imbe_extracted += n;
        c.ldu1 += n / 18;
        c.ldu2 += n / 18;
        c.vocoder_pcm_samples += n * 160;
    });
}

struct Rig {
    active: Vec<ActiveSummary>,
    pending: Pending,
    f: Arc<ImbeForwarder>,
    clear: GrantStatsRing,
    enc: GrantStatsRing,
    rev: GrantStatsRev,
}

impl Rig {
    fn new() -> Self {
        Rig {
            active: Vec::new(),
            pending: Pending::default(),
            f: forwarder(),
            clear: new_ring(),
            enc: new_ring(),
            rev: new_rev(),
        }
    }
    fn ev(&mut self, e: CallTrackerEvent) {
        let f = [self.f.clone()];
        handle_event(e, &mut self.active, &mut self.pending, &f, &self.clear, &self.enc, &self.rev);
    }
    fn refresh(&mut self, now: Instant) {
        refresh_pending(&mut self.pending, &self.f, &self.clear, &self.enc, &self.rev, now);
    }
    fn summary(&self, call_id: u64) -> GrantDecodeSummary {
        self.clear.lock().unwrap().iter().find(|s| s.call_id == call_id).cloned().unwrap()
    }
}

#[test]
fn counters_are_the_calls_own_not_global_deltas() {
    let mut r = Rig::new();
    let t = 5_600_000;
    r.ev(open(1, 3436046, t));
    frames(&r.f, 1, 81);
    // The reply is granted and starts decoding BEFORE this task sees
    // the predecessor's close (pre-057 this landed in call 1: 144–153).
    frames(&r.f, 2, 18);
    r.ev(close(1, CloseReason::TgChange, t + 2_500, 2_500));
    r.ev(open(2, 1014, t + 2_500));
    frames(&r.f, 2, 54);
    r.ev(close(2, CloseReason::CallEnd, t + 5_000, 2_500));

    let a = r.summary(1);
    assert_eq!(a.imbe_extracted, 81);
    assert_eq!(a.vocoder_pcm_samples, 81 * 160);
    assert_eq!((a.duration_ms, a.ended_unix_ms), (2_500, t + 2_500));
    assert_eq!(a.close_reason, CloseReason::TgChange);
    let b = r.summary(2);
    assert_eq!(b.imbe_extracted, 72);
    assert_eq!(b.close_reason, CloseReason::CallEnd);
}

#[test]
fn tail_counted_after_the_close_is_picked_up_and_bumps_rev() {
    let mut r = Rig::new();
    let t = 5_600_000;
    r.ev(open(7, 1014, t));
    frames(&r.f, 7, 63);
    r.ev(close(7, CloseReason::CallEnd, t + 3_000, 3_000));
    assert_eq!(r.summary(7).imbe_extracted, 63);
    let rev0 = r.rev.load(Ordering::Relaxed);

    // Air-time tail: the last LDU is decoded / vocoded after the close.
    frames(&r.f, 7, 9);
    let now = Instant::now();
    r.refresh(now);
    assert_eq!(r.summary(7).imbe_extracted, 72);
    assert_eq!(r.rev.load(Ordering::Relaxed), rev0 + 1);
    // Nothing new: no bump.
    r.refresh(now);
    assert_eq!(r.rev.load(Ordering::Relaxed), rev0 + 1);
    // Past the window the call is no longer refreshed.
    r.refresh(now + Duration::from_millis(REFRESH_WINDOW_MS + 1));
    assert!(r.pending.calls.is_empty());
    frames(&r.f, 7, 9);
    r.refresh(now + Duration::from_millis(REFRESH_WINDOW_MS + 2));
    assert_eq!(r.summary(7).imbe_extracted, 72);
}

#[test]
fn a_call_without_frames_reports_zero() {
    let mut r = Rig::new();
    r.ev(open(3, 1014, 1_000));
    r.ev(close(3, CloseReason::Timeout, 4_000, 3_000));
    let s = r.summary(3);
    assert_eq!((s.imbe_extracted, s.ldu1_count, s.hdu_count), (0, 0, 0));
    assert!(s.agc_gain_q97_at_close.is_none());
}

// Change 060: the call's source is the grant's unit. A unit the voice link
// control names later (site 2026-09-27: dispatch 1013 heard, LC 3402072)
// does not replace it; a source-less call (re-follow) takes the first one.
#[test]
fn source_update_fills_but_never_replaces_the_grant_unit() {
    let upd = |call_id, s| CallTrackerEvent {
        call_id,
        timestamp_unix_ms: 1,
        kind: CallTrackerEventKind::SourceUpdate {
            new_source: s,
            via: crate::app::grant_follower::SourceUpdateVia::Ldu1LcVote,
        },
        lane: Some(Lane::One),
    };
    let mut r = Rig::new();
    let t = 5_600_000;
    r.ev(open(1, 1013, t));
    r.ev(upd(1, 3402072));
    r.ev(close(1, CloseReason::CallEnd, t + 4_000, 4_000));
    assert_eq!(r.summary(1).source, Some(1013));

    let mut sourceless = open(2, 0, t + 5_000);
    if let CallTrackerEventKind::CallOpen { source, .. } = &mut sourceless.kind {
        *source = None;
    }
    r.ev(sourceless);
    r.ev(upd(2, 3400043));
    r.ev(close(2, CloseReason::CallEnd, t + 8_000, 3_000));
    assert_eq!(r.summary(2).source, Some(3400043));
}

// Change 065: an encrypted (not-followed) call is listed at once and its
// late CallClose fills in the channel time.
#[test]
fn not_followed_close_fills_the_channel_time() {
    let mut r = Rig::new();
    let t = 5_600_000;
    let mut nf_open = open(9, 3400015, t);
    if let CallTrackerEventKind::CallOpen { not_followed, encrypted, tg, .. } = &mut nf_open.kind {
        *not_followed = Some("encrypted");
        *encrypted = true;
        *tg = 402;
    }
    r.ev(nf_open);
    let listed = |r: &Rig| r.enc.lock().unwrap().iter().find(|s| s.call_id == 9).cloned().unwrap();
    assert_eq!(listed(&r).duration_ms, 0);
    let rev0 = r.rev.load(std::sync::atomic::Ordering::Relaxed);
    let mut c = close(9, CloseReason::Timeout, t + 4_500, 4_500);
    if let CallTrackerEventKind::CallClose { last_upd_at_unix_ms, .. } = &mut c.kind {
        *last_upd_at_unix_ms = t + 4_500;
    }
    r.ev(c);
    let s = listed(&r);
    assert_eq!((s.duration_ms, s.ended_unix_ms, s.air_duration_ms), (4_500, t + 4_500, Some(4_500)));
    assert!(r.rev.load(std::sync::atomic::Ordering::Relaxed) > rev0);
}

// Change 066: a call on each traffic chain at once; neither displaces the
// other (pre-066 a second CallOpen forced the first to Timeout).
#[test]
fn calls_on_two_chains_are_summarised_separately() {
    let mut r = Rig::new();
    let t = 5_600_000;
    let on = |mut e: CallTrackerEvent, lane| {
        e.lane = Some(lane);
        e
    };
    r.ev(on(open(1, 1013, t), Lane::One));
    r.ev(on(open(2, 3400043, t + 500), Lane::Two));
    frames(&r.f, 1, 81);
    frames(&r.f, 2, 72);
    r.ev(on(close(2, CloseReason::CallEnd, t + 3_000, 2_500), Lane::Two));
    r.ev(on(close(1, CloseReason::CallEnd, t + 4_000, 4_000), Lane::One));
    let (a, b) = (r.summary(1), r.summary(2));
    assert_eq!((a.close_reason, b.close_reason), (CloseReason::CallEnd, CloseReason::CallEnd));
    assert_eq!((a.imbe_extracted, b.imbe_extracted), (81, 72));
    assert_eq!((a.source, b.source), (Some(1013), Some(3400043)));
    // A new call on chain 2 with chain 2's summary still open replaces
    // only that one (defensive Timeout), never chain 1's.
    r.ev(on(open(3, 1013, t + 5_000), Lane::One));
    r.ev(on(open(4, 1, t + 5_100), Lane::Two));
    r.ev(on(open(5, 2, t + 5_200), Lane::Two));
    assert_eq!(r.summary(4).close_reason, CloseReason::Timeout);
    assert_eq!(r.active.iter().map(|a| a.call_id).collect::<Vec<_>>(), vec![3, 5]);
}
