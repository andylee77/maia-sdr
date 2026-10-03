//! The history store and its writer.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::store::*;
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
        channel: Some("0-1189".into()),
        timeslot: None,
        emergency: false,
        private: false,
        first_voice_ms: None,
        lane: 1,
        encrypted: enc,
        followed: !enc,
        not_followed: enc.then(|| "encrypted".to_string()),
        voice_ms,
        grant_ms: 6_000,
        codec: Some("imbe".into()),
        frames: voice_ms / 20,
        frame_errors: 1,
        close_reason: "call_end".into(),
        end_kind: None,
    }
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

fn temp(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("scanner-history-{}-{tag}.sqlite", std::process::id()));
    remove_db(&path);
    path
}

fn clay() -> SiteInfo {
    SiteInfo { id: "clay".into(), system: "clay_county".into(), protocol: "p25".into(), label: "Clay".into(), system_label: "Clay County".into() }
}

fn store_at(tag: &str) -> Store {
    let s = Store::open(&temp(tag)).unwrap();
    s.note_site(&clay()).unwrap();
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
    Range::site("clay", T0, T0 + 3 * H)
}

#[test]
fn an_older_database_gains_the_new_columns_and_they_round_trip() {
    let path = temp("upgrade");
    drop(Store::open(&path).unwrap());
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "ALTER TABLE calls DROP COLUMN emergency; ALTER TABLE calls DROP COLUMN first_voice_ms;
             DROP TABLE alerts; UPDATE meta SET value = '2' WHERE key = 'schema';",
        )
        .unwrap();
    let s = Store::open(&path).unwrap();
    s.note_site(&clay()).unwrap();
    let row = CallRow { emergency: true, private: true, first_voice_ms: Some(T0 + 300), ..call(7, T0, 4001, &[101], 1_000, false) };
    assert_eq!(s.insert_calls(&[row.clone()]).unwrap(), 1);
    assert_eq!(s.call(7).unwrap(), Some(row));
    s.insert_alerts(&[alert(7, T0, 1014)]).unwrap();
    assert_eq!(s.all_alerts().unwrap().len(), 1);
    let schema: String =
        rusqlite::Connection::open(&path).unwrap().query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| r.get(0)).unwrap();
    assert_eq!(schema, "4");
}

/// A console's warble in call `call_id`, which started at `started`.
fn alert(call_id: u64, started: u64, source: u32) -> AlertRow {
    AlertRow {
        site: "clay".into(),
        call_id,
        call_started_ms: started,
        at_ms: started + 150,
        tg: 300,
        source: Some(source),
        lane: 1,
        kind: "warble".into(),
        tones_hz: vec![806.5, 1506.1],
        segments: 5,
        offset_ms: 40,
        duration_ms: 1_120,
    }
}

#[test]
fn alerts_are_listed_newest_first_and_go_with_their_calls() {
    let s = store_at("alerts");
    let (a1, a2) = (alert(1, T0, 1014), alert(3, T0 + 2 * H, 1012));
    let later = AlertRow { offset_ms: 4_000, at_ms: a2.at_ms + 3_960, kind: "pulsed".into(), tones_hz: vec![1010.4], ..a2.clone() };
    s.insert_alerts(&[a1.clone(), a2.clone(), later.clone()]).unwrap();
    s.insert_alerts(&[a1.clone()]).unwrap();
    assert_eq!(s.alerts(&range(), SeriesFilter::default(), 10).unwrap(), vec![later.clone(), a2.clone(), a1.clone()]);
    let from_1012 = s.alerts(&range(), SeriesFilter { tg: None, unit: Some(1012) }, 10).unwrap();
    assert_eq!(from_1012.len(), 2);
    assert_eq!(s.call_alerts(&Range::site("clay", T0 + H, T0 + 3 * H)).unwrap(), vec![a2, later]);
    assert_eq!(s.prune(T0 + H).unwrap(), 2);
    assert_eq!(s.all_alerts().unwrap().len(), 2, "pruned with their calls");
    s.clear().unwrap();
    assert!(s.all_alerts().unwrap().is_empty());
}

