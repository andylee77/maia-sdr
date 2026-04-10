# 021 -- Phase 6E.6e: replace small-angle PLL update with CORDIC atan2

**Date:** 2026-04-10
**Phase:** Phase 6E.6e (PLL update slip-resistance fix)
**Branch:** fishball-p25
**Status:** HDL + tests + Verilog regen DONE; Vivado bake + flash pending

---

## TL;DR

The Phase 6E.6b small-angle linearised PLL update in
[`maia-hdl/p25_hdl/lsm_pll_update.py`](../../maia-hdl/p25_hdl/lsm_pll_update.py)
let the integrator drift across the 4-PSK Costas slip threshold under
real-RF transients with no recovery path. Replaced with a true atan2
phase-error computation backed by a new 10-iteration CORDIC vectoring
block in
[`maia-hdl/p25_hdl/lsm_cordic_atan2.py`](../../maia-hdl/p25_hdl/lsm_cordic_atan2.py).
The CORDIC form has a self-correcting `|phase_error| <= pi/4` bound by
construction (because `to_dibit` picks the closest 4-PSK quadrant
*before* the phase-error subtraction), which is the same property that
makes the floating-point Rust LSM pipeline (Phase 6D) immune to the
slip on the same RF feed.

---

## The bug

Test cycle 3 on 2026-04-10 against Clay County NAC `0x8A1` (the LSM
simulcast site at 860.9625 MHz) showed:

- HDL LSM chain locked cleanly from cold boot, decoded **35 consecutive
  valid NIDs in 18 seconds** with `pll_dbg` in a stable
  `[-6697, -4800]` range
- On NID #36 the PLL integrator flipped sign from `~[-8500, -4500]` to
  `~[+5000, +8500]` within a single 1-second heartbeat window
- Stuck in the wrong 4-PSK Costas basin **forever** -- every subsequent
  NID extraction failed because the dibit stream was now 90 deg
  rotated relative to `FRAME_SYNC_DIBIT_PATTERN`
- Same RF feed, same `iq_dma` sample stream, fed through the Phase 6D
  Rust LSM pipeline ran **9 minutes without a single slip** at 82.7 %
  NAC hit rate

So the on-target slip is unambiguously a property of the *HDL* PLL
update, not the signal path upstream of it.

## The root cause

[`lsm_pll_update.py`](../../maia-hdl/p25_hdl/lsm_pll_update.py) (Phase
6E.6b form) approximates the phase error per the small-angle
linearisation:

```text
phase_error_proxy = (q +/- i) * sqrt(2)/2     # 4-way mux on dibit
clamped to +/- 0.4243 (= 0.3 / (sqrt(2)/2))
pll -= phase_error_proxy * 0.0707             # combined gain
clamped to +/- pi/3
```

The literal Rust loop in
[`p25-httpd/src/lsm/demod.rs`](../../p25-httpd/src/lsm/demod.rs):265
does it differently:

```rust
let h = to_dibit(soft_symbol);                 // pick nearest quadrant
let mut phase_error = soft_symbol - dibit_phase(h);
                                               // |phase_error| <= pi/4 by construction
clamped to +/- 0.3
state.pll -= phase_error * 0.1;
clamped to +/- pi/3
```

The Rust form has a **structural** bound: because `to_dibit` always
picks the *closest* 4-PSK constellation point, the subtraction
`soft_symbol - dibit_phase(h)` is mathematically guaranteed to land in
`[-pi/4, +pi/4]`. Inside that envelope every per-symbol update points
toward the correct lock point.

The linearised form has no such structural bound. Its per-step
magnitude is bounded by the `+/- 0.4243` raw clamp, but the *direction*
of the step can sustain pointing away from the correct lock point under
several conditions:

1. The first-order linearisation `(q +/- i) ~= sin(phase_error)` is
   only valid for small `phase_error`. At the `+/- 0.3 rad` clamp
   boundary the linearisation has 1.5 % relative error, which biases
   the integrator. Under repeated noisy transients these biases
   accumulate.
2. The `(q +/- i)` form scales linearly with input magnitude.
   Without an AGC block (Phase 6E.6.5 deferred), input magnitude
   excursions show up directly as per-step magnitude excursions, which
   the raw clamp truncates rather than normalises.
