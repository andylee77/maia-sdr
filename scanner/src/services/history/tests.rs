//! The history store (p25-httpd's tests on schema v2), the writer, and the v1 copy checked against
//! the Activity snapshots of the units' databases (phase 0).

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
    Range { site: "clay".into(), from_ms: T0, to_ms: T0 + 3 * H }
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
    let all = Range { site: "clay".into(), from_ms: T0, to_ms: T0 + 40 * H };
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
    assert_eq!(s.recording_files().unwrap(), vec!["a.wav".to_string()]);
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
    assert_eq!(s.latest_calls(2).unwrap().iter().map(|r| r.call_id).collect::<Vec<_>>(), vec![4, 3]);
}

#[test]
fn the_writer_batches_and_fills_in_the_voice_counts() {
    let dir = std::env::temp_dir().join(format!("scanner-history-{}-writer", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = HistoryConfig::default();
    let h = History::open(Some(&dir), &cfg, &[clay()]).unwrap();
    assert!(h.note.is_empty(), "nothing to copy");
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

// ── The v1 copy, against the Activity snapshots ──────────────────────

fn fixture_dirs() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.join("history.sql").exists()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// A fixture dump loaded into a v1 database file.
fn v1_from_dump(dir: &Path) -> PathBuf {
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    let path = temp(&format!("v1-{name}"));
    let sql = std::fs::read_to_string(dir.join("history.sql")).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    // The dump creates `call_units` before `calls`.
    conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
    conn.execute_batch(&sql).unwrap();
    path
}

/// v2's call fields under their v1 names, for the snapshots p25-httpd answered.
fn as_v1(mut call: Value) -> Value {
    let o = call.as_object_mut().unwrap();
    for (v2, v1) in [("lane", "chain"), ("frames", "imbe"), ("frame_errors", "vocoder_errors")] {
        let v = o.remove(v2).unwrap();
        o.insert(v1.into(), v);
    }
    for gone in ["timeslot", "codec", "end_kind"] {
        o.remove(gone);
    }
    call
}

fn calls_v1(rows: Vec<CallRow>) -> Value {
    Value::Array(rows.into_iter().map(|r| as_v1(json!(r))).collect())
}

/// The queries p25-httpd's snapshot test ran, in its shape.
fn snapshot(store: &Store) -> Value {
    let sites = store.sites().unwrap();
    let mut out = Map::new();
    out.insert("sites".into(), json!(sites));
    for s in &sites {
        let r = Range { site: s.site.clone(), from_ms: s.first_ms / H * H, to_ms: s.last_ms + H };
        let all = SeriesFilter::default();
        let tgs = store.talkgroups(&r, 50).unwrap();
        let radios = store.radios(&r, 50).unwrap();
        let mut q = Map::new();
        q.insert("window".into(), json!({"from_ms": r.from_ms, "to_ms": r.to_ms}));
        q.insert("summary".into(), json!(store.summary(&r).unwrap()));
        q.insert("talkgroups?limit=50".into(), json!(tgs));
        q.insert("radios?limit=50".into(), json!(radios));
        for t in tgs.iter().take(3) {
            q.insert(format!("talkgroup/{}", t.tg), json!(store.talkgroup(&r, t.tg).unwrap()));
            let f = SeriesFilter { tg: Some(t.tg), unit: None };
            q.insert(format!("series?bucket=hour&tg={}", t.tg), json!(store.series(&r, H, 0, f).unwrap()));
            q.insert(format!("calls?tg={}&limit=20", t.tg), calls_v1(store.calls(&r, f, 20).unwrap()));
        }
        for u in radios.iter().take(3) {
            q.insert(format!("radio/{}", u.unit), json!(store.radio(&r, u.unit).unwrap()));
            let f = SeriesFilter { tg: None, unit: Some(u.unit) };
            q.insert(format!("calls?unit={}&limit=20", u.unit), calls_v1(store.calls(&r, f, 20).unwrap()));
        }
        q.insert("series?bucket=hour".into(), json!(store.series(&r, H, 0, all).unwrap()));
        q.insert("series?bucket=day&tz=-240".into(), json!(store.series(&r, 24 * H, -240, all).unwrap()));
        q.insert("calls?limit=1000".into(), calls_v1(store.calls(&r, all, 1_000).unwrap()));
        out.insert(s.site.clone(), Value::Object(q));
    }
    Value::Object(out)
}

/// One line per query, as the snapshot files are written.
fn lines(snapshot: &Value) -> String {
    let mut out = String::from("{\n");
    let mut rows = Vec::new();
    for (site, queries) in snapshot.as_object().unwrap() {
        match queries.as_object() {
            Some(q) => rows.extend(q.iter().map(|(k, v)| (format!("{site} {k}"), v))),
            None => rows.push((site.clone(), queries)),
        }
    }
    let n = rows.len();
    for (i, (k, v)) in rows.into_iter().enumerate() {
        out += &format!("{}: {}{}\n", json!(k), v, if i + 1 < n { "," } else { "" });
    }
    out + "}\n"
}

#[test]
fn the_units_histories_copy_to_v2_with_the_same_activity_answers() {
    let dirs = fixture_dirs();
    assert!(dirs.len() >= 2, "unit fixtures: {dirs:?}");
    for dir in dirs {
        let v1 = v1_from_dump(&dir);
        assert!(migrate_v1::is_v1(&v1));
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let v2 = temp(&format!("v2-{name}"));
        let store = Store::open(&v2).unwrap();
        let sites = [SiteInfo { id: "clay".into(), system: "clay_county".into(), protocol: "p25".into(), label: "Clay".into(), system_label: "Clay County".into() }];
        let report = migrate_v1::migrate(&v1, &store, &sites).unwrap();
        assert!(report.calls > 100, "{report:?}");
        let got = lines(&snapshot(&store));
        let want = std::fs::read_to_string(dir.join("activity.json")).unwrap().replace("\r\n", "\n");
        if got != want {
            let first = got.lines().zip(want.lines()).position(|(a, b)| a != b);
            let line = first.map(|i| (got.lines().nth(i).unwrap_or("").chars().take(300).collect::<String>(), want.lines().nth(i).unwrap_or("").chars().take(300).collect::<String>()));
            panic!("{name}: Activity answers differ after the copy; first difference: {line:?}");
        }
        // Each system's talkgroups and radios, from the calls.
        let n: i64 = store.with_writer(|c| c.query_row("SELECT COUNT(*) FROM talkgroups", [], |r| r.get(0))).unwrap();
        assert!(n > 0);
        drop(store);
        remove_db(&v1);
        remove_db(&v2);
    }
}
