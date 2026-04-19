# P25 PS Pipeline — Fishball vs. SDRTrunk Parity Review

**Purpose.** Side-by-side walkthrough of how the Fishball P25 `p25-httpd` process and [SDRTrunk](https://github.com/DSheirer/sdrtrunk) handle the same P25 Phase 1 FDMA decode problem, from a post-demod dibit stream down to recorded WAV. Weighted toward **correctness parity** — where the two diverge, does the user-visible behavior change?

**Scope.** Everything the Zynq ARM (PS) does after dibits leave the PL. The Fishball HDL-side demod + symbol-timing is out of scope; SDRTrunk's LSM demodulator is only referenced where it changes interface semantics (e.g. soft-symbol sync).

**Methodology.**

- Fishball tree: [../p25-httpd/src/](../p25-httpd/src/), commit `e5d20ca` (2026-04-19).
- SDRTrunk tree: `C:\Users\Andy\Projects\SDRTrunk\sdrtrunk`, Java sources under `src/main/java/io/github/dsheirer/module/decode/p25/phase1/` and sibling `audio/`, `edac/`, `message/` packages.
- Parity = "same on-air behavior and same user-visible output". Where one codebase does something the other doesn't, it is flagged as a gap, not a bug, unless it causes a recorded/live-audio difference.

**Sibling docs.**

- [P25_PS_PIPELINE.md](P25_PS_PIPELINE.md) — canonical Fishball PS walkthrough (the doc this one cross-references everywhere).
- [P25_API.md](P25_API.md) — Fishball REST/WS surface.

---

## 0. Summary — Parity Table

Legend: ✅ parity · ⚠ minor divergence · 🔴 real gap · 🆕 Fishball-only.

| Stage                                  | Fishball                                      | SDRTrunk                                         | Status |
|----------------------------------------|-----------------------------------------------|--------------------------------------------------|:-----:|
| Frame sync                             | Hard Hamming ≤ 6 on 48-bit sync               | Soft-symbol correlator, threshold 60             | ⚠     |
| NID BCH(63,16,23)                      | 65536-entry codebook linear scan, t ≤ tunable | Berlekamp–Massey + Chien + NAC cross-check       | ⚠     |
| DUID coverage                          | HDU/LDU1/LDU2/TDU/TDU_LC/TSBK                 | + PDU, AMBTC, UMBTC, SNDCP, IPPKT                | 🔴    |
| TSBK Trellis 1/2                       | 4-state Viterbi + BAAA de-interleave          | 49-state Viterbi + same de-interleave            | ⚠     |
| TSBK CRC-16                            | Plain + XOR 0xFFFF compare                    | `correctCCITT80` (t=1 algebraic correction)      | ⚠     |
| TSBK opcode acting                     | ~7 act-on, full emit                          | 100+ opcode classes (Moto + Harris + standard)   | 🔴    |
| HDU Golay(18,6)                        | Syndrome decode                               | Syndrome decode                                  | ✅    |
| HDU RS(36,20,17)                       | Shared BM in [rs_p25.rs](../p25-httpd/src/p25/rs_p25.rs) | `ReedSolomon_63_47_17_P25`             | ✅    |
| LDU1 IMBE offsets                      | `[0,144,328,512,696,880,1064,1248,1424]`      | Same offsets                                     | ✅    |
| LDU1 Hamming(10,6,3) × 12              | [voice_frame.rs:100](../p25-httpd/src/p25/voice_frame.rs#L100) | `Hamming10.checkAndCorrect`   | ✅    |
| LDU1/TDULC RS(24,12,13)                | Shared BM                                     | `ReedSolomonDecoder_24_12_13`                    | ✅    |
| LDU2 ESS RS(24,16,9)                   | Shared BM                                     | `ReedSolomonDecoder_24_16_9`                     | ✅    |
| TDU_LC Golay(24,12,7) × 12             | [voice_frame.rs:172](../p25-httpd/src/p25/voice_frame.rs#L172) | `Golay24.checkAndCorrect`     | ✅    |
| TDU_LC `MotorolaTalkComplete` (0x0F/0x90) | Finalises recording, stamps source         | `writeCallSequence` + null out                   | ✅    |
| LCW opcode coverage                    | 4 variants                                    | 30+ standard + Motorola regroup + Harris         | 🔴    |
| JMBE vocoder                           | Pure-Rust `jmbe/`                             | `jmbe` (Java, same algorithm)                    | ✅    |
| Silent-frame suppression               | `max(|pcm|) < 500` drop                       | Gain pipeline, no explicit silence drop          | 🆕    |
| PCM AGC (live path only)               | Peak-hold AGC in-process                      | `NonClippingGain(5.0, 0.95)` adaptive            | ⚠     |
| Encryption gate                        | HDU algo + LDU2 ESS + TSBK service_options    | Same three sources; uses LDU2 as HDU fallback    | ✅    |
| Cached LDU1 before HDU                 | Not implemented                               | Caches LDU1, processes once LDU2 ESS decides     | 🔴    |
| Post-TDU hold                          | 2 s (matches SDRTrunk PR #2010)               | 2 s                                              | ✅    |
| Grant retune                           | Local HDL NCO write                           | Software-tuner channel follower                  | ⚠     |
| HDU-driven recording split             | Yes (2026-04-19)                              | Yes                                              | ✅    |
| Per-speaker split on `TALK_COMPLETE`   | Yes                                           | Yes                                              | ✅    |
| Grace finalise                         | 1.5 s no chunks                               | Squelch-state listener                           | ⚠     |
| Talker Alias reassembly                | Not implemented                               | Harris 4-block + Motorola header-then-data       | 🔴    |
| Extended source LC                     | Not implemented                               | `IExtendedSourceMessage` path                    | 🔴    |
| Adjacent-site tracking                 | Parsed + emitted, not acted on                | `AdjacentStatusBroadcast` → roaming              | 🔴    |

Row counts: 12 ✅, 8 ⚠, 7 🔴, 1 🆕. No row where Fishball is strictly wrong and SDRTrunk strictly correct on a covered stage — all 🔴 rows are "Fishball does not implement X" or "Fishball's opcode surface is narrower", not "Fishball gets it wrong".

---

## 1. Entry Point: Dibits Into the Decoder

**Fishball.** PL DMA rings ([fpga.rs](../p25-httpd/src/fpga.rs)), one tokio task per ring, each byte carries one dibit in the low 2 bits. Pump loops live in [main.rs](../p25-httpd/src/main.rs); one control-chain `ControlChannelDecoder`, one traffic-chain. Both are the same type with different `VoiceHandler`s wired in.

**SDRTrunk.** Entirely software: LSM or C4FM demod in `P25P1DemodulatorLSM.java` / `P25P1DemodulatorC4FM.java` produces dibits, fed into `P25P1MessageFramer.java`. No DMA, no kernel driver — the symbol stream is already in the JVM.

**Difference.** Out-of-scope for this doc, but note that Fishball's PL-side demod hands the PS an arbitrary-length `Bytes` buffer with no per-sample timing — latency stats are stamped by the pump loop on `read_*_dma().await` completion. SDRTrunk's symbol source has per-symbol timestamps because the demodulator is in-process.

**Verdict.** ⚠ Architectural, not correctness. Fishball's throughput is bounded by DMA depth; SDRTrunk's by JVM GC.

---

## 2. Frame Synchronization

### 2.1 Sync detection

**Fishball** ([control_channel.rs](../p25-httpd/src/p25/control_channel.rs), `process_dibit`):

- Rolling 48-bit window in a `u64`, compared against `0x5575F5FF77FF` every dibit.
- Soft Hamming distance: `popcount(window ^ SYNC) ≤ SYNC_THRESHOLD` (default `6`, [:592](../p25-httpd/src/p25/control_channel.rs#L592)).
- Per-chain runtime override via `PUT /api/sync_tune?threshold=N&side=traffic|control`.

**SDRTrunk** (`P25P1MessageFramer.java`, `P25P1SoftSyncDetector.java`):

- Two detectors: `P25P1HardSyncDetector` (bitwise) + a vectorized `P25P1SoftSyncDetector` (Scalar/Vector128/Vector256/Vector512 variants).
- Soft-symbol correlator holds a 48-element ring of soft symbol magnitudes and accumulates a correlation score against the canonical sync pattern; threshold `60` (hardcoded).
- Soft detector is what the LSM demod calls — it's tracking magnitude, not bit flips.

**Difference.** Conceptually equivalent at high SNR; at marginal SNR they respond to different signal features. Soft correlation weathers one-symbol flat spots better; hard Hamming is predictable and easier to tune from telemetry (we log the histogram of hit distances).

**Verdict.** ⚠ Measurement surface differs. No correctness gap.

### 2.2 Status dibit removal

Both decoders strip the 1-dibit-every-36 status insertion before any FEC. Fishball's helper is `is_body_status_dibit` in [types.rs](../p25-httpd/src/p25/types.rs); SDRTrunk's is a dibit-counter reset-to-21 in the framer (`resetStatusCounter()`, every 36 dibits). Same positions, identical outcome.

**Verdict.** ✅

---

## 3. NID — BCH(63,16,23)

**Fishball** ([lsm/nid_fec.rs](../p25-httpd/src/lsm/nid_fec.rs)):

- One-shot `codebook()` expands all 2¹⁶ valid codewords into a `[u64; 65536]` table.
- Decode = linear scan: find codeword with minimum Hamming distance; reject if `d > RUNTIME_BCH_T`.
- No algebraic solve. Roughly 1 ms / NID on Cortex-A9.

**SDRTrunk** (`BCH_63_16_23_P25.java`, inherits from `BCH_63`):

- Full Berlekamp–Massey → Chien search → Berlekamp-Trace root factorisation.
- Returns corrected word + actual error count.
- Cross-checks recovered NAC against a `NACTracker` running histogram; over-corrected NIDs that resolve to a never-before-seen NAC are rejected as algebraic false-positives.

**Difference.** Both correct up to t = 11. Fishball's codebook is equivalent to a ML decoder at any t < 11. The `NACTracker` cross-check is an SDRTrunk-only guard — without it, a ~10⁻¹⁶ probability exists that 11+ random bit flips land on a valid but wrong codeword. Fishball's equivalent guard is the external `nid_invalid_duid` counter plus the DUID enum narrowness — unknown DUIDs from over-correction get dropped upstream.

**Verdict.** ⚠ Functionally equivalent, algorithmically different. Fishball's lookup is faster per-decode but fixed-size; SDRTrunk is parameter-flexible at the cost of CPU. Neither approach fails in practice at the SNRs we operate.

---

## 4. Per-DUID Body Handling

### 4.1 DUID coverage

**Fishball** DataUnit enum ([types.rs:34](../p25-httpd/src/p25/types.rs#L34)): `Hdu, Tdu, Ldu1, Tsdu, Ldu2, TduLc`. Six values.

**SDRTrunk** `P25P1DataUnitID` enum: above six + `PACKET_DATA_UNIT` (0xC), `PACKET_DATA_UNIT_BLOCK_1..5`, `PACKET_DATA_UNIT_BLOCK_EXTENDED`, `SUBNETWORK_DEPENDENT_CONVERGENCE_PROTOCOL` (SNDCP), `IP_PACKET_DATA`, `ALTERNATE_MULTI_BLOCK_TRUNKING_CONTROL` (AMBTC), `UNCONFIRMED_MULTI_BLOCK_TRUNKING_CONTROL` (UMBTC).

**Gap.** On a conventional voice trunking site, PDU and friends are rare — mostly used for data calls, firmware delivery, encryption key management (OTAR). Not decoding them means Fishball misses:

- Packet data session setup and teardown.
- SNDCP IP-over-P25 (what some agencies use for in-car computer traffic).
- Multi-block trunking payloads that carry richer grant metadata than TSBK (AMBTC is TIA-102's extended-addressing replacement for some TSBK opcodes on newer Astro 25 infrastructure).

**Verdict.** 🔴 Real gap. Impact is site-dependent. On Clay County (the current validation target) PDU is silent. On an Astro 25 public-safety trunked system with integrated data, missing PDU means silently dropping agency data.

### 4.2 TSBK

**Pipeline (both).** Strip status dibits → de-interleave → 1/2-rate Viterbi → CRC-16 → opcode dispatch.

**De-interleave.** Both use the TIA-102 BAAA Table 7-7 permutation. Fishball table in [p25/fec.rs](../p25-httpd/src/p25/fec.rs) `DATA_DEINTERLEAVE`; SDRTrunk's in `P25P1Interleave.DATA_DEINTERLEAVE`. Identical.

**Viterbi.** Both trellis decoders are 1/2-rate. Fishball's is a 4-state 1 Hamming-distance implementation (`TRANSITION_MATRIX[prev][curr]`, traceback from state 0); SDRTrunk's `ViterbiDecoder_1_2_P25` is structured around a 49-state window (the trellis length of the encoded payload) with the same transition table. Output is bit-identical on any noise-free input; differ only in branch-metric precision under very high error counts.

**CRC-16.** Fishball's [tsbk.rs](../p25-httpd/src/p25/tsbk.rs) checks both plain and `XOR 0xFFFF` conventions — both are seen in the wild. SDRTrunk's `CRCP25.correctCCITT80` applies 1-bit algebraic correction before deciding pass/fail. Fishball does not do CRC correction; a single bit-flip in the payload kills the TSBK.

**Multi-block TSBK.** Both support 1/2/3-block. Fishball's block count is derived from the last-block flag during decode; SDRTrunk dispatches sequentially from the same buffer after the assembler completes.

**Opcode dispatch.** Fishball parses ~40 opcodes, **acts on** 7 (see [P25_PS_PIPELINE.md §4.1](P25_PS_PIPELINE.md#41-tsbk-control-channel--the-decoders-main-job-on-the-control-chain)). SDRTrunk has dedicated Java classes for 100+ opcodes (standard + Motorola MFID 0x90 + L3Harris MFID 0xBF + vendor). Parse-everything, act-on-a-subset holds on both sides for the acted-on set; beyond the acted set, SDRTrunk extracts richer fields (e.g. Harris talker alias reassembly, Motorola regroup).

**Verdict.** ⚠ CRC-16 1-bit correction is a real parity gap — cheap to add, and would recover a measurable fraction of our current CRC-fail TSBKs. Opcode surface gap is 🔴 on non-standard systems.

### 4.3 HDU

**FEC stack.** Both: Golay(18,6,8) × 36 → Reed-Solomon(36,20,17) shortened from (63,47,17). Both deliver 120 payload bits (MI 72, MFID 8, Algo 8, KeyID 16, TG 16).

**Fishball:** `parse_hdu_body` at [voice_frame.rs:1074](../p25-httpd/src/p25/voice_frame.rs#L1074). Inner Golay at [:228](../p25-httpd/src/p25/voice_frame.rs#L228), outer RS in [rs_63_47_17.rs](../p25-httpd/src/p25/rs_63_47_17.rs) delegating to the shared BM in [rs_p25.rs](../p25-httpd/src/p25/rs_p25.rs).

**SDRTrunk:** `HDUMessage.java` loop over 36 `GOLAY_WORD_STARTS[]` + `Golay18.checkAndCorrect()`, then `ReedSolomon_63_47_17_P25` (with the characteristic P25 reverse-order hexword array layout before decode).

**Encryption.** Both: `algo == 0x80` ⇒ clear; anything else ⇒ encrypted. Both latch a sticky `call_encrypted` flag.

**Verdict.** ✅

### 4.4 LDU1

**IMBE.** 9 frames × 144 bits at `[0, 144, 328, 512, 696, 880, 1064, 1248, 1424]`. Both. No PS-side FEC on the IMBE bits — the vocoder owns Golay(23,12)/Hamming(15,11)/derand.

**LC FEC.** 12 × Hamming(10,6,3) + RS(24,12,13). Both. Fishball's Hamming at [voice_frame.rs:100](../p25-httpd/src/p25/voice_frame.rs#L100) (correct) and [:76](../p25-httpd/src/p25/voice_frame.rs#L76) (syndrome); SDRTrunk's `Hamming10.checkAndCorrect`. RS: same as HDU but on fewer hexwords.

**LCW dispatch.** Fishball: GroupVoiceChannelUser (0x00) is the acted-on case; `parse_ldu1_source` recovers source_radio_id and emits `CallBoundary::TdulcComplete` — Fishball piggybacks on the same boundary kind for LDU1 and TDU_LC mid-call source, which is why the variant name is misleading but harmless. SDRTrunk: full `LinkControlWordFactory` dispatch to 30+ variants.

**Verdict.** ⚠ FEC and extraction are parity; LCW acted-on surface is narrower on Fishball.

### 4.5 LDU2 — ESS

**ESS FEC.** 16 × Hamming(10,6,3) + RS(24,16,9). Both. Fishball's `parse_ldu2_ess` at [voice_frame.rs:1202](../p25-httpd/src/p25/voice_frame.rs#L1202).

**Payload.** Algo 8, KeyID 16, MI 72. Both.

**Encryption fallback.** Both treat LDU2 as the backstop for missed HDUs — if the HDU's algo didn't decode (or was missed), ESS's algo latches `call_encrypted`. SDRTrunk additionally **caches LDU1** that arrived before an HDU and replays it through the vocoder once LDU2 resolves the encryption state (`P25P1AudioModule.java`, lines ~98–120). Fishball does not — if LDU1 lands before any encryption signal, its 9 IMBE frames are forwarded to the vocoder unconditionally (modulo the existing gates). If the call turns out to be encrypted, those 9 frames play as vocoder garbage before the gate catches up on the next LDU1/LDU2.

**Verdict.** 🔴 Real parity gap. In practice it's ~180 ms of garbled audio at call start on out-of-order-open calls — small but audible. Medium-effort fix: buffer the first LDU1's PCM until the first LDU2/HDU arrives and decides.

### 4.6 TDU

Trivial body, both just bump a counter and feed the traffic state machine.

**Verdict.** ✅

### 4.7 TDU_LC

**FEC.** Golay(24,12,7) × 12 + RS(24,12,13). Both. Fishball's Golay24 `golay24_correct` at [voice_frame.rs:172](../p25-httpd/src/p25/voice_frame.rs#L172) (syndrome helper at [:152](../p25-httpd/src/p25/voice_frame.rs#L152)); SDRTrunk's `Golay24.checkAndCorrect`.

**LCW variants acted-on** (from [P25_PS_PIPELINE.md §4.6](P25_PS_PIPELINE.md#46-tdu_lc--tdu-with-link-control)):

| Variant                    | Fishball                     | SDRTrunk                                     |
|----------------------------|------------------------------|----------------------------------------------|
| GroupVoiceChannelUser 0x00 | source + TG extracted        | `LCGroupVoiceChannelUser` + LC dispatch      |
| GroupVoiceChannelUpdate 0x02 | dual TG channel A/B        | `LCGroupVoiceChannelUpdate`                  |
| CallTermination 0x0F       | emitted                      | triggers `writeCallSequence`                 |
| **MotorolaTalkComplete 0x0F/0x90** | `SpeakerEnd{source}` → recorder finalises | `MOTOROLA_TALK_COMPLETE` → `writeCallSequence` + null out |

Both rely on the Motorola vendor `TALK_COMPLETE` LCW for per-speaker recording splits. Sites that don't emit it produce one-WAV-per-grant on both decoders.

**Verdict.** ✅ on the acted-on set. 🔴 on the long tail (Harris talker alias blocks, Motorola regroup, unit-to-unit setup variants — SDRTrunk has dedicated classes; Fishball doesn't).

---

## 5. FEC Primitives Inventory

See [P25_PS_PIPELINE.md §5](P25_PS_PIPELINE.md#5-fec-primitive-inventory) for the Fishball inventory. All P25 Phase 1 voice-path primitives are implemented in the Rust tree; there are no stubs. Same is true of SDRTrunk for its covered DUID set. Both trees use a shared Berlekamp–Massey RS decoder under the three (n,k,d) parameterisations.

The only FEC-layer capability SDRTrunk has that Fishball doesn't is the **TSBK CRC-16 1-bit correction** mentioned in §4.2 — not in the primitives table because it's a CRC, not a code.

**Verdict.** ✅ on the inventory. ⚠ on the CRC-correction delta.

---

## 6. IMBE → PCM → Audio

### 6.1 Vocoder

**Fishball** [vocoder/mod.rs](../p25-httpd/src/vocoder/mod.rs) → [jmbe/mod.rs](../p25-httpd/src/jmbe/mod.rs). Pure-Rust port of mbelib with spectral enhancement. mbelib FFI removed 2026-04-17; `mbelib-sys` retained only for `SAMPLES_PER_FRAME = 160`.

**SDRTrunk** `ImbeAudioModule.java` + `jmbe` library (the original Java JMBE). Same algorithm, same output rate.

**Verdict.** ✅

### 6.2 Audio path shaping

**Fishball** (`ImbeForwarder` in [main.rs](../p25-httpd/src/main.rs) + vocoder task):

1. Channel-full back-pressure gate (mpsc `try_send`) — bounded channel, drops 9-frame batch if vocoder task is too far behind. Counter `imbe_frames_dropped`.
2. Encryption gate (`call_encrypted` sticky flag, seeded by grant TSBK, refreshed by HDU + LDU2 ESS `algorithm_id`).
3. Slow-attack RMS-EMA PCM AGC (commit `e700351`, 2026-04-19). Target RMS 2500, scale clamp [0.25, 8.0], only voiced frames update the EMA so pauses don't pump the gain. Applied to the `AudioChunk.pcm` before broadcast — **both `/ws/audio` and the recorder see the same AGC'd samples**.
4. Silent-frame counter (`vocoder_frames_silent_suppressed`) — increments when `max(|pcm|) < 16` after JMBE, but **does not drop the frame** (reverted 2026-04-19 late, commit `e700351`). Silence now propagates to match SDRTrunk behaviour and prevent false recorder splits from mid-speaker silent-frame bursts.
5. ~~TG-0 gate~~ — removed 2026-04-19 late (commit `0662f02`). Previously dropped 9-frame batches when `current_talkgroup` flickered to 0 (Phase 7F.3 phantom-LDU guard), but in practice also dropped legitimate mid-call frames during grant-refresh races. `imbe_frames_dropped_idle` counter preserved for API stability, no longer increments.

**SDRTrunk** (`P25P1AudioModule`):

1. Encryption gate.
2. `NonClippingGain(5.0f, 0.95f)` — 5× initial multiplier, 0.95 target RMS, per-frame adaptive.
3. No silent-frame drop; silence propagates to the audio mixer.

**Difference.** Fishball's RMS-EMA AGC and SDRTrunk's `NonClippingGain` are similar-intent / different-algorithm. Fishball applies its AGC pre-broadcast so the recorder sees normalised PCM; SDRTrunk applies its `NonClippingGain` at the audio-mixer stage. The practical effect is the same: both stacks produce normalised PCM to the listener.

**Verdict.** ✅ Near-parity. Both have encryption gate + per-frame adaptive gain, both let silence propagate. The only Fishball-specific extras (back-pressure gate, silent-frame counter) are observability or bounded-queue housekeeping, not behavioural divergence.

---

## 7. Call / Grant Handling — TrafficManager

**Fishball** [traffic_manager.rs](../p25-httpd/src/p25/traffic_manager.rs): `Idle / Acquiring / Active` state machine. Grant arrives via control chain → 50 ms poller picks newest → `handle_grant` → NCO write → state=Acquiring. Traffic chain decodes HDU/LDU/TDU independently → feeds back to manager. Post-TDU hold 2 s, activity timeout 2 s.

**SDRTrunk.** Grant received on control channel → `P25ChannelProcessingManager` routes to a follow-channel (a separate software-tuner bound to the granted freq). That channel runs its own `P25P1DecoderState`. Same post-TDU / activity semantics (SDRTrunk PR #2010 is the reference Fishball copied).

**Difference.** Mechanics only:

- Fishball retunes a single DDC; one traffic channel at a time.
- SDRTrunk instantiates a follow-channel per active grant — can follow multiple grants on the same system in parallel when the dongle has the bandwidth.

**Verdict.** ⚠ Architectural. On the current single-DDC hardware, Fishball is a strict subset by design (see [project_direct_traffic_tune_todo.md](../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/project_direct_traffic_tune_todo.md) for the "two LSM chains" architectural question).

---

## 8. Recording Pipeline

**Fishball** [recorder.rs](../p25-httpd/src/recorder.rs):

- State: `Option<ActiveCall>` + finalize-grace timer.
- Start triggers: first non-zero-TG `AudioChunk`, or different-TG chunk.
- Split triggers: `HduStart` finalises + idles; `SpeakerEnd{source}` stamps source and finalises (TG-mismatch guarded); `TdulcComplete{source}` stamps source but does **not** finalise.
- Grace finalise: 1.5 s no chunks.
- Filename: `rec_<start_unix_ms>_<id>_tg<TG>[_from<source>].wav` in `/tmp/p25_recordings/`.
- 8 kHz PCM-16 mono WAV, fixed format (same as vocoder output).

**SDRTrunk** `P25P1CallSequenceRecorder`:

- State: `Option<MBECallSequence>`.
- Start triggers: HDU → new sequence (latches encryption).
- Split triggers: `MOTOROLA_TALK_COMPLETE` → `writeCallSequence(...)` + `mCallSequence = null`; `CALL_TERMINATION_OR_CANCELLATION` → same.
- Squelch-state listener closes the audio segment on prolonged silence.
- Filename: per `MBECallSequence` template — JSON metadata sidecar + WAV; template includes timestamp, TG, source alias.

**Difference.** Fishball's per-speaker split is wired identically to SDRTrunk's; both depend on Motorola vendor LCW. Fishball's grace timer replaces SDRTrunk's squelch listener — equivalent role, different trigger. Fishball does not emit a JSON sidecar (metadata is in `/api/recordings/{id}/events` via the structured event log); SDRTrunk does. Fishball's filename is UTC-epoch; SDRTrunk's is a rendered local-time template.

**Verdict.** ✅ on core behavior. ⚠ on metadata delivery (sidecar vs. HTTP endpoint) — this is a UI choice, not a correctness gap.

---

## 9. HTTP / WebSocket Surface

Fishball's is a single-process HTTP+WS daemon — see [P25_API.md](P25_API.md).

SDRTrunk has no HTTP surface — it's a JavaFX desktop app with JMS-style internal event buses, call event history views, rrd-style logs, and an audio playlist. Different product shape; no parity comparison attempted here.

**Verdict.** Not applicable.

---

## 10. SDRTrunk Capabilities Fishball Lacks Entirely

Ordered by how often you'd notice the gap on a real voice-trunking site:

### 10.1 Talker Alias Reassembly — 🔴 HIGH

SDRTrunk assembles fragmented Harris and Motorola "talker alias" LC blocks across multiple LDUs:

- Harris: `HarrisTalkerAliasAssembler` — 4 fragment blocks (L3Harris MFID 0xBF).
- Motorola: `LCMotorolaTalkerAliasAssembler` — header block + data blocks.

Output is a printable alias string (e.g. `"UNIT 123"` or `"SGT SMITH"`). Fishball source: zero matches for `TalkerAlias` / `talker_alias` in the `p25-httpd` tree. This is the single highest-yield parity gap for user experience on modern systems that send aliases.

### 10.2 Extended Source / Addressing LCW — 🔴 MEDIUM

SDRTrunk's `P25P1MessageProcessor.java` (lines ~119–136) walks an `IExtendedSourceMessage` interface — whenever a main LC carries only a truncated source ID, subsequent LCs can carry the extended bits and the processor stitches them together before emitting the call event. Relevant on Astro 25 IV&D sites using extended (WACN-spanning) unit IDs.

Fishball treats each LCW independently.

### 10.3 Multi-Block PDU + SNDCP — 🔴 LOW to MEDIUM

Already covered in §4.1. Impact is nil on control-voice-only trunking sites; non-zero if any agency uses integrated data.

### 10.4 Motorola Group Regroup — 🔴 SITE-DEPENDENT

Dedicated handlers for `MotorolaGroupRegroupChannelGrant` / `MotorolaGroupRegroupChannelUpdate` (MFID 0x90). These are the "super group" overlays Motorola sites use for cross-talkgroup ad-hoc dispatcher-controlled regroupings. Fishball parses the opcodes into `RecentTsbk` but does not follow the regroup; if a regroup is active you see the parent TG, not the child super-group.

### 10.5 Adjacent-Site Roaming — 🔴 MULTI-SITE SYSTEMS ONLY

SDRTrunk's `AdjacentStatusBroadcast` parser (TSBK 0x3D) feeds a site-roam module that can retune to a neighbor if the home site loses sync. Fishball parses the opcode and emits it, but does not act on it. Fishball is single-site by architecture (single DDC); this gap is a consequence of the hardware choice, not an oversight.

### 10.6 Call Metadata JSON Sidecar — ⚠ COSMETIC

SDRTrunk writes a `.json` alongside each WAV with the full call event log. Fishball's equivalent is `/api/recordings/{id}/events` at runtime; the data is there, the presentation is HTTP-only.

### 10.7 CRC-16 1-Bit Correction — 🔴 LOW but trivial to fix

SDRTrunk's `CRCP25.correctCCITT80` algebraically corrects single-bit errors before pass/fail. Fishball's TSBK CRC check is pure verification. Effort: ~30 lines. Payoff: recovers measurable TSBK fraction from marginal sites (exact fraction requires a measurement — suggested follow-up).

### 10.8 LDU1-Before-HDU Caching — 🔴 LOW

Covered in §4.5. Fix: small ring buffer + replay once encryption state resolves.

---

## 11. Priority Gap List — Actionable

Ranked by user-visible impact per hour of implementation effort:

1. **TSBK CRC-16 1-bit correction** — tiny implementation, broad effect, measurable immediately in the /api/recent_tsbks stream.
2. **Talker alias reassembly** — biggest single "feels like SDRTrunk" upgrade for dashboard users on Harris / Motorola talker-alias sites. Requires multi-LDU state accumulation + an alias cache keyed by unit ID. Medium effort.
3. **LDU1-before-HDU cache** — fixes 180 ms of click/garble at call start in rare cases. Small effort.
4. **JSON sidecar next to each WAV** — ~50 lines. Makes `/tmp/p25_recordings/` self-describing if anyone ever pulls it off the board.
5. **Motorola regroup handlers** — only matters on sites that use it; not a default priority unless a validation site needs it.
6. **PDU / SNDCP** — defer until a deployment actually needs data decoding.
7. **Adjacent-site roaming** — requires multi-site architecture Fishball doesn't have; parked until two-DDC or multi-chain hardware appears.

None of these are correctness bugs on the covered DUIDs. All of them are coverage/completeness gaps relative to SDRTrunk's much larger surface area. The Fishball decoder is bit-correct on every voice-path primitive it implements; the question for future work is how much of SDRTrunk's surface is worth porting.

---

## Appendix A — File Cross-Reference

| Concern                 | Fishball                                                                  | SDRTrunk (under `src/main/java/io/github/dsheirer/`)         |
|-------------------------|---------------------------------------------------------------------------|--------------------------------------------------------------|
| Framer / sync           | [p25/control_channel.rs](../p25-httpd/src/p25/control_channel.rs)         | `module/decode/p25/phase1/P25P1MessageFramer.java`           |
| NID FEC                 | [lsm/nid_fec.rs](../p25-httpd/src/lsm/nid_fec.rs)                         | `edac/bch/BCH_63_16_23_P25.java`                             |
| TSBK Viterbi / deint.   | [p25/fec.rs](../p25-httpd/src/p25/fec.rs)                                 | `ViterbiDecoder_1_2_P25.java` + `P25P1Interleave.java`       |
| TSBK opcode dispatch    | [p25/tsbk.rs](../p25-httpd/src/p25/tsbk.rs)                               | `module/decode/p25/phase1/message/tsbk/TSBKMessageFactory.java` |
| HDU                     | [p25/voice_frame.rs::parse_hdu_body](../p25-httpd/src/p25/voice_frame.rs) | `module/decode/p25/phase1/message/hdu/HDUMessage.java`       |
| LDU1                    | [p25/voice_frame.rs::parse_ldu1_source](../p25-httpd/src/p25/voice_frame.rs) | `module/decode/p25/phase1/message/ldu/LDU1Message.java`   |
| LDU2 ESS                | [p25/voice_frame.rs::parse_ldu2_ess](../p25-httpd/src/p25/voice_frame.rs) | `module/decode/p25/phase1/message/ldu/LDU2Message.java`      |
| TDULC + LCW             | [p25/voice_frame.rs::parse_tdulc_lcw](../p25-httpd/src/p25/voice_frame.rs)| `module/decode/p25/phase1/message/tdu/TDULCMessage.java` + `message/lc/LinkControlWordFactory.java` |
| Vocoder                 | [vocoder/mod.rs](../p25-httpd/src/vocoder/mod.rs)                         | `audio/ImbeAudioModule.java` + `jmbe` package                |
| Audio gate + AGC        | `ImbeForwarder` in [main.rs](../p25-httpd/src/main.rs)                    | `audio/P25P1AudioModule.java`                                |
| Grant / state           | [p25/traffic_manager.rs](../p25-httpd/src/p25/traffic_manager.rs)         | `module/decode/p25/P25P1DecoderState.java` + channel mgr     |
| Recorder                | [recorder.rs](../p25-httpd/src/recorder.rs)                               | `audio/P25P1CallSequenceRecorder.java`                       |

---

## Appendix B — Verified / Fixed Drift in P25_PS_PIPELINE.md

This review found and corrected six drift items in the canonical pipeline doc:

1. §2.1 sync threshold default was stated as `7`; code has `SYNC_THRESHOLD = 6`.
2. §2.1 sync-tune endpoint was stated as `POST /api/sync_tune?side=...&value=N`; actual is `PUT /api/sync_tune?threshold=N[&side=traffic|control]`.
3. §4.3 Hamming(10,6,3) was linked to `voice_frame.rs:76` (the syndrome helper); corrected to `:100` for the `hamming10_correct` entry point.
4. §4.6 Golay(24,12,7) was linked to `voice_frame.rs:152` (the syndrome helper); corrected to `:172`.
5. §4.6 `MotorolaTalkComplete` opcode was listed as `0x00`; actual is `0x0F` with MFID `0x90`.
6. §8 recorder event table said `SpeakerEnd` merely stamps `active.source`; actually it stamps **and** finalises (TG-mismatch guarded). Updated.

All six are fixed in the current doc.
