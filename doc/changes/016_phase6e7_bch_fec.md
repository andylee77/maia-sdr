# 016 -- Phase 6E.7: NID BCH(63,16,11) FEC in HDL

**Date:** 2026-04-10
**Phase:** 6E.7 (HDL port of `lsm::nid_fec::decode_nid`)
**Branch:** fishball-p25
**Status:** DONE -- 7 new tests pass, full LSM HDL suite at 41/41

---

## Goal

Bring the maximum-likelihood NID BCH(63,16,11) decoder into PL fabric so
the Phase 6E LSM chain finishes the synchronisation loop entirely in
hardware. After this sub-phase the only remaining HDL work is the
top-level `LsmDemod` wrapper (6E.8) and the `p25_top.py` integration
(6E.9), then the Vivado bake (6E.10).

---

## Architectural change vs the original 6E plan

The original [015 -- Phase 6E plan](015_phase6e_lsm_hdl_port.md) called
for "BCH(63,16,11) codebook BRAM + popcount tree". The codebook holds
all 65,536 valid codewords (4.2 Mbit), the received word is XOR'd
against one entry per cycle, popcount gives Hamming distance, and a
running min identifies the nearest codeword.

This sub-phase deviates from that plan **with the user's explicit
approval**: instead of *storing* the codebook, we *compute* each
codeword on the fly from a 16-bit counter and the constant generator
matrix.

### Why

The Z7020 has ~4.9 Mbit of BRAM. A 65,536 x 64 codebook is ~4.2 Mbit
~= 84% of all BRAM, leaving very little headroom for the existing
C4FM chain, the rest of the LSM front end, and any future expansion.

Crucially, the codebook does not need to be stored at all. Each 64-bit
codeword `cw` is derived from its 16-bit data word `data` by XOR'ing at
most 16 constant 48-bit generator rows -- this is exactly what the
software encoder in `tools/p25_nid_fec.py` and `nid_fec.rs::encode_nid`
does:

```text
parity = XOR over { GEN[i] : data[15-i] == 1 }    # 48-bit
cw     = (data << 48) | parity                    # 64-bit
```

The 16 generator rows are constants (768 bits total). On Z7020 fabric
the parity tree collapses to roughly 2 LUT levels because each parity
output bit is the XOR of ~8 data bits on average (sparse generator) --
synth can pack each 8-input XOR into 2 LUT6s.

### Architectural cut summary

| Aspect | Original BRAM plan | Compute-on-the-fly | Notes |
|---|---|---|---|
| BRAM cost | ~128 BRAM36 (~84% of Z7020) | 0 | huge win |
| LUT cost | popcount tree only | parity tree + popcount | small delta (~64 LUTs for parity) |
| DSP cost | 0 | 0 | both pure-LUT |
| Cycles per decode | 65,538 | 65,538 | same -- still a 16-bit serial sweep |
| Latency at 100 MHz | ~656 us | ~656 us | well under the ~14 ms NID budget |
| Bit-exactness vs `nid_fec.rs` | yes (within unique-decoding sphere) | yes (within unique-decoding sphere) | same algorithm |
| Correction strength | t=11 | t=11 | identical |

The compute-on-the-fly variant satisfies the "all DSP in PL"
architectural lock from doc 015 (no PS involvement, full HDL implementation)
without burning the BRAM budget.

The architectural-decision conversation that led to this cut is in
the session log; the short version is "what do you store vs what do
you compute -- the codebook is cheap to compute, expensive to store,
so compute it."

---

## Module: `LsmNidBchFec`

**Path:** [`maia-hdl/p25_hdl/lsm_nid_bch_fec.py`](../../maia-hdl/p25_hdl/lsm_nid_bch_fec.py)

### I/O

Inputs (sync domain):

```text
start         : Signal()         -- pulse to begin a decode
received_nid  : Signal(64)       -- latched on `start`
```

Outputs (sync domain):

