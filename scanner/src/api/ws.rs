//! The WebSockets.
//!
//! - `/ws/live`: the radio's state as it changes (below).
//! - `/ws/events`: the notices as they are.
//! - `/ws/audio`: live audio. With `?v=2`, every lane: binary frames are `[lane index, 0, 0, 0]`
//!   and 20 ms of 8 kHz 16-bit mono. Without it, p25-httpd's first framing for its tools: lane
//!   one only, the samples alone. Before a lane's first frame of a call, a text frame
//!   `{"type":"meta","lane","tg","src","call_id","speaker"}` names it; `speaker` (left, right or
//!   both) is where the profile routes the talkgroup. A listener that falls behind gets
//!   `{"type":"lag","skipped"}` and stays connected.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::api::v1::calls::{NamedCall, Names};
use crate::api::v1::status::status;
use crate::audio::live::audio_frame;
use crate::boot::state::AppState;
use crate::hardware::p25core::Lane;
use crate::services::config::profiles::Side;
use crate::services::notices::Notice;

/// How often `/ws/live` sends the status (its counters change every second).
const LIVE_TICK: Duration = Duration::from_secs(1);

#[derive(serde::Deserialize)]
pub struct AudioParams {
    /// 2: every lane, tagged.
    pub v: Option<u8>,
}

pub async fn audio(ws: WebSocketUpgrade, State(s): State<Arc<AppState>>, Query(p): Query<AudioParams>) -> impl IntoResponse {
    let tagged = p.v == Some(2);
    ws.on_upgrade(move |socket| stream(socket, s, tagged))
}

struct Listener(Arc<AppState>);

