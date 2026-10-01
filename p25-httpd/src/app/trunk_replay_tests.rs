//! Replay of recorded trunking traces through the call lifecycle (076 phase 0).
//!
//! A trace (`app::trunk_trace`) holds the lifecycle's inputs as the unit saw them. Fed back here
//! with the lifecycle's clock pinned to each event's receipt time, it yields the calls the old
//! lifecycle makes of that traffic. The result is stored next to the trace as the expected call
//! list; the fresh crate's follower and CallBook must reproduce it in phase 3, apart from the
//! intended differences listed in the 076 design.
//!
//! Scenarios live in `scanner/tests/fixtures/replay/<name>/` (`trace.jsonl`, `expected.json`).
//! `TRUNK_REPLAY_BLESS=1` rewrites `expected.json`; `TRUNK_TRACE=<path>` replays any trace and
//! prints how its calls compare with the calls the unit itself recorded in the trace.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::*;
use crate::app::imbe_forwarder::ForwarderShared;
use crate::audio::TerminatorKind;

/// One call as the lifecycle closed it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct CallRecord {
    call: u64,
    tg: u32,
    lane: Option<u8>,
    freq: Option<u64>,
    encrypted: bool,
    not_followed: Option<String>,
    opened_via: String,
    sources: Vec<u32>,
    start: u64,
    end: u64,
    close: String,
    end_lc: Option<String>,
    /// Voice frames (20 ms chunks) the decoder attributed to the call.
    frames: u64,
}

fn statics() -> impl FnMut(&str) -> &'static str {
    let mut seen: HashMap<String, &'static str> = HashMap::new();
    move |s: &str| *seen.entry(s.to_string()).or_insert_with(|| Box::leak(s.to_string().into_boxed_str()))
}

fn lane_of(v: &Value) -> Option<Lane> {
    match v.as_u64() {
        Some(1) => Some(Lane::One),
        Some(2) => Some(Lane::Two),
        _ => None,
    }
}

fn u32_of(v: &Value) -> Option<u32> {
    v.as_u64().map(|x| x as u32)
}

fn terminator(s: &str) -> TerminatorKind {
    match s {
        "MotTalkComplete" => TerminatorKind::MotTalkComplete,
        "CallTermination" => TerminatorKind::CallTermination,
        _ => TerminatorKind::BareTdu,
    }
}

enum Input {
    Boundary(CallBoundary),
    Audio(AudioChunk),
}

fn input(e: &Value, intern: &mut impl FnMut(&str) -> &'static str) -> Option<Input> {
    let lane = lane_of(&e["lane"]);
    let nac = e["nac"].as_u64().unwrap_or(0) as u16;
    let boundary = |kind| Some(Input::Boundary(CallBoundary {
        kind,
        nac,
        talkgroup: None,
        expected_submit_count: 0,
        lane,
    }));
    match e["ev"].as_str()? {
        "grant" => boundary(CallBoundaryKind::CcGrantArrival {
            tg: u32_of(&e["tg"])?,
            source: u32_of(&e["src"]),
            freq_hz: e["freq"].as_u64(),
            channel: e["ch"].as_u64().unwrap_or(0) as u16,
            encrypted: e["enc"].as_bool().unwrap_or(false),
            not_followed: e["nf"].as_str().map(&mut *intern),
        }),
        "grant_update" => boundary(CallBoundaryKind::CcGrantUpdate {
            tg: u32_of(&e["tg"])?,
            freq_hz: e["freq"].as_u64(),
            channel: e["ch"].as_u64().unwrap_or(0) as u16,
        }),
        "hdu" => boundary(CallBoundaryKind::HduStart),
        "lc_source" => boundary(CallBoundaryKind::TdulcComplete { source: u32_of(&e["src"]) }),
        "speaker_end" => boundary(CallBoundaryKind::SpeakerEnd {
            source: u32_of(&e["src"]),
            kind: terminator(e["kind"].as_str().unwrap_or("")),
        }),
        "nid" => boundary(CallBoundaryKind::TrafficNidObserved { voice: e["voice"].as_bool()? }),
        "voice_end" => boundary(CallBoundaryKind::VoiceEnd {
            call_id: e["call"].as_u64()?,
            air_ms: e["air"].as_u64()?,
            lc: intern(e["lc"].as_str()?),
        }),
        "audio" => Some(Input::Audio(AudioChunk {
            pcm: [0; 160],
            talkgroup: u32_of(&e["tg"])?,
            source: u32_of(&e["src"]).unwrap_or(0),
            call_id: e["call"].as_u64()?,
            captured_at_ms: e["air"].as_u64()?,
            airtime: e["airtime"].as_bool().unwrap_or(false),
            lane: lane?,
        })),
        _ => None,
    }
}

