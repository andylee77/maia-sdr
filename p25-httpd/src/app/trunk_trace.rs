//! Trunking trace: the call lifecycle's inputs and outputs, one JSON line each.
//!
//! Enabled by `P25_TRUNK_TRACE=<path>` at start. It records every call boundary (grants with the
//! follower's decision, grant updates, traffic NIDs, link control, end markers), every audio
//! chunk's metadata and every call event the lifecycle publishes, stamped with the receipt time.
//! A recorded trace replays through the lifecycle on the host (`trunk_replay_tests`) and is the
//! input of the 076 replay scenarios.

use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::broadcast::{self, error::RecvError};

use crate::app::grant_follower::{CallTrackerEvent, CallTrackerEventKind, CallTrackerEventTx};
use crate::app::now_unix_ms as unix_ms;
use crate::audio::{AudioChunk, CallBoundary, CallBoundaryKind, CallBoundaryTx};
use crate::services::ui_settings::CallPolicy;

pub const ENV: &str = "P25_TRUNK_TRACE";

fn lane(l: Option<crate::hardware::traffic_lane::Lane>) -> Value {
    l.map_or(Value::Null, |l| json!(l.number()))
}

pub fn boundary_json(t: u64, b: &CallBoundary) -> Value {
    let l = lane(b.lane);
    match b.kind {
        CallBoundaryKind::CcGrantArrival { tg, source, freq_hz, channel, encrypted, not_followed } => {
            json!({"t": t, "ev": "grant", "tg": tg, "src": source, "freq": freq_hz, "ch": channel,
                   "enc": encrypted, "nf": not_followed, "nac": b.nac, "lane": l})
        }
        CallBoundaryKind::CcGrantUpdate { tg, freq_hz, channel } => {
            json!({"t": t, "ev": "grant_update", "tg": tg, "freq": freq_hz, "ch": channel})
        }
        CallBoundaryKind::HduStart => json!({"t": t, "ev": "hdu", "nac": b.nac, "lane": l}),
        CallBoundaryKind::TdulcComplete { source } => {
            json!({"t": t, "ev": "lc_source", "src": source, "lane": l})
        }
        CallBoundaryKind::SpeakerEnd { source, kind } => {
            json!({"t": t, "ev": "speaker_end", "src": source, "kind": format!("{kind:?}"), "lane": l})
        }
        CallBoundaryKind::TrafficNidObserved { voice } => {
            json!({"t": t, "ev": "nid", "voice": voice, "lane": l})
        }
        CallBoundaryKind::VoiceEnd { call_id, air_ms, lc } => {
            json!({"t": t, "ev": "voice_end", "call": call_id, "air": air_ms, "lc": lc, "lane": l})
        }
    }
}

pub fn audio_json(t: u64, c: &AudioChunk) -> Value {
    json!({"t": t, "ev": "audio", "call": c.call_id, "tg": c.talkgroup, "src": c.source,
           "air": c.captured_at_ms, "airtime": c.airtime, "lane": c.lane.number()})
}

pub fn tracker_json(t: u64, e: &CallTrackerEvent) -> Value {
    let l = lane(e.lane);
    match &e.kind {
        CallTrackerEventKind::CallOpen { tg, source, freq_hz, encrypted, not_followed, opened_via, site, .. } => {
            json!({"t": t, "ev": "open", "call": e.call_id, "tg": tg, "src": source, "freq": freq_hz,
                   "enc": encrypted, "nf": not_followed, "via": opened_via, "site": site, "lane": l})
        }
        CallTrackerEventKind::CallClose {
            reason, final_source, started_unix_ms, ended_unix_ms, sources_observed, end_lc, ..
        } => {
            json!({"t": t, "ev": "close", "call": e.call_id, "reason": reason, "src": final_source,
                   "start": started_unix_ms, "end": ended_unix_ms, "sources": sources_observed,
                   "end_lc": end_lc, "lane": l})
        }
        CallTrackerEventKind::SourceUpdate { new_source, .. } => {
            json!({"t": t, "ev": "source", "call": e.call_id, "src": new_source, "lane": l})
        }
        CallTrackerEventKind::ActualSpeakerObserved { speaker, .. } => {
            json!({"t": t, "ev": "speaker", "call": e.call_id, "src": speaker, "lane": l})
        }
    }
}

/// Subscribe now (before the lifecycle starts) and write the trace from a task.
pub fn spawn_if_enabled(
    boundary_tx: &CallBoundaryTx,
    audio_tx: &broadcast::Sender<AudioChunk>,
    tracker_tx: &CallTrackerEventTx,
    policy: Arc<CallPolicy>,
    first_call_id: u64,
    lanes: usize,
    site: String,
) {
    let Ok(path) = std::env::var(ENV) else {
        return;
    };
    let file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("trunk trace: cannot create {path}: {e}");
            return;
        }
    };
    tracing::info!("trunk trace: writing {path}");
    let mut boundary_rx = boundary_tx.subscribe();
    let mut audio_rx = audio_tx.subscribe();
    let mut tracker_rx = tracker_tx.subscribe();
    tokio::spawn(async move {
        let mut out = BufWriter::new(file);
        let header = json!({"t": unix_ms(), "ev": "start", "build": crate::BUILD_TAG, "site": site,
                            "first_call_id": first_call_id, "lanes": lanes,
                            "hang_ms": policy.hang_ms(), "end_grace_ms": policy.end_grace_ms()});
        let _ = writeln!(out, "{header}");
        let mut flush = tokio::time::interval(Duration::from_secs(1));
        loop {
            let line = tokio::select! {
                r = boundary_rx.recv() => match r {
                    Ok(b) => boundary_json(unix_ms(), &b),
                    Err(RecvError::Lagged(n)) => json!({"t": unix_ms(), "ev": "lagged", "stream": "boundary", "n": n}),
                    Err(RecvError::Closed) => break,
                },
                r = audio_rx.recv() => match r {
                    Ok(c) => audio_json(unix_ms(), &c),
                    Err(RecvError::Lagged(n)) => json!({"t": unix_ms(), "ev": "lagged", "stream": "audio", "n": n}),
                    Err(RecvError::Closed) => break,
                },
                r = tracker_rx.recv() => match r {
                    Ok(e) => tracker_json(unix_ms(), &e),
                    Err(RecvError::Lagged(n)) => json!({"t": unix_ms(), "ev": "lagged", "stream": "tracker", "n": n}),
                    Err(RecvError::Closed) => break,
                },
                _ = flush.tick() => {
                    let _ = out.flush();
                    continue;
                }
            };
            if writeln!(out, "{line}").is_err() {
                tracing::warn!("trunk trace: write failed; stopped");
                break;
            }
        }
        let _ = out.flush();
    });
}