#[test]
fn inserts_are_idempotent_and_kept_per_system() {
    let s = store_at("idem");
    assert_eq!(s.insert_calls(&[call(1, T0, 300, &[101], 4_000, false)]).unwrap(), 0);
    assert_eq!(s.insert_calls(&[call(5, T0 + 10, 300, &[101], 100, false)]).unwrap(), 1);
    let site = &s.sites().unwrap()[0];
    assert_eq!((site.site.as_str(), site.calls, site.last_ms), ("clay", 5, T0 + 2 * H + 1));
    assert_eq!(s.max_call_id().unwrap(), 5);
    s.with_writer(|c| {
        let system: String = c.query_row("SELECT system FROM sites WHERE id = 'clay'", [], |r| r.get(0))?;
        assert_eq!(system, "clay_county");
        let (calls, enc, clear): (i64, Option<i64>, Option<i64>) = c.query_row(
            "SELECT calls, last_encrypted_ms, last_clear_ms FROM talkgroups WHERE system = 'clay_county' AND tg = 300",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!((calls, enc, clear), (3, None, Some((T0 + 60_000) as i64)));
        let radio: i64 = c.query_row("SELECT calls FROM radios WHERE system = 'clay_county' AND unit = 101", [], |r| r.get(0))?;
        assert_eq!(radio, 4);
        Ok(())
    })
    .unwrap();
}

#[test]
fn summary_and_talkgroups() {
    let s = store_at("summary");
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
    missed.not_followed = Some("busy".into());
    s.insert_calls(&[missed]).unwrap();
    let sum = s.summary(&range()).unwrap();
    assert!((sum.clear_grant_s - 6.0).abs() < 1e-9 && (sum.encrypted_grant_s - 6.0).abs() < 1e-9);
}

#[test]
fn radios_and_what_they_use() {
    let s = store_at("radios");
    let radios = s.radios(&range(), 10).unwrap();
    let r101 = radios.iter().find(|r| r.unit == 101).unwrap();
    // Primary in calls 1 and 3 (4 s + 1 s), secondary in call 2.
    assert_eq!((r101.calls, r101.talkgroups), (3, 2));
    assert!((r101.voice_s - 5.0).abs() < 1e-9 && r101.grant_s == 0.0);
    // The encrypted call's grant time goes to the radio granted.
    let r200 = radios.iter().find(|r| r.unit == 200).unwrap();
    assert!((r200.encrypted, r200.voice_s) == (1, 0.0) && (r200.grant_s - 6.0).abs() < 1e-9);
    let note = |first_ms, last_ms, count| UnitNote { unit: 101, tg: 305, kind: UnitEventKind::GroupAffiliation, first_ms, last_ms, count };
    s.note_units("clay", &[note(T0 + 500, T0 + 900, 3)]).unwrap();
    s.note_units("clay", &[note(T0 + 100, T0 + 400, 2)]).unwrap();
    let d = s.radio(&range(), 101).unwrap();
    assert_eq!(d.talkgroups.iter().map(|t| t.tg).collect::<Vec<_>>(), vec![300, 301]);
    assert_eq!(d.events.len(), 1);
    let e = &d.events[0];
    assert_eq!((e.tg, e.first_ms, e.last_ms, e.count), (305, T0 + 100, T0 + 900, 5));
    assert_eq!(s.talkgroup(&range(), 305).unwrap().affiliated_radios, 1);
}

#[test]
fn trimmed_to_size_oldest_hours_first() {
    let s = Store::open(&temp("trim")).unwrap();
    // A call a minute for 33 hours.
    let rows: Vec<CallRow> = (0..2000).map(|i| call(i, T0 + i * 60_000, 300 + (i % 7) as u32, &[100 + (i % 50) as u32], 1_000, false)).collect();
    s.insert_calls(&rows).unwrap();
    let used = s.used_bytes().unwrap();
    assert!(used > 100_000, "{used}");
    let gone = s.trim_to(used * 3 / 4).unwrap();
    assert!((400..2000).contains(&gone), "{gone}");
    assert!(s.used_bytes().unwrap() <= used * 3 / 4);
    // Whole hours went: the totals still match the calls.
    let site = s.sites().unwrap()[0].clone();
    assert_eq!(site.calls, 2000 - gone as u64);
    assert_eq!(site.first_ms % H, 0, "the oldest call left starts an hour");
    let all = Range::site("clay", T0, T0 + 40 * H);
    assert_eq!(s.summary(&all).unwrap().calls, site.calls);
    assert_eq!(site.last_ms, T0 + 1999 * 60_000, "the newest call stays");
    assert_eq!(s.trim_to(u64::MAX).unwrap(), 0);
}

#[test]
fn a_prune_takes_whole_hours_with_their_totals() {
    let s = store_at("prune");
    // Mid-hour: the hour it is in stays, calls and totals alike.
    assert_eq!(s.prune(T0 + 30 * 60_000).unwrap(), 0);
    assert_eq!(s.prune(T0 + H + 1).unwrap(), 2);
    assert_eq!(s.summary(&range()).unwrap().calls, 2);
    // Radios of pruned calls go with them.
    assert!(s.radios(&range(), 10).unwrap().iter().all(|r| r.unit != 102));
    assert_eq!(s.sites().unwrap()[0].calls, 2);
}

#[test]
fn recordings_link_to_their_calls() {
    let s = store_at("recordings");
    let rec = |file: &str, call_id, started_ms| RecordingRow {
        file: file.into(),
        store: "sd".into(),
        site: "clay".into(),
        call_id,
        started_ms,
        tg: 300,
        source: Some(102),
        bytes: 44,
        duration_ms: 0,
    };
    // p25-httpd's recorder stamped its own start, a little after the grant.
    let rows = [rec("a.wav", 2, T0 + 60_000 + 4_000), rec("b.wav", 3, T0 + 3 * H), rec("c.wav", 99, T0)];
    assert_eq!(s.add_recordings(&rows).unwrap(), 3);
    assert_eq!(s.add_recordings(&rows[..1]).unwrap(), 0, "listed once");
    let files: Vec<String> = ["a.wav", "b.wav", "c.wav"].map(String::from).to_vec();
    let info = s.recording_info(&files).unwrap();
    let a = &info["a.wav"];
    assert_eq!((a.freq_hz, a.channel.as_deref(), a.lane, a.units.clone(), a.frames), (Some(858_437_500), Some("0-1189"), 1, vec![102, 101], 100));
    assert!(!info.contains_key("b.wav"), "too far from its call's start");
    assert!(!info.contains_key("c.wav"));
    assert_eq!(s.remove_recordings(&files[1..]).unwrap(), 2);
    assert_eq!(s.recording_sizes().unwrap().into_keys().collect::<Vec<_>>(), vec!["a.wav".to_string()]);
}

#[test]
fn talkgroups_wider_than_16_bits() {
    let s = store_at("tg24");
    s.insert_calls(&[call(9, T0 + 60_000, 87_921, &[101], 1_000, false)]).unwrap();
    s.note_units("clay", &[UnitNote { unit: 101, tg: 87_926, kind: UnitEventKind::GroupAffiliation, first_ms: T0, last_ms: T0, count: 1 }]).unwrap();
    let only = s.calls(&range(), SeriesFilter { tg: Some(87_921), unit: None }, 10).unwrap();
    assert_eq!(only.iter().map(|r| r.tg).collect::<Vec<u32>>(), vec![87_921]);
    assert_eq!(s.talkgroup(&range(), 87_921).unwrap().calls, 1);
    assert_eq!(s.radio(&range(), 101).unwrap().events[0].tg, 87_926);
}

#[test]
fn a_system_range_covers_its_sites() {
    let s = store_at("system");
    s.note_site(&SiteInfo { id: "clay_2".into(), ..clay() }).unwrap();
    s.note_site(&SiteInfo { id: "duval".into(), system: "duval".into(), ..clay() }).unwrap();
    let elsewhere = |id, site: &str| CallRow { site: site.into(), ..call(id, T0 + 30_000, 300, &[101], 1_000, false) };
    s.insert_calls(&[elsewhere(10, "clay_2"), elsewhere(11, "duval")]).unwrap();
    let system = Range { site: "clay_county".into(), by_system: true, ..range() };
    assert_eq!(s.summary(&system).unwrap().calls, 5);
    assert_eq!(s.summary(&range()).unwrap().calls, 4);
    assert_eq!(s.talkgroups(&system, 10).unwrap()[0].calls, 3);
    assert_eq!(s.calls(&system, SeriesFilter { tg: None, unit: Some(101) }, 10).unwrap().len(), 4);
    assert_eq!(s.radio(&system, 101).unwrap().talkgroups[0].calls, 3);
    assert_eq!(s.talkgroup(&system, 300).unwrap().calls, 3);
    assert_eq!(s.series(&system, H, 0, SeriesFilter::default()).unwrap()[0].calls, 3);
}

#[test]
fn encryption_history_per_talkgroup() {
    let s = store_at("enc");
    s.insert_calls(&[call(6, T0 + H, 403, &[200], 0, true), call(7, T0 + H + 5, 403, &[201], 900, false)]).unwrap();
    let d = s.talkgroup(&range(), 403).unwrap();
    assert_eq!((d.calls, d.encrypted), (3, 2));
    assert_eq!(d.first_encrypted_ms, Some(T0 + H));
    assert_eq!(d.last_encrypted_ms, Some(T0 + 2 * H + 1));
    assert_eq!(d.last_clear_ms, Some(T0 + H + 5));
}

#[test]
fn series_buckets_with_gaps_and_filters() {
    let s = store_at("series");
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
fn calls_listing_newest_first_with_their_radios() {
    let s = store_at("calls");
    let rows = s.calls(&range(), SeriesFilter { tg: Some(300), unit: None }, 10).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].call_id, 2, "newest first");
    assert_eq!(rows[0].sources, vec![102, 101]);
    assert_eq!(rows[0], call(2, T0 + 60_000, 300, &[102, 101], 2_000, false));
    let by101 = s.calls(&range(), SeriesFilter { tg: None, unit: Some(101) }, 10).unwrap();
    assert_eq!(by101.iter().map(|r| r.call_id).collect::<Vec<_>>(), vec![3, 2, 1]);
    assert_eq!(s.latest_calls("clay", 2).unwrap().iter().map(|r| r.call_id).collect::<Vec<_>>(), vec![4, 3]);
    assert!(s.latest_calls("cec_gcs", 2).unwrap().is_empty());
    assert_eq!(s.call(2).unwrap().map(|r| r.sources), Some(vec![102, 101]));
    assert_eq!(s.call(99).unwrap(), None);
    let mut csv = String::new();
    s.export_csv(&range(), SeriesFilter::default(), 10, |c| {
        csv.push_str(&c);
        true
    })
    .unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], CSV_HEADER.trim_end());
    assert_eq!(lines.len(), 5, "the header and the four calls");
    assert!(lines[1].starts_with("clay,4,") && lines[4].starts_with("clay,1,"), "newest first");
}