fn read_trace(path: &Path) -> (Value, Vec<Value>) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut lines = text.lines().filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("trace line is JSON"));
    let header = lines.next().expect("trace has a header");
    assert_eq!(header["ev"], "start", "first line is the trace header");
    (header, lines.collect())
}

/// Collects open and close events into call records.
#[derive(Default)]
struct Book {
    open: BTreeMap<u64, Value>,
    done: Vec<CallRecord>,
}

impl Book {
    fn event(&mut self, e: &Value) {
        let Some(call) = e["call"].as_u64() else { return };
        match e["ev"].as_str() {
            Some("open") => {
                self.open.insert(call, e.clone());
            }
            Some("close") => {
                let Some(o) = self.open.remove(&call) else { return };
                self.done.push(CallRecord {
                    call,
                    tg: u32_of(&o["tg"]).unwrap_or(0),
                    lane: o["lane"].as_u64().map(|l| l as u8),
                    freq: o["freq"].as_u64(),
                    encrypted: o["enc"].as_bool().unwrap_or(false),
                    not_followed: o["nf"].as_str().map(str::to_string),
                    opened_via: o["via"].as_str().unwrap_or("").to_string(),
                    sources: e["sources"].as_array().map(|a| a.iter().filter_map(u32_of).collect())
                        .unwrap_or_default(),
                    start: e["start"].as_u64().unwrap_or(0),
                    end: e["end"].as_u64().unwrap_or(0),
                    close: e["reason"].as_str().unwrap_or("").to_string(),
                    end_lc: e["end_lc"].as_str().map(str::to_string),
                    frames: 0,
                });
            }
            _ => {}
        }
    }

    fn records(mut self, frames: &HashMap<u64, u64>) -> Vec<CallRecord> {
        for r in &mut self.done {
            r.frames = frames.get(&r.call).copied().unwrap_or(0);
        }
        self.done.sort_by_key(|r| r.call);
        self.done
    }
}

fn tracker_json(e: &CallTrackerEvent, t: u64) -> Value {
    crate::app::trunk_trace::tracker_json(t, e)
}

