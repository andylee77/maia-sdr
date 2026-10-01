//! Host tests for `services::history` (change 072).

use super::*;

const H: u64 = 3_600_000;
const T0: u64 = 1_789_999_200_000; // on an hour boundary (UTC)

fn call(call_id: u64, at: u64, tg: u32, sources: &[u32], voice_ms: u64, enc: bool) -> CallRow {
    CallRow {
        site: "clay".into(),
        call_id,
        started_ms: at,
        ended_ms: at + 5_000,
        tg,
        source: sources.first().copied(),
        sources: sources.to_vec(),
        freq_hz: Some(858_437_500),
        channel: Some("1189".into()),
        chain: 1,
        encrypted: enc,
        followed: !enc,
        not_followed: enc.then(|| "encrypted".to_string()),
        voice_ms,
        grant_ms: 6_000,
        imbe: voice_ms / 20,
        vocoder_errors: 1,
        close_reason: "call_end".into(),
    }
}

fn store() -> HistoryStore {
    let s = HistoryStore::open_memory().unwrap();
    s.insert_calls(&[
        call(1, T0, 300, &[101], 4_000, false),
        call(2, T0 + 60_000, 300, &[102, 101], 2_000, false),
        call(3, T0 + 2 * H, 301, &[101], 1_000, false),
        call(4, T0 + 2 * H + 1, 403, &[200], 0, true), // encrypted: grant time only
    ])
    .unwrap();
    s
}

fn range() -> Range {
    Range { site: "clay".into(), from_ms: T0, to_ms: T0 + 3 * H }
}

#[test]
fn inserts_are_idempotent() {
    let s = store();
    assert_eq!(s.insert_calls(&[call(1, T0, 300, &[101], 4_000, false)]).unwrap(), 0);
    assert_eq!(s.insert_calls(&[call(5, T0 + 10, 300, &[101], 100, false)]).unwrap(), 1);
    assert_eq!(s.last_started_ms("clay").unwrap(), Some(T0 + 2 * H + 1));
    assert_eq!(s.last_started_ms("duval").unwrap(), None);
}

#[test]
fn summary_and_talkgroups() {
    let s = store();
    let sum = s.summary(&range()).unwrap();
    assert_eq!((sum.calls, sum.followed, sum.encrypted, sum.talkgroups, sum.radios), (4, 3, 1, 3, 3));
    // Voice and grant time are kept apart: 4 + 2 + 1 decoded, 6 granted.
    assert!((sum.voice_s - 7.0).abs() < 1e-9, "{}", sum.voice_s);
    assert!((sum.encrypted_grant_s - 6.0).abs() < 1e-9);
    assert_eq!(sum.clear_grant_s, 0.0);
    // Decoded voice per grant second on the followed calls: 7 / 18.
    assert!((sum.voice_per_grant.unwrap() - 7.0 / 18.0).abs() < 1e-9);
    let tgs = s.talkgroups(&range(), 10).unwrap();
    assert_eq!(tgs.iter().map(|t| t.tg).collect::<Vec<_>>(), vec![300, 403, 301], "most time first, then most calls");
    let t300 = &tgs[0];
    assert_eq!((t300.calls, t300.radios, t300.encrypted), (2, 2, 0));
    assert!((t300.voice_s - 6.0).abs() < 1e-9 && t300.grant_s == 0.0);
    assert!(tgs[1].voice_s == 0.0 && (tgs[1].grant_s - 6.0).abs() < 1e-9);
    // A clear call that was not followed: grant time, not encrypted.
    let mut missed = call(8, T0 + 5, 302, &[103], 0, false);
    missed.followed = false;
    missed.not_followed = Some("no_chain".into());
    s.insert_calls(&[missed]).unwrap();
    let sum = s.summary(&range()).unwrap();
    assert!((sum.clear_grant_s - 6.0).abs() < 1e-9 && (sum.encrypted_grant_s - 6.0).abs() < 1e-9);
}

