# P25 Vocoder Pipeline — Architecture, Costs, and Migration Plan

**Status:** Reference doc. Last updated 2026-04-29 with the audio-pacer shipping notes for build `2026-04-30-audio-pacer`: the vocoder→broadcast path is now gated by a tokio pacer that emits one chunk per 20 ms wall-clock, eliminating the post-Tier-A burst pattern. Verified live: ws/audio inter-arrival p50 = 19.83 ms (target 20.0), `burst_gaps_lt_5ms = 0`, ring buffer holds at 20-50 ms during playback. Pre-pacer: vocoder dumped 9 chunks in <5 ms then quiet for 175 ms, ring oscillated ±720 samples around target, AudioWorklet PLL couldn't lock cleanly.

This document covers:

1. The **JMBE IMBE vocoder** pipeline at [p25-httpd/src/jmbe/mod.rs](../p25-httpd/src/jmbe/mod.rs) — synthesis, per-stage cost, optimisation roadmap, HDL-migration sketch.
2. The **PCM delivery pipeline** from vocoder thread to browser AudioContext — pacer architecture, latency budget, and the AudioWorklet ring/PLL on the receive side.
3. NEON SIMD opportunities elsewhere in the tree.

---

## TL;DR

- **JMBE today (post-Tier-A)**: 483 µs median per-frame, 863 µs p99, 20 000 µs budget. Vocoder duty cycle ~2.4 % of one core. **30× speedup** vs the pre-Tier-A baseline by swapping the O(N²) hand-rolled DFT in `get_unvoiced` for `realfft`/`rustfft`. cpu1 fully freed for the scanner pivot.
- **PCM delivery is now exact realtime.** Build `2026-04-30-audio-pacer` adds a tokio pacer task between the vocoder thread and the broadcast channel. Vocoder writes to a bounded mpsc; pacer drains at exactly one chunk per 20 ms wall-clock and broadcasts to all subscribers (recorder + lifecycle + ws_audio). Idle re-anchor: when `next_at < now`, snap to `now` so silence stays silence. Verified p50 = 19.83 ms, p99 = 21.28 ms over 882 chunks. Vocoder backpressures via `blocking_send` if a sustained over-realtime burst exceeds the 5 s pacer mpsc — no silent drops.
- **AudioWorklet PLL** still runs on the receive side (target = 1200 samples = 150 ms; can probably drop to 320 samples = 40 ms now that delivery is paced and ring oscillation is ±20 samples instead of ±720). PREFILL = 1200 (warm-up). Catch-up policy in worklet was REMOVED 2026-04-30 — it was misinterpreting upstream burst delivery as stale backlog and dropping ~10 s of audio per call.
- **HDL vocoder migration** remains an option but is overkill — Tier A's success means even another 5× headroom isn't needed. Keep FPGA budget for the channelizer redesign.

---

## Voiced vs Unvoiced — the IMBE model in one minute

IMBE models the vocal tract as a hybrid:

- **Voiced** energy (vowels, voiced consonants like /m/ /n/ /v/ /z/) = quasi-periodic, dominated by a fundamental frequency ω₀ and its harmonics. Synthesised as a **sum of sinusoids** at integer multiples of ω₀.
- **Unvoiced** energy (fricatives /s/ /f/, stops, breath noise) = aperiodic, broadband. Synthesised as **shaped white noise** — a wideband DFT pulls a noise spectrum, scales each band by the encoded magnitudes, and inverse-DFTs back to time domain.

The encoder splits the spectrum into ~7–12 frequency bands and decides voiced-or-unvoiced **per band** (the "multi-band" in MBE). A typical voiced consonant might have low bands voiced + high bands unvoiced; pure /s/ has all bands unvoiced; pure /a:/ has nearly all bands voiced.

The decoder synthesises both halves independently and **adds** them. With band-by-band V/UV split this captures most of human speech faithfully at 4 800 bps voice + 2 800 bps FEC = 7 200 bps total.

**Why the cost asymmetry?**

- Voiced synthesis = a few dozen `cos(ω·t + φ)` evaluations per output sample, easily reduced via the recurrence `cos(θ + Δ) = cos(θ)·cos(Δ) − sin(θ)·sin(Δ)` (already shipped).
- Unvoiced synthesis = forward + inverse 256-pt real DFT per frame. **Currently O(N²) hand-rolled**, not FFT — 65 k `cos`/`sin` calls per frame, twice per frame, no easy recurrence trick.

That's why voiced is 33 µs and unvoiced is 14 445 µs in production today.

---

## Pipeline overview