/// Replays `events` through the lifecycle; returns the calls it closed.
fn replay(header: &Value, events: &[Value]) -> Vec<CallRecord> {
    let lanes = header["lanes"].as_u64().unwrap_or(1).clamp(1, 2) as usize;
    let hang_ms = header["hang_ms"].as_u64().unwrap_or(3_000);
    let grace_ms = header["end_grace_ms"].as_u64().unwrap_or(2_000);
    let shared = ForwarderShared::default();
    let mut slots: Vec<Slot> = Lane::ALL[..lanes].iter().map(|&l| {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let fwd = Arc::new(crate::app::imbe_forwarder::ImbeForwarder::with_lane(tx, l, &shared));
        Slot::new(fwd, new_active_call_shared())
    }).collect();
    let mut nf = NfCalls::default();
    let mut next_id = header["first_call_id"].as_u64().unwrap_or(1).max(1);
    let tx = new_event_tx();
    let mut rx = tx.subscribe();
    let mut dedup = HashMap::new();
    let mut intern = statics();
    let mut book = Book::default();
    let mut frames: HashMap<u64, u64> = HashMap::new();

    let t0 = header["t"].as_u64().unwrap_or(0);
    let mut next_tick = t0 + TIMEOUT_TICK_MS;
    let drain = |rx: &mut broadcast::Receiver<CallTrackerEvent>, book: &mut Book, t: u64| {
        while let Ok(e) = rx.try_recv() {
            book.event(&tracker_json(&e, t));
        }
    };
    for e in events {
        let Some(t) = e["t"].as_u64() else { continue };
        while next_tick <= t {
            test_clock::set(Some(next_tick));
            sweep(&mut slots, &mut nf, &mut next_id, &tx, next_tick, hang_ms, grace_ms);
            drain(&mut rx, &mut book, next_tick);
            next_tick += TIMEOUT_TICK_MS;
        }
        test_clock::set(Some(t));
        match input(e, &mut intern) {
            Some(Input::Boundary(b)) => {
                handle_boundary(b, &mut slots, &mut nf, &mut next_id, &tx, &mut dedup, hang_ms);
            }
            Some(Input::Audio(c)) => {
                *frames.entry(c.call_id).or_default() += 1;
                handle_audio(c, &mut slots);
            }
            None => {}
        }
        drain(&mut rx, &mut book, t);
    }
    // Let every open call reach its close.
    let end = next_tick + hang_ms + grace_ms + 1_000;
    while next_tick <= end {
        test_clock::set(Some(next_tick));
        sweep(&mut slots, &mut nf, &mut next_id, &tx, next_tick, hang_ms, grace_ms);
        drain(&mut rx, &mut book, next_tick);
        next_tick += TIMEOUT_TICK_MS;
    }
    test_clock::set(None);
    book.records(&frames)
}

/// The calls the unit itself closed during the trace.
fn recorded(events: &[Value]) -> Vec<CallRecord> {
    let mut book = Book::default();
    let mut frames: HashMap<u64, u64> = HashMap::new();
    for e in events {
        if e["ev"] == "audio" {
            if let Some(c) = e["call"].as_u64() {
                *frames.entry(c).or_default() += 1;
            }
        }
        book.event(e);
    }
    book.records(&frames)
}

/// The tap and the lifecycle stamp the same event a few ms apart, and the replay's sweep runs on
/// its own 100 ms phase.
const TIMING_SLACK_MS: u64 = 150;

/// The same call, its times within `TIMING_SLACK_MS`.
fn same_call(a: &CallRecord, b: &CallRecord) -> bool {
    a.start.abs_diff(b.start) <= TIMING_SLACK_MS
        && a.end.abs_diff(b.end) <= TIMING_SLACK_MS
        && CallRecord { start: 0, end: 0, ..a.clone() } == CallRecord { start: 0, end: 0, ..b.clone() }
}

/// How a replay compares with the unit's own calls: (same call, same id with other fields,
/// only in the replay, only on the unit).
fn compare(replayed: &[CallRecord], unit: &[CallRecord]) -> (usize, usize, usize, usize) {
    let by_id: HashMap<u64, &CallRecord> = unit.iter().map(|r| (r.call, r)).collect();
    let (mut same, mut differ, mut extra) = (0, 0, 0);
    for r in replayed {
        match by_id.get(&r.call) {
            Some(u) if same_call(u, r) => same += 1,
            Some(_) => differ += 1,
            None => extra += 1,
        }
    }
    let ids: std::collections::HashSet<u64> = replayed.iter().map(|r| r.call).collect();
    let missing = unit.iter().filter(|r| !ids.contains(&r.call)).count();
    (same, differ, extra, missing)
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../scanner/tests/fixtures/replay")
}

