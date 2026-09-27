//! Host tests for `app::ui_state` (change 056). Attached via
//! `#[cfg(test)] #[path = "ui_state_tests.rs"] mod tests;`.
//!
//! Numbers follow the 2026-09-26 bench replay (Clay County, TG 300,
//! 72- and 81-frame PTTs, 10 s hang on the last call of each burst).

use super::*;
use crate::app::grant_follower::CloseReason;

const T0: u64 = 5_600_000; // board clock never set (1970), like unit A

fn summary(call_id: u64, start: u64, imbe: u64) -> GrantDecodeSummary {
    GrantDecodeSummary {
        call_id,
        tg: 300,
        nac: 0x8A1,
        source: Some(1014),
        actual_speaker: None,
        started_unix_ms: start,
        ended_unix_ms: start + 2_500,
        duration_ms: 2_500,
        first_imbe_ms: if imbe > 0 { Some(150) } else { None },
        first_audio_at_unix_ms: None,
        hdu_count: 0,
        ldu1_count: imbe / 18,
        ldu2_count: imbe / 18,
        tdu_count: 0,
        tdu_lc_count: 0,
        framer_arm_hdu: 0,
        framer_arm_ldu1: 0,
        framer_arm_ldu2: 0,
        framer_arm_tdu: 0,
        framer_arm_tdu_lc: 0,
        imbe_extracted: imbe,
        imbe_dropped: 0,
        vocoder_pcm_samples: imbe * 160,
        vocoder_errors: 0,
        vocoder_silent: 0,
        vocoder_encrypted: 0,
        encrypted: false,
        not_followed: None,
        freq_hz: Some(857_987_500),
        channel: Some("1117".into()),
        close_reason: CloseReason::TgChange,
        sources_observed: vec![1014],
        agc_gain_q97_at_close: None,
        air_duration_ms: Some(2_400),
    }
}

fn recording(id: u64, start: u64, dur: u64) -> RecordingEntry {
    RecordingEntry {
        id,
        talkgroup: 300,
        source: Some(1014),
        started_unix_ms: start,
        duration_ms: dur,
        path: std::path::PathBuf::from(format!("/tmp/p25_recordings/rec_{id}.wav")),
        size_bytes: 44 + dur * 16,
        filename: format!("rec_{start}_{id}_tg300_from1014.wav"),
        imbe_extracted: Some(153), // the pre-056 overlapping global delta
        imbe_dropped: Some(0),
        hdu_count: None,
        ldu1_count: Some(9),
        ldu2_count: Some(8),
        tdu_count: None,
        tdu_lc_count: None,
        vocoder_pcm: None,
        vocoder_errors: None,
        vocoder_silent: None,
        freq_hz: Some(857_987_500),
        channel: Some("1117".into()),
        chunks_match: Some(dur / 20),
        chunks_zero_callid: Some(0),
        chunks_drain: Some(0),
        first_chunk_after_open_ms: Some(400),
        sources_observed: vec![1014],
        max_chunk_lag_ms: None,
        mean_chunk_lag_ms: None,
    }
}

fn never(_: u64) -> bool {
    false
}

#[test]
fn clock_validity() {
    assert!(!clock_valid(T0));
    assert!(clock_valid(1_790_470_523_000));
}

#[test]
fn phase_acquiring_voice_hang() {
    // Granted, no voice yet.
    assert_eq!(call_phase(T0 + 500, T0, None), "acquiring");
    assert_eq!(call_phase(T0 + ACQUIRE_WINDOW_MS + 1, T0, None), "hang");
    // Voice within the hold window.
    assert_eq!(call_phase(T0 + 2_000, T0, Some(T0 + 1_900)), "voice");
    assert_eq!(call_phase(T0 + 2_000, T0, Some(T0 + 2_000 - VOICE_HOLD_MS)), "voice");
    // The bench's last PTT of a burst: voice ended, call still open.
    assert_eq!(call_phase(T0 + 8_000, T0, Some(T0 + 1_440)), "hang");
}

