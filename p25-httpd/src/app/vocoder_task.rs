//! Vocoder OS thread — JMBE synthesis off the tokio worker pool.
//!
//! Extracted from main.rs on 2026-04-19. Spawned from main during
//! startup; consumes IMBE frame batches from the control-channel
//! decoder's voice handler, runs JMBE, applies a post-vocoder AGC,
//! and broadcasts AudioChunks for both the live WebSocket stream and
//! the per-call recorder. Runs on a dedicated std::thread so blocking
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
/// 2026-04-19: runs synthesis on a `std::thread` rather than a
/// `tokio::spawn(async move ...)` task. Moves JMBE decode off the
/// shared tokio worker pool so:
///   - JMBE blocking (5-15 ms/frame on ARM) can't preempt other
///     tokio tasks;
///   - HTTP request / log-export / broadcast fanout cannot preempt
///     the vocoder's next wake-up;
///   - the `imbe_tx` queue (cap 16) can't back up and start
///     dropping frames on worker-pool scheduling jitter.
/// `blocking_recv()` keeps the same backpressure semantics as the
/// async version.
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

            // 2026-04-19 late: post-vocoder PCM AGC. JMBE outputs raw
            // PCM at whatever level each radio's mic + deviation
            // produced. Different speakers → different levels → the
            // user hears "loud and quiet" because we emit them as-is.
            // SDRTrunk's audio stage applies per-call normalisation;
            // this is the equivalent — a slow-attack EMA on the
            // voiced-frame RMS that scales toward `AGC_TARGET_RMS`.
            // Silent frames (peak under `SILENT_PEAK`) don't update
            // the EMA, so inter-word pauses don't pump the gain up.
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
            // Phase 9.1 (2026-04-15): track the wall clock of the
            // most recent IMBE frame we decoded so `duration_ms` in
            // the call_end summary reflects the actual
            // voice-arrival span, not the full retune-to-retune
            // interval. Before Phase 9.1, duration_ms used
            // `started.elapsed()` which is "time since the first
            // frame of this call was decoded" -- if the follower
            // stayed locked on a TG for 97 s with only 900 ms of
            // real voice and the rest silence+noise-TDU_LCs, the
            // duration was reported as 97244 ms (retune-to-retune
            // wall clock) instead of ~900 ms (actual audio).
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
                // 2026-04-19 late: DO emit TG=0 summaries. These fire
                // when an Idle flicker lets IMBE frames reach the
                // vocoder with tg=0 — they're useful telemetry and we
                // want them in the log ring for debug. SDRTrunk's
                // TalkgroupIdentifier.isValid() filters TG=0 at render
                // time (MutableIdentifierCollection.java:125), so for
                // side-by-side SDRTrunk comparison run the exporter
                // with --sdrtrunk-strict to drop these from the
                // rendered output. The raw /api/log ring keeps them.
                // Phase 9.1: duration = time from first decoded
                // frame to last decoded frame. When only one burst
                // of voice lives inside a long retune-to-retune
                // lock, this shows the real voice length. Falls
                // back to 0 if we somehow flushed without ever
                // latching a frame timestamp (shouldn't happen when
                // frames_in > 0, but be safe).
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
                // 2026-04-19 count-based recorder close — advance
                // the consumed counter for EVERY batch pulled off
                // the queue, including batches that will be skipped
                // (encrypted call) or dropped (TG-change mid-batch).
                // Must stay in lockstep with `frames_submitted`
                // (bumped by 9 in forward_frames on successful
                // send). The recorder reads this to know the
                // vocoder has advanced past a given boundary's
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
                    // Phase 9.1: latch the wall clock of this frame
                    // so the next flush reports the true voice
                    // span instead of the retune-to-retune gap.
                    call_last_frame_at = Some(std::time::Instant::now());

                    // 2026-04-19 late: the silent-chunk drop was
                    // causing single-speaker calls to split into
                    // multiple recordings — a burst of JMBE-silent
                    // frames exceeded the recorder's 1500 ms grace
                    // window and forced a finalise + new-file. The
                    // counter stays for observability, but silent
                    // frames now pass through so the recorder sees
                    // continuous audio. The original "empty WAV"
                    // concern is handled by MIN_KEEPABLE_MS on the
                    // recorder side.
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

                    // 2026-04-19: pull the currently-stashed source
                    // radio ID off the ImbeForwarder atomic. Set by
                    // the grant follower from `GRP_VCH_GRANT.FM`
                    // (primary) and refreshed by the traffic LDU1
                    // LC decoder / Motorola TDULC TALK_COMPLETE
                    // (fallback). `0` = unknown, in which case the
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