#[test]
fn recorded_scenarios_replay_to_their_expected_calls() {
    let Ok(dirs) = std::fs::read_dir(fixtures()) else {
        return;
    };
    let bless = std::env::var("TRUNK_REPLAY_BLESS").is_ok_and(|v| v == "1");
    let mut ran = 0;
    for dir in dirs.flatten().map(|d| d.path()).filter(|p| p.join("trace.jsonl").exists()) {
        let (header, events) = read_trace(&dir.join("trace.jsonl"));
        let calls = replay(&header, &events);
        let expected = dir.join("expected.json");
        let got = serde_json::to_string_pretty(&calls).unwrap() + "\n";
        if bless {
            std::fs::write(&expected, got).unwrap();
        } else {
            let want = std::fs::read_to_string(&expected)
                .unwrap_or_else(|e| panic!("{}: {e} (bless with TRUNK_REPLAY_BLESS=1)", expected.display()));
            assert!(want.replace("\r\n", "\n") == got, "{}: replay differs from expected.json", dir.display());
        }
        ran += 1;
    }
    assert!(ran > 0 || bless, "no scenarios under {}", fixtures().display());
}

/// `TRUNK_TRACE=<path> cargo test trunk_replay -- --ignored --nocapture`
#[test]
#[ignore = "needs TRUNK_TRACE"]
fn trace_replay_against_the_unit() {
    let Ok(path) = std::env::var("TRUNK_TRACE") else {
        return;
    };
    let (header, events) = read_trace(Path::new(&path));
    let replayed = replay(&header, &events);
    let unit = recorded(&events);
    let (same, differ, extra, missing) = compare(&replayed, &unit);
    println!("{}", json!({
        "trace": path, "events": events.len(), "unit_calls": unit.len(), "replayed_calls": replayed.len(),
        "same": same, "differ": differ, "only_replayed": extra, "only_unit": missing,
    }));
    let by_id: HashMap<u64, &CallRecord> = unit.iter().map(|r| (r.call, r)).collect();
    for r in replayed.iter().filter(|r| by_id.get(&r.call).is_some_and(|u| !same_call(u, r))).take(20) {
        println!("replay {r:?}\nunit   {:?}", by_id[&r.call]);
    }
}

#[test]
fn a_synthetic_trace_replays_to_one_followed_and_one_encrypted_call() {
    let t = 1_790_000_000_000u64;
    let header = json!({"t": t, "ev": "start", "first_call_id": 7, "lanes": 1,
                        "hang_ms": 3000, "end_grace_ms": 2000});
    let mut events = vec![
        json!({"t": t + 100, "ev": "grant", "tg": 300, "src": 3406028, "freq": 857_987_500u64,
               "ch": 1117, "enc": false, "nf": null, "nac": 0, "lane": 1}),
        json!({"t": t + 150, "ev": "grant", "tg": 402, "src": 3400015, "freq": 858_437_500u64,
               "ch": 1189, "enc": true, "nf": "encrypted", "nac": 0, "lane": null}),
        json!({"t": t + 600, "ev": "hdu", "nac": 0x8A1, "lane": 1}),
    ];
    for i in 0..50u64 {
        events.push(json!({"t": t + 700 + 20 * i, "ev": "audio", "call": 7, "tg": 300, "src": 3406028,
                           "air": t + 650 + 20 * i, "airtime": true, "lane": 1}));
    }
    events.push(json!({"t": t + 1_800, "ev": "voice_end", "call": 7, "air": t + 1_700,
                       "lc": "talk_complete", "lane": 1}));
    let calls = replay(&header, &events);
    assert_eq!(calls.len(), 2);
    let followed = calls.iter().find(|c| c.call == 7).unwrap();
    assert_eq!((followed.tg, followed.frames, followed.close.as_str()), (300, 50, "call_end"));
    assert_eq!(followed.end_lc.as_deref(), Some("talk_complete"));
    let enc = calls.iter().find(|c| c.call == 8).unwrap();
    assert_eq!((enc.encrypted, enc.not_followed.as_deref(), enc.close.as_str()),
               (true, Some("encrypted"), "timeout"));
    assert_eq!(enc.end, t + 150, "a not-followed call ends at its last announcement");
}