```
                                    ┌──────────────────────────────────────────────┐
  P25 LDU1/LDU2 → 9 IMBE frames     │       JmbeDecoder::decode_frame              │
  18 bytes/frame, 144 bits          │  (ImbeDecoder owns inter-frame state:        │
                                    │   prev_params, prev_phase_o/v, prev_uw,      │
                                    │   noise_gen, white_noise_gen, last_times)    │
                                    └──────────────────────────────────────────────┘
                                                       │
                                                       ▼
              ┌────────────────────────────────────────────────────────────────────┐
              │ 1. Bit unpack (byte→bool[144])                                     │  ~5 µs
              │ 2. Deinterleave (DEINTERLEAVE table, 144 swaps)                    │   │
              │ 3. FEC: Golay(23,12) coset 0  ────── 12 data + 11 parity bits      │   │ "fec"
              │ 4. Derandomise (XOR with seed-driven scrambler)                    │   │ ≈ 92 µs
              │ 5. FEC: Golay(23,12) cosets 1-3                                    │   │ p99 259 µs
              │ 6. FEC: Hamming(15,11) cosets 4-6                                  │   │
              │ 7. Parameter extraction:                                           │   │
              │      ω₀ (8-bit fund_index → freq + L harmonics from table)        │   │
              │      L  (3 ≤ L ≤ 56)                                               │   │
              │      voicing[1..=L]  (per-band V/UV bits)                          │   │
              │      magnitudes m_l (residual decode + log2 + inverse-quantise)    │   │
              │ 8. enhance_spectral_amplitudes() — Algs #105–#111                  │   │
              │ 9. apply_adaptive_smoothing()    — Algs #112–#116                  │   │
              └────────────────────────────────────────────────────────────────────┘
                                                       │
                              ▼ ModelParameters { w0, l, voicing, m_l, … }
                                                       │
              ┌─────────────────────────────────────────────────────────────────────┐
              │ synthesize_voice                                                    │
              │   ┌─────────────────────────────────────────────────────────────┐  │
              │   │ get_unvoiced(params, u)            ←── 98 % of CPU          │  │
              │   │   • Window 256-sample white noise with synthesis_window     │  │ "unvoiced"
              │   │   • Forward DFT 256-pt:  uw → uw_freq                       │  │ ≈ 14 445 µs
              │   │   • Per unvoiced band li:                                   │  │ p99 24 550 µs
              │   │       compute energy in bins [a_min, b_max]                 │  │ MAX 34 581 µs
              │   │       scalar = UNVOICED_SCALING_COEFFICIENT × m_l / √(E/N)  │  │
              │   │       multiply each bin by scalar                           │  │
              │   │   • Inverse DFT 256-pt:  uw_freq → uw_time                  │  │
              │   │   • Weighted overlap-add with previous_uw                   │  │
              │   │   ⇒ 160 unvoiced f32 samples                                │  │
              │   └─────────────────────────────────────────────────────────────┘  │
              │   ┌─────────────────────────────────────────────────────────────┐  │
              │   │ get_voiced(params, u)              ←── 0.2 % (recurrence)   │  │ "voiced"
              │   │   • Phase update: prev_phase_v + ω̄·SPF·li                  │  │ ≈ 33 µs
              │   │   • For each voiced harmonic li ∈ [1..L]:                   │  │ p99 489 µs
              │   │       linear-phase recurrence over 160 samples              │  │
              │   │       (Algs #131/#132/#133 — common case)                   │  │
              │   │     OR direct cos for quadratic-phase Alg #136 (~14 %)      │  │
              │   │   ⇒ 160 voiced f32 samples                                  │  │
              │   └─────────────────────────────────────────────────────────────┘  │
              │   Mix:                                                              │ "mix"
              │     audio[n] = clip((voiced[n] + unvoiced[n]) × 1/32767)            │ ≈ 7 µs
              │   ⇒ [f32; 160]                                                      │
              └─────────────────────────────────────────────────────────────────────┘
                                                       │
                             ▼ [f32; 160] ∈ [-1.0, 1.0]
                                                       │
              ┌─────────────────────────────────────────────────────────────────────┐
              │ vocoder::JmbeDecoder::decode_frame  (wrapper)                       │ "pcm_convert"
              │   Per-sample clamp & convert to i16  ⇒ [i16; 160]                   │ ≈ 14 µs
              └─────────────────────────────────────────────────────────────────────┘
                                                       │
                                                       ▼
              ┌─────────────────────────────────────────────────────────────────────┐
              │ vocoder_task: per-call AGC (RMS EMA + scale smoothing) ⇒ scaled i16 │  ~50 µs (est)
              │ broadcast::Sender<AudioChunk> → /ws/audio + recorder                │
              └─────────────────────────────────────────────────────────────────────┘
```

160 PCM samples per IMBE frame at 8 kHz mono = 20 ms of audio. P25 LDU1 + LDU2 = 9 IMBE frames per 360 ms wire-clock = 50 frames/sec real-time. Per-call ~6 minutes typical; longest typical TG (Clay County hospital TG 320) is single-digit minutes.

---

## Per-stage measured cost

### Post-Tier-A (build `2026-04-29-rustfft-unvoiced`, current)

TG=850 call, 711 frames, NEON build flag + `realfft`/`rustfft`. All µs. Frame budget = 20 000 µs.

| Stage | Median | p99 | Mean | Max | % of total |
|---|---:|---:|---:|---:|---:|
| `unvoiced` (realfft 256-pt) | **75** | 216 | 96 | 10 128 | 15.6 % |
| `voiced` (recurrence + auto-vec) | 263 | 553 | 231 | 9 088 | 54.8 % |
| `fec` | 92 | 228 | 100 | 350 | 19.2 % |
| `pcm_convert` | 14 | 15 | 14 | 253 | 2.9 % |
| `mix` | 7 | 8 | 7 | 150 | 1.5 % |
| **total** | **483** | 863 | 480 | 10 575 | 100 % |

Vocoder thread duty cycle: 483 µs × 50 frames/sec = **2.4 % of one A9 core**. Below the `/api/ps_cores` 250 ms sampling resolution — the thread effectively disappears from the OS-level tasklist.

### Pre-Tier-A (build `2026-04-29-jmbe-stage-timing+neon`, historical)

| Stage | Median | p99 | % of total |
|---|---:|---:|---:|
| `unvoiced` (hand-rolled O(N²) DFT) | **14 445** | 24 550 | **98.2 %** |
| total | 14 703 | 25 829 | 100 % |