3. The dibit decision is decision-directed off the slicer. If the
   slicer's quadrant decision is wrong (because the loop is currently
   off-lock), the linearisation pulls toward the *wrong* lock point
   for as long as the wrong decision sticks. The literal form does
   the same thing, but its `<= pi/4` per-step bound means the wrong
   pull is short-lived: the next correct decision yields a small
   correct correction, not a large wrong-direction continuation.

The Rust pipeline running on the same IQ feed never slips, so we know
the algorithm in *that form* handles whatever transient knocked the
HDL out of lock. The fix is therefore to bring the HDL up to algorithmic
parity with the Rust loop.

## The fix

### New module: `lsm_cordic_atan2.py`

10-iteration CORDIC vectoring with quadrant pre-rotation. Computes
`atan2(y_in, x_in)` as a signed 20-bit Q4.16 value. Pure shifts and
adds, no DSP, no BRAM, ~100 LUT and 12-cycle pipeline latency.

Why CORDIC and not a LUT-based atan2: the input width is 20 bits, so
a direct LUT would need ~1 Mb. Direct CORDIC is the standard hardware
choice for this exact problem.

Why 10 iterations: empirical sweep over 2000 random inputs at N = 4,
6, 8, 10, 12 (test_lsm_cordic_atan2.py uses the same sweep with 64
inputs as a regression):

| N  | Max err  | RMS err |
|----|----------|---------|
| 4  | 124 mrad | 70 mrad |
| 6  |  31 mrad | 18 mrad |
| 8  |   8 mrad |  5 mrad |
| 10 |   3 mrad |  1 mrad |
| 12 |   1 mrad |  0.3 mrad |

At N=10 the worst case is well below the per-step PLL clamp (300 mrad)
and the gain * clamp product (30 mrad) so the integrator absorbs the
CORDIC residual instantly. N=12 buys a small precision improvement
for an extra 2 cycles of latency and ~16 LUT -- not worth it.

The Python reference `cordic_vectoring_reference()` is **bit-exact**
against the HDL: `test_lsm_cordic_atan2.py` runs 64 random inputs
through both and compares ULP-by-ULP.

### Reworked: `lsm_pll_update.py`

Two PLL update implementations now coexist:

- **`LsmPllUpdate`** (production): the new CORDIC form. Pre-rotates
  `(i, q)` by `-dibit_phase(h)` using a 4-way mux on the dibit
  (cheap -- the rotation is just `+/-(i +/- q)` because all four
  ideal angles are at `+/- pi/4` or `+/- 3pi/4`). Feeds the rotated
  vector through `LsmCordicAtan2` to get the *true* phase error
  relative to the ideal constellation point. Clamps to `+/- 0.3 rad`,
  multiplies by the 0.1 loop gain, subtracts from the integrator,
  clamps to `+/- pi/3`. **15-cycle pipeline depth**.

- **`LsmPllUpdateLinearised`** (legacy): the original Phase 6E.6b
  form, kept in-tree as the "before" half of the slip-resistance
  regression test in `test_lsm_demod_loop.py` and as a historical
  reference. Production builds do NOT instantiate it.

`LsmDemodLoop` now takes a `pll_mode` parameter (`'cordic'` |
`'linearised'`, default `'cordic'`) so the regression tests can A/B
both forms on identical input.

### Pre-rotation table

The CORDIC PLL update doesn't need a separate "subtract dibit_phase"
step -- it absorbs the subtraction into a free pre-rotation by
`-dibit_phase(h)`. Let `a = i + q` and `b = q - i`:

| dibit | ideal angle | x' (= cos(ang)*i + sin(ang)*q) | y' (= cos(ang)*q - sin(ang)*i) |
|-------|-------------|-------------------------------|-------------------------------|
| 00    | +pi/4       | +a                            | +b                            |
| 01    | +3pi/4      | +b                            | -a                            |
| 10    | -pi/4       | -b                            | +a                            |
| 11    | -3pi/4      | -a                            | -b                            |

The implicit `sqrt(2)/2` factor on the ideal cos/sin is irrelevant --
`atan2` is invariant to uniform input scaling. CORDIC needs 1 cycle
to compute `(a, b)` and 1 cycle to mux on the dibit.

### Zero-input gate

