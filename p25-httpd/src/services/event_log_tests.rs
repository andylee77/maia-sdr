//! Host tests for `services::event_log` (change 056: `/api/log`
//! query semantics). Attached via
//! `#[cfg(test)] #[path = "event_log_tests.rs"] mod tests;`.

use super::*;

/// 20 entries: seq 1..=20. Every 4th is a follower grant decision,
/// every other one a TSBK mirror line (`grant` + `event_type`), the
/// rest recorder lines.
fn filled() -> EventLog {
    let log = EventLog::new(64);
    for i in 1..=20u64 {
        match i % 4 {
            0 => log.push(LogCategory::Grant, format!("grant TG=300 #{i}"),
                          serde_json::json!({"tg": 300})),
            1 | 3 => log.push(LogCategory::Grant, format!("TSBK2 GRP_VCH_GRNT_UPD #{i}"),
                              serde_json::json!({"event_type": "GRP_VCH_GRNT_UPD"})),
            _ => log.push(LogCategory::Recorder, format!("call_saved #{i}"),
                          serde_json::json!({"recording_id": i})),
        }
    }
    log
}

fn seqs(v: &[LogEntry]) -> Vec<u64> {
    v.iter().map(|e| e.seq).collect()
}

#[test]
fn default_query_matches_the_pre_056_read() {
    let log = filled();
    let q = LogQuery { since: 5, limit: 3, ..Default::default() };
    assert_eq!(seqs(&log.query(&q)), seqs(&log.recent_since(5, 3)));
    assert_eq!(seqs(&log.query(&q)), vec![6, 7, 8]);
}

#[test]
fn tail_returns_newest_in_order() {
    let log = filled();
    let q = LogQuery { limit: 3, tail: true, ..Default::default() };
    assert_eq!(seqs(&log.query(&q)), vec![18, 19, 20]);
    // Filters apply before the limit.
    let q = LogQuery { limit: 2, tail: true, category: Some("recorder".into()), ..Default::default() };
    assert_eq!(seqs(&log.query(&q)), vec![14, 18]);
}

#[test]
fn tsbk_mirror_lines_can_be_excluded() {
    let log = filled();
    let q = LogQuery { limit: 100, include_tsbk: false, category: Some("grant".into()), ..Default::default() };
    let got = log.query(&q);
    assert_eq!(seqs(&got), vec![4, 8, 12, 16, 20], "only the follower's decisions");
    assert!(got.iter().all(|e| !LogQuery::is_tsbk(e)));
}

#[test]
fn time_window_is_inclusive() {
    let log = filled();
    let all = log.query(&LogQuery { limit: 100, ..Default::default() });
    let t = all[9].timestamp_ms;
    let q = LogQuery { limit: 100, from_ms: Some(t), to_ms: Some(t), ..Default::default() };
    assert!(log.query(&q).iter().all(|e| e.timestamp_ms == t));
    assert!(!log.query(&q).is_empty());
}