#[test]
fn radios_and_what_they_use() {
    let s = store();
    let radios = s.radios(&range(), 10).unwrap();
    let r101 = radios.iter().find(|r| r.unit == 101).unwrap();
    // Primary in calls 1 and 3 (4 s + 1 s), secondary in call 2.
    assert_eq!((r101.calls, r101.talkgroups), (3, 2));
    assert!((r101.voice_s - 5.0).abs() < 1e-9 && r101.grant_s == 0.0);
    // The encrypted call's grant time goes to the radio granted.
    let r200 = radios.iter().find(|r| r.unit == 200).unwrap();
    assert!((r200.encrypted, r200.voice_s) == (1, 0.0) && (r200.grant_s - 6.0).abs() < 1e-9);
    s.note_unit("clay", 101, 305, UnitEventKind::GroupAffiliation, T0).unwrap();
    s.note_unit("clay", 101, 305, UnitEventKind::GroupAffiliation, T0 + 1000).unwrap();
    let d = s.radio(&range(), 101).unwrap();
    assert_eq!(d.talkgroups.iter().map(|t| t.tg).collect::<Vec<_>>(), vec![300, 301]);
    assert_eq!(d.events.len(), 1);
    assert_eq!((d.events[0].tg, d.events[0].count, d.events[0].last_ms), (305, 2, T0 + 1000));
    let tg = s.talkgroup(&range(), 305).unwrap();
    assert_eq!(tg.affiliated_radios, 1);
}

#[test]
fn trimmed_to_size_oldest_first() {
    let s = HistoryStore::open_memory().unwrap();
    // A call a minute for 33 hours (the hourly totals go by whole hours).
    let rows: Vec<CallRow> = (0..2000).map(|i| call(i, T0 + i * 60_000, 300 + (i % 7) as u32, &[100 + (i % 50) as u32], 1_000, false)).collect();
    s.insert_calls(&rows).unwrap();
    let used = s.used_bytes().unwrap();
    assert!(used > 100_000, "{used}");
    // Pages keyed by talkgroup or radio stay part full: half the calls
    // free about a third of the space.
    let gone = s.trim_to(used * 3 / 4).unwrap();
    assert!(gone >= 400 && gone < 2000, "{gone}");
    assert!(s.used_bytes().unwrap() <= used * 3 / 4);
    // The newest call stays.
    assert_eq!(s.last_started_ms("clay").unwrap(), Some(T0 + 1999 * 60_000));
    assert_eq!(s.sites().unwrap()[0].calls, 2000 - gone as u64);
    assert_eq!(s.trim_to(u64::MAX).unwrap(), 0);
}

#[test]
fn recording_info_by_call_id() {
    let s = store();
    let i = s.recording_info(2, T0 + 60_000 + 4_000).unwrap().unwrap();
    assert_eq!(
        (i.site.as_str(), i.freq_hz, i.channel.as_deref(), i.chain, i.units),
        ("clay", Some(858_437_500), Some("1189"), 1, vec![102, 101])
    );
    assert_eq!(s.recording_info(3, T0 + 3 * H).unwrap(), None, "too far from its start");
    assert_eq!(s.recording_info(99, T0).unwrap(), None);
}

