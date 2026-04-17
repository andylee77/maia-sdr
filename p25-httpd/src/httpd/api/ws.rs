//! WebSocket streams: /ws/events and /ws/audio.
//!
//! Consumer orientation: "push me updates as they happen." Two
//! streams, different framing:
//!
//!   - `/ws/events` — text frames, one JSON-serialised `TsbkEvent`
//!     per message. Fed from the `event_tx` broadcast channel on
//!     `AppState`. Every parsed TSBK + every system event (grant,
//!     call start/end, retune, error) lands here.
//!   - `/ws/audio` — binary frames, 320 bytes per message (160 i16
//!     little-endian = 20 ms of 8 kHz mono). Fed from `audio_tx`
//!     broadcast. Optional text control frames (`{"type":"lag"}`)
//!     when the channel overruns.
//!
//! Both handlers survive `Lagged` (a slow consumer falling behind
//! the broadcast ring). The old behaviour was to close on Lagged,
//! triggering a reconnect cycle per gap; Stage 2 changed this to
//! send a synthetic lag marker and stay connected. Clients should
//! use the marker to flush any local jitter buffer rather than
//! reconnect.
//!
//! Reconnect guidance: consumers should use exponential backoff
//! (1s → 15s ceiling). The dashboard implements this in
//! `connectWs()` in `dashboard.html`. A flat reconnect delay
//! hammers the server during daemon restart — see
//! `doc/API_CONSUMERS.md` §"Rules for adding a consumer".

use std::sync::Arc;

use axum::extract::State;

#[allow(unused_imports)]
use p25_json::*;

#[allow(unused_imports)]
use crate::httpd::AppState;
#[allow(unused_imports)]
use crate::p25::control_channel::{
    ControlChannelDecoder, RUNTIME_SYNC_THRESHOLD, SYNC_THRESHOLD,
};
use axum::{
    extract::{ws::WebSocket, WebSocketUpgrade},
    response::IntoResponse,
};

pub async fn ws_events(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}


pub async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = state.event_tx.subscribe();
    loop {
        match rx.recv().await {
            Ok(msg) => {
                if socket
                    .send(axum::extract::ws::Message::Text(msg.into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(RecvError::Lagged(skipped)) => {
                // Connection lagged behind the broadcast ring buffer.
                // Instead of closing (old behaviour — one slow consumer
                // would trigger a full reconnect cycle), send a
                // synthetic control event so the client knows a gap
                // occurred, and stay connected. Clients that don't
                // recognise the `ws_lag` event_type will drop it via
                // the existing event-type filter.
                let ctrl = serde_json::json!({
                    "event_type": "ws_lag",
                    "timestamp":  "",
                    "summary":    format!("broadcast lagged, {skipped} event(s) skipped"),
                    "skipped":    skipped,
                })
                .to_string();
                if socket
                    .send(axum::extract::ws::Message::Text(ctrl.into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(RecvError::Closed) => break,
        }
    }
}

// ── Phase 7D diagnostic: raw IMBE frame dump ──────────────────────────


/// WebSocket /ws/audio -- binary frames of 320 bytes (160 i16 LE
/// samples = 20 ms of 8 kHz mono audio per message).
pub async fn ws_audio(
    ws: axum::extract::WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_audio(socket, state))
}


pub async fn handle_ws_audio(
    mut socket: axum::extract::ws::WebSocket,
    state: Arc<AppState>,
) {
    use std::sync::atomic::Ordering;
    let mut rx = state.audio_tx.subscribe();
    loop {
        match rx.recv().await {
            Ok(chunk) => {
                let mut buf = [0u8; 320];
                for (i, &sample) in chunk.pcm.iter().enumerate() {
                    let le = sample.to_le_bytes();
                    buf[i * 2] = le[0];
                    buf[i * 2 + 1] = le[1];
                }
                if socket
                    .send(axum::extract::ws::Message::Binary(buf.to_vec().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                // The broadcast channel dropped `skipped` chunks because
                // this consumer fell behind. Each lagged chunk is a gap
                // the listener will hear. Bump the global counter so
                // /api/stats.audio_ws_lag_total reflects it and the
                // dashboard can distinguish this (server-side loss) from
                // browser-side jitter-buffer underruns.
                state.audio_ws_lag_total.fetch_add(skipped, Ordering::Relaxed);
                // Also tell the client so it can flush its jitter
                // buffer rather than blending the pre-gap and post-gap
                // samples into a click. Sent as a Text frame; the
                // current client's binary-only filter ignores it, but
                // future clients (Android app) can react on it.
                let ctrl = format!(r#"{{"type":"lag","skipped":{skipped}}}"#);
                let _ = socket
                    .send(axum::extract::ws::Message::Text(ctrl.into()))
                    .await;
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

// ── Dashboard HTML ─────────────────────────────────────────────────────


