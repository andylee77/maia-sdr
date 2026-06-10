# Live-Glitch Validation Plan — HDL Control + Traffic

**Goal:** keep the HDL chain as the production control + traffic decoder, and find the
live-only glitch through a staged bisect with explicit pass/fail gates.

**Date:** 2026-06-09. **Owner:** Andy (board/bakes) + Claude (code/analysis).

## The framing observations (2026-06)

Three field facts drive this plan:

1. The PS software demod, run **live** on the board, produced the **same glitches** as
   the HDL chain.
2. **Perfect offline decoding has only ever been achieved on libiio-path IQ captures**
   (the known-good ADI capture path kept on the same boot).
3. **The custom wideband DMA ring has never produced a stable traffic capture.**

Conclusion: the demod math on both sides is exonerated (SW pipeline is ~99 % bit-exact
with SDRTrunk offline; HDL implements the same algorithms). RF, AD9361, and front-end
gain are exonerated by fact 2 (clean libiio captures come through the same hardware).
What is NOT exonerated — and is now the prime suspect — is the **custom DMA ring
transport**: HDL `DmaStreamRingWrite` → `maia-kmod` → PS readers. It carries:

- the **wideband IQ** that feeds the live SW demod (glitchy) and the DMA-ring captures
  (never stable), and
- the **HDL dibits** that feed the PS framer (forensics runs already showed
  `gap_dibits=14336, overflows=1` on a 0.85 s window).

One transport-layer fault explains every symptom at once: both demods glitch live,
DMA-ring captures are unstable, libiio captures are clean, and the HDL chain "looks"
like it decodes badly when in fact its dibits may be arriving gapped/corrupted.

Secondary suspects (tested only if transport is exonerated): grant/call **state
handling** (mid-call resets, NCO writes, spurious closes — PLL/AGC handling of
traffic) and the shared **audio path** (vocoder → pacer → WS).

## Bench rig — deterministic TX→RX (2nd Pluto + attenuators)

Hardware available 2026-06: a second PlutoSDR (TX) and SMA attenuators. This makes
every stage below repeatable and gives each build a quantitative regression score.

**Setup:**

- Pluto TX → attenuator chain → Fishball RX, **cabled, antenna disconnected** (no
  over-the-air transmission on public-safety frequencies; also kills multipath and
  interference, making runs bit-repeatable).
- Start with heavy attenuation (~60 dB total incl. TX gain backoff) and trim until
  the Fishball front end sees off-air-like levels — match the AGC gain /
  constellation amplitude observed on the real site, since RX gain is fixed manual.
- Measure the Pluto-vs-Fishball clock offset once on the bench (CC PLL bias, same
  method as the lo_shift=470 calibration) and set `lo_shift_hz` accordingly; it is a
  deterministic constant on the bench.

**Stimulus options, ranked:**

1. **Wideband site replay (primary).** TX-replay a 4 MSPS wideband capture from
   `my_captures/` (cyclic IIO buffer, e.g. pyadi-iio script — new tool
   `tools/p25_bench_tx_replay.py`). The capture spans CC + traffic channels, so the
   full chain (CC decode → grant → retune → traffic decode) runs against RF whose
   ground truth is already known bit-exactly (SDRTrunk logs/.bits/audio for the same
   recording + our SW-oracle decode). Glitches become reproducible run-over-run.
2. **Scenario clips.** Cut wideband excerpts: single call, rapid re-grant burst,
   same-freq back-to-back PTTs, call with mid-call fade — each becomes a named,
   repeatable test case for Stage C (lifecycle) and reacquisition timing.
3. **Synthetic patterns.** CW / two-tone for transport integrity at exact known
   content; straight-C4FM known-dibit stream for end-to-end BER (note: synthetic is
   C4FM, not LSM simulcast — fine for transport/BER, not for simulcast realism).

**What each stage gains:** Stage A — identical stimulus into both transports, so any
libiio-vs-DMA-ring delta is pure transport; Stage B — known content makes corruption
detection exact, and attenuator/TX-gain steps probe load and level sensitivity;
Stage C — scripted grant/PTT scenarios on demand instead of waiting for live traffic;
Stage D — level sweeps via attenuation for AGC/PLL robustness curves. Cross-cutting —
a fixed replay clip + LDU/IMBE/glitch-count score is the per-build regression gate.