/// A database made before change 074a gets the channel column.
#[test]
fn old_database_gains_the_channel_column() {
    let path = std::env::temp_dir().join(format!("p25-history-074a-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(&SCHEMA.replace("    channel TEXT,\n", "")).unwrap();
        let cols: i64 = c.query_row("SELECT COUNT(*) FROM pragma_table_info('calls') WHERE name = 'channel'", [], |r| r.get(0)).unwrap();
        assert_eq!(cols, 0, "the old schema has no channel column");
    }
    let s = HistoryStore::open(&path, false).unwrap();
    s.insert_calls(&[call(1, T0, 300, &[101], 1_000, false)]).unwrap();
    assert_eq!(s.recording_info(1, T0).unwrap().unwrap().channel.as_deref(), Some("1189"));
    drop(s);
    let _ = std::fs::remove_file(&path);
}

/// Change 075a: talkgroups are u32. Rows an older build stored (the TG
/// bound from a u16: a plain INTEGER) read back the same, and a 24-bit
/// (DMR Tier III) talkgroup goes in and out of every table.
#[test]
fn talkgroups_wider_than_16_bits() {
    let path = std::env::temp_dir().join(format!("p25-history-075a-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute(
            "INSERT INTO calls (site, call_id, started_ms, ended_ms, tg, encrypted, followed, voice_ms, grant_ms)
             VALUES ('clay', 1, ?1, ?2, ?3, 0, 1, 1000, 2000)",
            rusqlite::params![T0 as i64, (T0 + 5_000) as i64, 65_535u16],
        )
        .unwrap();
        c.execute(
            "INSERT INTO hour_tg (site, t, tg, calls, followed, encrypted, voice_ms, clear_grant_ms, enc_grant_ms,
             voiced_grant_ms, first_ms, last_ms) VALUES ('clay', ?1, ?2, 1, 1, 0, 1000, 2000, 0, 2000, ?1, ?1)",
            rusqlite::params![T0 as i64, 65_535u16],
        )
        .unwrap();
    }
    let s = HistoryStore::open(&path, false).unwrap();
    s.insert_calls(&[call(2, T0 + 60_000, 87_921, &[101], 1_000, false)]).unwrap();
    s.note_unit("clay", 101, 87_926, UnitEventKind::GroupAffiliation, T0 + 1).unwrap();
    let rows = s.calls(&range(), SeriesFilter::default(), 10).unwrap();
    assert_eq!(rows.iter().map(|r| r.tg).collect::<Vec<u32>>(), vec![87_921, 65_535]);
    let mut tgs: Vec<u32> = s.talkgroups(&range(), 10).unwrap().iter().map(|t| t.tg).collect();
    tgs.sort();
    assert_eq!(tgs, vec![65_535, 87_921]);
    let only = s.calls(&range(), SeriesFilter { tg: Some(87_921), unit: None }, 10).unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(s.talkgroup(&range(), 87_921).unwrap().calls, 1);
    let r = s.radio(&range(), 101).unwrap();
    assert_eq!(r.talkgroups[0].tg, 87_921);
    assert_eq!(r.events[0].tg, 87_926);
    drop(s);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn batched_radio_events_merge() {
    let s = store();
    let note = |first_ms, last_ms, count| UnitNote { unit: 101, tg: 305, kind: UnitEventKind::GroupAffiliation, first_ms, last_ms, count };
    s.note_units("clay", &[note(T0 + 500, T0 + 900, 3)]).unwrap();
    s.note_units("clay", &[note(T0 + 100, T0 + 400, 2)]).unwrap();
    let e = &s.radio(&range(), 101).unwrap().events[0];
    assert_eq!((e.first_ms, e.last_ms, e.count), (T0 + 100, T0 + 900, 5));
}

#[test]
fn encryption_history_per_talkgroup() {
    let s = store();
    s.insert_calls(&[call(6, T0 + H, 403, &[200], 0, true), call(7, T0 + H + 5, 403, &[201], 900, false)]).unwrap();
    let d = s.talkgroup(&range(), 403).unwrap();
    assert_eq!((d.calls, d.encrypted), (3, 2));
    assert_eq!(d.first_encrypted_ms, Some(T0 + H));
    assert_eq!(d.last_encrypted_ms, Some(T0 + 2 * H + 1));
    assert_eq!(d.last_clear_ms, Some(T0 + H + 5));
}

#[test]
fn series_buckets_with_gaps_and_filters() {
    let s = store();
    let b = s.series(&range(), H, 0, SeriesFilter::default()).unwrap();
    assert_eq!(b.len(), 3, "{b:?}");
    assert_eq!((b[0].calls, b[1].calls, b[2].calls), (2, 0, 2));
    assert_eq!(b[2].encrypted, 1);
    assert!((b[2].encrypted_grant_s - 6.0).abs() < 1e-9 && (b[2].voice_s - 1.0).abs() < 1e-9);
    let only301 = s.series(&range(), H, 0, SeriesFilter { tg: Some(301), unit: None }).unwrap();
    assert_eq!(only301.iter().map(|x| x.calls).sum::<u64>(), 1);
    let by102 = s.series(&range(), H, 0, SeriesFilter { tg: None, unit: Some(102) }).unwrap();
    assert_eq!(by102.iter().map(|x| x.calls).sum::<u64>(), 1);
    // Day buckets aligned to a local offset (UTC-4): starts at local midnight.
    let d = s.series(&Range { from_ms: T0 - 24 * H, ..range() }, 24 * H, -240, SeriesFilter::default()).unwrap();
    assert!(d.iter().all(|x| (x.t as i64 - 240 * 60_000).rem_euclid(24 * H as i64) == 0), "{d:?}");
    assert_eq!(d.iter().map(|x| x.calls).sum::<u64>(), 4);
}

#[test]
fn calls_listing_csv_and_prune() {
    let s = store();
    let rows = s.calls(&range(), SeriesFilter { tg: Some(300), unit: None }, 10).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].call_id, 2, "newest first");
    assert_eq!(rows[0].sources, vec![102, 101]);
    let csv = to_csv(&rows);
    assert!(csv.starts_with("site,call_id,started_utc"));
    assert_eq!(csv.lines().count(), 3);
    assert_eq!(iso_utc(0), "1970-01-01 00:00:00");
    assert_eq!(s.prune(T0 + H).unwrap(), 2);
    assert_eq!(s.summary(&range()).unwrap().calls, 2);
    // Units of pruned calls go with them.
    assert!(s.radios(&range(), 10).unwrap().iter().all(|r| r.unit != 102));
    assert_eq!(s.sites().unwrap()[0].calls, 2);
}