#[test]
fn build_call_reports_hang_countdown_and_aliases() {
    let mut tg = BTreeMap::new();
    tg.insert(300u16, "EMS Dispatch".to_string());
    let mut unit = BTreeMap::new();
    unit.insert(1014u32, "Console 14".to_string());
    let aliases = Aliases { tg: Some(&tg), unit: Some(&unit) };
    let snap = ActiveCallSnapshot {
        call_id: 257,
        tg: 300,
        source: Some(1014),
        freq_hz: Some(857_987_500),
        channel: Some("1117".into()),
        started_unix_ms: T0,
        sources_observed: vec![1014],
        voice_frames: 72,
        first_voice_unix_ms: Some(T0 + 111),
        last_voice_unix_ms: Some(T0 + 1_700),
        last_activity_unix_ms: T0 + 2_800, // trailing CC updates
        ..Default::default()
    };
    let c = build_call(&snap, T0 + 6_000, aliases, true);
    assert_eq!(c.phase, "hang");
    assert_eq!(c.voice_ms, 1_440);
    assert_eq!(c.elapsed_ms, 6_000);
    assert_eq!(c.close_in_ms, IDLE_TIMEOUT_MS - 3_200);
    assert_eq!(c.tg_alias.as_deref(), Some("EMS Dispatch"));
    assert_eq!(c.source_alias.as_deref(), Some("Console 14"));
    assert!(c.recording);

    let c = build_call(&snap, T0 + 20_000, aliases, true);
    assert_eq!(c.close_in_ms, 0, "saturates once overdue");

    let enc = ActiveCallSnapshot { encrypted: true, ..snap.clone() };
    assert!(!build_call(&enc, T0, aliases, true).recording);
    assert!(!build_call(&snap, T0, aliases, false).recording);
}

#[test]
fn site_health_states() {
    assert_eq!(site_health(false, Some(10)), "searching");
    assert_eq!(site_health(true, Some(300)), "ok");
    assert_eq!(site_health(true, Some(CC_STALE_MS + 1)), "stale");
    assert_eq!(site_health(true, None), "stale");
}

#[test]
fn rate_window_rates_and_reset() {
    let mut w = RateWindow::new();
    assert!(w.rates().is_none());
    w.observe(0, 1_000, 20);
    w.observe(500, 1_020, 20); // too soon: ignored
    assert!(w.rates().is_none());
    w.observe(1_000, 1_039, 21);
    let (per_s, pct) = w.rates().unwrap();
    assert!((per_s - 39.0).abs() < 1e-9);
    assert!((pct.unwrap() - 97.5).abs() < 1e-9);
    // Window slides: 20 samples keep only the last 11 (10 s).
    for i in 2..=20u64 {
        w.observe(i * 1_000, 1_000 + 39 * i, 20 + i);
    }
    let (per_s, _) = w.rates().unwrap();
    assert!((per_s - 39.0).abs() < 1e-9);
    // Decoder reset: counters go backwards → restart.
    w.observe(21_000, 5, 0);
    assert!(w.rates().is_none());
    // No blocks attempted → rate 0, no percentage.
    w.observe(22_000, 5, 0);
    assert_eq!(w.rates(), Some((0.0, None)));
}