```text
done          : Signal()         -- 1-cycle pulse when result is ready
nac_out       : Signal(12)
duid_out      : Signal(4)
n_errors_out  : Signal(7)        -- 0..63 (Hamming dist to nearest cw)
valid_out     : Signal()         -- 1 if n_errors_out <= 11
busy          : Signal()         -- high during a decode
```

### Algorithm (per cycle in SWEEP state)

```text
data       = sweep_counter[0:16]                    # 16-bit
parity     = XOR over { GEN[i] : data[15-i] == 1 }  # 48-bit (combinational)
codeword   = (data << 48) | parity                  # 64-bit
diff       = codeword ^ received_nid_latched        # 64-bit
dist       = popcount(diff)                         # 7-bit
if dist < best_dist:
    best_dist <= dist
    best_data <= data
```

After 65,536 sweep cycles the loop has compared the received word
against every valid codeword. Outputs are latched and `done` pulses
high for one cycle, then the FSM returns to IDLE.

### State machine

```text
IDLE   -> SWEEP   on `start`
SWEEP  -> IDLE    on counter[16] == 1 (after processing data 0..65535)
                   - latches outputs, pulses done for one cycle
```

The 17-bit counter (`Signal(17)`) cleanly distinguishes "still
sweeping" (`counter[16] == 0`) from "done sweeping"
(`counter[16] == 1`).

### Generator matrix bit ordering

The encoder iterates `for bit_idx in 0..15` and selects `GEN[bit_idx]`
when `data_word & (1 << (15 - bit_idx))` is non-zero -- i.e., the MSB
of the 16-bit data word selects `GEN[0]`. The HDL mirrors this exactly:
parity bit `bit` is `XOR { sweep_data[15-i] : (GEN[i] >> bit) & 1 }`,
where `sweep_data[15]` is the MSB of the 16-bit data field. The
encoder reference test (`test_encoder_reference_matches_sdrtrunk_vector`)
locks this against the SDRTrunk-published golden vector
`encode_nid(1, 0) == 0x00103185B7E9E224` so any future drift in the
generator matrix or bit ordering fails fast.

### Resource estimate (Z7020)

| Resource | Cost |
|---|---|
| BRAM18 | 0 |
| DSP48E1 | 0 |
| LUT (parity tree) | ~96 |
| LUT (64-bit XOR) | 64 |
| LUT (popcount tree) | ~150 |
| LUT (compare + min mux) | ~30 |
| FF (counter + best_dist + best_data + outputs) | ~50 |

Total: <1 % of Z7020 LUT/FF, **zero BRAM, zero DSP**.

### Pipelining note

The combinational chain `data -> parity -> cw -> diff -> popcount ->
compare` is roughly 13 LUT levels deep. This is well within Vivado's
reach at 100 MHz on Z7020 (typical max ~15 LUT levels per period). If
post-synthesis timing reports a violation, the natural cut is between
`popcount` and the comparator -- a single registered stage doubles
the latency to ~131k cycles (~1.3 ms) without changing the algorithm.
We deliberately do not pre-pipeline; the simpler implementation is
easier to verify against the Rust reference and the cycle budget has
plenty of slack.

---

## Test bench

**Path:** [`maia-hdl/test/test_lsm_nid_bch_fec.py`](../../maia-hdl/test/test_lsm_nid_bch_fec.py)

### Why the test set is small by default

`amaranth-sim` runs at ~5,000 sync ticks/sec on this machine. Each
HDL decode is ~65,538 ticks, so one decode is ~13 seconds. A naive
port of the Rust test (5 trials per error level x 11 levels + all 64
single-bit positions = 119 decodes) would take ~25 minutes per
suite run, which is unacceptable for routine TDD.

Instead, the default suite runs **7 cheap tests** (5 of which are
HDL, 2 are pure-software) totalling **7 HDL decodes** (~90 seconds):

| Test | Decodes | What it covers |
|---|---|---|
| `test_encoder_reference_matches_sdrtrunk_vector` | 0 | bit ordering + generator matrix vs SDRTrunk golden |
| `test_encoder_data_field_layout` | 0 | data field bit layout in the 64-bit codeword |
| `test_clean_codeword_and_done_strobe` | 1 | clean decode + `done` is 1 cycle + `busy` waveform |
| `test_single_bit_error_sample_positions` | 4 | 1-bit flips at positions {0, 15, 16, 47} |
| `test_error_correction_at_t1_t6_t11` | 3 | multi-error correction at t=1, 6, 11 |