The Tier-A swap took unvoiced from 14 445 µs → 75 µs (**192×**), total from 14 703 µs → 483 µs (**30×**), and pulled p99 from 25.8 ms (above budget) → 0.86 ms (4 % of budget).

### What's left

After the unvoiced collapse, the cost picture flipped: `voiced` (post-recurrence) is now the largest single line at 263 µs median. It's still under 1.5 % of frame budget — chasing it would be premature. The remaining max-frame outliers (10 575 µs) are infrequent and look like FFT-planner edge cases or scheduler hiccups; not worth investigating until they're a known quality issue.

---

## Stage-by-stage analysis

### Stage 1–6: bit unpack + FEC chain (~92 µs / 0.6 %)

Sites: [jmbe/mod.rs:827-959](../p25-httpd/src/jmbe/mod.rs#L827-L959), `decode_frame` free function.

- 18 input bytes → 144 bits (LE-interpreted MSB-first).
- `deinterleave` permutes via `DEINTERLEAVE` table (144 entries).
- 4 × Golay(23,12) cosets correct up to t=3 errors per coset.
- 3 × Hamming(15,11) cosets correct up to t=1 error per coset.
- `derandomize` runs after coset 0 to undo the encoder's XOR scrambler.

**Optimization status**: not a bottleneck. Don't touch. The Golay23 implementation is bit-rotational and already efficient. The 8.3 ms `fec` max in the table reflects an outlier frame with multiple uncorrectable errors triggering retry loops; uncommon and not worth chasing.

### Stage 7–9: parameter extraction + spectral processing (rolled into `fec`)

Sites: [compute_fundamental:300](../p25-httpd/src/jmbe/mod.rs#L300), [enhance_spectral_amplitudes:707](../p25-httpd/src/jmbe/mod.rs#L707), [apply_adaptive_smoothing:774](../p25-httpd/src/jmbe/mod.rs#L774).

- ω₀ from 8-bit `fund_index` → table lookup gives (frequency, L harmonic count). L ranges 9–56.
- `voicing[1..=L]` is a packed bit field — V/UV per band.
- `m_l` magnitudes come from spectral residual prediction across `previous_params.log2_spectral` (40 lines of inverse-quantisation table walking + log2 reconstruction).
- Spectral enhancement (Algs #105–#111) is a 50-line numeric refinement of the magnitudes.
- Adaptive smoothing (Algs #112–#116) gates aggressive amplitudes when SNR is low.

**Optimization status**: not a bottleneck (~50 µs total). The `harmonic_allocations`, `quantized_value_indexes`, `step_sizes` and `gain_indexes` lookups currently allocate `Vec<usize>` and `Vec<f32>` per call — see Tier B below for the cleanup.

### Stage 10: `get_voiced` (33 µs median, 0.2 %)

Site: [jmbe/mod.rs:1467-1626](../p25-httpd/src/jmbe/mod.rs#L1467).

What it does: synthesises the periodic part. For each voiced harmonic `li ∈ [1..L]`, contributes `cos(li·ω̄·n + phase)` × magnitude × synthesis_window summed across 160 samples. With L typically 30–50, that's ~30 × 160 = 4 800 cos evaluations per frame *if naive*.

**Optimization status**: shipped 2026-04-29. Two changes:

1. Loop swap `(outer=n, inner=li)` → `(outer=li, inner=n)` lets each harmonic precompute its `cos(Δ)`/`sin(Δ)` once and iterate.
2. Linear-phase complex recurrence: `cos(θ + Δ) = cos(θ)·cos(Δ) − sin(θ)·sin(Δ)`. Two multiply-adds per sample replace one `cos()` libm call.

The quadratic-phase branch (Alg #136, ~14 % of harmonics: `li < 8` AND `|Δω| ≥ 10 % ω₀`) still uses direct `cos` — the recurrence math is messier and the branch was small enough that we shipped without it. Revisit only if `voiced` re-emerges as a bottleneck after Tier A.

NEON contribution (post-build-flag): the recurrence's inner loop is vectorisable f32 multiply-add. Auto-vec almost certainly catches it. The combined effect (recurrence + auto-vec) is what dropped voiced from a measured ~98 % pre-recurrence to 0.2 % today.

### Stage 11: `get_unvoiced` — **THE BOTTLENECK** (14 445 µs median, 98.2 %)

Site: [jmbe/mod.rs:1381-1465](../p25-httpd/src/jmbe/mod.rs#L1381).

```
white_noise[256] (from MbeNoiseGenerator)
   │  multiply by synthesis_window (precomputed)
   ▼
uw[256]
   │  real_dft_forward_256()         ←── 65 536 cos/sin calls
   ▼
uw_freq[256]   (packed re/im pairs)
   │  per-band scaling using m_l and band edges (a_min, b_max)
   ▼
uw_freq_scaled[256]
   │  real_dft_inverse_256()         ←── 65 536 cos/sin calls
   ▼
uw_time[256]
   │  weighted overlap-add with previous_uw
   ▼
unvoiced[160]
```

The **two hand-rolled DFTs** at [real_dft_forward_256:1216](../p25-httpd/src/jmbe/mod.rs#L1216) and [real_dft_inverse_256:1251](../p25-httpd/src/jmbe/mod.rs#L1251) are textbook DFT-by-definition: a doubly-nested loop over (bin, sample) doing `cos(2π·bin·k/N)` and `sin(...)` per inner iteration. **Each invocation does 127 × 256 × 2 = 65 024 transcendental calls**. Per frame both run, so ~130 000 trig calls per 20 ms frame, of which the libm `cos`/`sin` are ~80–100 cycles each on Cortex-A9 (no native trig instruction).

This is an O(N²) algorithm where O(N log N) is well-known. The author's comment at the top of [services/spectrum.rs](../p25-httpd/src/services/spectrum.rs) notes "FFT is hand-rolled rather than pulled in from `rustfft` to avoid the dep" — same author may have made the same call here, but for the hot vocoder path the trade-off is the wrong one by ~30–290×.

**Optimization status**: **next session — Tier A in the roadmap below**.

---

---

## PCM delivery pipeline (vocoder thread → speaker)

This is the path from a 160-sample `[i16; 160]` chunk leaving the vocoder thread to a sample landing on the operator's speaker. With Tier A complete, this is now the dominant source of perceived audio quality issues during live playback. Goal: minimise added buffering, push end-to-end latency toward sub-100 ms, eliminate the inter-LDU clicks/clips.

### End-to-end pipeline diagram

```
                              CORTEX-A9 (PS, p25-httpd)
  ┌────────────────────────────────────────────────────────────────────┐
  │ p25-vocoder thread                                                 │
  │   decoder.decode_frame(imbe) → [f32; 160] → JmbeDecoder → [i16; 160]│
  │   peak-gate (SILENT_PEAK=16) → post-vocoder AGC                    │
  │     RMS EMA α=0.025, scale EMA α=0.08, clamp ±AGC_PCM_CLAMP=30000  │
  │   voc_audio_tx.send(AudioChunk { pcm[160], tg, src, call_id, … })  │
  └─────────────────────┬──────────────────────────────────────────────┘
                        │ tokio::sync::broadcast::channel(256)  ←── 256×20 ms = 5.12 s of buffer
                        │ Fanout to: ws_audio handler, recorder, grant_follower
                        ▼
  ┌────────────────────────────────────────────────────────────────────┐
  │ tokio worker thread → handle_ws_audio                              │
  │   rx.recv() → Message::Binary([320 bytes; LE i16 × 160])           │
  │   tx_sock.send(Binary)                                             │
  │   On Lagged(n): emit Text {"type":"lag","skipped":n}               │
  └─────────────────────┬──────────────────────────────────────────────┘
                        │ TCP / WebSocket binary frames
                        ▼
                              BROWSER (dashboard.html)
  ┌────────────────────────────────────────────────────────────────────┐
  │ ws.onmessage  (main thread)                                        │
  │   evt.data → ArrayBuffer(320) → Int16Array(160)                    │
  │   convert i16 → f32: Float32Array(160) with x = s/32768            │
  │   AUDIO.node.port.postMessage({type:'pcm', data})                  │
  └─────────────────────┬──────────────────────────────────────────────┘
                        │ structured-clone postMessage to worklet thread
                        ▼
  ┌────────────────────────────────────────────────────────────────────┐
  │ AudioWorkletProcessor `p25-audio`  (audio thread)                  │
  │   ring: Float32Array(16384)         ←── 2 s @ 8 kHz                │
  │   PREFILL = 1200 samples (150 ms)   ←── prime threshold            │
  │   UNDERRUN_REPRIME = max(4800, 2 s × ctxRate) ≈ 96000              │
  │   readFrac advances by ratio = 8000 / sampleRate (≈ 0.167 @ 48k)   │
  │   linear interp: out[i] = ring[r] + (ring[r+1] - ring[r]) × frac   │
  │   Empty ring → emit silence + count one underrun event             │
  └─────────────────────┬──────────────────────────────────────────────┘
                        │ AudioContext destination (browser audio device)
                        ▼
  ┌────────────────────────────────────────────────────────────────────┐
  │ Operating system audio (PulseAudio / CoreAudio / WASAPI / …)       │
  │   typical buffer 5-15 ms                                           │
  │ DAC → speaker                                                      │
  └────────────────────────────────────────────────────────────────────┘
```

### Server-side specifics

- **`broadcast::channel(256)`** — [audio/mod.rs:54](../p25-httpd/src/audio/mod.rs#L54). 256 chunks × 20 ms = **5.12 s** of buffering on the server side. Excessive for live playback but matters only if a consumer (recorder, WS) lags. With Tier A's fast vocoder + idle WS handler, lag is rare; current `imbe.queue_high_water=12` confirms it.
- **Fan-out**: `voc_audio_tx` has three real subscribers — recorder, ws_audio, grant_follower. Each gets its own `Receiver`. The recorder is the fattest (sync WAV writes via `tokio::fs`) — it lags first under load, but doesn't block the WS path because broadcast channels have per-receiver cursors.
- **Bursty post-Tier-A**: vocoder produces all 9 chunks of an LDU batch in <5 ms (was ~125 ms pre-Tier-A). 9 `tx.send()` calls land in <5 ms. The WS handler then pumps 9 binary frames to the socket back-to-back.
- **TCP_NODELAY**: not explicitly set. Linux default is Nagle ON (40 ms ACK delay) which CAN coalesce small writes. axum/hyper sets NODELAY for HTTP/2 but for HTTP/1 + WebSocket upgrade the default depends on hyper version. **Worth verifying on next session** — could be 40 ms of unintended latency.
- **WebSocket frame format**: 320-byte binary, little-endian `i16` interleaved. No per-frame metadata (TG/source comes via the lateral `/ws/events` stream). One frame = 20 ms of audio.

### Client-side specifics ([dashboard.html:2225-2400](../p25-httpd/src/httpd/dashboard.html))

The 2026-04-15 redesign already replaced the broken per-chunk `BufferSource` scheduler with a proper AudioWorklet + ring + linear interp. Current state:

| Parameter | Value | Notes |
|---|---|---|
| Ring size | 16 384 samples (2 s) | Headroom only; rarely above ~360 ms in practice |
| PREFILL | 1 200 samples (**150 ms**) | Wait for this much before starting playback |
| Underrun re-prime | max(4 800, 2 s × ctxRate) ≈ 96 000 samples | Continuous empty for 2 s before re-priming |
| Source rate | 8 kHz fixed | Hard-coded constant `SRC_RATE = 8000` |
| Output rate | `sampleRate` (browser native, typically 48 kHz) | No clock discipline — fixed ratio |
| Resample | Linear interp 8 → 48 kHz | One sample-clock step per 6 output samples |
| Insecure-context fallback | `ScriptProcessorNode` with same logic | Works on plain-IP `http://` to the board |

**Underrun handling**: a single empty quantum emits silence and counts as one underrun event (counter ticks once per gap, not per sample — per the comment, "Much more useful health signal than a per-sample counter"). 2 s of continuous empty re-enters priming.

### Latency anatomy (post-Tier-A)

End-to-end "DAC peak coincides with how-much-after-the-original-RF-event":

| Stage | Latency contribution (typical) |
|---|---:|
| RF capture → AD9361 → DDC → LSM demod (HDL) | ~5-10 ms |
| Dibit DMA → control-channel framer → IMBE batch dispatch | ~30-50 ms (LDU buffering) |
| Vocoder thread (post-Tier-A) | <1 ms |
| broadcast → WS handler → socket send | <2 ms |
| TCP / WiFi / LAN | 1-5 ms (LAN) |
| Browser WS recv → postMessage | <1 ms |
| **Worklet PREFILL** | **150 ms** |
| Worklet linear-interp + AudioContext output | 5-15 ms |
| OS audio + DAC | 5-10 ms |
| **Total live-listen latency** | **~200-250 ms** |

**The 150 ms PREFILL is the single largest controllable contribution.** Almost everything else is constrained by physics or browser internals.

### Failure modes for "audio clips during live play"

Three plausible interpretations of "clips," each with a specific mechanism:

#### (a) Inter-LDU click / pop / dropout

**Mechanism**: post-Tier-A bursty arrival. 9 chunks land in <5 ms, ring jumps from low to ~1 440 samples, drains over 180 ms at 8 kHz output rate. Just before next batch arrives, ring hits zero. Network jitter (±5-20 ms LAN, more on WiFi) means the gap may be longer than the residual ring depth → **brief silence at every LDU boundary** = clicks at ~5.5 Hz (one per LDU pair).

**Why pre-Tier-A was less audible**: vocoder spread the 9-chunk decode over ~125 ms of wall-clock, so chunks trickled into the ring continuously. Net ring level stayed ~constant. The decoder's slowness was acting as an inadvertent jitter buffer.

**Fix candidates**:

- **Increase PREFILL slightly** (1 200 → 1 800 samples = 225 ms). +75 ms latency for ~zero clicks. Trivial change to a constant.
- **Add inter-arrival jitter measurement and adaptive PREFILL**: track arrival timestamps; size PREFILL to 2× observed inter-LDU jitter standard deviation. Self-tuning, holds latency at the minimum that's safe.
- **Smooth post-Tier-A burstiness server-side**: vocoder thread `thread::sleep` between chunks (or between LDU batches) to space sends out at ~20 ms. Trades CPU efficiency for steadier delivery. Cleaner on the wire.

#### (b) Distortion from AGC saturation

**Mechanism**: post-vocoder AGC has `AGC_MAX_SCALE = 8.0` and `AGC_PCM_CLAMP = 30000` ([vocoder_task.rs:68-71](../p25-httpd/src/app/vocoder_task.rs#L68-L71)). A quiet speaker with RMS ~500 hits scale=5×; a transient peak at raw 8000 → scaled 40000 → hard-clamped to 30000. Audible as harsh clipping on syllables.

**Fix candidates**:

- **Soft knee limiter** instead of hard clamp. Maps `[-30000, +30000]` linear and `[30000, ∞]` to a tanh that asymptotes to ±32000. Few lines of code.
- **Lower AGC_MAX_SCALE** to 4× or 5× and accept that quiet speakers stay quiet. Loses some dynamic range but eliminates limiter saturation.
- **Look-ahead peak limiter** (a few-ms ring on the AGC output, normalize by ring peak). Higher quality, more code.

#### (c) Sample-rate drift between source and destination

**Mechanism**: vocoder's nominal 8 kHz comes from IMBE wire-clock (P25 standard). Browser's AudioContext nominal 48 kHz comes from the PC's audio crystal. These are independent clocks — typical drift is 50-200 ppm. Over a 30-second call, 100 ppm drift = 3 ms of accumulated phase error. The fixed `ratio = 8000 / sampleRate` doesn't compensate. Eventually the ring either fills (output too slow) → silently drops oldest, or empties early (output too fast) → spurious underruns.

**Fix candidates**:

- **PLL on ring fill level**: target a fixed ring depth (e.g., 600 samples = 75 ms), measure the drift between actual fill and target, slowly adjust `ratio` to drive error toward zero. SDRTrunk does exactly this. ~30 lines in the worklet.
- **Variable-rate resampler** (cubic / sinc) instead of linear interp. Higher quality but doesn't address drift on its own.

### Recommended path: minimum-buffering scanner playback

Operator goal is "as close to realtime as possible." The realistic target with the existing AudioWorklet is **~50-80 ms total live-listen latency** (down from ~200-250 ms today). Plan, in order of value-per-effort:

1. **Verify TCP_NODELAY** on the WS socket (1 hour). Could be a free 0-40 ms saving if Nagle is ON. Set explicitly via `socket.set_nodelay(true)` on the upgraded socket.

2. **Sample-clock PLL in the worklet** (half day). Keeps ring at a fixed target depth (initially 75 ms, can be tuned lower with confidence). Eliminates accumulated drift, which today is the #1 reason PREFILL is sized large. Once PLL is in, PREFILL can drop.

3. **Drop PREFILL to ~40 ms** (5 minutes after PLL lands). One LDU-batch worth of pre-roll. The PLL absorbs jitter; PREFILL only protects against initial ramp.

4. **Adaptive jitter measurement** (3 hours). Track inter-arrival timestamps in the main thread; surface min/median/max/p99 inter-arrival in the dashboard. Use observed variance to tune PREFILL automatically. Optional once #2+#3 are in.

5. **Soft-knee post-vocoder limiter** (1 hour). Replaces the AGC's hard clamp with a tanh. Eliminates "(b)" distortion regardless of latency improvements.

6. **Server-side jitter smoothing** (only if #1-#5 insufficient): add per-chunk wall-clock-paced send in the vocoder thread or a dedicated paced WS sender task. Complicates code and isn't necessary if the client-side PLL works.

After 1-3 land, expected latency budget:

| Stage | Latency |
|---|---:|
| RF + HDL + framer | ~40-60 ms (unchanged) |
| Vocoder thread | <1 ms |
| Server → browser | 1-5 ms |
| **Worklet PREFILL** | **~40 ms** (was 150 ms) |
| AudioContext output | 5-10 ms |
| OS audio | 5-10 ms |
| **Total** | **~90-130 ms** |

Most of the "~40-60 ms RF+HDL+framer" is the LDU batch wait — IMBE frames arrive in 9-frame bursts because the framer dispatches per-LDU. Pushing below 90 ms would require streaming individual IMBE frames out of the framer (fragmenting the 9-frame `ImbeFrameRaw` batch contract), which is a much larger change and probably not worth chasing.

### Diagnostic gap: "I never see the vocoder thread on the dashboard"

Symptom (raised post-Tier-A 2026-04-29): the PS Cores panel shows 12-13 threads, never `p25-vocoder`. Vocoder pcm_produced counter increments normally during calls. Two distinct contributors:

1. **Truncation**: [dashboard.html:3837](../p25-httpd/src/httpd/dashboard.html#L3837) polls `/api/ps_cores?top_n=12`. There are 14 threads (main + vocoder + 12 tokio). At `top_n=12` and sorted by cpu_pct desc, p25-vocoder lands last and gets cut. **Trivial fix: bump to `top_n=20`.**

2. **Sampling alignment**: even with no truncation, the 250 ms `/proc/stat` window catches a 5 ms vocoder burst with probability ~1.4 %. At 2 s panel cadence ≈ 1 hit per 2-3 minutes, and the hit reads ~2 % cpu — barely above noise.

**Better diagnostic**: read `/proc/self/task/<tid>/stat` `utime`+`stime` over a longer rolling window (e.g. 10 s) for a steadier average. Vocoder would surface a stable ~2.4 % regardless of burst alignment. Add as a `cpu_pct_10s` field on each thread in `/api/ps_cores`. ~30 lines in [httpd/api/system.rs](../p25-httpd/src/httpd/api/system.rs) `get_ps_cores`. Bundle with PCM-pipeline work next session.

### What this pipeline is *not* for

Two non-goals worth being explicit about:

- **Bit-perfect playback**: linear interp 8 → 48 kHz introduces ~0.1-0.3 % THD. Voice intelligibility is fine, but anyone trying to ID a speaker by spectral fingerprint should use the recordings, not the live stream.
- **Recording fidelity**: the recorder takes its `AudioChunk` directly from the broadcast — it sees exactly the AGC-scaled i16 the WS sees. No worklet, no interp, no drift. WAV files are clean; only the live stream has the jitter / drift / interp issues. (Confirmed empirically — see memory `project_live_audio_click_artifact.md`.)

---

## Optimisation roadmap

### Tier A — ✅ DONE (`2026-04-29-rustfft-unvoiced`)

**Shipped**: `rustfft = "6"` + `realfft = "3"` added; `ImbeDecoder` owns `Arc<dyn RealToComplex<f32>>` + `Arc<dyn ComplexToReal<f32>>` plans built once in `new()`. Per-frame `process_with_scratch` reuses I/O + scratch buffers — zero heap allocs in the FFT path. Old `real_dft_forward_256` / `real_dft_inverse_256` deleted.

**Result on target** (TG=850, 711 frames):

| Stage | Pre | Post | Speedup |
|---|---:|---:|---:|
| unvoiced median | 14 445 µs | 75 µs | **192×** |
| total median | 14 703 µs | 483 µs | **30×** |
| total p99 | 25 829 µs | 863 µs | **30×** |
| vocoder duty on cpu1 | ~33 % | ~2.4 % | 14× |

Note on NEON specifics: `rustfft` v6.x has explicit NEON kernels only on **aarch64**, not armv7. On Cortex-A9 (armv7) we get the algorithmic O(N²) → O(N log N) win plus the `-C target-feature=+neon,+vfp3` build flag's auto-vec on the inner butterflies. Algorithmic class change dominates regardless.

`test_synthesis_signature_stable` validated within 1 % relative tolerance — synthesis output matches the pre-Tier-A reference, so audio quality is preserved.

### Tier B — heap allocation cleanup (small, opportunistic)

**Effort**: 2 hours.

The IMBE param decode allocates per-frame `Vec`s for fixed lookup tables:

- [`harmonic_allocations(l)`:460](../p25-httpd/src/jmbe/mod.rs#L460) → `[Vec<usize>; 6]`. Static for each L value. Replace with `&'static [&'static [u8]; 6]`.
- [`quantized_value_indexes(l)`:519](../p25-httpd/src/jmbe/mod.rs#L519) → `Vec<Vec<usize>>`. Same — static table.
- [`step_sizes(l)`:577](../p25-httpd/src/jmbe/mod.rs#L577) → `Vec<f32>`. Same.
- [`gain_indexes(l)`:384](../p25-httpd/src/jmbe/mod.rs#L384) — already returns `[usize; 6]`, fine.
- [`get_spectral_amplitude_prediction_residuals`:977](../p25-httpd/src/jmbe/mod.rs#L977), [`resize_spectral`:1063](../p25-httpd/src/jmbe/mod.rs#L1063), [`get_log2_spectral_amplitudes`:1078](../p25-httpd/src/jmbe/mod.rs#L1078) — return `Vec<f32>`. Could be small `[f32; 57]`-style stack arrays since L ≤ 56.

Cortex-A9 with the Buildroot allocator: each `vec!` is roughly 100–300 ns of allocator work. ~14 allocations per frame × 50 frames/sec ≈ 700 allocator hits/sec, ~70–200 µs/sec total. Small but real, and removing it makes the per-frame timing more deterministic (allocator fragmentation jitter goes away).

Keep this in pocket for after Tier A — it'll show up cleanly in stage-timing once unvoiced is sub-millisecond.

### Tier C — NE10 for FFT only (only if Tier A insufficient)

**Effort**: ~1 week elapsed (Buildroot package, cross-compile dance, FFI bindings, sim, test).

NE10's hand-tuned ARMv7 NEON assembly is ~10–20 % faster than rustfft on small FFTs. For a 256-pt complex FFT the difference is maybe 30 µs vs 40 µs — invisible against everything else.

**Don't pursue** unless Tier A measurement shows unvoiced still north of ~3 ms after rustfft+NEON. The scanner pivot only needs vocoder duty < 50 % on one core; rustfft+NEON gets us to ~5 %, so this tier is academic.

### Tier D — HDL migration (escalation path, not the plan)

**When to consider**: Tier A + B lands, vocoder is at 5 % cpu1, but second-chain audio still drops because Linux scheduler jitter exceeds 20 ms occasionally and the queue can't smooth over it. Or for portability to a smaller / cheaper SoC where one A9 core for vocoder is too much.

**What goes in PL** (FPGA fabric, ~6–8 weeks elapsed for a working block):

| PL block | Function | Resources (est.) |
|---|---|---|
| Param register file | AXI-Lite slave for ω₀, L, voicing[57], m_enhanced[57] per frame | 2-3 BRAM, ~1k LUT |
| Harmonic NCO bank | 56 NCOs @ 8 kHz output, time-multiplexed across 160 samples | 8 DSP48, 4 BRAM |
| 256-pt complex IFFT | Xilinx FFT IP, fixed point or float | 1 instance, ~12 DSP48, 6 BRAM |
| Window + overlap-add | Multiply by synthesis_window, sum with prev_uw | 4 DSP48, 2 BRAM |
| Mix + clip | Voiced + unvoiced sum, saturate to i16 | scalar logic |
| AXI-Stream PCM out | DMA target into PS DDR ring | reuse existing iq_dma pattern |

**What stays in PS**: bit unpack, FEC, parameter extraction, V/UV decisions, spectral enhancement, adaptive smoothing, post-vocoder AGC, broadcast/recorder integration. ~all of `decode_frame()` except the synthesis half.

**Interface**: PS computes ModelParameters per incoming IMBE frame, writes ω₀ + voicing + m_l (~250 bytes) to PL register file via AXI-Lite, kicks a `synth_strobe`. PL emits 160 samples to PCM DMA ring within ~2 ms (worst-case path through the IFFT pipeline). PS reads via existing AudioChunk plumbing.

**Why we probably don't need this**: rustfft+NEON projection is a 30–290× speedup, taking us from 33 % cpu1 to 2–5 %. That's already 10× the headroom needed for the scanner pivot. HDL migration costs a release cycle and a Vivado bake just to gain an academic 5 % → 0 %.

**Where HDL is genuinely worth it**: see "Where NEON does NOT apply" below — the things that *should* go to FPGA aren't in the vocoder.

---

## Where NEON applies *outside* the JMBE pipeline

The `RUSTFLAGS="-C target-cpu=cortex-a9 -C target-feature=+neon,+vfp3"` we just enabled in [p25-httpd.mk](../../Tezuka/tezuka_fw/package/p25-httpd/p25-httpd.mk) is global to the binary. Auto-vectorisation should now activate on any tight f32/f64/i16/i32 loop where rustc can prove no aliasing and a vectorisable shape. Confirmed effects beyond the vocoder:

### Already covered by the build flag (no code change needed)

- **`vocoder_task::AGC` RMS + scale loop** [app/vocoder_task.rs:241-262](../p25-httpd/src/app/vocoder_task.rs#L241-L262) — per-frame RMS sum-of-squares + scale apply over 160 i16 samples. Both loops are simple and aliasing-clean; auto-vec should pick them up.
- **`JmbeDecoder` f32→i16 conversion** [vocoder/mod.rs](../p25-httpd/src/vocoder/mod.rs) — 160-sample clamp/round/cast loop. Same.
- **`get_voiced` linear-phase recurrence inner loop** — 160-sample accumulate-multiply. Same. (Confirmed reduction visible in 33 µs median.)
- **Mix + clip loop in `synthesize_voice`** — 160-sample two-input add + scalar multiply + clamp. Same.

For all of these, the right move is **measure before optimising further** — they're all sub-100 µs; adding manual NEON intrinsics is unlikely to be visible.

### Code-change candidates worth considering

| Site | What | Why NEON helps | Priority |
|---|---|---|---|
| [services/spectrum.rs:94](../p25-httpd/src/services/spectrum.rs#L94) `fft_in_place` | Hand-rolled radix-2 DIT FFT (1024–16384 pt) for waterfall | Same `rustfft+neon` swap as Tier A; unifies code. Waterfall poll rate is ~2 Hz so total CPU is small, but the `wideband_power_db` 16k FFT is the only one that's individually visible. | **Low** — do it when touching this file anyway; not worth a dedicated change. |
| [app/autoppm.rs](../p25-httpd/src/app/autoppm.rs) | Trimmed-mean + EMA over residual samples; per-second window | Tight f32 loops over a small vec. Auto-vec should catch; manual NEON would be premature optimisation. | **Skip** unless profiling shows it. |
| [audio/recorder.rs](../p25-httpd/src/audio/recorder.rs) | WAV writer; per-chunk i16 LE serialisation | The serialisation loop is one byte-pack per sample. Trivially auto-vec'd. | **Skip** — already free. |
| `MbeNoiseGenerator` / `WhiteNoiseGenerator` [jmbe/mod.rs:1136-1206](../p25-httpd/src/jmbe/mod.rs#L1136-L1206) | LFSR-style noise; per-sample integer ops | LFSR is sequential by nature. Hard to vectorise. | **Skip**. |
| [maia-httpd/...] (separate daemon) | Spectrum FFT for Maia waterfall | Should mirror the same `+neon` flag in `maia-httpd.mk`. Currently has none. | **Quick win** — same one-liner as we did for p25-httpd. |

### Where NEON does *not* apply (and why this matters for HDL planning)

These paths are PS-bound but **not vectorisable**:

- **Bit-level FEC**: Golay(23,12) and Hamming(15,11) are bit-rotational with branchy correction tables. NEON's f32/i32 SIMD doesn't help; the hot path is `count_ones()`/`u32` XOR. Already cheap (~50 µs total).
- **Per-frame FEC syndrome lookups**: same.
- **TSBK/LDU2 ESS bit unpacking**: same.
- **Control-channel framer**: bit-pattern matching. Already in HDL anyway (`lsm_demod`, `lsm_nid_pipeline`, etc.).

These are also bad candidates for HDL migration — they're already cheap, branchy, decision-heavy code that runs better as ARM scalar than as fabric pipeline. Don't waste FPGA on them.

**The one PS-bound path that is genuinely worth HDL even with rustfft+NEON in place**: large software channelizers (the Stage-1 IQ buffer catch-up plan in `CHANNELIZER_REDESIGN.md`). Those want long-runtime wideband FFTs and per-channel decimating FIRs — exactly what DSP48 slices and Xilinx FFT IP do best. That's where to spend the next FPGA bake budget if Tier A frees vocoder cpu1 as projected.

---

## Open decisions

1. **Land Tier A next session?** If yes: cargo add rustfft, swap the two DFT sites, re-test with `test_synthesis_signature_stable` (loosen tolerance to ~1e-4 relative if needed), bump BUILD_TAG, queue for next bake.
2. **Bundle Tier B with Tier A?** Cleaner change set if combined; both touch JMBE only. Adds maybe 90 minutes.
3. **Mirror NEON flag to `maia-httpd.mk`?** Same one-line edit as p25-httpd, exposes the spectrum FFT to auto-vec. No on-target downside.
4. **Defer Tiers C+D indefinitely**, revisit only if Tier A measurement disappoints.

---

## Appendix: stage-timing instrumentation

The numbers in this doc come from build `2026-04-29-jmbe-stage-timing+neon`. Per-frame stage timings are accumulated by `vocoder_task` and emitted on call boundary as JSON in the `/api/log` ring under category `vocoder`, message `call_end`. Schema:

```json
{
  "tg":           300,
  "frames_in":    612,
  "duration_ms":  77359,
  "stage_us": {
    "frames":      612,
    "fec":         {"median": 92,    "p99": 259,   "max": 8353,  "mean": 123},
    "voiced":      {"median": 33,    "p99": 489,   "max": 498,   "mean": 145},
    "unvoiced":    {"median": 14445, "p99": 24550, "max": 34581, "mean": 14831},
    "mix":         {"median": 7,     "p99": 8,     "max": 25,    "mean": 7},
    "pcm_convert": {"median": 14,    "p99": 15,    "max": 25,    "mean": 14},
    "total":       {"median": 14703, "p99": 25829, "max": 35117, "mean": 15153}
  }
}
```

Pull with `curl -s 'http://192.168.2.1:8080/api/log?category=vocoder&n=200'`. Flush is on TG-change boundary; if you're parked on a single TG, flush won't fire until a different TG lands. Ring is 16 384 entries, default-non-verbose so chatter doesn't evict vocoder summaries (build `2026-04-29-log-verbose-gate` and later).
