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
use crate::protocol::p25::control_channel::{
    ControlChannelDecoder, RUNTIME_SYNC_THRESHOLD, CC_SYNC_THRESHOLD,
};
use axum::{
    extract::{ws::WebSocket, WebSocketUpgrade},
    response::IntoResponse,
};

/// Poll cadence for the `/ws/iq` + related IQ streaming handlers.
/// Sub-buffers arrive every ~131 ms (post-DDC) or ~262 ms (post-LSM,
/// half rate). 80 ms catches both with headroom while halving lock
/// pressure vs the earlier 40 ms tick.
const IQ_POLL_INTERVAL_MS: u64 = 80;

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

// ── /ws/iq : post-DDC IQ streaming (2026-04-18) ─────────────────────────
//
// Pushes raw post-DDC IQ sub-buffers (32 KB each, 8192 complex samples
// at 62.5 kSPS ≈ 131 ms) as binary WebSocket frames. Layout is the
// iq_dma ring's native format — little-endian i16 interleaved
// (re, im), same as /api/control_iq_dump minus the WAV header.
//
// First message is a text hello frame carrying {sample_rate_hz,
// format, chain, buf_bytes}. All subsequent messages are Binary.
// Clients should consume Binary frames as IQ and ignore any Text
// frames they don't recognise (room to add control messages later).
//
// Scope — single-consumer. The handler calls read_iq_buffers()
// directly, which races with /api/spectrum and /api/constellation
// polling; if multiple consumers ask for ring data in the same
// window, they split the sub-buffers between them. Multi-consumer
// broadcast is a follow-up if streaming usage actually collides with
// spectrum polling on the same board. For the initial browser-FFT /
// eye-plot / live-constellation use case on the dashboard, single-
// consumer suffices.

#[cfg(target_os = "linux")]
pub async fn ws_iq(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let chain = params
        .get("chain")
        .cloned()
        .unwrap_or_else(|| "control".to_string());
    // Phase 10.6 source param: post_ddc (default, backwards-compatible)
    // reads the `iq_dma` / `traffic_iq_dma` rings at 62.5 kSPS; post_lsm
    // reads the new `lsm_iq_dma` / `traffic_lsm_iq_dma` rings (post-RRC
    // matched-filter, 31.25 kSPS).
    let source = params
        .get("source")
        .cloned()
        .unwrap_or_else(|| "post_ddc".to_string());
    ws.on_upgrade(move |socket| handle_ws_iq(socket, state, chain, source))
}

#[cfg(not(target_os = "linux"))]
pub async fn ws_iq(
    ws: WebSocketUpgrade,
    State(_state): State<Arc<AppState>>,
    axum::extract::Query(_params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    ws.on_upgrade(|mut socket| async move {
        let _ = socket
            .send(axum::extract::ws::Message::Text(
                r#"{"type":"error","error":"ws_iq only available on linux target"}"#
                    .to_string()
                    .into(),
            ))
            .await;
    })
}

#[cfg(target_os = "linux")]
async fn handle_ws_iq(
    mut socket: axum::extract::ws::WebSocket,
    state: Arc<AppState>,
    chain: String,
    source: String,
) {
    use axum::extract::ws::Message;
    use std::time::Duration;
    use tokio::select;

    // Validate chain + source up front so the hello frame is
    // accurate and the polling loop doesn't have to re-match every tick.
    let chain = match chain.as_str() {
        "control" | "traffic" => chain,
        other => {
            let err = format!(
                r#"{{"type":"error","error":"unknown chain '{}'; expected control|traffic"}}"#,
                other.replace('"', "'")
            );
            let _ = socket.send(Message::Text(err.into())).await;
            return;
        }
    };
    let source = match source.as_str() {
        // Phase 10.7: `post_pll` is the canonical clean-eye source. The
        // two pre-PLL options stay for diagnostics/legacy access but
        // the Plots tab only surfaces `post_pll`.
        "post_ddc" | "post_lsm" | "post_pll" => source,
        other => {
            let err = format!(
                r#"{{"type":"error","error":"unknown source '{}'; expected post_ddc|post_lsm|post_pll"}}"#,
                other.replace('"', "'")
            );
            let _ = socket.send(Message::Text(err.into())).await;
            return;
        }
    };

    // Sample rate depends on source:
    //   post_ddc = 62.5 kSPS (post-DDC)
    //   post_lsm = 31.25 kSPS (post-RRC matched filter)
    //   post_pll = 9.6 kSPS (post-LsmPllRotate, 2 samples per symbol)
    let sample_rate_hz: u32 = match source.as_str() {
        "post_lsm" => 31_250,
        "post_pll" => 9_600,
        _ => 62_500,
    };

    // Hello frame. buf_bytes matches the underlying DMA sub-buffer
    // size (both rings are 32 KB regardless of source rate).
    let hello = format!(
        r#"{{"type":"hello","sample_rate_hz":{sample_rate_hz},"format":"i16le-iq-stereo","chain":"{chain}","source":"{source}","buf_bytes":32768}}"#
    );
    if socket.send(Message::Text(hello.into())).await.is_err() {
        return;
    }

    // Poll cadence: sub-buffers arrive every ~131 ms (post-DDC) or
    // ~262 ms (post-LSM, half rate). 80 ms polling catches both with
    // headroom while halving lock pressure vs the earlier 40 ms tick.
    let mut tick = tokio::time::interval(Duration::from_millis(IQ_POLL_INTERVAL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        select! {
            biased;
            // Client sent us something (usually Close).
            recv = socket.recv() => {
                match recv {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => {},
                    Some(Err(_)) => return,
                }
            }
            _ = tick.tick() => {
                let bufs: Vec<Vec<u8>> = {
                    let mut core = state.ip_core.lock().await;
                    let raw: Vec<&[u8]> = match (chain.as_str(), source.as_str()) {
                        ("control", "post_ddc") => core.read_iq_buffers(),
                        ("traffic", "post_ddc") => core.read_traffic_iq_buffers(),
                        ("control", "post_lsm") => core.read_lsm_iq_buffers(),
                        ("traffic", "post_lsm") => core.read_traffic_lsm_iq_buffers(),
                        ("control", "post_pll") => core.read_post_pll_iq_buffers(),
                        ("traffic", "post_pll") => core.read_traffic_post_pll_iq_buffers(),
                        _ => Vec::new(),
                    };
                    raw.into_iter().map(|b| b.to_vec()).collect()
                };
                for b in bufs {
                    if socket.send(Message::Binary(b.into())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

// ── Dashboard HTML ─────────────────────────────────────────────────────