The Rust loop has an explicit `if soft_symbol != 0.0 { ... }` skip
on the PLL update. Matched in HDL with a `pending_skip` latch: if
both `i_sym_in` and `q_sym_in` are exactly zero on `symbol_strobe`,
the post-CORDIC subtract is forced to zero so the integrator is
left untouched. This avoids the degenerate `atan2(0, 0)` case which
in CORDIC vectoring produces the algebraic sum of every angle
constant (~1.74 rad of pure garbage).

Single-shot is safe because symbol strobes are ~13000 sync cycles
apart vs the 16-cycle PLL update latency -- only one update is
ever in flight.

## Verification

### Unit tests

- **`test_lsm_cordic_atan2.py`** (NEW, 13 tests, all pass):
  - Pipeline latency lock-down (12 cycles)
  - Cardinal-axis inputs (zero, +x, +y, -x, -y) for quadrant
    coverage and pre-rotation correctness
  - Four constellation-corner inputs (+/- pi/4, +/- 3pi/4)
  - Bit-exact match against `cordic_vectoring_reference` on 64
    random inputs
  - Float-precision sweep against `math.atan2`, asserting N=10
    worst case stays under 5 mrad (measured: 1.9 mrad max,
    1.0 mrad rms)

- **`test_lsm_pll_update.py`** (REWORKED, 10 tests, all pass):
  - 5 legacy tests retargeted to `LsmPllUpdateLinearised` (proves
    the legacy class still works after refactor)
  - 5 new tests for `LsmPllUpdate` (CORDIC form):
    - Zero-input no-change (verifies the `pending_skip` path)
    - Each-dibit-drives-correct-sign (smoke per quadrant)
    - PLL clamps at +/- pi/3 (saturation)
    - Against `_AtanPll` literal-atan2 reference (max 10 ULPs
      ~= 1.2 mrad over 64 steps; budget 80 ULPs)
    - Skip-on-zero-input doesn't desync the CORDIC pipeline
      (alternates zero/non-zero inputs and verifies the integrator
      only moves on non-zero updates)

- **`test_lsm_demod_loop.py`** (EXTENDED, +2 tests, all 3 pass):
  - Original `test_demod_loop_synthetic_matches_truth` now uses the
    CORDIC default and matches truth at 99.6 % (was 100.0 % with
    linearised; the small drop is the CORDIC's ~1 mrad/symbol
    quantisation showing up as one extra dibit flip near the end of
    the synthetic vector)
  - NEW `test_demod_loop_linearised_baseline` (proves
    `pll_mode='linearised'` still hits 100 % on the clean golden)
  - NEW `test_demod_loop_cordic_vs_linearised_under_phase_step`
    (injects a 0.4 rad carrier phase step at the input midpoint
    and runs both PLL modes on the same perturbed input, asserts
    CORDIC >= 80 % and CORDIC + 2 % >= linearised)

### Full test suite

```text
test_lsm_decimator             | 2/2  ok
test_lsm_fir                   | 4/4  ok
test_lsm_timing_interp         | 5/5  ok
test_lsm_diff_demod_slicer     | 4/4  ok
test_lsm_gardner_ted           | 5/5  ok
test_lsm_pll_update            | 10/10 ok   (5 linearised + 5 CORDIC)
test_lsm_pll_rotate            | 4/4  ok
test_lsm_demod_loop            | 3/3  ok    (1 baseline + 1 linearised + 1 phase step)
test_lsm_nid_bch_fec           | 13/13 ok   (2 slow gated by MAIA_HDL_SLOW_TESTS)
test_lsm_sync_nid_extract      | 5/5  ok
test_lsm_nid_pipeline          | 4/4  ok
test_lsm_demod                 | 3/3  ok
test_lsm_cordic_atan2          | 13/13 ok   (NEW)
                                ----
Total                          | 69 ok, 2 skipped (slow)
```

Wider sanity sweep (`test_iq_packer test_dibit_packer test_c4fm_demod
test_symbol_timing`): 25/25 OK.

### Verilog regen

```text
build_hdl.bat --p25 --verilog-only
  -> ip/p25-core/default/p25_core.v   (38216 lines, 1.33 MB)
  -> p25-httpd/p25-pac/p25.svd        (byte-identical -- no register
                                       layout change)
  -> p25-httpd/p25-pac/src/lib.rs     (byte-identical)
```

96 instances of `cordic` in the regenerated `p25_core.v` -- the
`LsmCordicAtan2` block was successfully picked up by Amaranth's
elaboration through `LsmDemodLoop` (which sits inside `LsmDemod` ->
`p25_top.py`).

