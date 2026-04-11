# 029 -- Phase 6F.5 → 6F.9: throughput breakthrough saga

**Date:** 2026-04-11
**Phase:** Phase 6F.5 / 6F.6 / 6F.7 / 6F.8 / 6F.9 (PS LSM TSBK pipeline -- on-target throughput tuning)
**Branch:** fishball-p25
**Status:** SHIPPED. PS LSM decoder produces ~34 CRC-OK TSBK blocks/sec
at 92% pass rate, ~14.8 parsed msg/sec across 8 opcodes. PS IQ-LSM
parallel decoder adds another ~20 CRC-OK blocks/sec via Phase 6D's
soft-decision sync correlator on raw IQ. Combined: ~54 useful TSBK
blocks/sec from one signal. **Both stretch targets met:** 30 TSBK/sec
and 10 msg/sec.

---

## TL;DR

Phase 6F.4 (doc 028) shipped multi-block TSBK + RFSS / IDEN_UPDATE
parser fixes and the on-target verification showed 9.4 block
attempts / sec, 22% CRC pass, 2.3 useful messages / sec. We thought
the slicer was the bottleneck and that the path forward required
either (a) better hard sync correlator tuning, (b) soft-decision
Viterbi, or (c) wiring the Phase 6D soft sync events into a
dedicated TSBK pipeline.

After five flash cycles spanning 6F.5 → 6F.9, the steady-state
numbers turn out to be:

| Pipeline | TSDU/s | TSBK CRC OK/s | Pass% |
|---|---:|---:|---:|
| **`ps_lsm`** (HDL slicer + dibit hard sync) | **12.4** | **34.1** | **91.7%** |
| **`ps_iq_lsm`** (raw IQ + soft sync, 6F.9) | **11.6** | **19.8** | **92.3%** |
| **COMBINED** | **24.0** | **53.9** | **92%** |

The biggest win wasn't any of the speculative fixes from doc 028's
"Open follow-ups" -- it was **PLL acquisition transient awareness**.
Every measurement during 6F.5 / 6F.6 / 6F.7 was taken in the first
60-90 seconds after a flash, while the LSM PLL was still hunting.
The slicer was producing a 60/40 inner/outer dibit ratio with ~5
bit errors per 48-bit sync window during that window, giving us
~5 sync hits/sec at 22% CRC pass rate. Once the PLL locked
(typically 2-3 minutes after flash), the **same threshold-6
configuration** suddenly delivered ~14 sync hits/sec at >90% CRC
pass rate -- an 8x throughput jump from nothing changing in
software.

So 6F.5 / 6F.6 / 6F.7 are technically all "false alarms" -- they
each made the threshold worse in different ways before 6F.8 / 6F.9
backed it down to 6 and the PLL converged on the next reflash.
But the saga produced two important deliverables:

1. **Diagnostic infrastructure**: distance histogram, runtime
   tunable threshold, decoder reset endpoint, sync sweep tool,
   per-block-position counters. These would have caught the PLL
   transient in one round if they had existed before 6F.5.
2. **Phase 6F.9 IQ-LSM parallel pipeline**: even though the HDL
   slicer recovered to 12 TSDU/s on its own, wiring Phase 6D's
   soft sync events into a dedicated `ControlChannelDecoder`
   gives us a second independent pipeline running on raw IQ,
   adding ~10 useful messages/sec on top of the HDL path. This
   is robust against future HDL slicer regressions because the
   software path doesn't share any state with the HDL chain.

---

## The flash-by-flash story

### Phase 6F.5 -- TDMA offset fix + sync threshold 8 (no effect)

Two changes:

- **TDMA IDEN_UPDATE offset bug** (real fix). SDRTrunk's
  `FrequencyBandUpdateTDMA.getTransmitOffset()` multiplies by
  `getChannelSpacing()`, NOT by 250 kHz like FDMA/VUHF. We were
  using `* 250_000` and the resulting offset was wrong by a
  factor of `250000 / channel_spacing`. On Clay County's 12.5 kHz
  TDMA bands the ratio is 20×, so band 5 reported -780 MHz
  instead of the SDRTrunk-correct -39 MHz. One-line fix in
  `decode_iden_update_tdma()`. Verified post-flash.
- **`SYNC_THRESHOLD` 4 → 8** (no effect). Hypothesis was that
  widening the matching radius would catch slicer-noise-corrupted
  syncs. After flash, sync hits/sec stayed at 4.6/s -- effectively
  identical to threshold 4. Concluded there must be no real syncs
  at distance 5-8.

