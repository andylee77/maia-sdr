//! Vocoder OS thread — JMBE synthesis off the tokio worker pool.
//!
//! Consumes IMBE frame batches from the control-channel decoder's voice
//! handler, runs JMBE, applies a post-vocoder AGC, and broadcasts
//! AudioChunks for both the live WebSocket stream and the per-call
//! recorder. Runs on a dedicated std::thread so blocking
//! `decoder.decode(...)` calls (5-15 ms/frame on ARM) cannot starve
//! the rest of the daemon.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::mpsc::Receiver;
use tokio::sync::broadcast;

use crate::audio::{self, AudioChunk};
use crate::services::event_log::EventLog;
use crate::app::imbe_forwarder::ImbeForwarder;
use crate::protocol::p25::voice_frame::ImbeFrameRaw;
use crate::vocoder;

/// Spawn the dedicated vocoder OS thread. Consumes `imbe_rx`,
/// broadcasts on `voc_audio_tx`, logs per-call summaries into
/// `voc_event_log`.
///
/// Runs synthesis on a `std::thread` rather than `tokio::spawn` to
/// move JMBE decode (5-15 ms/frame on ARM) off the shared tokio
/// worker pool — prevents JMBE blocking from preempting other tokio
/// tasks, prevents HTTP/log/broadcast work from preempting the
/// vocoder's next wake-up, and keeps the `imbe_tx` queue (cap 16)
/// from backing up on worker-pool scheduling jitter. `blocking_recv()`
/// preserves the backpressure semantics of the async version.
pub fn spawn_vocoder_thread(
    imbe_rx: Receiver<[ImbeFrameRaw; 9]>,
    voc_forwarder: Arc<ImbeForwarder>,
    voc_audio_tx: broadcast::Sender<AudioChunk>,
    voc_event_log: Arc<EventLog>,
) {
    std::thread::Builder::new()
        .name("p25-vocoder".into())
        .spawn(move || {
            use std::sync::atomic::Ordering;
            let mut decoder = vocoder::JmbeDecoder::new();
            let mut rx = imbe_rx;

            // Per-call accumulators. `call_tg` is the TG the current
            // accumulator belongs to; flushed on reset or TG change.
            let mut call_tg: u16 = 0;
            let mut call_frames_in: u32 = 0;
            let mut call_frames_skipped_enc: u32 = 0;
            let mut call_pcm_samples: u64 = 0;
            let mut call_started: Option<std::time::Instant> = None;

            // Post-vocoder PCM AGC. JMBE outputs raw PCM at whatever
            // level each radio's mic + deviation produced, so different
            // speakers arrive at different levels. Mirrors SDRTrunk's
            // per-call normalisation: slow-attack EMA on voiced-frame
            // RMS that scales toward `AGC_TARGET_RMS`. Silent frames
            // (peak under `SILENT_PEAK`) don't update the EMA, so
            // inter-word pauses don't pump the gain up.
            let mut agc_rms_ema: f32 = 2500.0;  // seed at target
            let mut agc_scale: f32 = 1.0;
            const AGC_TARGET_RMS: f32 = 2500.0;
            // ~40-frame (800 ms) time constant on the RMS tracker.
            const AGC_RMS_ALPHA: f32 = 0.025;
            // Scale smoothing keeps per-frame gain changes gentle
            // even if the RMS EMA jumps between speakers.
            const AGC_SCALE_ALPHA: f32 = 0.08;
            const AGC_MIN_SCALE: f32 = 0.25;
            const AGC_MAX_SCALE: f32 = 8.0;
            // Hard ceiling to prevent clipping on scaled output.
            const AGC_PCM_CLAMP: f32 = 30000.0;
            // Wall clock of the most recent decoded IMBE frame, so
            // `duration_ms` in call_end reflects first-frame to
            // last-frame voice span, not retune-to-retune interval.
            let mut call_last_frame_at: Option<std::time::Instant> = None;

            let flush_call_summary = |
                tg: u16,
                frames_in: u32,
                frames_skipped_enc: u32,
                pcm_samples: u64,
                started: Option<std::time::Instant>,
                last_frame_at: Option<std::time::Instant>,
                log: &std::sync::Arc<crate::services::event_log::EventLog>,
            | {
                if frames_in == 0 && frames_skipped_enc == 0 {
                    return;
                }
                // TG=0 summaries are emitted (useful telemetry when an
                // Idle flicker lets IMBE frames through with tg=0).
                // SDRTrunk's TalkgroupIdentifier.isValid() filters TG=0
                // at render time (MutableIdentifierCollection.java:125);
                // run the exporter with --sdrtrunk-strict to match.
                // duration_ms = first decoded frame → last decoded
                // frame; falls back to 0 if we flushed without ever
                // latching a frame timestamp.
                let duration_ms = match (started, last_frame_at) {
                    (Some(s), Some(l)) => {
                        l.duration_since(s).as_millis() as u64
                    }
                    _ => 0,
                };
                log.push(
                    crate::services::event_log::LogCategory::Vocoder,
                    format!(
                        "call_end TG={} frames={} pcm={} ({} ms){}",
                        tg, frames_in, pcm_samples, duration_ms,
                        if frames_skipped_enc > 0 {
                            format!(" enc_skipped={}", frames_skipped_enc)
                        } else {
                            String::new()
                        },
                    ),
                    serde_json::json!({
                        "tg":                 tg,
                        "frames_in":          frames_in,
                        "frames_skipped_enc": frames_skipped_enc,
                        "pcm_samples":        pcm_samples,
                        "duration_ms":        duration_ms,
                    }),
                );
            };

            tracing::info!(target: "p25_vocoder", "vocoder thread started (dedicated OS thread)");
            while let Some(frames) = rx.blocking_recv() {
                // Count-based recorder close: advance the consumed
                // counter for EVERY batch pulled off the queue,
                // including skipped (encrypted) and dropped (TG change
                // mid-batch). Must stay in lockstep with
                // `frames_submitted` (bumped by 9 in forward_frames on
                // successful send); the recorder reads this to know
                // the vocoder has advanced past a boundary's
                // `expected_submit_count`.
                voc_forwarder
                    .frames_consumed
                    .fetch_add(9, Ordering::Relaxed);

                // Reset on call boundary (raised by the follower on
                // new retune).
                if voc_forwarder.vocoder_reset_pending.swap(false, Ordering::Relaxed) {
                    flush_call_summary(
                        call_tg, call_frames_in, call_frames_skipped_enc,
                        call_pcm_samples, call_started, call_last_frame_at,
                        &voc_event_log,
                    );
                    decoder.reset();
                    call_tg = voc_forwarder
                        .current_talkgroup.load(Ordering::Relaxed);
                    call_frames_in = 0;
                    call_frames_skipped_enc = 0;
                    call_pcm_samples = 0;
                    call_started = Some(std::time::Instant::now());
                    call_last_frame_at = None;
                    voc_event_log.push(
                        crate::services::event_log::LogCategory::Vocoder,
                        format!("call_start TG={}", call_tg),
                        serde_json::json!({ "tg": call_tg }),
                    );
                }
                let encrypted = voc_forwarder.call_encrypted.load(Ordering::Relaxed);
                if encrypted {
                    voc_forwarder
                        .vocoder_frames_encrypted
                        .fetch_add(9, Ordering::Relaxed);
                    call_frames_skipped_enc += 9;
                    continue;
                }
                let tg = voc_forwarder.current_talkgroup.load(Ordering::Relaxed);
                // Auto-flush if TG changed without an explicit reset
                // (e.g. follower mid-call TG reassignment).
                if tg != call_tg && (call_frames_in > 0 || call_started.is_some()) {
                    flush_call_summary(
                        call_tg, call_frames_in, call_frames_skipped_enc,
                        call_pcm_samples, call_started, call_last_frame_at,
                        &voc_event_log,
                    );
                    call_tg = tg;
                    call_frames_in = 0;
                    call_frames_skipped_enc = 0;
                    call_pcm_samples = 0;
                    call_started = Some(std::time::Instant::now());
                    call_last_frame_at = None;
                }
                for frame in &frames {
                    let pcm = decoder.decode_frame(frame);
                    voc_forwarder
                        .vocoder_pcm_produced
                        .fetch_add(vocoder::SAMPLES_PER_FRAME as u64, Ordering::Relaxed);
                    call_frames_in += 1;
                    call_pcm_samples += vocoder::SAMPLES_PER_FRAME as u64;
                    // Latch frame wall clock for the first-frame to
                    // last-frame duration_ms in call_end.
                    call_last_frame_at = Some(std::time::Instant::now());

                    // Silent frames pass through so the recorder sees
                    // continuous audio and single-speaker calls don't
                    // split across JMBE-silent bursts exceeding the
                    // 1500 ms grace window. Counter stays for
                    // observability; empty-WAV is handled by
                    // MIN_KEEPABLE_MS on the recorder side.
                    const SILENT_PEAK: u16 = 16;
                    let peak = pcm.iter()
                        .map(|s| s.unsigned_abs())
                        .max()
                        .unwrap_or(0);
                    if peak < SILENT_PEAK {
                        voc_forwarder
                            .vocoder_frames_silent_observed
                            .fetch_add(1, Ordering::Relaxed);
                    }

                    // Source radio ID: set by the grant follower from
                    // `GRP_VCH_GRANT.FM` (primary); refreshed by the
                    // traffic LDU1 LC decoder / Motorola TDULC
                    // TALK_COMPLETE (fallback). `0` = unknown →
                    // recorder leaves the `_fromN` suffix off.
                    let source = voc_forwarder
                        .current_source
                        .load(Ordering::Relaxed);

                    // Post-vocoder AGC. Only voiced frames drive the
                    // RMS EMA; silent frames get the current scale
                    // applied but don't update gain state. Updates
                    // are exponential so speaker A → speaker B level
                    // change is tracked over ~1 s, which is fast
                    // enough to be audible-correct without pumping
                    // on individual loud syllables.
                    let mut scaled = pcm;
                    if peak >= SILENT_PEAK {
                        let sum_sq: f64 = scaled
                            .iter()
                            .map(|&s| (s as f64) * (s as f64))
                            .sum();
                        let rms = (sum_sq / scaled.len() as f64).sqrt() as f32;
                        if rms > 0.0 {
                            agc_rms_ema =
                                (1.0 - AGC_RMS_ALPHA) * agc_rms_ema
                                + AGC_RMS_ALPHA * rms;
                            let target = (AGC_TARGET_RMS / agc_rms_ema.max(1.0))
                                .clamp(AGC_MIN_SCALE, AGC_MAX_SCALE);
                            agc_scale =
                                (1.0 - AGC_SCALE_ALPHA) * agc_scale
                                + AGC_SCALE_ALPHA * target;
                        }
                    }
                    for s in scaled.iter_mut() {
                        let v = (*s as f32) * agc_scale;
                        *s = v.clamp(-AGC_PCM_CLAMP, AGC_PCM_CLAMP) as i16;
                    }

                    // Push to audio broadcast (ignore if no subscribers)
                    let _ = voc_audio_tx.send(audio::AudioChunk {
                        pcm: scaled,
                        talkgroup: tg,
                        source,
                    });
                }
            }
            tracing::warn!(target: "p25_vocoder", "vocoder thread exiting (channel closed)");
        })
        .expect("failed to spawn p25-vocoder OS thread");
}
