//! Vocoder OS thread — JMBE synthesis off the tokio worker pool.
//!
//! Consumes IMBE frame batches from the control-channel decoder's voice
//! handler, runs JMBE, applies a post-vocoder AGC, and broadcasts
//! AudioChunks for both the live WebSocket stream and the per-call
//! recorder. Runs on a dedicated std::thread so blocking
//! `decoder.decode(...)` calls (5-15 ms/frame on ARM) cannot starve
//! the rest of the daemon.

use std::sync::Arc;

use tokio::sync::mpsc::Sender;

use crate::audio::{self, AudioChunk};
use crate::services::event_log::EventLog;
use crate::app::imbe_forwarder::{ImbeBatchRx, ImbeForwarder};
use crate::vocoder;

/// Spawn the dedicated vocoder OS thread. Consumes `imbe_rx`, pushes
/// decoded `AudioChunk`s into the audio pacer's mpsc inbox, logs
/// per-call summaries into `voc_event_log`.
///
/// Runs synthesis on a `std::thread` rather than `tokio::spawn` to
/// move JMBE decode (5-15 ms/frame on ARM) off the shared tokio
/// worker pool — prevents JMBE blocking from preempting other tokio
/// tasks, prevents HTTP/log/broadcast work from preempting the
/// vocoder's next wake-up, and keeps the `imbe_tx` queue (cap 16)
/// from backing up on worker-pool scheduling jitter. `blocking_recv()`
/// preserves the backpressure semantics of the async version.
///
/// `voc_audio_tx` is the mpsc input to `app::audio_pacer`; the pacer
/// drains it at exactly one chunk per 20 ms of wall-clock and
/// broadcasts on the existing `audio::AudioTx`. Vocoder uses
/// `blocking_send` so transient bursts that exceed the pacer's input
/// capacity (5 s @ 50 fps) backpressure the vocoder thread instead of
/// silently dropping audio.
pub fn spawn_vocoder_thread(
    imbe_rx: ImbeBatchRx,
    voc_forwarder: Arc<ImbeForwarder>,
    voc_audio_tx: Sender<AudioChunk>,
    voc_event_log: Arc<EventLog>,
) {
    // Change 066: one thread per traffic chain.
    let lane = voc_forwarder.lane;
    let name = match lane {
        crate::hardware::traffic_lane::Lane::One => "p25-vocoder".to_string(),
        l => format!("p25-vocoder{}", l.number()),
    };
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            use std::sync::atomic::Ordering;
            let mut decoder = vocoder::JmbeDecoder::new();
            let mut rx = imbe_rx;

            // Per-call accumulators. `call_tg` is the TG the current
            // accumulator belongs to; flushed on reset or TG change.
            let mut call_tg: u16 = 0;
            // 2026-04-30 agc-speaker-reset: track the SRC of the most
            // recent batch so we can snap the AGC EMA on speaker
            // change within a multi-speaker call. Without this, a
            // quiet field-radio first half of a bundled call leaves
            // the AGC scale cranked up; the dispatcher taking over
            // mid-call gets crackle from tanh saturation until the
            // ~800 ms RMS EMA tau catches up. 2026-04-30 rec 176
            // confirmed: same dispatcher (1013) on rec 171 had
            // R=0.94 (clean), but second half of rec 176 dropped to
            // R=0.22-0.54 with every window peaking at -0.2 dBFS.
            let mut call_source: u32 = 0;
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
            // Soft-knee limiter (post-AGC). Linear pass-through up to
            // ±AGC_KNEE; beyond that, samples saturate via tanh toward
            // ±AGC_CEIL — well inside the i16 range (±32767). Replaced
            // a flat hard clamp at ±30000 which produced harsh clip
            // distortion on loud transients (e.g. console mic peaks).
            // Cost: one tanh call per saturated sample, taken only on
            // the rare overshoots.
            //
            // 2026-04-30 v2: KNEE set ABOVE JMBE's natural peak so
            // normal voice passes linearly. JMBE clips synthesis at
            // MAX_AUDIO_AMPLITUDE=0.95 (= 31128 in i16). Earlier
            // attempt (24000/28000) had the inverse intuition — a
            // knee BELOW 31128 means every loud transient
            // tanh-saturates. KNEE=31000 puts the soft-knee at
            // JMBE's clip line; CEIL=32700 leaves margin before i16
            // wrap. Limiter therefore engages only on AGC-overshoot
            // peaks (when AGC scaled a quiet source up enough that
            // scaled-output exceeds the i16 range) — exactly the use
            // case it exists for.
            const AGC_KNEE: f32 = 31000.0;
            const AGC_CEIL: f32 = 32700.0;
            // Wall clock of the most recent decoded IMBE frame, so
            // `duration_ms` in call_end reflects first-frame to
            // last-frame voice span, not retune-to-retune interval.
            let mut call_last_frame_at: Option<std::time::Instant> = None;

            // Per-frame stage timings accumulated across the call.
            // Flushed (sorted, summarised) by flush_call_summary on
            // call boundary. One entry per IMBE frame decoded.
            let mut call_stage_times: Vec<vocoder::DecodeStageTimes> = Vec::new();

            let flush_call_summary = |
                tg: u16,
                frames_in: u32,
                frames_skipped_enc: u32,
                pcm_samples: u64,
                started: Option<std::time::Instant>,
                last_frame_at: Option<std::time::Instant>,
                stage_times: Vec<vocoder::DecodeStageTimes>,
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

                // Per-stage profile. Sort each stage's u32 vector once,
                // pull median / p99 / max / mean. With a 30 s call at
                // ~50 frames/sec, each Vec is 1500 entries — sort is
                // ~12 µs once per call.
                let stage_summary = summarise_stage_times(&stage_times);

                log.push(
                    crate::services::event_log::LogCategory::Vocoder,
                    format!(
                        "call_end TG={} frames={} pcm={} ({} ms) total_med_us={}{}",
                        tg, frames_in, pcm_samples, duration_ms,
                        stage_summary
                            .as_ref()
                            .and_then(|s| s.get("total"))
                            .and_then(|t| t.get("median"))
                            .and_then(|m| m.as_u64())
                            .unwrap_or(0),
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
                        "stage_us":           stage_summary,
                    }),
                );
            };

            tracing::info!(target: "p25_vocoder", "vocoder thread started (dedicated OS thread)");
            while let Some(batch) = rx.blocking_recv() {
                let batch_tg = batch.talkgroup;
                let batch_source = batch.source;
                let batch_call_id = batch.call_id;
                let batch_captured_at_ms = batch.captured_at_ms;
                let batch_airtime = batch.airtime;
                let frames = batch.frames;
                // Surface the TG of the batch we're ABOUT to decode
                // on /api/traffic. Distinct from current_talkgroup
                // (follower's intent) — this is what the audio path
                // is actually working on.
                voc_forwarder.last_batch_tg.store(batch_tg, Ordering::Relaxed);
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

                // Resolve the effective TG for THIS batch. batch_tg is
                // the TG the forwarder captured at send time. A 0
                // (briefly during retune races, or no-TG-yet start-up)
                // keeps the previous call_tg so a single idle flicker
                // doesn't split a call. A real non-zero change from
                // the prior call's TG is our cue to flush + reset.
                let effective_tg = if batch_tg == 0 { call_tg } else { batch_tg };

                // Consume (and ignore) the reset_pending flag. With
                // per-batch TG labelling, the batch's own TG is the
                // sole boundary signal — reset_pending from the
                // follower raced the tail of the previous call and
                // was causing premature JMBE resets on frames that
                // belonged to the OLD call. Kept as a no-op swap
                // so the follower's store is consumed (prevents
                // stale flag surviving across multiple retunes).
                let _ = voc_forwarder
                    .vocoder_reset_pending
                    .swap(false, Ordering::Relaxed);

                // Call boundary = the batch's TG differs from the
                // prior call's TG. Flush summary, reset JMBE, reseed.
                let tg_changed = effective_tg != call_tg
                    && (call_frames_in > 0 || call_started.is_some());
                if tg_changed {
                    flush_call_summary(
                        call_tg, call_frames_in, call_frames_skipped_enc,
                        call_pcm_samples, call_started, call_last_frame_at,
                        std::mem::take(&mut call_stage_times),
                        &voc_event_log,
                    );
                    decoder.reset();
                    call_tg = effective_tg;
                    // Fresh call: clear source tracking + snap AGC
                    // to seed so a previous-call's adapted gain
                    // doesn't bleed into the new TG's first frames.
                    call_source = 0;
                    agc_rms_ema = AGC_TARGET_RMS;
                    agc_scale = 1.0;
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

                // 2026-04-30 agc-speaker-reset. Same TG, different
                // speaker (multi-source bundled call) — the prior
                // speaker's AGC adapted gain is wrong for this one.
                // Snap the EMA back to TARGET so the new speaker's
                // first ~5 frames adapt fresh, instead of riding
                // the limiter for ~800 ms while the EMA drifts.
                // Gates: both sides non-zero (a 0 batch_source is a
                // racey-no-source-yet artefact, not a real change),
                // and we already have at least one frame in the
                // call (so we don't trigger on call open before
                // call_source has been seeded).
                if batch_source != 0
                    && call_source != 0
                    && batch_source != call_source
                    && call_frames_in > 0
                {
                    agc_rms_ema = AGC_TARGET_RMS;
                    agc_scale = 1.0;
                    voc_event_log.push(
                        crate::services::event_log::LogCategory::Vocoder,
                        format!(
                            "agc_reset TG={} src {} -> {}",
                            call_tg, call_source, batch_source,
                        ),
                        serde_json::json!({
                            "tg":         call_tg,
                            "from_src":   call_source,
                            "to_src":     batch_source,
                        }),
                    );
                }
                if batch_source != 0 {
                    call_source = batch_source;
                }
                // Change 054: the encryption decision travels with the
                // batch (captured when the LDU was decoded, from the
                // air-time epoch in airtime mode). Reading the live
                // `call_encrypted` here let a retune to a clear call
                // decode an encrypted call's in-flight tail, or skip a
                // clear call's tail after a retune to an encrypted one.
                let encrypted = batch.encrypted;
                if encrypted {
                    voc_forwarder
                        .vocoder_frames_encrypted
                        .fetch_add(9, Ordering::Relaxed);
                    call_frames_skipped_enc += 9;
                    // Change 057: per-call count, by the batch's call.
                    voc_forwarder
                        .call_counts
                        .update(batch_call_id, |c| c.vocoder_encrypted += 9);
                    continue;
                }
                let tg = effective_tg;
                // Change 057: this batch's vocoder counts, added to its
                // call in one update after the loop.
                let (mut b_errors, mut b_silent) = (0u64, 0u64);
                for frame in &frames {
                    let pcm = decoder.decode_frame(frame);
                    call_stage_times.push(decoder.last_decode_times());
                    voc_forwarder
                        .vocoder_pcm_produced
                        .fetch_add(vocoder::SAMPLES_PER_FRAME as u64, Ordering::Relaxed);
                    if decoder.last_error_count() > vocoder::ERROR_FRAME_BITS {
                        voc_forwarder.vocoder_errors.fetch_add(1, Ordering::Relaxed);
                        b_errors += 1;
                    }
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
                        b_silent += 1;
                    }

                    // Source radio ID bundled with the batch at submit
                    // time by the forwarder. Pre-2026-04-24 the vocoder
                    // read `current_source` here, which created a race:
                    // a follower retune updating current_source mid-
                    // tail-drain would stamp in-flight audio with the
                    // NEW speaker's source, making the recorder split
                    // the tail into a zero-LDU fragment file. Per-batch
                    // source eliminates the race the same way per-batch
                    // TG did for talkgroup.
                    let source = batch_source;

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
                        let abs_v = v.abs();
                        let limited = if abs_v <= AGC_KNEE {
                            v
                        } else {
                            let span = AGC_CEIL - AGC_KNEE;
                            let soft = span * ((abs_v - AGC_KNEE) / span).tanh();
                            v.signum() * (AGC_KNEE + soft)
                        };
                        *s = limited as i16;
                    }

                    // Hand off to the audio pacer (mpsc, bounded). Pacer
                    // drains at 20 ms wall-clock cadence and broadcasts
                    // to all subscribers. `blocking_send` matches the
                    // OS-thread context: a full pacer mpsc means we've
                    // produced > 5 s of audio faster than realtime, at
                    // which point the right behaviour is to backpressure
                    // the vocoder (=> backpressure the IMBE forwarder
                    // => surface as imbe_queue depth) rather than drop.
                    // Phase 2h (2026-04-25): batch_call_id stamps every
                    // chunk with the GrantFollower call_id active when
                    // forward_frames submitted the batch. The recorder
                    // routes by this directly — no tg+source heuristic,
                    // no cross-call bleed from in-flight PCM.
                    let _ = voc_audio_tx.blocking_send(audio::AudioChunk {
                        pcm: scaled,
                        talkgroup: tg,
                        source,
                        call_id: batch_call_id,
                        captured_at_ms: batch_captured_at_ms,
                        airtime: batch_airtime,
                        lane,
                    });
                }
                voc_forwarder.call_counts.update(batch_call_id, |c| {
                    c.vocoder_pcm_samples += (frames.len() * vocoder::SAMPLES_PER_FRAME) as u64;
                    c.vocoder_errors += b_errors;
                    c.vocoder_silent += b_silent;
                });
            }
            tracing::warn!(target: "p25_vocoder", "vocoder thread exiting (channel closed)");
        })
        .expect("failed to spawn p25-vocoder OS thread");
}