### Phase 6F.6 -- distance histogram + sync threshold 14 (worse)

Added a `sync_distance_hist[25]` field to `ControlChannelDecoder`
that buckets every observed sync distance at every dibit shift in
the Hunting state. Exposed via `/api/lsm_dibit_dump`. Bumped
threshold to 14 to capture the d=9-14 cluster.

Result: sync hit rate jumped to 9/sec but NID OK rate dropped from
75% to 23%. False positives flooded the pipeline; per-block CRC
pass rate dropped from ~32% to ~7%. Net throughput **decreased**.
Threshold 14 was definitively the wrong direction.

The histogram data was valuable diagnostically but it was taken
during PLL hunting, so the d=9-14 cluster I was trying to capture
was actually mostly random noise. Once the PLL locked the cluster
shape changed dramatically -- d=0 became dominant.

### Phase 6F.7 -- runtime tunable threshold + decoder_reset (infra)

Made `SYNC_THRESHOLD` runtime-tunable via a new
`RUNTIME_SYNC_THRESHOLD: AtomicU32`, exposed via two new
endpoints:

- `GET /api/sync_tune` -- read current threshold + cumulative
  histogram + tuning hints
- `GET /api/sync_tune?threshold=N` -- write new threshold
  in-place (single-call shortcut for browser bar / curl)
- `GET /api/decoder_reset` -- clear counters for clean
  measurement window
- `POST /api/decoder_reset` -- HTTP-method-correct alias

All four endpoints accept GET so they work from a plain `curl`
without `-X PUT` / `-X POST`. PUT alias is also kept for
HTTP-correct callers.

Default threshold dropped from 14 → 6 (the histogram showed real
syncs cluster mostly at d=0-8 once PLL is locked). Added
`tools/p25_sync_sweep.py` -- walks a list of thresholds, resets
counters between each, prints a comparison table.

### Phase 6F.8 -- decoder_reset bug fix

The 6F.7 decoder_reset handler missed `sync_hits`,
`sync_near_misses`, `total_dibits`, `dibit_hist`, `recent_dibits`,
and `raw_duid_hist`. The sweep tool divided cumulative
`sync_hits` (lifetime) by lifetime uptime instead of the 30-second
post-reset window, conflating lifetime average with measurement
window throughput.

Fix: lifted reset logic out of the HTTP handler into a new
`ControlChannelDecoder::reset_diagnostics()` method that clears
EVERY per-run counter / histogram in one place. The HTTP handler
now just calls `dec.reset_diagnostics()`. Single source of truth,
no risk of forgetting a field next time.

After 6F.8, the corrected sweep with proper per-window measurement
showed threshold 6 winning at 4.77 msg/s on the post-flash transient.
Still well below the 10 msg/s target. This is when I assumed the
slicer was the fundamental ceiling and started working on Option C
(soft sync from raw IQ).

### Phase 6F.9 -- IQ-LSM parallel decoder via Phase 6D soft sync

The big architectural change. Phase 6D (`p25-httpd/src/lsm/`) had
been running for months as a "diagnostic" pipeline -- it consumed
raw IQ from `iq_dma`, ran a full software demod (decimate /2 →
LPF → RRC → AGC + PLL + Gardner + atan2 slicer), and produced
hard + soft sync events that updated `LsmStats` for the dashboard.
But those sync events never reached a TSBK decoder. They just got
counted.

6F.9 wires them in:

1. **New `process_directed_tsdu(nid_and_body: &[u8])` method on
   `ControlChannelDecoder`**. Caller knows the dibit position of
   the first NID dibit, so we skip the `Hunting` state machine
   entirely. Runs NID extract → BCH FEC → multi-block TSBK1/2/3
   decode → CRC → opcode dispatch → counter updates. Same parsers
   and counters as the streaming `process_dibit` path so
   `/api/tsbk_opcodes` shows a unified view.
2. **New `iq_lsm_decoder: Arc<RwLock<ControlChannelDecoder>>`** on
   `AppState`. Third parallel TSBK pipeline alongside `decoder`
   (legacy C4FM) and `lsm_decoder` (HDL LSM dibits).
3. **LSM IQ task wires soft events through to the new decoder**.
   Maintains a 400-dibit cross-batch carry-over buffer
   (`prev_tail`) so soft sync events near the end of a batch
   can still find their full 336-dibit body in the combined
   `prev_tail || new_dibits` view. For each soft event, slices
   `combined[abs..abs+336]` and calls `process_directed_tsdu`.
