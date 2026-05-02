//! Audio pacer — gates vocoder→broadcast at exact wire-rate.
//!
//! ## Why
//!
//! The post-Tier-A JMBE decoder runs at ~30× realtime on a Z7020 (5-15 ms
//! to decode a 9-frame LDU batch that represents 180 ms of audio). The
//! vocoder thread therefore emits 9 chunks back-to-back inside a few ms
//! whenever an LDU arrives, then waits 175 ms for the next batch. Worse,
//! when an IMBE backlog has accumulated (call-start, sync-recovery,
//! encryption-skip resume) the vocoder can clear seconds of audio in
//! tens of ms.
//!
//! Without pacing, that burst hits the broadcast channel and propagates
//! straight through to every subscriber — recorder, lifecycle authority,
//! WebSocket audio clients. The browser never sees true real-time audio:
//! it sees bursts that overrun its 2 s ring or arrive with no consistent
//! cadence the AudioWorklet PLL can lock on to. Empirically (2026-04-30
//! `p25_ws_audio_capture` traces) entire calls have arrived at clients
//! in single sub-500 ms bursts.
//!
//! ## What
//!
//! The pacer sits between the vocoder and the existing broadcast
//! channel. The vocoder writes to a bounded mpsc; the pacer drains that
//! mpsc and broadcasts at exactly one chunk per [`FRAME_PACE`]. All
//! existing subscribers (recorder, lifecycle, ws_audio, future native
//! clients) read the broadcast unchanged and now see chunks arriving on
//! a steady 20 ms wall-clock cadence regardless of decode burstiness.
//!
//! ## Idle handling
//!
//! When the upstream goes quiet (mid-call sync loss, between calls), the
//! pacer's `next_at` falls behind `now`. We snap it back to `now` rather
//! than "catching up" — silence stays silence. The first chunk of the
//! next call emits at its actual arrival time; subsequent chunks pace
//! at 20 ms intervals from there.
//!
//! ## Capture-time invariance
//!
//! [`AudioChunk::captured_at_ms`] is stamped at LDU dispatch in the
//! framer. Pacing only affects DELIVERY timing; the routing key the
//! recorder uses is unchanged. Recorder's per-slot `[open_at_ms,
//! close_at_ms+CLOSING_DRAIN_MS]` window naturally aligns with paced
//! delivery because vocoder + pacer + broadcast latency is bounded by
//! one batch (~180 ms).

use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant as TokioInstant;

use crate::audio::AudioChunk;

/// Wall-clock spacing between consecutive chunks on the broadcast.
/// 160 samples @ 8 kHz = exactly 20 ms of audio per chunk; pacing at
/// the same cadence keeps the broadcast at native source rate.
pub const FRAME_PACE: Duration = Duration::from_millis(20);

/// Default capacity for the vocoder→pacer mpsc. Sized to absorb a
/// transient burst of ~5 s of audio (250 chunks × 20 ms) without
/// blocking the vocoder thread on `blocking_send`. In steady state
/// the queue oscillates 0-9 (one LDU batch); this capacity is purely
/// burst absorption for the post-Tier-A 30× catch-up case.
pub const PACER_INPUT_CAPACITY: usize = 256;

/// Build the vocoder→pacer mpsc. The vocoder thread keeps the
/// `Sender` and uses `blocking_send`; the pacer task takes the
/// `Receiver`.
pub fn pacer_input_channel() -> (mpsc::Sender<AudioChunk>, mpsc::Receiver<AudioChunk>) {
    mpsc::channel(PACER_INPUT_CAPACITY)
}

/// Spawn the pacer task. Drains `rx`, broadcasts on `tx` at one
/// chunk per `FRAME_PACE`. Exits cleanly when the vocoder thread
/// drops the mpsc Sender (graceful shutdown).
pub fn spawn_audio_pacer(
    mut rx: mpsc::Receiver<AudioChunk>,
    tx: broadcast::Sender<AudioChunk>,
) {
    tokio::spawn(async move {
        // The pacing clock. `next_at` is the wall-clock instant at
        // which the next chunk should leave the broadcast. Initialised
        // to "now" so the very first chunk emits immediately.
        let mut next_at = TokioInstant::now();

        while let Some(chunk) = rx.recv().await {
            let now = TokioInstant::now();
            // If we're behind realtime — i.e., the upstream went silent
            // long enough that our scheduled `next_at` is in the past
            // — re-anchor to `now`. The alternative (let `next_at`
            // catch up) would dump the post-silence backlog as a burst,
            // which is exactly what we're here to prevent.
            if next_at < now {
                next_at = now;
            }
            tokio::time::sleep_until(next_at).await;
            // Broadcast errors only happen when zero subscribers exist;
            // ignore — the recorder always subscribes so this is mostly
            // theoretical, and even if it fires the right behavior is
            // to keep draining `rx` (preventing vocoder backpressure).
            let _ = tx.send(chunk);
            next_at += FRAME_PACE;
        }
        tracing::warn!(
            target: "p25_audio_pacer",
            "audio pacer exiting (vocoder mpsc closed)",
        );
    });
}
