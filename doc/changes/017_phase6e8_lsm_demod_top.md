# 017 -- Phase 6E.8: LSM Demod Top-Level (sync detect + NID pipeline)

**Date:** 2026-04-10
**Phase:** 6E.8 (HDL top-level wiring of the full LSM chain)
**Branch:** fishball-p25
**Status:** DONE -- 8 new tests, full LSM HDL suite at 49/49

---

## Goal

Wrap the closed-loop demod (6E.6d) and the BCH FEC decoder (6E.7)
behind a single top-level Elaboratable so the rest of the FPGA only
needs to instantiate one block to get the complete LSM chain. After
this sub-phase, the only HDL work left is wiring `LsmDemod` into
`p25_top.py` (6E.9) and the Vivado bake (6E.10).

This sub-phase also closes the gap between dibits and NIDs by
introducing the **hard sync detector** and the **status-skipping
NID extractor**, which between them turn the demod loop's dibit
stream into the 64-bit NID words that `LsmNidBchFec` consumes.

---

## What's new

Three new Amaranth modules and four new test files. The split is
deliberate -- the tests do not need a single monolithic test of
`LsmDemod` that costs ~13 s of BCH sweep time AND ~2 s of demod loop
sim time, when both halves can be tested independently for less
total cost.

### `LsmSyncNidExtract` -- hard sync detector + NID extractor

**Path:** [`maia-hdl/p25_hdl/lsm_sync_nid_extract.py`](../../maia-hdl/p25_hdl/lsm_sync_nid_extract.py)

Streaming HDL equivalent of `find_sync_events_hard()` +
`extract_nid_skipping_status()` in [`p25-httpd/src/lsm/sync.rs`](../../p25-httpd/src/lsm/sync.rs).
Owns three pieces of state:

- A 48-bit shift register that always holds the most recent 24
  dibits, with new dibits shifted into the LSB end.