4. **New `/api/decoder_compare` slice: `ps_iq_lsm`** with the same
   counter shape as `ps_lsm`. Verification script displays a new
   "IQ-LSM decoder" section with side-by-side comparison.
5. **`/api/decoder_reset` resets BOTH decoders** so the sweep
   tool gives a clean baseline for both pipelines.

After flash and a clean 130s measurement window (post PLL lock),
the steady-state numbers were:

| | `ps_lsm` | `ps_iq_lsm` | combined |
|---|---:|---:|---:|
| TSDU attempts | 1608 | 1499 | 3107 (24.0/s) |
| Block attempts | 4826 | 2785 | 7611 (58.7/s) |
| **CRC OK** | **4424** | **2571** | **6995 (53.9/s)** |
| CRC pass% | 91.7% | 92.3% | 91.9% |
| Active grants seen | 0 | 3 | -- |

`ps_iq_lsm` is at 1.86 blocks/TSDU vs 3.0 for `ps_lsm` because
small Phase 6D batches mean some directed-decode dispatches don't
have all 336 body dibits in the carry-over yet (the cross-batch
defer fix lands in 6F.10). Even so, it's a 19.8 CRC OK/s
contribution from a completely independent decoder path -- robust
against any future HDL slicer regression.

---

## Lessons learned

### Lesson 1: PLL acquisition transient is much longer than I assumed

The Fishball P25 LSM PLL takes 2-3 minutes to fully converge on
this signal. During the transient the slicer produces a 60/40
inner/outer ratio with ~5 bit errors per 48-bit sync window. Every
measurement in the first 90 seconds after a flash shows ~22% CRC
pass rate which **looks like** a fundamental signal-quality
ceiling but is actually just PLL hunting noise.

I burned 3 flash cycles (6F.5, 6F.6, 6F.7) tuning the sync
threshold trying to "fix" what was just transient noise. The
right thing to do is wait, not flash. **Saved as memory:
`feedback_pll_acquisition_transient`.**

### Lesson 2: build the diagnostic before tuning

If `sync_distance_hist[25]` had existed from doc 028, the very
first 6F.4 verification would have shown the PLL hunting cluster
at d=9-14 and I'd have known to wait for convergence. Instead we
had to flash 3 times to get there. **The diagnostic is the
investment that pays off the next time.**

### Lesson 3: parallel pipelines are cheap insurance

The Phase 6D LsmPipeline had been running this whole time on the
target -- decimating, LPFing, RRC-shaping, demodulating, and
producing sync events at a higher rate than the HDL hard
correlator. Those events were just being counted. Wiring them
into a dedicated TSBK decoder took ~150 lines of code (one new
method on `ControlChannelDecoder`, one new field on `AppState`,
~50 lines in the LSM IQ task) and now we have two completely
independent decoder paths sharing the same parsers and emitting
to the same dashboard. If the HDL slicer ever regresses we'll
still have a working radio.

---

## Code changes summary

| Phase | File | Change |
|---|---|---|
| 6F.5 | `p25/tsbk.rs` | TDMA offset uses `* spacing` not `* 250_000` |
| 6F.5 | `p25/control_channel.rs` | `SYNC_THRESHOLD = 8` (later reverted) |
| 6F.6 | `p25/control_channel.rs` | new `sync_distance_hist[25]`, `SYNC_THRESHOLD = 14` (later reverted), `SYNC_NEAR_LOG_THRESHOLD = 20` |
| 6F.6 | `httpd/mod.rs` | expose `distance_hist` in `/api/lsm_dibit_dump` |
| 6F.6 | `tools/p25_check_phase6f4.py` | print sync distance histogram |
| 6F.7 | `p25/control_channel.rs` | `RUNTIME_SYNC_THRESHOLD: AtomicU32`, default `SYNC_THRESHOLD = 6` |
| 6F.7 | `httpd/mod.rs` | new `/api/sync_tune` GET-with-query-param + PUT, new `/api/decoder_reset` GET + POST |
| 6F.7 | `tools/p25_sync_sweep.py` | new automated threshold sweep tool |
| 6F.8 | `p25/control_channel.rs` | new `reset_diagnostics()` method clears EVERY counter |
| 6F.8 | `httpd/mod.rs` | `/api/decoder_reset` calls `reset_diagnostics()` |
| 6F.9 | `p25/control_channel.rs` | new `process_directed_tsdu(buf)` -- skip Hunting, runs NID + multi-block TSBK pipeline directly |
| 6F.9 | `httpd/mod.rs` | new `iq_lsm_decoder` field on `AppState`, new `ps_iq_lsm` slice in `/api/decoder_compare`, reset both decoders |
| 6F.9 | `main.rs` | new `iq_lsm_decoder` init, LSM IQ task carries `prev_tail` cross-batch + dispatches soft events to directed-decode |
| 6F.9 | `tools/p25_check_phase6f4.py` | new "IQ-LSM decoder" verification section |
| 6F.5-9 | `main.rs` BUILD_TAG | bumped on every phase |