**Bench caveats:** Pluto TX adds its own impairments (TX LO leakage at band center,
IQ imbalance, 12-bit DAC). Keep channels of interest off the replay center frequency
(the captures' center 859.21297 MHz already sits between channels), and confirm any
bench-found fix against one live off-air session before declaring victory.

## Stage A — Transport A/B: libiio vs DMA ring (DECISIVE, no new build)

Same air, two transports, one decoder. Runs on the currently-flashed build.

- **A.1 Paired capture.** Capture the control channel (always-on, strong: Clay
  860.9625) and then a traffic window: once via the **libiio path**, once via the
  **wideband DMA ring** (`/api/wideband_iq_capture` to /mnt/sd). Interleave
  back-to-back repeatedly; also run one truly **simultaneous** pair and one
  **isolated** pair (only one transport active) to separate bandwidth-contention
  effects from inherent ring faults.
- **A.2 Decode both with the same SW oracle** (`SOFTDEC_HALFBAND=1`, ±2 ppm sweep,
  full-chain per the validation memory). Score: NID/CRC rate, LDU1+LDU2 counts,
  sync-loss events per minute.
- **A.3 Raw-stream integrity check (demod-free).** Scan both captures for transport
  artifacts directly: amplitude/phase discontinuities, repeated blocks, zero-runs,
  spectrogram seams, sample-count vs wall-clock (a 30 s capture at 4 MSPS must be
  120 M samples — shortfall = dropped buffers). Script lands in
  `doc/diagnostics/<date>/transport_ab/`.
- **A.4 Load sensitivity.** Repeat the DMA-ring capture while varying competing load:
  capture to SD vs to /dev/null (SD writes share the interconnect), dashboard polling
  on/off, vocoder active vs idle.

**Verdicts:**

| Observation | Conviction | Next |
|---|---|---|
| DMA-ring capture decodes/scans worse than libiio on same air | Transport layer | Stage B |
| Both decode clean offline; only LIVE consumption glitches | PS reader/scheduling side of transport | Stage B (reader half) |
| Both transports clean, even simultaneous | Transport exonerated | Stage C |

## Stage B — DMA-ring layer bisect (where in the transport?)

The path has four segments: HDL `DmaStreamRingWrite` → DDR ring buffer → `maia-kmod`
(mapping/IRQ/cache) → PS reader task. Bisect with these probes:

- **B.1 Corruption fingerprint from A.3** classifies the fault: stale/repeated blocks
  ⇒ cache-coherency or read-pointer race in `maia-kmod`/reader; missing spans with
  clean joins ⇒ overflow/wrap handling; bit-level garbage ⇒ AXI/HDL write side.
- **B.2 Overflow accounting.** Cross-check the HDL overflow flag
  (`wideband_iq_overflow()`) against measured sample shortfall — if data is lost while
  overflow never asserts, the loss is downstream of the HDL (kmod/reader).
- **B.3 Reader-side audit (code, desk):** wakeup handling, ring index arithmetic at
  wrap, cache invalidation in `maia-kmod` for the non-coherent ARM port, lock hold
  times around `Arc<Mutex<IpCore>>` (one mutex serving wideband reader, dibit reader,
  and HTTP API — the CC-stall bug already implicates it; B.4 measures it).
- **B.4 Contention experiment.** Throttle all API/dashboard polling to zero during a
  capture; if gaps vanish, the conviction is lock/scheduling, not the ring itself.
- **B.5 Dibit-ring replication.** Whatever fault A/B finds on the wideband ring,
  verify on the dibit ring with fixed forensics (Stage 0.2 below): per-call
  gap_dibits/overflows across ≥20 calls, correlated with audible glitch timestamps.
- **B.6 Test-pattern mode (only if needed, one bake):** a counter-ramp source muxed
  into the ring write makes every dropped/stale/corrupt word trivially detectable and
  separates HDL-write faults from readout faults conclusively.

**Gate:** fix the convicted segment (likely PS/kmod-side, no bake), then re-run Stage
A until DMA-ring captures decode equal to libiio. Only then do live-decode quality
claims mean anything.

## Stage 0 — Lock-in + instrumentation build (desk + one PS-only flash, parallel with Stage A)

- **0.1 Commit the working tree** (entire 2026-05-03 forensics arc) in 3–4 logical
  commits; exclude `p25_log_tmp.json`; `cargo check --target
  armv7-unknown-linux-gnueabihf` first; tag so the flashed BUILD_TAG maps to a commit.
- **0.2 Forensics bug-fix bundle** (~60–80 lines): per-call_id captures replacing the
  clobbered single state (`forensics.rs` ~:389 + call_id guard in
  `record_dma_words`), `/api/forensics_runs` list endpoint, per-chunk
  `(total_offset, n_dibits)` metadata so gappy captures stay alignable.
- **0.3 Transition instrumentation via EventLog** (info-level tracing is filtered):
  timestamped entries for NCO writes, `lsm_reset` pulses, seed writes, chain
  open/close with reason, forensics lifecycle. Feeds Stage C timelines.
- **0.4 Delivery counters in the API:** wideband overflow flag + dibit-reader
  gap/overflow counters per call in `/api/forensics_status` / `/api/sys_health`.

## Stage C — State-transition audit (if transport is exonerated or fixed)

- **C.1 Per-call timeline** from EventLog: GRANT → NCO write → reset → first dibit →
  first sync → HDU → LDU cadence → close reason, aligned with audible-glitch
  timestamps from recorded WAVs.
- **C.2 Mid-call action census.** Between first LDU and CallClose the expected count
  of NCO writes, `lsm_reset` pulses, seed writes is **zero**. Any non-zero entry
  coincident with a glitch convicts the lifecycle logic — candidate sources:
  GRP_VCH_GRNT_UPD handling, same-freq re-grant paths, preemption, encrypted
  follow-bypass side effects.
- **C.3 Close-logic audit.** SyncLost (nid_age > 1.5 s), 10 s timeout, hybrid close:
  look for rapid CallClose→CallOpen pairs on the same TG/freq (mid-PTT reopen = full
  reset = audible glitch).
- **C.4 Code read** (end-to-end, per the operator-hypothesis rule): every path in
  `grant_follower.rs` + `control_channel/` that can touch the traffic chain while a
  call is open; deliverable is a written list of mutation sites + trigger conditions.

## Stage D — Loop behavior under live conditions (PLL/AGC observation)

Only if transport AND state handling are clean.

- **D.1** High-rate sampling of AGC gain / PLL register / timing stats + call-gated
  constellation during glitchy calls; align excursions with glitch timestamps.
- **D.2** Classify triggers: coincident with re-grant/UPD/CC-stall (back to Stage C)
  vs spontaneous on fades (genuine loop robustness on simulcast).
- **D.3** Gain headroom sweep ±6 dB around the manual gain (mode stays manual —
  confirm nothing re-enabled AGC).
- **D.4** White-box replay: Amaranth sim loopback on captured IQ around a glitch
  window with per-symbol state dump, mirrored dump in the SW pipeline. Zero flashes;
  one bake permitted only after a divergence is reproduced in a unit test.

## Stage F — Audio path (if dibits prove clean but audio still glitches)

- **F.1** Vocoder queue depth/underruns at glitch timestamps; queue drain on retune
  (known open follow-up).
- **F.2** Pacer/WS arrival timing (`tools/p25_ws_audio_capture.py`).
- **F.3** Re-render the same call's IMBE offline and listen — clean offline render
  convicts the live audio transport.

## Rules for the whole campaign

- One variable per build; bump BUILD_TAG every flash; commit before flashing.
- Every stage writes a per-run folder under `doc/diagnostics/<date>/<topic>/` with a
  self-contained `FINDINGS.md`.
- No HDL hypothesis (loop constants, prev_sym init, warm-up transients) gets a bake
  before a stage above convicts the HDL loop specifically.
- Refuted-hypotheses table in `PROJECT_TIMELINE.md` §5 applies — nothing there gets
  retried without new evidence from these stages.
