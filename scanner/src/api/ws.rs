//! `/ws/audio`: live audio. With `?v=2`, every lane: binary frames are `[lane index, 0, 0, 0]` and
//! 20 ms of 8 kHz 16-bit mono. Without it, p25-httpd's first framing for its tools: lane one
//! only, the samples alone. Before a lane's first frame of a call, a text frame
//! `{"type":"meta","lane","tg","src","call_id","speaker"}` names it; `speaker` (left, right or
//! both) is where the profile routes the talkgroup. A listener that falls behind gets
//! `{"type":"lag","skipped"}` and stays connected.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use crate::audio::live::audio_frame;
use crate::boot::state::AppState;
use crate::hardware::p25core::Lane;
use crate::services::config::profiles::Side;

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