impl Drop for Listener {
    fn drop(&mut self) {
        self.0.audio.listeners.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn stream(socket: WebSocket, s: Arc<AppState>, tagged: bool) {
    s.audio.listeners.fetch_add(1, Ordering::Relaxed);
    let _listener = Listener(s.clone());
    let mut rx = s.audio.subscribe();
    let (mut out, mut inbound) = socket.split();
    let mut announced: [Option<(u32, u64, Side)>; 2] = [None; 2];
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Ok(c) if !tagged && c.lane != Lane::One => {}
                Ok(c) => {
                    let li = c.lane.index().min(1);
                    if announced[li] != Some((c.tg, c.call, c.speaker)) {
                        announced[li] = Some((c.tg, c.call, c.speaker));
                        let meta = serde_json::json!({
                            "type": "meta", "lane": li, "tg": c.tg, "src": c.source.unwrap_or(0), "call_id": c.call,
                            "speaker": c.speaker,
                        });
                        if out.send(Message::Text(meta.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    if out.send(Message::Binary(audio_frame(&c, tagged).into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    let lag = serde_json::json!({ "type": "lag", "skipped": skipped });
                    let _ = out.send(Message::Text(lag.to_string().into())).await;
                }
                Err(RecvError::Closed) => break,
            },
            // Driven so a closed tab is noticed while no audio flows.
            msg = inbound.next() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

/// `/ws/events`: a text frame per notice (`{"type":"call_opened",...}`), for pages to refresh
/// at once; `{"type":"lag"}` when some were missed.
pub async fn events(ws: WebSocketUpgrade, State(s): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| notices(socket, s))
}

async fn notices(socket: WebSocket, s: Arc<AppState>) {
    let mut rx = s.notices.subscribe();
    let (mut out, mut inbound) = socket.split();
    loop {
        tokio::select! {
            n = rx.recv() => {
                let text = match n {
                    Ok(n) => serde_json::to_string(&n).unwrap_or_default(),
                    Err(RecvError::Lagged(_)) => r#"{"type":"lag"}"#.to_string(),
                    Err(RecvError::Closed) => break,
                };
                if out.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            },
            msg = inbound.next() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

/// `/ws/live`: the radio's state as it changes, so a page shows it without asking. On connect a
/// `snapshot` (status, calls, traffic, scan); then `status` once a second, `traffic` when a
/// traffic channel changes, `call_opened` and `call_closed` with the call, `recording` with a
/// saved recording, `scan` while one runs, and `changed` naming the part of the configuration
/// to read again. After `lag` a fresh snapshot follows.
pub async fn live(ws: WebSocketUpgrade, State(s): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| live_feed(socket, s))
}

/// One traffic channel as a page shows it.
#[derive(Serialize)]
struct TrafficChannel {
    lane: u8,
    tuned_hz: Option<u64>,
    following_tg: Option<u32>,
    on_data_channel: bool,
    voice_frames: u64,
    last_voice_ms_ago: Option<u64>,
    /// The call it carries.
    call: Option<NamedCall>,
}

#[derive(Serialize)]
struct Traffic {
    channels: Vec<TrafficChannel>,
    /// Every call on the air, followed or not.
    open: Vec<NamedCall>,
}

fn traffic(s: &AppState, names: &Names) -> Traffic {
    let calls = s.trunking.calls();
    let channels = s
        .trunking
        .lanes()
        .into_iter()
        .map(|l| TrafficChannel {
            lane: l.lane,
            tuned_hz: l.tuned_hz,
            following_tg: l.following_tg,
            on_data_channel: l.on_data_channel,
            voice_frames: l.voice_frames,
            last_voice_ms_ago: l.last_voice_ms_ago,
            call: l.call.and_then(|id| calls.open.iter().find(|c| c.call == id).cloned()).map(|c| names.call(c)),
        })
        .collect();
    Traffic { channels, open: calls.open.into_iter().map(|c| names.call(c)).collect() }
}

/// `{"type": kind, key: value}`.
fn message(kind: &str, key: &str, value: impl Serialize) -> String {
    let mut m = serde_json::Map::new();
    m.insert("type".into(), kind.into());
    m.insert(key.into(), serde_json::to_value(value).unwrap_or_default());
    serde_json::Value::Object(m).to_string()
}

async fn snapshot(s: &AppState, names: &Names) -> String {
    let calls = s.trunking.calls();
    serde_json::json!({
        "type": "snapshot",
        "status": status(s),
        "calls": {
            "open": calls.open.into_iter().map(|c| names.call(c)).collect::<Vec<_>>(),
            "recent": calls.recent.into_iter().map(|c| names.call(c)).collect::<Vec<_>>(),
        },
        "traffic": traffic(s, names),
        "scan": s.discovery.state(),
    })
    .to_string()
}

/// A call by id, from the open and recent calls.
fn named_call(s: &AppState, names: &Names, id: u64) -> Option<NamedCall> {
    let v = s.trunking.calls();
    v.open.into_iter().chain(v.recent).find(|c| c.call == id).map(|c| names.call(c))
}

async fn live_feed(socket: WebSocket, s: Arc<AppState>) {
    let mut notices = s.notices.subscribe();
    let mut live = s.live.subscribe();
    let (mut out, mut inbound) = socket.split();
    let mut names = Names::load(&s).await;
    if out.send(Message::Text(snapshot(&s, &names).await.into())).await.is_err() {
        return;
    }
    // What was sent last, to send only what changed.
    let mut last_traffic = String::new();
    let mut last_scan = String::new();
    let mut tick = tokio::time::interval(LIVE_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let mut msgs = Vec::new();
        let mut traffic_now = false;
        tokio::select! {
            _ = tick.tick() => {
                msgs.push(message("status", "status", status(&s)));
                traffic_now = true;
                let scan = message("scan", "scan", s.discovery.state());
                if scan != last_scan {
                    last_scan = scan.clone();
                    msgs.push(scan);
                }
            }
            changed = live.changed() => {
                if changed.is_err() {
                    break;
                }
                msgs.push(message("status", "status", status(&s)));
                traffic_now = true;
            }
            n = notices.recv() => match n {
                Ok(Notice::CallOpened { call, .. }) => {
                    msgs.extend(named_call(&s, &names, call).map(|c| message("call_opened", "call", c)));
                    traffic_now = true;
                }
                Ok(Notice::CallClosed { call, .. }) => {
                    msgs.extend(named_call(&s, &names, call).map(|c| message("call_closed", "call", c)));
                    traffic_now = true;
                }
                Ok(Notice::RecordingSaved { call }) => {
                    msgs.extend(s.recordings.get(call).map(|r| message("recording", "recording", r)));
                }
                Ok(Notice::Changed { what }) => {
                    if what == "systems" {
                        names = Names::load(&s).await;
                    }
                    msgs.push(message("changed", "what", what));
                }
                Err(RecvError::Lagged(_)) => {
                    names = Names::load(&s).await;
                    msgs.push(r#"{"type":"lag"}"#.to_string());
                    msgs.push(snapshot(&s, &names).await);
                }
                Err(RecvError::Closed) => break,
            },
            msg = inbound.next() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            },
        }
        if traffic_now {
            let t = message("traffic", "traffic", traffic(&s, &names));
            if t != last_traffic {
                last_traffic = t.clone();
                msgs.push(t);
            }
        }
        for m in msgs {
            if out.send(Message::Text(m.into())).await.is_err() {
                return;
            }
        }
    }
}