#[test]
fn calls_join_by_call_id_newest_first() {
    let clear = vec![summary(254, T0, 72), summary(255, T0 + 2_500, 72), summary(256, T0 + 10_000, 81)];
    let recs = vec![recording(254, T0, 1_440), recording(255, T0 + 2_500, 1_440)];
    let q = CallsQuery { limit: 10, include_not_followed: false };
    let out = build_calls(&clear, &[], &recs, Aliases::default(), q, T0 + 12_600, &never);
    let ids: Vec<u64> = out.iter().map(|c| c.call_id).collect();
    assert_eq!(ids, vec![256, 255, 254]);
    // 256 closed 100 ms ago: its WAV is still being finalised.
    assert_eq!(out[0].audio_status, "saving");
    assert_eq!(out[0].voice_ms, 81 * 20);
    assert_eq!(out[1].audio_status, "recorded");
    let r = out[1].recording.as_ref().unwrap();
    assert_eq!((r.id, r.url.as_str()), (255, "/api/recordings/255.wav"));
    // Voice length comes from the WAV, not the (inflated) open time.
    assert_eq!(out[1].voice_ms, 1_440);
    assert_eq!(out[1].open_ms, 2_500);
    assert_eq!(out[1].close_reason, "tg_change");
}

#[test]
fn audio_status_reasons() {
    let now = T0 + 60_000;
    let skipped = |id: u64| id == 7;
    let oldest = Some(100);
    let g = summary(150, T0, 72);
    assert_eq!(audio_status(&g, None, now, oldest, &skipped), "missing");
    let old = summary(50, T0, 72);
    assert_eq!(audio_status(&old, None, now, oldest, &skipped), "evicted");
    let off = summary(7, T0, 72);
    assert_eq!(audio_status(&off, None, now, oldest, &skipped), "not_recorded");
    let silent = summary(151, T0, 0);
    assert_eq!(audio_status(&silent, None, now, oldest, &skipped), "no_voice");
    let enc = GrantDecodeSummary { encrypted: true, not_followed: Some("encrypted"), ..summary(152, T0, 0) };
    assert_eq!(audio_status(&enc, None, now, oldest, &skipped), "encrypted");
    let nf = GrantDecodeSummary { not_followed: Some("sticky_lock"), ..summary(153, T0, 0) };
    assert_eq!(audio_status(&nf, None, now, oldest, &skipped), "not_followed");
}

#[test]
fn not_followed_rows_are_optional_and_orphans_are_kept() {
    let clear = vec![summary(10, T0, 72)];
    let enc = vec![GrantDecodeSummary {
        encrypted: true,
        not_followed: Some("encrypted"),
        tg: 402,
        ..summary(11, T0 + 1_000, 0)
    }];
    // Recording 9's grant summary rolled out of the ring.
    let recs = vec![recording(9, T0 - 5_000, 1_000), recording(10, T0, 1_440)];
    let hide = CallsQuery { limit: 10, include_not_followed: false };
    let out = build_calls(&clear, &enc, &recs, Aliases::default(), hide, T0 + 60_000, &never);
    let ids: Vec<u64> = out.iter().map(|c| c.call_id).collect();
    assert_eq!(ids, vec![10, 9]);
    assert_eq!(out[1].audio_status, "recorded");
    assert_eq!(out[1].voice_ms, 1_000);

    let show = CallsQuery { limit: 10, include_not_followed: true };
    let out = build_calls(&clear, &enc, &recs, Aliases::default(), show, T0 + 60_000, &never);
    let ids: Vec<u64> = out.iter().map(|c| c.call_id).collect();
    assert_eq!(ids, vec![11, 10, 9]);
    assert_eq!(out[0].audio_status, "encrypted");
    assert_eq!(out[0].not_followed.as_deref(), Some("encrypted"));

    let one = CallsQuery { limit: 1, include_not_followed: true };
    assert_eq!(build_calls(&clear, &enc, &recs, Aliases::default(), one, T0, &never).len(), 1);
}

#[test]
fn calls_rev_changes_with_any_ring() {
    let a = calls_rev(Some(256), 200, None, 0, Some(255), 40);
    assert_eq!(a, calls_rev(Some(256), 200, None, 0, Some(255), 40));
    assert_ne!(a, calls_rev(Some(257), 200, None, 0, Some(255), 40));
    assert_ne!(a, calls_rev(Some(256), 200, Some(3), 1, Some(255), 40));
    assert_ne!(a, calls_rev(Some(256), 200, None, 0, Some(256), 40));
}