- A small fill counter that gates the threshold check until the
  register has seen 24 dibits since reset (matching the Rust
  loop's `if i < FRAME_SYNC_DIBITS continue`).
- A 6-bit dibit counter for the 33-dibit NID window, plus a
  64-bit `nid_word` accumulator.

Per cycle in IDLE, on a `dibit_strobe`:

```text
new_reg  = Cat(dibit_in, sync_reg[:46])
new_diff = new_reg ^ FRAME_SYNC_DIBIT_PATTERN  # 0x5575_F5FF_77FF
new_dist = popcount(new_diff)
if reg_full_after_this_dibit and new_dist <= SYNC_THRESHOLD (4):
    -> COLLECT_NID
```

In COLLECT_NID, on each `dibit_strobe`, append the dibit to
`nid_word` (skipping index 11 -- the status dibit) until 33 dibits
have been seen. Then transition to a one-cycle EMIT state that
latches `nid_out`, `nid_distance`, pulses `nid_strobe`, and clears
the sync register so we don't re-trigger on the dibits we just
consumed. We deliberately do **not** reset `reg_fill` in EMIT --
that would impose an artificial 24-dibit refill gate the Rust
reference doesn't have. The natural high popcount of the cleared
register vs. the sync pattern (`popcount(0 ^ 0x5575_F5FF_77FF) = 37`)
keeps the threshold check from spuriously firing during the
post-EMIT refill.

**Why hard detector and not soft.** The Rust soft detector inner-
products the demod's `atan2` soft phase output against the 24 ideal
sync phases (`+/- 3PI/4`). We deliberately did not produce a soft
phase tap from `LsmDiffDemodSlicer` in 6E.5 -- the slicer just
returns `Cat(i_sign, q_sign)`. Implementing the soft detector in
HDL would either need a CORDIC vector mode in front of the slicer
or a separate quantised-phase tap. Both are doable but neither is
on the critical path for first bring-up. The Rust comments call
the hard detector "the simple HDL-friendly version" and that is
exactly what this module is. If hardware ever shows the hard
detector under-performing on real RF (e.g. simulcast capture with
heavy multipath), the soft detector slots in as a sub-phase
6E.8.5: add an `atan2` tap, build a 24-tap fixed-point inner-
product correlator, and OR the two detectors' `nid_strobe`s.

**I/O.**

```text
inputs:
    dibit_in     : Signal(2)
    dibit_strobe : Signal()

outputs:
    nid_out         : Signal(64)   -- valid when nid_strobe is high
    nid_distance    : Signal(7)    -- Hamming dist of the sync hit
    nid_strobe      : Signal()     -- 1-cycle pulse per recovered NID
    in_nid_window   : Signal()     -- high while collecting NID dibits
```

**Resource estimate.** <50 LUT, <130 FF, **0 BRAM, 0 DSP**.

### `LsmNidPipeline` -- sync + BCH chain

**Path:** [`maia-hdl/p25_hdl/lsm_nid_pipeline.py`](../../maia-hdl/p25_hdl/lsm_nid_pipeline.py)

Thin wrapper that chains `LsmSyncNidExtract` + `LsmNidBchFec` and
owns the start/done handshake plus the NID-drop counter. Extracted
from `LsmDemod` for testability: this is the only piece of new
control logic in 6E.8, so we want to integration-test it without
dragging the IQ-to-dibit demod loop and a synthetic IQ golden into
the test bench.

The handshake policy:

```text
bch.received_nid <= sync_nid.nid_out
bch_start_pulse  <= sync_nid.nid_strobe & ~bch.busy
bch.start        <= bch_start_pulse

if bch_start_pulse:
    latched_sync_distance <= sync_nid.nid_distance
elif sync_nid.nid_strobe & bch.busy:
    nid_drop_count <= saturating_inc(nid_drop_count)

if bch.done:
    latch (nac_out, duid_out, n_errors_out, valid_out,
           sync_distance_out)
    pulse nid_event_strobe for one cycle
```

**Why drop and not queue.** NIDs on real RF are spaced ~14 ms apart
(one P25 frame). The BCH decoder takes ~656 us. The collision
probability is therefore ~5%, but only if the very first NID after
power-on lands during the very first BCH sweep -- which itself
won't happen because the BCH decoder is quiescent at power-on. In
practice the drop counter should be 0 forever. The 16-bit
saturating drop counter exists purely as a safety net so the
dashboard can flag the rare event if it does happen. If we ever see
non-zero drops on hardware, the right fix is a tiny 1- or 2-deep
FIFO between `sync_nid` and `bch`, not a more complex handshake.

### `LsmDemod` -- the top-level

**Path:** [`maia-hdl/p25_hdl/lsm_demod.py`](../../maia-hdl/p25_hdl/lsm_demod.py)

```text
re_in/im_in/strobe_in -> LsmDemodLoop -> dibits -> LsmNidPipeline
                              |                          |
                              v                          v
                       dibit_out (passthrough     (NAC, DUID,
                       to existing dibit DMA)      n_errors,
                                                   valid,
                                                   sync_distance,
                                                   nid_event_strobe,
                                                   nid_drop_count,
                                                   bch_busy)
```

The dibit pass-through is essential -- the existing dibit DMA path
in `p25_top.py` still consumes `(dibit_out, symbol_strobe)` exactly
the same way it does today, so 6E.9 won't have to plumb a second
dibit channel. The new NID outputs are siblings of the dibit path,
each with their own AXI register exposure (TBD in 6E.9).

The `pll_dbg` and `sample_point_dbg` debug taps are passed through
from `LsmDemodLoop` so the dashboard can keep showing PLL and
sample-point traces without instrumenting deeper into the design.

---

## Tests

Four new test files, eight new test methods:

| File | Tests | Sim cost | What it covers |
|---|---|---|---|
| `test_lsm_sync_nid_extract.py` | 6 | <0.5 s | sync detect + NID extract standalone (no BCH) |
| `test_lsm_nid_pipeline.py`     | 1 | ~13 s   | sync + BCH integration on a constructed dibit stream |
| `test_lsm_demod.py`            | 1 | ~1.7 s  | LsmDemod pass-through + NID-pipeline-quiescent on a real IQ golden |

### `test_lsm_sync_nid_extract.py` (6 tests)

- `test_clean_sync_and_nid_extraction` -- sync pattern + clean NID
  for NAC=0x8A1/DUID=7 -> exactly one nid_strobe with distance==0
  and `nid_out == encode_nid(0x8A1, 7)`.
- `test_one_dibit_error_in_sync_still_triggers` -- flip one bit in
  the second sync dibit; threshold (4) is loose enough to still
  fire; mirrors `hard_detector_tolerates_one_dibit_error_in_sync`
  in `nid_fec.rs::tests`.
- `test_status_dibit_is_skipped` -- two streams identical except
  for the value at NID dibit index 11; the extracted NIDs must be
  bit-identical.
- `test_back_to_back_sync_events` -- two complete sync+NID windows
  in one stream -> two `nid_strobe`s.
- `test_no_false_trigger_before_register_fills` -- 23 zero dibits
  followed by a clean sync+NID; the fill gate must keep the
  detector silent until 24 dibits have been seen.
- `test_in_nid_window_tracks_state` -- `in_nid_window` is 0 in
  IDLE, 1 during the 33-dibit NID collection, 0 again after EMIT.

These run in <0.5 s total because they don't involve the BCH
decoder.

### `test_lsm_nid_pipeline.py` (1 test)

`test_clean_sync_and_bch_decode` -- end-to-end on a constructed
sync + clean NID dibit stream:

- exactly one `nid_event_strobe` fires
- `nac_out == 0x8A1`, `duid_out == 7`
- `n_errors_out == 0`, `valid_out == 1`
- `sync_distance_out == 0`
- `nid_drop_count == 0`

Runs in ~13 s (one BCH decode, ~65,538 sync ticks at the
serial-sweep rate of the BCH decoder). Adds nothing on top of the
13-s baseline because the dibits are constructed in pure Python
and shifted in at one tick per dibit.

### `test_lsm_demod.py` (1 test)

`test_dibit_passthrough_and_quiescent_nid_pipeline` -- drives the
demod_loop synthetic IQ golden (1666 input samples, ~254 output
symbols) through `LsmDemod` and verifies:

1. Dibit pass-through still produces ~254 dibits via `dibit_out`/
   `symbol_strobe` -- LsmDemodLoop's job is unchanged.
2. The synthetic golden contains no sync pattern, so:
   - `bch_busy` stays low across the entire run
   - `in_nid_window` stays low
   - `nid_event_strobe` never fires
   - `nid_drop_count` finishes at 0

Runs in ~1.7 s. The standalone demod loop's per-dibit accuracy
versus truth is covered by `test_lsm_demod_loop` and is not
re-checked here -- the LsmDemod test is purely a wiring test.

### Test results

```text
$ python -m unittest test.test_lsm_decimator test.test_lsm_fir \
    test.test_lsm_timing_interp test.test_lsm_diff_demod_slicer \
    test.test_lsm_gardner_ted test.test_lsm_pll_update \
    test.test_lsm_pll_rotate test.test_lsm_demod_loop \
    test.test_lsm_nid_bch_fec test.test_lsm_sync_nid_extract \
    test.test_lsm_nid_pipeline test.test_lsm_demod
...
Ran 49 tests in 172.729s
OK (skipped=2)
```

Up from 41/41 LSM HDL tests in 6E.7. Two skips are the slow-mode
BCH sweeps from 6E.7 still gated behind `MAIA_HDL_SLOW_TESTS=1`.
All previous LSM tests still pass unchanged.

---

## Resource estimate (Z7020)

Sum of submodule estimates:

| Component | DSP48 | BRAM18 | LUT | FF |
|---|---|---|---|---|
| LsmDemodLoop (6E.6d) | ~30 | 2 | ~3500 | ~1500 |
| LsmSyncNidExtract (6E.8a) | 0 | 0 | ~50 | ~130 |
| LsmNidBchFec (6E.7) | 0 | 0 | ~340 | ~50 |
| LsmNidPipeline glue (6E.8b) | 0 | 0 | ~30 | ~50 |
| LsmDemod glue (6E.8c) | 0 | 0 | ~10 | 0 |
| **Total** | **~30** | **2** | **~3930** | **~1730** |

That's roughly 14 % of Z7020 DSP48, 1.4 % of BRAM18, and ~7 % of
LUT/FF for one full LSM channel. Plenty of room for the existing
C4FM chain, the Phase 6C IQ DMA, the Maia SDR base platform, and
any future AGC follow-up.

---

## What changed

### New files

```text
maia-hdl/p25_hdl/lsm_sync_nid_extract.py  ~270 lines  -- 6E.8a hard sync + NID extractor
maia-hdl/p25_hdl/lsm_nid_pipeline.py      ~140 lines  -- 6E.8b sync + BCH chain
maia-hdl/p25_hdl/lsm_demod.py             ~180 lines  -- 6E.8c top-level
maia-hdl/test/test_lsm_sync_nid_extract.py ~290 lines -- 6 tests
maia-hdl/test/test_lsm_nid_pipeline.py    ~140 lines  -- 1 integration test
maia-hdl/test/test_lsm_demod.py           ~140 lines  -- 1 pass-through test
doc/changes/017_phase6e8_lsm_demod_top.md  this document
```

### Modified files

```text
doc/changes/015_phase6e_lsm_hdl_port.md   6E.8 row marked DONE
DEVLOG.md                                  session log entry
CHANGELOG_FORK.md                          phase entry
```

---

## Phase ladder status (post-6E.8)

- 6A: Python LSM demod -- DONE (`e1980aa`, doc 011)
- 6B: NID BCH FEC -- DONE (`46630d6`, doc 012)
- 6C: IQ DMA path in FPGA gateware -- DONE (`9f35f34`, doc 013)
- 6D: Rust LSM port to PS -- DONE (`7d69bac` + `b386c5b`, doc 014)
- 6E.0-6E.6: HDL front end + demod loop -- DONE (`ce9633b`, doc 015)
- 6E.6.5: AGC in HDL -- deferred follow-up
- 6E.7: BCH FEC in HDL -- DONE (doc 016)
- **6E.8: LsmDemod top-level (sync detect + NID pipeline) -- DONE (THIS commit, doc 017)**
- 6E.8.5: Soft sync detector -- deferred follow-up (only if hard
  detector under-performs on real RF)
- 6E.9: wire `LsmDemod` into `p25_top.py` alongside C4FM
- 6E.10: regen Verilog + bitstream + on-target validation

---

## Notes for the next session (6E.9)

`p25_top.py` currently instantiates the C4FM chain directly. The
6E.9 work is:

1. Add an `LsmDemod` submodule alongside the existing C4FM chain.
   Both consume the same post-DDC IQ stream. The C4FM chain
   produces dibits via the existing `dibit_dma` ring; the LSM
   chain has its own `dibit_out`/`symbol_strobe` pair that needs
   to feed a parallel `dibit_dma` (or share the existing one
   behind a mux on a runtime select bit -- TBD).
2. Surface the new NID event outputs as AXI registers in a new
   register bank (or extend an existing one). At minimum the
   dashboard needs to read `nac_out`, `duid_out`, `n_errors_out`,
   `valid_out`, `sync_distance_out`, `nid_drop_count`, and a
   "new event" flag that latches on `nid_event_strobe` and clears
   on read.
3. Decide whether to gate `LsmDemod`'s clock-enable on the
   existing "demod enable" register so the LSM chain can be
   disabled in the field for power or debugging reasons.

Once 6E.9 lands, 6E.10 is the standard regen-Verilog + Vivado
synth + on-target validation cycle, with the bitstream tagged so
the firmware build that ships it knows it's the LSM-capable
variant.
