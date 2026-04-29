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
    socket: axum::extract::ws::WebSocket,
    state: Arc<AppState>,
) {
    use std::sync::atomic::Ordering;
    use futures::{SinkExt, StreamExt};
    let mut rx = state.audio_tx.subscribe();
    // Split so we can concurrently drive a recv (to observe Close
    // frames + remote drops) and a send (audio chunks). Without this
    // split the loop blocks on rx.recv().await during idle and stale
    // tabs accumulate in audio_tx.receiver_count() until the next
    // broadcast send fails — which is what produced the "3 audio WS
    // clients" reading on the dashboard with only one real listener.
    let (mut tx_sock, mut rx_sock) = socket.split();
    loop {
        tokio::select! {
            // Audio broadcast → push to client.
            broadcast = rx.recv() => {
                match broadcast {
                    Ok(chunk) => {
                        let mut buf = [0u8; 320];
                        for (i, &sample) in chunk.pcm.iter().enumerate() {
                            let le = sample.to_le_bytes();
                            buf[i * 2] = le[0];
                            buf[i * 2 + 1] = le[1];
                        }
                        if tx_sock
                            .send(axum::extract::ws::Message::Binary(buf.to_vec().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        state.audio_ws_lag_total.fetch_add(skipped, Ordering::Relaxed);
                        let ctrl = format!(r#"{{"type":"lag","skipped":{skipped}}}"#);
                        let _ = tx_sock
                            .send(axum::extract::ws::Message::Text(ctrl.into()))
                            .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            // Client → server. We don't expect any messages, but we
            // must drive the receive side so Close frames + transport
            // errors are observed promptly. Without this branch a
            // browser tab closed mid-call goes undetected for as long
            // as the chain is idle.
            ws_in = rx_sock.next() => {
                match ws_in {
                    None => break,                          // peer closed cleanly
                    Some(Err(_)) => break,                  // transport error
                    Some(Ok(axum::extract::ws::Message::Close(_))) => break,
                    Some(Ok(_)) => { /* ignore other client messages */ }
                }
            }
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
    // Phase 10.8: the only live IQ source on the new bitstream is
    // `pre_diff` (tapped inside LsmDemod after rotate+AGC but before
    // the diff-demod/slicer). The retired `post_ddc` / `post_lsm` /
    // `post_pll` values produced `iq_dma` / `lsm_iq_dma` / `post_pll_
    // iq_dma` buffers on the old bitstream; those rings are gone.
    let source = params
        .get("source")
        .cloned()
        .unwrap_or_else(|| "pre_diff".to_string());
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
        // Phase 10.8: `pre_diff` is the only live source. It taps
        // inside LsmDemod after `LsmPllRotate` + AGC but BEFORE the
        // diff-demod/slicer, so samples sit on the 4-cluster LSM
        // constellation at 9.6 kSPS (2 samples per symbol interleaved).
        "pre_diff" => source,
        other => {
            let err = format!(
                r#"{{"type":"error","error":"unknown source '{}'; expected pre_diff"}}"#,
                other.replace('"', "'")
            );
            let _ = socket.send(Message::Text(err.into())).await;
            return;
        }
    };

    // Sample rate is always 9.6 kSPS for the pre-diff tap (2 samples
    // per symbol at the P25 4800 sym/s rate).
    let sample_rate_hz: u32 = 9_600;

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
                        ("control", "pre_diff") => core.read_pre_diff_iq_buffers(),
                        ("traffic", "pre_diff") => core.read_traffic_pre_diff_iq_buffers(),
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