/// Sort + summarise per-frame stage timings for a single call.
///
/// Returns `Some` only if at least one frame was decoded; the
/// `flush_call_summary` early-out for `frames_in == 0` already covers
/// the empty case but the `None` here keeps the JSON tidy if the call
/// was all-encrypted.
fn summarise_stage_times(
    times: &[vocoder::DecodeStageTimes],
) -> Option<serde_json::Value> {
    if times.is_empty() {
        return None;
    }

    let n = times.len();
    let mut fec: Vec<u32> = Vec::with_capacity(n);
    let mut voiced: Vec<u32> = Vec::with_capacity(n);
    let mut unvoiced: Vec<u32> = Vec::with_capacity(n);
    let mut mix: Vec<u32> = Vec::with_capacity(n);
    let mut pcm_convert: Vec<u32> = Vec::with_capacity(n);
    let mut total: Vec<u32> = Vec::with_capacity(n);
    for t in times {
        fec.push(t.fec_us);
        voiced.push(t.voiced_us);
        unvoiced.push(t.unvoiced_us);
        mix.push(t.mix_us);
        pcm_convert.push(t.pcm_convert_us);
        total.push(t.total_us);
    }
    Some(serde_json::json!({
        "frames":      n,
        "fec":         stage_stats(&mut fec),
        "voiced":      stage_stats(&mut voiced),
        "unvoiced":    stage_stats(&mut unvoiced),
        "mix":         stage_stats(&mut mix),
        "pcm_convert": stage_stats(&mut pcm_convert),
        "total":       stage_stats(&mut total),
    }))
}

/// Sort in place, return median / p99 / max / mean as JSON.
/// Sort is stable across the call (~12 µs at 1500 frames per call).
fn stage_stats(samples: &mut [u32]) -> serde_json::Value {
    if samples.is_empty() {
        return serde_json::json!({"median": 0, "p99": 0, "max": 0, "mean": 0});
    }
    samples.sort_unstable();
    let n = samples.len();
    let median = samples[n / 2];
    // p99 index — round-down is fine for our sample sizes.
    let p99_idx = ((n as f32 * 0.99) as usize).min(n - 1);
    let p99 = samples[p99_idx];
    let max = *samples.last().unwrap();
    let mean = (samples.iter().map(|&x| x as u64).sum::<u64>() / n as u64) as u32;
    serde_json::json!({
        "median": median,
        "p99":    p99,
        "max":    max,
        "mean":   mean,
    })
}