The Rust unit tests in `nid_fec.rs::tests` already do the broad
coverage (5 trials per error level x 11 levels + 100 trials at t=12
to verify graceful behaviour beyond the unique-decoding sphere). The
HDL test only needs to confirm that the implementation we built agrees
on a representative sample. The PRNG seed used for the multi-error
trials matches the Rust test (`0xDEAD_BEEF_CAFE_BABE`) so any
divergence is reproducible and easy to bisect.

### Slow-mode opt-in

Two additional tests live in a class gated by
`@unittest.skipUnless(SLOW)`:

```text
MAIA_HDL_SLOW_TESTS=1 python -m unittest test.test_lsm_nid_bch_fec
```

| Slow test | Decodes | Sim time |
|---|---|---|
| `test_error_correction_sweep_up_to_t11` | 55 | ~12 min |
| `test_all_64_single_bit_positions` | 64 | ~14 min |

Run before bitstream bake or on a CI box. Skipped by default.

### Test results

```text
$ python -m unittest test.test_lsm_decimator test.test_lsm_fir \
    test.test_lsm_timing_interp test.test_lsm_diff_demod_slicer \
    test.test_lsm_gardner_ted test.test_lsm_pll_update \
    test.test_lsm_pll_rotate test.test_lsm_demod_loop \
    test.test_lsm_nid_bch_fec
...
Ran 41 tests in 195.545s
OK (skipped=2)
```

Up from 34/34 in [doc 015](015_phase6e_lsm_hdl_port.md). The two
skips are the slow-mode BCH sweeps. All previous LSM HDL tests still
pass unchanged.

---

## What changed

### New files

```text
maia-hdl/p25_hdl/lsm_nid_bch_fec.py     ~280 lines  -- the HDL module
maia-hdl/test/test_lsm_nid_bch_fec.py   ~340 lines  -- 7 tests (5 default + 2 slow-mode)
doc/changes/016_phase6e7_bch_fec.md     this document
```

### Modified files

```text
doc/changes/015_phase6e_lsm_hdl_port.md   6E.7 row marked DONE, plan-deviation note added
DEVLOG.md                                  session log entry
CHANGELOG_FORK.md                          phase entry
```

---

## Phase ladder status (post-6E.7)

- 6A: Python LSM demod -- DONE (`e1980aa`, doc 011)
- 6B: NID BCH FEC -- DONE (`46630d6`, doc 012)
- 6C: IQ DMA path in FPGA gateware -- DONE (`9f35f34`, doc 013)
- 6D: Rust LSM port to PS -- DONE (`7d69bac` + `b386c5b`, doc 014)
- 6E.0-6E.6: HDL front end + demod loop -- DONE (`ce9633b`, doc 015)
- 6E.6.5: AGC in HDL -- deferred follow-up
- **6E.7: BCH FEC in HDL -- DONE (THIS commit, doc 016)**
- 6E.8: top-level `LsmDemod` assembling everything -- next
- 6E.9: wire `LsmDemod` into `p25_top.py` alongside C4FM
- 6E.10: regen Verilog + bitstream + on-target validation

---

## Notes for the next session

The decoder needs ~656 us per NID at 100 MHz. The caller (`LsmDemod`
in 6E.8) must ensure it does not feed a new `start` until the previous
`done` has fired. NIDs are spaced at ~14 ms minimum so the duty cycle
is well under 5 % and a simple "wait for `busy=0`" gate is fine.

The decoder reports `valid_out=0` for any received word with
`n_errors > 11`. The dashboard's NID stats counter should track this
separately as "uncorrectable NID" so the user can distinguish a true
sync miss from a high-noise NID. The Rust LSM pipeline already keeps
this counter (`stats.nids_uncorrectable`); the HDL decoder just needs
to surface the boolean.