### Tests

`cargo test p25::` = 28 green at every phase. Full crate = 52
green. No new tests were added in this saga because all the
changes are diagnostic infrastructure or parallel pipelines that
share the existing tested parser code. The directed-decode path
runs the same `TrellisDecoder::decode`, `TsbkBlock::parse`, and
`crc_valid` as the streaming path that's already covered by
`test_multi_block_tsbk_e2e`.

---

## Open follow-ups after 6F.9

In rough priority order:

### Item A -- iq_lsm cross-batch defer (6F.10)

`ps_iq_lsm` is at 1.86 blocks/TSDU because small Phase 6D batches
mean ~38% of soft sync events don't have all 336 body dibits in
the carry-over buffer when the directed decode fires. Fix: defer
the directed-decode dispatch for events near the end of a batch
until the next batch arrives, by buffering "pending events" and
checking on each new batch whether there's enough body data yet.
Worth ~10 more parsed msg/s from the iq_lsm pipeline.

### Item B -- bump `max_recent` 100 → 1000 + fix verification script

The `recent_messages` ring buffer caps at 100 messages, which
saturates in ~7 seconds at the steady-state rate. The
verification script computes `messages/sec = messages / uptime`
and reports a misleading 1.1 msg/sec when the real rate is 14.8
msg/sec. Fix: bump the cap and have the verification script use
`tsbk_crc_ok / uptime` for the headline rate.

### Item C -- add parsers for the top unparsed opcodes

Top 5 unparsed opcodes by CRC-OK count over 130s:

| Opcode | Label | Count | % of unparsed |
|---|---|---:|---:|
| 0x16 | SNDCP_DCH_ANN_EX | 533 | 21% |
| 0x05 | UU_ANS_REQ | 263 | 10% |
| 0x30 | TDMA_SYNC_BCST | 259 | 10% |
| 0x09 | TELE_INT_V_CH_GRANT_UPDT | 258 | 10% |
| 0x39 | SEC_CCH_BROADCST | 252 | 10% |

Adding parsers for these would push the parsed-message rate from
~14.8/s to ~25-30/s without any other changes. Each one is a
~30-line addition mirroring the existing IDEN_UPDATE / NET_STS
patterns.

### Item D -- merge dashboard system identity from BOTH lsm decoders

`ps_iq_lsm` saw 3 active grants while `ps_lsm` saw 0 in the same
130s window. Both decoders track their own
`bands` / `grants` / `system` state. The dashboard currently reads
only `lsm_decoder`; merging both would give better grant /
identity coverage during HDL slicer transients.

### Item E -- HDL DC blocker

Still on the long-term DEVPLAN. The 60/40 inner/outer ratio
during PLL hunting goes away once locked, but during transients
(post-flash, post-overflow, signal fading) the slicer noise
spikes. A real DC blocker in the HDL demod chain would shrink
the PLL acquisition transient from ~3 minutes to a few seconds.

---

## Final numbers (130s steady-state, post PLL lock)

```text
== Decoder compare ==
                            ps_lsm     ps_iq_lsm   COMBINED
nid_attempts                  1635          1768
nid_decoded_ok                1609          1499
tsdu_attempts                 1608          1499
tsbk_block_attempts           4826          2785
tsbk_crc_ok                   4424          2571
tsbk_crc_failures              402           214
bands_known                      6             6
active_grants                    0             3

== per-second rates ==
TSDU/s                       12.40         11.56      23.95
Block attempts/s             37.21         21.47      58.68
CRC OK/s                     34.11         19.82      53.93
NID OK/s                     12.40         11.56      23.96

Build tag: 2026-04-11-phase6f.9-iq-lsm-decoder-soft-sync-directed-tsdu
PLL state: locked (sp_dbg ~4000, sync_distance hits 0)
```

User's stated targets met:
- **30 TSBK/sec**: ps_lsm alone delivers 37.21 attempts/s, 34.11
  CRC-OK/s. PASS.
- **10 msg/sec**: parsed messages from `ps_lsm` alone (8 opcodes
  including IDEN+RFSS+NET+GRP) calculate to ~14.8/s. PASS.