#[test]
fn the_writer_batches_and_fills_in_the_voice_counts() {
    let dir = std::env::temp_dir().join(format!("scanner-history-{}-writer", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = HistoryConfig::default();
    let h = History::open(Some(&dir), &cfg, &[clay()]).unwrap();
    let tx = h.sender();
    let mut c = call(1, T0, 300, &[101], 2_000, false);
    c.frame_errors = 0;
    tx.call(c);
    tx.voice(VoiceResult { site: "clay".into(), call_id: 1, started_ms: T0, frames: 100, frame_errors: 7 });
    tx.unit("clay", 101, 300, UnitEventKind::GroupAffiliation, T0);
    tx.unit("clay", 101, 300, UnitEventKind::GroupAffiliation, T0 + 5);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(h.flush(Duration::from_secs(5)));
    let rows = h.store().calls(&range(), SeriesFilter::default(), 10).unwrap();
    assert_eq!((rows[0].frames, rows[0].frame_errors), (100, 7));
    // A voice count after the commit updates the stored call.
    tx.voice(VoiceResult { site: "clay".into(), call_id: 1, started_ms: T0, frames: 100, frame_errors: 9 });
    rt.block_on(h.flush(Duration::from_secs(5)));
    assert_eq!(h.store().calls(&range(), SeriesFilter::default(), 10).unwrap()[0].frame_errors, 9);
    let e = &h.store().radio(&range(), 101).unwrap().events[0];
    assert_eq!((e.count, e.first_ms, e.last_ms), (2, T0, T0 + 5));
}