/// Query times on a month of a busy site (10k calls a day). Run with
/// `cargo test --release history_month_bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn history_month_bench() {
    let path = std::env::temp_dir().join("p25-history-bench.sqlite");
    let _ = std::fs::remove_file(&path);
    let s = HistoryStore::open(&path, true).unwrap();
    let t = std::time::Instant::now();
    for day in 0..30u64 {
        let rows: Vec<CallRow> = (0..10_000u64)
            .map(|i| {
                let n = day * 10_000 + i;
                call(n, T0 + day * 24 * H + i * 8_000, 300 + (n % 40) as u32, &[3_400_000 + (n * 7 % 900) as u32, 3_400_000 + (n % 900) as u32], if n % 3 == 0 { 0 } else { 4_000 }, n % 3 == 0)
            })
            .collect();
        for chunk in rows.chunks(20) {
            s.insert_calls(chunk).unwrap();
        }
    }
    println!("insert 300k calls: {:?}, {} MB", t.elapsed(), s.used_bytes().unwrap() >> 20);
    let month = Range { site: "clay".into(), from_ms: T0, to_ms: T0 + 30 * 24 * H };
    let day = Range { site: "clay".into(), from_ms: T0 + 29 * 24 * H, to_ms: T0 + 30 * 24 * H };
    for (name, q) in [("day", &day), ("month", &month)] {
        let t = std::time::Instant::now();
        s.summary(q).unwrap();
        let a = t.elapsed();
        s.series(q, if name == "day" { H } else { 24 * H }, -240, SeriesFilter::default()).unwrap();
        let b = t.elapsed();
        s.talkgroups(q, 25).unwrap();
        let c = t.elapsed();
        s.radios(q, 25).unwrap();
        let d = t.elapsed();
        s.calls(q, SeriesFilter::default(), 50).unwrap();
        let e = t.elapsed();
        s.radio(q, 3_400_007).unwrap();
        s.talkgroup(q, 305).unwrap();
        let f = t.elapsed();
        println!("{name}: summary {a:?} series {:?} talkgroups {:?} radios {:?} calls {:?} details {:?}", b - a, c - b, d - c, e - d, f - e);
    }
    drop(s);
    let _ = std::fs::remove_file(&path);
}

