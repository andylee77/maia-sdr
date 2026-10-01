//! Activity snapshots of the units' histories (076 phase 0).
//!
//! Runs the Activity queries on each committed fixture database
//! (`scanner/tests/fixtures/unit_*/history.sql`) and compares the results with `activity.json`
//! beside it. The fresh crate's history v2 must give the same answers after it migrates these
//! databases (076 phase 5). `ACTIVITY_SNAPSHOT_BLESS=1` rewrites the snapshots.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::*;

fn unit_dirs() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scanner/tests/fixtures");
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path())
        .filter(|p| p.join("history.sql").exists())
        .collect();
    dirs.sort();
    dirs
}

/// A fixture database loaded into a scratch file.
fn fixture_store(dir: &Path) -> (HistoryStore, PathBuf) {
    let sql = std::fs::read_to_string(dir.join("history.sql")).unwrap();
    let name = dir.file_name().unwrap().to_string_lossy();
    let path = std::env::temp_dir().join(format!("activity-snapshot-{}-{name}.sqlite", std::process::id()));
    remove_db(&path);
    // The dump creates tables in name order, `call_units` before `calls`.
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
    conn.execute_batch(&sql).unwrap();
    drop(conn);
    (HistoryStore::open(&path, true).unwrap(), path)
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// One line per query, so a difference shows which answer changed.
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

fn snapshot(store: &HistoryStore) -> Value {
    let sites = store.sites().unwrap();
    let mut out = Map::new();
    out.insert("sites".into(), json!(sites));
    for s in &sites {
        // The whole stored span, from the hour of the first call.
        let r = Range { site: s.site.clone(), from_ms: s.first_ms / HOUR_MS * HOUR_MS,
                        to_ms: s.last_ms + HOUR_MS };
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
            q.insert(format!("series?bucket=hour&tg={}", t.tg), json!(store.series(&r, HOUR_MS, 0, f).unwrap()));
            q.insert(format!("calls?tg={}&limit=20", t.tg), json!(store.calls(&r, f, 20).unwrap()));
        }
        for u in radios.iter().take(3) {
            q.insert(format!("radio/{}", u.unit), json!(store.radio(&r, u.unit).unwrap()));
            let f = SeriesFilter { tg: None, unit: Some(u.unit) };
            q.insert(format!("calls?unit={}&limit=20", u.unit), json!(store.calls(&r, f, 20).unwrap()));
        }
        q.insert("series?bucket=hour".into(), json!(store.series(&r, HOUR_MS, 0, all).unwrap()));
        q.insert("series?bucket=day&tz=-240".into(), json!(store.series(&r, 24 * HOUR_MS, -240, all).unwrap()));
        q.insert("calls?limit=1000".into(), json!(store.calls(&r, all, 1_000).unwrap()));
        out.insert(s.site.clone(), Value::Object(q));
    }
    Value::Object(out)
}

#[test]
fn activity_queries_on_the_unit_fixtures_match_their_snapshots() {
    let bless = std::env::var("ACTIVITY_SNAPSHOT_BLESS").is_ok_and(|v| v == "1");
    for dir in unit_dirs() {
        let (store, path) = fixture_store(&dir);
        let got = lines(&snapshot(&store));
        drop(store);
        remove_db(&path);
        let file = dir.join("activity.json");
        if bless {
            std::fs::write(&file, got).unwrap();
            continue;
        }
        let want = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("{}: {e} (bless with ACTIVITY_SNAPSHOT_BLESS=1)", file.display()));
        assert!(want.replace("\r\n", "\n") == got, "Activity answers differ from {}", file.display());
    }
}
