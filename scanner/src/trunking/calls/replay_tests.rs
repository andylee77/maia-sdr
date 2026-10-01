//! The phase 3 acceptance test: live traces recorded on the units (`tests/fixtures/replay/`)
//! replayed through the call book give the calls p25-httpd's lifecycle made of the same traffic
//! (`expected.json`, blessed from p25-httpd's replay of the trace).
//!
//! A trace holds the lifecycle's inputs as the unit saw them: the follower's grant decisions,
//! grant updates, and each lane's decoder reports, each stamped with its receipt time. The
//! replay runs the 100 ms tick on the same phase as p25-httpd's.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::*;

/// One call as the book closed it (`expected.json`'s shape).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    frames: u64,
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

fn intern(s: &str) -> &'static str {
    match s {
        "talk_complete" => "talk_complete",
        "channel_user" => "channel_user",
        "call_termination" => "call_termination",
        "link_control" => "link_control",
        "network_teardown" => "network_teardown",
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

fn read_trace(path: &Path) -> (Value, Vec<Value>) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut lines = text.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str::<Value>(l).expect("trace line"));
    let header = lines.next().expect("a header");
    assert_eq!(header["ev"], "start");
    (header, lines.collect())
}

/// Unix ms of the trace onto both clocks.
struct Clock {
    base: Instant,
    origin_ms: u64,
}

impl Clock {
    fn stamp(&self, ms: u64) -> Stamp {
        Stamp { mono: self.base + Duration::from_millis(ms.saturating_sub(self.origin_ms)), unix_ms: ms }
    }
}