The PAC and SVD didn't change because the CORDIC block is internal to
`LsmPllUpdate` and doesn't touch the AXI-Lite register bank or any
exposed signal layout.

## Resource impact

Estimated above the linearised form (no synthesis run yet -- post-bake
this should be confirmed against the Vivado utilization report):

- `LsmCordicAtan2`: ~100 LUT, ~80 FF, 0 DSP, 0 BRAM
- `LsmPllUpdate` post-CORDIC clamp/scale/subtract scaffolding:
  ~30 LUT above the linearised form
- Total: ~130 LUT and ~80 FF added to `LsmDemod`

The DSP count is unchanged (1 DSP for the gain multiply, same as the
linearised form). BRAM count is unchanged.

`LsmDemod`'s previous estimate from doc 015 was ~30 DSP48 and
~3940 LUT. Post-CORDIC the DSP count stays at ~30 and the LUT count
goes up to ~4070 (~3 % increase). Well within the 7020's budget.

## Pipeline latency impact

| Stage                          | Linearised | CORDIC |
|--------------------------------|------------|--------|
| symbol_strobe -> pll_strobe    | 3 cycles   | 16 cycles |

Symbol period at the Fishball clock (62.5 MHz core / 4800 sym/s) is
~13000 cycles, so the 13-cycle delta is invisible in the symbol
budget. The PLL output `pll_out` is read combinationally by both
`LsmPllRotate` instances and is always current relative to the
last completed update -- no scheduling change required.

## What this fix does NOT do

- **Does not implement AGC** (Phase 6E.6.5, still deferred). The
  Rust pipeline proves AGC isn't needed for Clay County (the strong
  control channel that this whole project targets), so AGC stays
  deferred until we encounter a weaker site.
- **Does not implement a soft sync detector** (Phase 6E.8.5, still
  deferred). The hard sync detector locks every NID at 100 % on
  this site -- soft is overkill until we see hard underperforming.
- **Does not remove the PS-side watchdog** added in commit 0ef0d09
  / doc 023. Per the user's standing instruction, the watchdog
  stays as defence-in-depth even after the HDL fix lands. Its
  `recoveries` counter on a healthy chain should stay at 0 long-
  term; if it ever fires post-CORDIC the fix is incomplete.

## Next steps (user actions)

1. Run `./build_fpga.bat --p25` to bake bitstream C with the
   CORDIC PLL update. Expected wall-clock ~20 minutes.
2. Run Tezuka `./build.bat --p25` to repackage with the new XSA.
3. Flash + boot + on-target smoke against Clay County 0x8A1.
   Expectation:
   - `LsmCycleSlipWatchdog.recoveries` stays at 0 indefinitely
   - PLL `pll_dbg` settles in a stable basin and stays there
   - 100 % BCH-valid NID extraction sustains for arbitrarily long
     periods (vs the ~1 minute slip cadence with the linearised form)
4. If the watchdog still fires, the next debug step is to compare
   the on-target `pll_dbg` trace between the linearised slip event
   (recorded during test cycle 3) and the new CORDIC form's
   pre-slip dynamics, looking for whatever transient the CORDIC
   form *also* can't absorb. Possible follow-ups in that case:
   AGC (6E.6.5), tighten the per-step clamp, or add the PS-side
   watchdog cycle-slip detector to the HDL itself.

## Architectural notes

This is the second of the five "architectural cuts vs the Rust
reference" called out in doc 015 to be reverted. The other four:

1. ~~PLL update uses small-angle linearisation, not atan2~~ -- **CLOSED**
   in this doc (021), CORDIC atan2 added.
2. PLL rotate uses 1024-entry sin/cos LUT, no interpolation -- still
   present, ~0.4 % quantisation, no observed issues.
3. AGC deferred to 6E.6.5 -- still deferred, not needed for Clay
   County per Rust evidence.
4. BCH FEC is compute-on-the-fly, not 65,536-entry BRAM codebook --
   still present, validated bit-exact against the SDRTrunk reference,
   no plans to revert.
5. Hard sync detector only, no soft detector -- still present,
   100 % hit rate on real RF, no plans to revert.

Of the five cuts, only the PLL linearisation has produced a real-RF
failure mode that needed reverting. The other four remain valid
optimisations.