fn replay(header: &Value, events: &[Value]) -> Vec<CallRecord> {
    let lanes = &[Lane::One, Lane::Two][..header["lanes"].as_u64().unwrap_or(1).clamp(1, 2) as usize];
    let policy = CallPolicy {
        hang: Duration::from_millis(header["hang_ms"].as_u64().unwrap_or(3_000)),
        end_grace: Duration::from_millis(header["end_grace_ms"].as_u64().unwrap_or(2_000)),
    };
    let site = header["site"].as_str().unwrap_or("site");
    let mut book = CallBook::new(site, lanes, policy, header["first_call_id"].as_u64().unwrap_or(1));
    let t0 = header["t"].as_u64().unwrap_or(0);
    // Air times can be a little older than the trace's start.
    let clock = Clock { base: Instant::now(), origin_ms: t0.saturating_sub(3_600_000) };
    let mut out = Vec::new();
    let mut frames: HashMap<u64, u64> = HashMap::new();
    let mut next_tick = t0 + 100;
    for e in events {
        let Some(t) = e["t"].as_u64() else { continue };
        while next_tick <= t {
            book.tick(clock.stamp(next_tick), &mut out);
            next_tick += 100;
        }
        let at = clock.stamp(t);
        let lane = lane_of(&e["lane"]);
        let on = lane.unwrap_or(Lane::One);
        match e["ev"].as_str() {
            Some("grant") => {
                let Some(tg) = u32_of(&e["tg"]) else { continue };
                let decision = match e["nf"].as_str() {
                    Some(nf) => Decision::NotFollowed(NotFollowed::parse(nf).expect("a known reason")),
                    None => Decision::Followed(on),
                };
                let g = GrantIn {
                    tg,
                    source: u32_of(&e["src"]),
                    channel: ChannelKey { freq_hz: e["freq"].as_u64(), slot: None },
                    channel_label: e["ch"].as_u64().map(|c| c.to_string()),
                    encrypted: e["enc"].as_bool().unwrap_or(false),
                    nac: e["nac"].as_u64().unwrap_or(0) as u16,
                    decision,
                };
                book.grant(g, at, &mut out);
            }
            Some("grant_update") => {
                let Some(tg) = u32_of(&e["tg"]) else { continue };
                let channel = ChannelKey { freq_hz: e["freq"].as_u64(), slot: None };
                book.grant_update(tg, channel, e["ch"].as_u64().unwrap_or(0) as u16, at);
            }
            Some("hdu") => book.hdu(on, e["nac"].as_u64().unwrap_or(0) as u16, at, &mut out),
            Some("lc_source") => {
                if let Some(s) = u32_of(&e["src"]) {
                    book.link_control_source(on, s, &mut out);
                }
            }
            Some("speaker_end") => {
                if let Some(s) = u32_of(&e["src"]) {
                    book.talk_complete_source(on, s, &mut out);
                }
            }
            Some("nid") => {
                if let Some(voice) = e["voice"].as_bool() {
                    book.nid(on, voice, at, &mut out);
                }
            }
            Some("voice_end") => {
                if let (Some(call), Some(air), Some(lc)) = (e["call"].as_u64(), e["air"].as_u64(), e["lc"].as_str()) {
                    book.voice_end(on, call, clock.stamp(air).mono, intern(lc), at);
                }
            }
            Some("audio") => {
                let (Some(lane), Some(call), Some(air)) = (lane, e["call"].as_u64(), e["air"].as_u64()) else { continue };
                *frames.entry(call).or_default() += 1;
                let attributed = (e["airtime"].as_bool().unwrap_or(false) && call != 0).then_some(call);
                book.voice(lane, attributed, clock.stamp(air).mono, at);
            }
            _ => {}
        }
    }
    let end = next_tick + policy.hang.as_millis() as u64 + policy.end_grace.as_millis() as u64 + 1_000;
    while next_tick <= end {
        book.tick(clock.stamp(next_tick), &mut out);
        next_tick += 100;
    }

    let mut opened: BTreeMap<u64, Opened> = BTreeMap::new();
    let mut calls = Vec::new();
    for e in out {
        match e {
            CallEvent::Opened(o) => {
                opened.insert(o.call, o);
            }
            CallEvent::Closed(c) => {
                let Some(o) = opened.remove(&c.call) else { continue };
                calls.push(CallRecord {
                    call: c.call,
                    tg: o.tg,
                    lane: o.lane.map(|l| l.number()),
                    freq: o.channel.freq_hz,
                    encrypted: o.encrypted,
                    not_followed: o.not_followed.map(|n| n.as_str().to_string()),
                    opened_via: serde_json::to_value(o.via).unwrap().as_str().unwrap().to_string(),
                    sources: c.sources,
                    start: c.started_unix_ms,
                    end: c.ended_unix_ms,
                    close: serde_json::to_value(c.reason).unwrap().as_str().unwrap().to_string(),
                    end_lc: c.end_lc.map(str::to_string),
                    frames: frames.get(&c.call).copied().unwrap_or(0),
                });
            }
            _ => {}
        }
    }
    calls.sort_by_key(|r| r.call);
    calls
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay")
}

#[test]
fn recorded_traces_replay_to_the_lifecycles_calls() {
    let dirs: Vec<PathBuf> = std::fs::read_dir(fixtures())
        .expect("replay fixtures")
        .flatten()
        .map(|d| d.path())
        .filter(|p| p.join("trace.jsonl").exists())
        .collect();
    assert!(!dirs.is_empty());
    for dir in dirs {
        let (header, events) = read_trace(&dir.join("trace.jsonl"));
        let got = replay(&header, &events);
        let want: Vec<CallRecord> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("expected.json")).unwrap()).unwrap();
        let by_id: HashMap<u64, &CallRecord> = want.iter().map(|r| (r.call, r)).collect();
        let differing: Vec<String> = got
            .iter()
            .filter(|r| by_id.get(&r.call) != Some(r))
            .map(|r| format!("got  {r:?}\nwant {:?}", by_id.get(&r.call)))
            .collect();
        assert!(
            differing.is_empty() && got.len() == want.len(),
            "{}: {} of {} calls differ ({} replayed)\n{}",
            dir.display(),
            differing.len(),
            want.len(),
            got.len(),
            differing.iter().take(5).cloned().collect::<Vec<_>>().join("\n"),
        );
    }
}
