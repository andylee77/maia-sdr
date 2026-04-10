# 015 -- Phase 6E: LSM Demod HDL Port

**Date:** 2026-04-09
**Phase:** 6E (HDL/PL port of the validated Rust LSM pipeline)
**Branch:** fishball-p25
**Status:** In progress -- 6E.0 done, 6E.1+ pending

---

## Goal

Bring the entire LSM demod chain into PL fabric, alongside (not replacing)
the existing C4FM chain. Final result is a dual-mode P25 bitstream that
produces dibits for both modulations in hardware, with the PS doing only
post-dibit decode and dashboard.

The Rust port from Phase 6D
([doc 014](014_phase6d_lsm_rust_port.md)) is the reference. Each HDL
module is tested against bit-exact (within fixed-point tolerance) golden
vectors emitted from the validated Rust pipeline.

## Architectural decisions (locked 2026-04-09)

1. **All DSP in PL.** Filters, demod loop, *and* BCH(63,16,11) FEC end up
   in fabric. The PS retains only the dibit -> TSBK protocol decode and
   the dashboard.
2. **Dual-mode C4FM + LSM, both in PL.** The existing
   [`c4fm_demod`](../../maia-hdl/p25_hdl/c4fm_demod.py) +
   [`symbol_timing`](../../maia-hdl/p25_hdl/symbol_timing.py) chain stays
   untouched. The new LSM chain runs in parallel and produces dibits via
   its own ring DMA, mirroring the way Phase 6C added `iq_dma` alongside
   `dibit_dma`. Same pattern, zero risk to the existing path, direct A/B
   in the dashboard. The user only has LSM-modulated systems within RF
   range right now, but C4FM support stays for future targets.
3. **Bottom-up sub-phases.** Each new HDL module locks a fixed reference
   for the next via golden vectors emitted from the Rust pipeline.

## Sub-phase ladder

| Sub-phase | Block | Golden source | Status |
|---|---|---|---|
| 6E.0 | Golden vector emitter + Python loader | n/a | DONE |
| 6E.1 | Streaming /2 decimator HDL | `decimator_62k5_to_31k25.json` | DONE |
| 6E.2 | 83-tap LPF FIR HDL | `lpf_31250.json` | DONE |
| 6E.3 | 105-tap RRC matched filter HDL | `rrc_31250.json` | DONE |
| 6E.4 | Timing recovery + 4-way fractional-sample linear interp (AGC deferred) | Python lerp ref | DONE |
| 6E.5 | Differential demod + 4-PSK quadrant slicer (PLL rotate is identity at pll=0) | Quadrant truth table | DONE |
| 6E.6a | Gardner TED standalone | Python lerp ref | DONE |
| 6E.6b | PLL update (small-angle linearisation, no CORDIC) | Python lin ref | DONE |
| 6E.6c | PLL rotate (1024-entry sin/cos LUT, no interp) | Python lin ref | DONE |
| 6E.6d | `LsmDemodLoop` top-level integration | `demod_loop_synthetic.json` | DONE (100 %) |
| 6E.6.5 | AGC | (deferred to follow-up) | DEFERRED |
| 6E.7 | BCH(63,16,11) codebook BRAM + popcount tree | (TBD synthetic) | pending |
| 6E.8 | `LsmDemod` top-level Amaranth module | n/a | pending |
| 6E.9 | Wire LsmDemod into [p25_top.py](../../maia-hdl/p25_hdl/p25_top.py) alongside C4FM chain | n/a | pending |
| 6E.10 | Regen Verilog + bitstream + on-target validation | n/a | pending |

---

## 6E.0 -- Golden vector emitter (DONE)

The HDL tests need a fixed reference to compare against. Phase 6D's Rust
unit tests already validated the LSM pipeline against the Python
reference (which was in turn validated against SDRTrunk's truth log on
captured wavs), so the Rust pipeline *is* ground truth at this point. The
HDL tests therefore consume bit-exact-ish reference output emitted from
the same Rust code that's running on the Cortex-A9 in Phase 6D.

### Producer: `lsm::golden_dump` (test-only)

New file: [p25-httpd/src/lsm/golden_dump.rs](../../p25-httpd/src/lsm/golden_dump.rs).

`#[cfg(test)]` module wired in via
[lsm/mod.rs](../../p25-httpd/src/lsm/mod.rs). Lives inside the `lsm`
crate (not in `tests/`) because `p25-httpd` is a binary-only crate with
no `lib.rs` -- integration tests would need a lib refactor to import
`lsm::*`. An internal `#[cfg(test)]` module is the simplest path that
keeps the producer next to the consumer (a `git grep LPF_TAPS_31250`
finds both ends).

Four `#[test]` functions, one per stage, each always (re)writes its
golden file:

```text
emit_decimator_62k5_to_31k25 -> decimator_62k5_to_31k25.json
emit_lpf_31250               -> lpf_31250.json
emit_rrc_31250               -> rrc_31250.json
emit_demod_loop_synthetic    -> demod_loop_synthetic.json
```

Each test also `assert_eq!`s its streaming-API call against the batch
helper, so a regression in `StreamingFir` or `StreamingDecimator2` fails
the test before it writes the golden, instead of silently corrupting the
fixture.

Output directory is computed from `CARGO_MANIFEST_DIR`:
`p25-httpd/../maia-hdl/test/golden_vectors/`. The directory is created
on first run.

Run the producer (and refresh the goldens after a Rust algorithm
change) with:

```bash
cd p25-httpd
cargo test --bin p25-httpd lsm::golden_dump
```

### Synthetic input fixtures

- **Frequency sweep** (`frequency_sweep`) -- linear chirp from DC to
  Nyquist over the buffer. Used by the LPF and RRC tests so the
  passband and stopband are both exercised in one drive. Deterministic
  function of `n` and `sample_rate_hz`, no RNG.
- **`synth_lsm_iq`** -- crude P25 LSM transmitter: maps a known dibit
  sequence to ideal LSM phase deltas (Dibit.java angles `±π/4` and
  `±3π/4`), cumulative-sums to absolute phase, then linear-interpolates
  the phase to the output sample rate and emits `e^(j·phase)`. Used as
  the demod-loop test fixture. Linear interp instead of an actual
  transmit RRC because the receiver's RRC matched filter does the
  pulse shaping at test time -- the goal here is just a continuous IQ
  stream that gives the AGC, PLL and timing loops something to lock
  onto. Truth comparison happens at the dibit layer, not in the IQ
  domain.

### JSON file format

One file per stage. Common fields:

```json
{
  "name": "lpf_31250",
  "stage": "lsm::filters::StreamingFir(LPF_TAPS_31250)",
  "input_rate_hz": 31250,
  "output_rate_hz": 31250,
  "n_input": 2048,
  "n_output": 2048,
  "input_re":  [...f32 in scientific notation, length n_input...],
  "input_im":  [...],
  "output_re": [...length n_output...],
  "output_im": [...]
}
```

The demod-loop file extends this with `n_symbols`, `soft_re`,
`soft_im`, `soft_phase`, `hard_dibit`, `pll`, `sample_point`, and a
`truth_dibit` echo of the source dibit sequence so the HDL test can
compute BER against ground truth without re-deriving it.

f32 values are written via `{:.9e}` (9 significant digits, scientific
notation) -- the f32 round-trip minimum, which keeps the goldens
regen-stable across machines.

JSON instead of binary so the goldens are diff-friendly when reviewing
the impact of a Rust algorithm change in a PR.

### Consumer: `golden_vector_loader.py`

New helper: [maia-hdl/test/golden_vector_loader.py](../../maia-hdl/test/golden_vector_loader.py).

Tiny module with `load_iq_stage(name)` returning an `IQStage` dataclass,
`load_demod_stage(name)` returning a `DemodStage` dataclass, and a
`to_fixed(values, frac_bits, width=None)` helper for the HDL drive
side. Each amaranth-sim test imports the loader, picks its golden by
name, runs the HDL block, and compares.

Smoke test (run `python maia-hdl/test/golden_vector_loader.py`):

```text
decimator_62k5_to_31k25           in= 4096  out= 2048  rate 62500 -> 31250
lpf_31250                         in= 2048  out= 2048  rate 31250 -> 31250
rrc_31250                         in= 2048  out= 2048  rate 31250 -> 31250
demod_loop_synthetic              in= 1666  symbols=  254  sps=6.5104
```

### Files committed for 6E.0

| File | Purpose |
|---|---|
| [p25-httpd/src/lsm/golden_dump.rs](../../p25-httpd/src/lsm/golden_dump.rs) | Producer (Rust, test-only) |
| [p25-httpd/src/lsm/mod.rs](../../p25-httpd/src/lsm/mod.rs) | `mod golden_dump;` wiring |
| [maia-hdl/test/golden_vector_loader.py](../../maia-hdl/test/golden_vector_loader.py) | Consumer-side Python loader |
| [maia-hdl/test/golden_vectors/decimator_62k5_to_31k25.json](../../maia-hdl/test/golden_vectors/decimator_62k5_to_31k25.json) | 4096 → 2048 samples, 198 KB |
| [maia-hdl/test/golden_vectors/lpf_31250.json](../../maia-hdl/test/golden_vectors/lpf_31250.json) | 2048 in/out, 132 KB |
| [maia-hdl/test/golden_vectors/rrc_31250.json](../../maia-hdl/test/golden_vectors/rrc_31250.json) | 2048 in/out, 132 KB |
| [maia-hdl/test/golden_vectors/demod_loop_synthetic.json](../../maia-hdl/test/golden_vectors/demod_loop_synthetic.json) | 1666 IQ → 254 symbols, 73 KB |

Total: ~535 KB of golden vector data, all hand-rolled JSON, all
regen-stable across machines via the `{:.9e}` formatter.

## 6E.1 -- Streaming /2 decimator HDL (DONE)

First HDL block of the LSM chain, and the simplest. Direct Amaranth
port of `lsm::filters::StreamingDecimator2` from the Phase 6D Rust
reference. No DSP cost: one 16-bit re/im register pair plus a 1-bit
phase counter.

### Module: `LsmDecimator2`

New file: [maia-hdl/p25_hdl/lsm_decimator.py](../../maia-hdl/p25_hdl/lsm_decimator.py).

```text
inputs : re_in[16], im_in[16], strobe_in
outputs: re_out[16], im_out[16], strobe_out
state  : phase[1]   (init 0)
```

On the first `strobe_in` after reset, `phase==0` -> emit, latch
inputs into the registered outputs, assert `strobe_out` for one
cycle, advance `phase -> 1`. On the next strobe, `phase==1` -> drop
the sample, no output strobe, `phase -> 0`. Continues forever.

This is exactly the Rust default `StreamingDecimator2::new()
{ skip: 0 }` behaviour: emit `input[0::2]`, the same as the batch
helper `decimate_by_2`. The phase counter has no external load --
"phase preservation across chunks" in the Rust software view becomes
"the counter is a single register, never reset between drives" in
the HDL view.

### Why a separate block

Folding /2 decimation into the LPF stage would save zero LUTs but
entangle the rate change with the FIR scheduling. Keeping it
standalone draws a clean line between the 62.5 kSPS clock domain
(matches the existing C4FM chain on the same DDC output) and the
31.25 kSPS rate that drives the LPF, RRC, and demod loop. Same
philosophy as Phase 6C splitting `iq_packer` from `iq_dma`.

The naive (sample-dropping) /2 is safe here because the upstream
Maia DDC's stage-3 FIR is a 64-tap Kaiser LPF with 8 kHz passband
and 166+ dB stopband -- everything outside ±8 kHz at 62.5 kSPS is
already deep in the noise floor by the time it reaches this block,
so folding the upper half into 0..8 kHz at 31.25 kSPS adds no
measurable distortion to the 6.25 kHz P25 channel. The rationale
is the same as the Rust reference; see the comment block at the
top of [filters.rs](../../p25-httpd/src/lsm/filters.rs).

### Tests: `test_lsm_decimator.py`

New file: [maia-hdl/test/test_lsm_decimator.py](../../maia-hdl/test/test_lsm_decimator.py).

Four `unittest.TestCase` methods:

- `test_basic_alternating_pattern` -- feed `[0,1,2,3,...,19]`
  on both re and im, confirm we emit `[0,2,4,...,18]` (10 outputs).
  Smoke test that catches gross wiring errors before the golden test
  even runs.
- `test_no_strobe_no_output` -- assert no `strobe_out` while
  `strobe_in` is held low. Catches a regression where `strobe_out`
  is wired to a free-running counter instead of being gated by
  `strobe_in`.
- `test_first_sample_emitted` -- the polarity-of-phase test. Drive
  4 strobed inputs and assert the *first* and *third* are emitted,
  not the second and fourth. A swapped `phase` polarity would
  silently shift the entire downstream sample stream by one input
  sample; this test catches it before any of the FIR/demod tests
  start blaming a phase bug on a DC offset somewhere.
- `test_golden_vector_decimator_62k5_to_31k25` -- the integration
  test. Loads `decimator_62k5_to_31k25.json`, quantises both input
  and expected output to Q15 (16-bit signed) via
  `golden_vector_loader.to_fixed`, drives the HDL, asserts
  `len(hdl_out) == 2048` and that every output sample is bit-equal
  to the (quantised) Rust output. Bit-exact comparison rather than
  fixed-point tolerance is correct here because the block does no
  arithmetic -- a tolerance would mask a phase bug.

Result:

```text
test_basic_alternating_pattern ... ok
test_first_sample_emitted ... ok
test_golden_vector_decimator_62k5_to_31k25 ... ok
test_no_strobe_no_output ... ok
Ran 4 tests in 0.279s
OK
```

### One bug found and fixed in 6E.0

The frequency-sweep golden input lands exactly on `1.0 + 0j` for its
first sample, which is one ULP outside the Q15 representable range
(Q15 max ≈ 0.99997). The first run of the golden vector test failed
loudly with `OverflowError`. Fixed by adding a `saturate=True`
default to `to_fixed` in
[golden_vector_loader.py](../../maia-hdl/test/golden_vector_loader.py)
-- now an out-of-range value is clipped to the nearest endpoint
(standard fixed-point convention). The `saturate=False` mode is
still available for tests that want to assert all goldens fit
cleanly without rounding.

### Files committed for 6E.1

| File | Purpose |
|---|---|
| [maia-hdl/p25_hdl/lsm_decimator.py](../../maia-hdl/p25_hdl/lsm_decimator.py) | `LsmDecimator2` Amaranth module |
| [maia-hdl/test/test_lsm_decimator.py](../../maia-hdl/test/test_lsm_decimator.py) | Unit + golden-vector tests |
| [maia-hdl/test/golden_vector_loader.py](../../maia-hdl/test/golden_vector_loader.py) | `to_fixed` saturation fix |

## 6E.2 / 6E.3 -- LPF and RRC matched filter HDL (DONE)

Single generic real-coefficient complex FIR module
([LsmFir](../../maia-hdl/p25_hdl/lsm_fir.py)) instantiated twice:
once with the 83-tap baseband LPF taps for 6E.2, once with the
105-tap RRC matched filter taps for 6E.3. The Phase 6D Rust port
already validated that the LPF -> RRC chain reproduces SDRTrunk's
behaviour on captured wavs; this sub-phase verifies the HDL FIR
matches the Rust f32 reference within fixed-point quantisation
noise.

### Module: `LsmFir`

```python
LsmFir(
    taps,                # list[float] -- LPF_TAPS_31250 or RRC_TAPS_31250
    input_width=16,      # Q1.15 IQ in
    coeff_width=18,      # Q1.17 frozen taps (matches Maia DDC)
    output_width=16,     # Q1.15 IQ out, saturated
)
```

I/O matches `LsmDecimator2` (`re_in/im_in/strobe_in` ->
`re_out/im_out/strobe_out`) so the modules chain trivially.

#### Architecture

- **Sample buffers**: two length-N shift registers (one per channel),
  written via `Array` of `Signal` so Vivado infers SLR (shift-register
  LUT) chains. Storage cost: ~2 LUTs per tap per channel
  (~330 LUTs for the LPF, ~420 LUTs for the RRC).
- **Coefficient ROM**: a Python list quantised to signed
  `coeff_width` integers at construction time and embedded as an
  `Array` of `Const`. Vivado picks distributed RAM or LUT logic
  depending on size.
- **MAC**: one multiply-accumulate per cycle, sequenced by a small
  counter (`k`) and a `running` flag. Two parallel MAC chains
  share the coefficient lookup -- one for I, one for Q -- so the
  cost is **2 DSP48E1 per FIR**.
- **Output stage**: shift right by `input_frac + coeff_frac -
  output_frac` (= 17 bits for the default Q1.15/Q1.17/Q1.15
  configuration), saturate to signed `output_width` via inline
  comparator+mux, latch into the registered output, assert
  `strobe_out` for one cycle.

#### Throughput

Each output takes `N + 2` cycles (N MAC + 1 latch + 1 strobe).
For the 105-tap RRC at 62.5 MHz that is 107 cycles. The input rate
is 31.25 kSPS which corresponds to 2000 cycles per input sample,
giving 19x headroom. The block accepts back-to-back strobes from
upstream as long as they are >= `N + 2` cycles apart.

#### DSP / fixed-point budget

- **DSP**: 2 per FIR, 4 total for LPF + RRC. The LSM chain after
  6E.6 will sit at ~12 DSP, vs the Z7020's 220 -- still leaves the
  existing C4FM chain (~36 DSP for control + traffic) and any
  future expansion comfortable headroom.
- **Coefficient quantisation**: Q1.17 ULP = 7.6e-6, smaller than
  the smallest LPF tap (1.86e-4) by ~24x, so even the worst-case
  relative quantisation error on any single tap is well under
  5 %. Verified by `test_lpf_dc_gain_quantised`: the quantised
  integer-sum DC gain matches the float reference (0.9899) to
  three decimal places.
- **Accumulator**: `input + coeff + ceil(log2(N))` bits, rounded
  up to a multiple of 8 -- 48 bits for both LPF and RRC, matching
  DSP48E1's native accumulator width.

### Tests: `test_lsm_fir.py`

New file: [maia-hdl/test/test_lsm_fir.py](../../maia-hdl/test/test_lsm_fir.py).

Six tests:

- **`test_lpf_dc_gain_quantised`** -- sums the integer LPF taps,
  divides by `2^17`, asserts the result is 0.9899 ± 1e-3 (matches
  the f32 sum from `test_lpf_dc_gain_close_to_unity` in
  `lsm::filters::tests`). Catches a sign-flip or scale-factor
  regression in the quantiser before any HDL simulation runs.
- **`test_rrc_taps_quantised`** -- asserts the RRC has 105 taps
  and the center tap (index 52) is the maximum-magnitude
  coefficient. Same purpose as the LPF DC-gain test, scoped to
  the RRC's different tap envelope.
- **`test_lpf_impulse_response_matches_quantised_taps`** -- drives
  a Kronecker delta (`re=32767, im=0` then zeros) and asserts
  the first N output samples reproduce `(tap_int * 32767) >> 17`
  for each `k`, within 2 ULPs. The impulse response of any FIR
  is its tap array, so this is a self-consistency check that
  doesn't depend on the Rust reference at all.
- **`test_rrc_impulse_response_matches_quantised_taps`** -- same
  test on the RRC kernel. Catches a tap-quantisation bug specific
  to the RRC's larger-magnitude lobes.
- **`test_lpf_golden_vector_31250`** -- the integration test.
  Loads `lpf_31250.json` (2048 sweep samples), drives the HDL
  FIR, asserts every steady-state output sample (after the
  `n_taps`-sample transient region where partial-sum rounding can
  briefly exceed the steady-state tolerance) lies within 4 Q15
  ULPs of the Rust f32 reference. **Result: passes with zero
  failures across 1965 steady-state samples.**
- **`test_rrc_golden_vector_31250`** -- same integration shape on
  the 105-tap RRC. **Result: passes with zero failures across
  1943 steady-state samples.**

#### Tolerance choice

`TOLERANCE_ULPS = 4` (= 1.2e-4 absolute on a Q15 output) is the
absolute tolerance per sample. The fixed-point pipeline (Q1.15 in,
Q1.17 coeffs, Q1.15 out) lands well within 1 ULP of the f32
reference for typical inputs; 4 ULPs is comfortable safety while
still catching real algorithmic regressions.

The first `n_taps` samples are excluded from the comparison
because both the Rust `apply_real_fir_complex` and the HDL shift
register treat pre-input history as zero, but the difference
between `<short partial sum> @ f32` and `<short partial sum> @
Q15` can briefly exceed the steady-state tolerance during the
convolution transient. Steady-state output (after the transient)
is what matters for the receiver and is what the test checks.

### One bug found

The first cut of `test_lpf_impulse_response_matches_quantised_taps`
computed the expected output as `tap_int >> shift` instead of
`(tap_int * input) >> shift`, which gave `0` for tap 0 vs the
HDL's `5`. The test was wrong, not the module -- fixed by writing
out the full multiply-then-shift expression. Caught by the
`got=5 expected=0` failure on the very first run, which is
exactly the kind of off-by-orders-of-magnitude failure the
impulse-response test is designed to catch.

### Combined LSM front-end test result

Running the full LSM front-end test set
(`test.test_lsm_decimator + test.test_lsm_fir`):

```text
Ran 10 tests in 21.939s
OK
```

The 22-second runtime is dominated by the two 2048-sample golden-
vector tests (LPF ~9s, RRC ~12s). Both run pure-Python amaranth-sim
on Windows; this is well within the per-sub-phase test budget.

### Files committed for 6E.2 / 6E.3

| File | Purpose |
|---|---|
| [maia-hdl/p25_hdl/lsm_fir.py](../../maia-hdl/p25_hdl/lsm_fir.py) | `LsmFir` Amaranth module + frozen `LPF_TAPS_31250` and `RRC_TAPS_31250` |
| [maia-hdl/test/test_lsm_fir.py](../../maia-hdl/test/test_lsm_fir.py) | 6 tests covering both kernels |

## 6E.4 -- Timing recovery + 4-way fractional-sample interp (DONE)

The streaming-friendly prefix of the Rust LSM demod loop: sample
buffer + `sample_point` register + linear interp at the symbol
decision point. AGC, differential demod, PLL rotate, slicer,
Gardner TED, and PLL update are all deferred to 6E.5 / 6E.6 -- this
sub-phase produces *just* the four fractional-sample IQ values per
symbol decision and a strobe.

### Why split AGC out

The Rust loop interleaves AGC into the same per-symbol step:
`magnitude = sqrt(i² + q²)` -> `required_gain = 1 / magnitude` ->
slewed update of `sample_gain`. Both `sqrt` and `1/x` are
non-trivial in HDL (CORDIC vector mode + reciprocal LUT or
Newton-Raphson) and would expand 6E.4 well past one sub-phase. AGC
is also tightly coupled to the PLL/Gardner feedback (it scales
the same `i_mid/q_mid/i_cur/q_cur` that the differential demod
consumes), so the cleanest split is "all the loop-update math
together in 6E.6".

For 6E.4 the test driver simply skips the AGC step in its Python
reference, and the HDL outputs raw lerp results. The full pipeline
integration test against `demod_loop_synthetic.json` lands in 6E.6
once AGC, differential demod, PLL, and Gardner are all wired in.

### Streaming reframe of the Rust loop

The Rust code walks `bp` through a static buffer and looks
*forward* in the buffer for the half-symbol-ahead current sample
(at `bp + sample_point + half_sps`). A streaming HDL has only past
samples, so the trick is to delay all decisions by `half_sps + 2`
input samples and keep the recent samples in a small shift register
indexed *backwards* from the head.

Concretely: a depth-8 IQ FIFO where slot 0 is the newest sample
just shifted in, and slot `BP_INDEX = 5` is the "decision now"
sample (== Rust's `buf[bp]` after `bp += 1`). The current-symbol
lookahead at `sample_point + half_sps ≈ 3.25..4.25` samples ahead
of `bp` lands at FIFO index `BP - {3 or 4}` = `{2 or 1}`,
comfortably inside the depth-8 window. `BP - 5` = slot 0 = the
just-shifted-in sample, used as the "next" half of the second
possible offset's lerp.

Since the FIFO depth is 8 and each slot stores 32 bits (re || im),
the storage cost is 256 bits ≈ 16 LUTs.

### Module: `LsmTimingInterp`

New file: [maia-hdl/p25_hdl/lsm_timing_interp.py](../../maia-hdl/p25_hdl/lsm_timing_interp.py).

```python
LsmTimingInterp(
    iq_width=16,    # Q1.15 IQ in/out (matches RRC output)
    fifo_depth=8,   # >= ceil(half_sps) + 2
)
```

I/O:

```text
inputs : re_in[16], im_in[16], strobe_in
outputs: i_mid_out[16], q_mid_out[16],   <- midpoint sample
         i_cur_out[16], q_cur_out[16],   <- current-symbol sample
         decision_strobe                  <- one cycle per symbol
debug  : sample_point_dbg[16]            <- Q4.12 sample_point
```

### Fixed-point format

- **IQ**: signed 16-bit Q1.15 in/out, matches the RRC output.
- **`sample_point`**: signed 16-bit Q4.12. Range [-8, +8), ULP
  2.4e-4. The 4 integer bits cover the [0, sps≈6.51] range that
  `sample_point` lives in plus the [-2, +2] excursion the Gardner
  TED can introduce in 6E.6, with one bit of headroom. The 12
  fractional bits give sub-sample timing resolution well below the
  noise floor of any P25 system.
- **Constants**: `SPS_Q12 = 26667`, `HALF_SPS_Q12 = 13334`,
  `ONE_Q12 = 4096` -- pre-computed at import time from the
  `P25_LSM_SAMPLE_RATE_HZ / P25_SYMBOL_RATE_HZ` ratio.
- **Lerp internal**: `(b - a) * mu` keeps a 13-bit signed mu and
  produces a 29-bit signed product, then arithmetic-right-shifts
  by 12 to discard the mu fractional bits and adds back `a`.
  Final result saturates to signed 16-bit Q1.15.

### Lerp expansion strategy

The current-symbol lookahead has two possible FIFO offsets
(`cur_int = 3` or `cur_int = 4`, depending on whether `ptr =
sample_point + half_sps` is below or above 4.0). Rather than
running a single lerp through a runtime-variable index (which
would force Vivado to build a 4:1 mux on every DSP48E1 input
port), the module instantiates **both** possible lerps in
parallel and Mux'es the results. Each lerp's inputs are static
across the cycle, so the FIR-style packing works cleanly:

```text
i_cur_3 = lerp(fifo[BP-3], fifo[BP-4], cur_frac)   # always wired
i_cur_4 = lerp(fifo[BP-4], fifo[BP-5], cur_frac)   # always wired
i_cur   = (cur_int == 3) ? i_cur_3 : i_cur_4
```

Cost: 4 lerps per decision on the I rail (i_mid, q_mid, i_cur_3,
i_cur_4) and 4 more on Q, or 6 effective lerps if you fold the Mux
into the count. Vivado packs each lerp into 1 DSP48E1 -> ~6 DSP48
for this block. Plus 4 LPF/RRC DSPs from 6E.2 / 6E.3, the LSM
front-end + timing block sits at ~10 DSP, well under budget.

### Pre-shift vs post-shift FIFO

A subtle bit: the lerp must use the FIFO contents *before* the
new input sample is shifted in, because the Rust loop's `buf[bp]`
corresponds to the previously-arrived sample, not the one being
processed this cycle. The HDL gets this right "for free" because
`m.d.sync` assignments only take effect at the next clock edge --
the `for i in N..0: fifo[i] <- fifo[i-1]` shift is scheduled but
the lerp expressions in the *same* `with m.If(strobe_in)` block
read the *current* FIFO state, which is the pre-shift state. The
just-arrived sample is referenced via `fifo[0]` (the
slot-about-to-receive-it-on-the-next-edge) only by the
`i_cur_4` / `q_cur_4` lerps, where reading the pre-shift slot 0
gives the previously-newest-sample, which is exactly "1 input
ahead of bp" in the Rust loop's terms.

### Tests: `test_lsm_timing_interp.py`

New file: [maia-hdl/test/test_lsm_timing_interp.py](../../maia-hdl/test/test_lsm_timing_interp.py).

Four tests:

- **`test_constants`** -- assert `SPS_Q12 / 4096 ≈ 6.5104` and
  `HALF_SPS_Q12 / 4096 ≈ 3.2552`, within 1 ULP of the float
  reference. Catches a Q-format scaling regression before any
  HDL simulation runs.
- **`test_decision_rate`** -- drive 1000 input strobes, count
  `decision_strobe` pulses, assert the count matches `1000 /
  6.5104 ≈ 153.6` within ±2 (the ±2 absorbs the FIFO transient
  during the first few input strobes). This is the "is the
  symbol clock running at all" check.
- **`test_constant_input_returns_constant`** -- drive a DC
  signal, assert all four lerp outputs equal the input constant
  for every decision after the FIFO has filled. The lerp `a +
  (b - a) * mu` reduces to `a` when `a == b`, so any wiring
  regression on the lerp inputs or any mu polarity flip will
  fail this check loudly.
- **`test_lerp_against_python_reference_ramp`** -- the heavy
  test. Drives an integer ramp and runs the same input stream
  through a pure-Python lerp reference (`_python_reference()`)
  that mirrors the HDL's pre-shift FIFO + sample_point logic in
  floats. Asserts every decision-time HDL output is within 2 Q15
  ULPs of the float reference. Catches mu rounding bugs, FIFO
  index off-by-one errors, and the integer-vs-fractional split
  on the current-symbol pointer.

The Python reference is hand-rolled in the test (~50 lines)
rather than imported from the Rust pipeline because (a) the Rust
loop has AGC + PLL + Gardner mixed in that we can't yet match,
and (b) keeping the reference local makes the test self-contained
and lets us iterate on lerp formula details in one file.

Result:

```text
test_constant_input_returns_constant ... ok
test_constants ... ok
test_decision_rate ... ok
test_lerp_against_python_reference_ramp ... ok
Ran 4 tests in 0.449s
OK
```

### One test-tolerance bug found

The first run of `test_constants` failed at `places=4` (1e-4
absolute) because the Q4.12 ULP is 2.4e-4 and `SPS_Q12 = 26667`
rounds to 6.510498 vs the float 6.510417 -- a sub-ULP rounding
difference that places=4 incorrectly flags as wrong. Fixed by
loosening the test to `places=3` (1e-3 absolute), which is
comfortably above one Q12 ULP. The HDL itself was correct from
the first run.

### Combined LSM HDL test result through 6E.4

```text
test.test_lsm_decimator + test.test_lsm_fir + test.test_lsm_timing_interp
Ran 14 tests in 22.204s
OK
```

### Files committed for 6E.4

| File | Purpose |
|---|---|
| [maia-hdl/p25_hdl/lsm_timing_interp.py](../../maia-hdl/p25_hdl/lsm_timing_interp.py) | `LsmTimingInterp` Amaranth module |
| [maia-hdl/test/test_lsm_timing_interp.py](../../maia-hdl/test/test_lsm_timing_interp.py) | 4 tests + Python lerp reference |

## 6E.5 -- Differential demod + 4-PSK quadrant slicer (DONE)

The first algorithmic block of the LSM demod loop proper. Streaming
Amaranth port of `p25-httpd/src/lsm/demod.rs` lines 233-283
(differential demod section + slicer), with the PLL rotation step
hard-coded to identity (`cos(0)=1`, `sin(0)=0`) for this sub-phase.
The PLL update + sin/cos generator land in 6E.6 alongside Gardner
TED and AGC, where they share the same loop-update timing.

### Module: `LsmDiffDemodSlicer`

New file: [maia-hdl/p25_hdl/lsm_diff_demod_slicer.py](../../maia-hdl/p25_hdl/lsm_diff_demod_slicer.py).

```python
LsmDiffDemodSlicer(
    iq_width=16,    # Q1.15 IQ in (matches LsmTimingInterp output)
    demod_width=18, # Q3.15 soft demod out (matches C4FMDemod)
)
```

I/O:

```text
inputs : i_mid_in[16], q_mid_in[16],
         i_cur_in[16], q_cur_in[16],
         decision_strobe
outputs: i_mid_demod_out[18], q_mid_demod_out[18],   <- midpoint diff demod
         i_sym_out[18],       q_sym_out[18],          <- symbol diff demod
         dibit_out[2],
         symbol_strobe
state  : prev_middle_i/q[16], prev_current_i/q[16]    (init 0)
```

### Algorithm

For each `decision_strobe` pulse from `LsmTimingInterp` in 6E.4,
compute `z_curr * conj(z_prev)` for both the midpoint sample
and the current-symbol sample:

```text
i_mid_demod = i_mid * prev_mid_i + q_mid * prev_mid_q
q_mid_demod = q_mid * prev_mid_i - i_mid * prev_mid_q
i_sym       = i_cur * prev_cur_i + q_cur * prev_cur_q
q_sym       = q_cur * prev_cur_i - i_cur * prev_cur_q
```

Then update the prev state and slice `(i_sym, q_sym)` into a 4-PSK
dibit by quadrant:

```text
i>=0, q>=0  ->  Q1, +1 phase, dibit 00
i< 0, q>=0  ->  Q2, +3 phase, dibit 01
i>=0, q< 0  ->  Q4, -1 phase, dibit 10
i< 0, q< 0  ->  Q3, -3 phase, dibit 11
```

This collapses to `dibit = Cat(i_sign, q_sign)` -- LSB = sign of
`i_sym`, MSB = sign of `q_sym`. The same `Cat(re_sign, im_sign)`
pattern the existing `SymbolTimingRecovery` slicer uses for C4FM,
and the same mapping as `Dibit.toDibit()` in SDRTrunk and
`to_dibit()` in [demod.rs](../../p25-httpd/src/lsm/demod.rs).

The sign bits are taken from the 33-bit *full* product, not the
truncated 18-bit demod output. Truncation can only zero out the
magnitude, never flip the sign, so this saves one round-trip
through the arithmetic shift without changing the result.

### Sign convention vs `C4FMDemod`

The Rust formula `prev_curr_i * i_cur + prev_curr_q * q_cur` (with
`prev` on the left of the multiplication) initially looks like
`z_prev * conj(z_curr)`, which is the *conjugate* of what
`C4FMDemod` computes (`z_curr * conj(z_prev)`). Expanding the
algebra confirms it's actually the same expression with the terms
reordered:

```text
prev_i*cur_i + prev_q*cur_q  ==  cur_i*prev_i + cur_q*prev_q   (real part: identical)
prev_i*cur_q - prev_q*cur_i  ==  cur_q*prev_i - cur_i*prev_q   (imag part: identical)
```

So both blocks compute `z_curr * conj(z_prev)` and the slicer
mappings line up. The Rust naming convention with `prev` on the
left is the only difference.

### Pipeline

Two registered stages from `decision_strobe` in to `symbol_strobe`
out:

- **Stage 1** (1 cycle): per-decision multiply + sum, latched into
  the four 33-bit `*_full` registers. Vivado packs the 8
  multiplies into 8 DSP48E1s with 1-cycle internal pipeline. The
  prev-state update runs in parallel with the multiplies in the
  same `m.d.sync` block; the multiplies read the *pre-update*
  prev values because Amaranth `.eq()` assignments only take
  effect at the next clock edge.
- **Stage 2** (1 cycle): truncate the 33-bit sums to `demod_width`
  via arithmetic right shift, extract sign bits for the dibit
  slicer, latch outputs and `symbol_strobe`.

Total latency: **2 sync cycles** from `decision_strobe` to
`symbol_strobe`. With `LsmTimingInterp` firing decisions roughly
every 6-7 input strobes (≈ 12000 cycles at 62.5 MHz), this is
trivially within the per-symbol budget.

### DSP48E1 cost

8 DSPs (4 multiplies × 2 sample sets). The PLL rotation in 6E.6
will add another 8, so the diff-demod / PLL section ends up around
16 DSP48 in the final design. Combined with the front end
(4 LPF/RRC + 6 timing/lerp + 8 diff demod), the LSM chain through
6E.5 sits at **18 DSP48** -- still well within the Z7020's
220-DSP budget even with the existing C4FM chain present.

### Tests: `test_lsm_diff_demod_slicer.py`

New file: [maia-hdl/test/test_lsm_diff_demod_slicer.py](../../maia-hdl/test/test_lsm_diff_demod_slicer.py).

Seven tests, no Rust reference needed -- the dibit truth table is
fixed by the LSM constellation and the test driver generates
known-rotation IQ streams in Python.

- **`test_dibit_for_constant_phase`** -- a stream of identical
  IQ samples produces `dibit=00` after the first decision (because
  `prev_curr` matches `curr` exactly, so the diff demod produces
  `(|z|², 0)` which slices to Q1 -> 00). Catches a slicer wiring
  bug that confuses `i_sym_full[-1]` with `q_sym_full[-1]`.
- **`test_dibit_for_plus_pi_4`** -> `0b00`
- **`test_dibit_for_plus_3pi_4`** -> `0b01`
- **`test_dibit_for_minus_pi_4`** -> `0b10`
- **`test_dibit_for_minus_3pi_4`** -> `0b11`

  Four single-quadrant tests, one per LSM symbol. Each drives the
  module with `n=20` consecutive samples on the unit circle
  advancing by the named phase delta per symbol, and asserts the
  steady-state dibit matches the expected quadrant. Together they
  exhaustively cover the 4-element slicer truth table.

- **`test_dibit_sequence_walks_all_quadrants`** -- the integration
  test on this block. Builds a known-good test pattern by stepping
  through all four LSM dibits in a fixed cycle (`[00, 01, 10, 11]`
  repeated 6 times for 24 dibits total), encoding each as the
  corresponding +/-pi/4 or +/-3pi/4 phase delta on a continuous
  unit-circle phase trajectory, and feeding the resulting samples
  into the slicer. The output dibit sequence (after the warmup
  transient) must match the input dibit sequence one-to-one. This
  is the closest thing to "decode a real LSM signal" we can do
  without the rest of the pipeline.

- **`test_demod_outputs_have_expected_sign`** -- sanity check on
  the truncated soft outputs (`i_sym_out`, `q_sym_out`). For a
  +pi/4 rotation stream, both should be positive and roughly
  equal (cos(pi/4) ≈ sin(pi/4)). Catches a soft-output truncation
  bug that the slicer test (which uses the full 33-bit product)
  would miss.

Result:

```text
test_demod_outputs_have_expected_sign ... ok
test_dibit_for_constant_phase ... ok
test_dibit_for_minus_3pi_4 ... ok
test_dibit_for_minus_pi_4 ... ok
test_dibit_for_plus_3pi_4 ... ok
test_dibit_for_plus_pi_4 ... ok
test_dibit_sequence_walks_all_quadrants ... ok
Ran 7 tests in 0.112s
OK
```

### Combined LSM HDL test result through 6E.5

```text
test.test_lsm_decimator + test.test_lsm_fir + test.test_lsm_timing_interp + test.test_lsm_diff_demod_slicer
Ran 21 tests in 22.392s
OK
```

The diff-demod tests are nearly free at 0.1 s -- they only run
~120 cycles each because the decision rate is driven directly by
the test, not derived from a long input sample stream. Total
runtime is still dominated by the 2048-sample LPF and RRC golden
vector tests.

### Files committed for 6E.5

| File | Purpose |
|---|---|
| [maia-hdl/p25_hdl/lsm_diff_demod_slicer.py](../../maia-hdl/p25_hdl/lsm_diff_demod_slicer.py) | `LsmDiffDemodSlicer` Amaranth module |
| [maia-hdl/test/test_lsm_diff_demod_slicer.py](../../maia-hdl/test/test_lsm_diff_demod_slicer.py) | 7 tests covering all four LSM quadrants + soft outputs |

## 6E.6 -- Closed-loop demod (Gardner + PLL update + PLL rotate + integration) (DONE)

The biggest sub-phase of 6E and the only one that touches a closed
control loop. Four sub-blocks (`6E.6a..d`) build up the complete
demod loop and connect it back to `LsmTimingInterp` via the
sample_point feedback path.

### Architectural cuts vs the Rust reference

Three places where the HDL diverges from a literal port of the
Rust loop, all to keep 6E.6 tractable in fabric:

1. **PLL update uses small-angle linearisation, not atan2.** The
   Rust loop computes `phase_error = atan2(q_sym, i_sym) -
   dibit_phase(h)`. atan2 in HDL would need a CORDIC vector mode
   (~16 cycles, ~3 DSP). The small-angle approximation
   `phase_error ~= sin(phase_error) = q*cos(dibit_phase) -
   i*sin(dibit_phase)` collapses to `+/-(q +/- i)` selected by
   dibit -- two ALUs and a 4-way mux, no CORDIC. Linearisation
   error is ~1.5 % at the +/- 0.3 rad clamp boundary, absorbed by
   the integrator.
2. **PLL rotate uses a sin/cos LUT, not CORDIC rotation.**
   1024-entry LUT (1 BRAM18, 32 Kbit) covering pll values in
   [-2, 2] rad. No interpolation -- the worst-case sin/cos
   quantisation error is ~0.4 % which is well inside the LSM
   loop's tolerance budget.
3. **AGC is deferred to a future sub-phase (`6E.6.5`).** The Rust
   loop computes `1 / sqrt(i^2 + q^2)` per symbol to normalise the
   diff demod magnitude before feeding it into the slicer + PLL +
   Gardner. Both `sqrt` and `1/x` are non-trivial in HDL. For the
   synthetic integration test we **pre-scale the input by 0.34**
   in software to match what the AGC would converge to, and the
   loop runs without it.
   On real hardware the AGC would be needed for robustness to
   variable RF input level. Adding it is a clean follow-up:
   slot a CORDIC vector + reciprocal LUT between
   `LsmTimingInterp` and `LsmDiffDemodSlicer`.

These are documented in the module headers so any future port of
the missing pieces has the rationale to point at.

## 6E.6a -- Gardner TED HDL (DONE)

Streaming Amaranth port of the Gardner timing-error block in
[demod.rs](../../p25-httpd/src/lsm/demod.rs#L254-L263):

```text
timing_adj = (prev_sym_i - i_sym) * i_mid_demod
           + (prev_sym_q - q_sym) * q_mid_demod
clamp +/- (sps/25)            ; ~ +/- 0.26
timing_adj *= ted_gain        ; sps/4 ~ 1.63
sample_point += timing_adj
prev_sym <- (i_sym, q_sym)
```

### Module: `LsmGardnerTed`

New file: [maia-hdl/p25_hdl/lsm_gardner_ted.py](../../maia-hdl/p25_hdl/lsm_gardner_ted.py).

### Why use the demodulated symbols not the raw IQ

The Gardner formulation above is invariant to a constant complex
rotation -- the dot product cancels the rotation. This decouples
the timing loop from the carrier loop, which is what makes joint
timing+carrier recovery converge cleanly. SDRTrunk and the Rust
reference both do it this way; we follow.

### Gardner TED fixed-point format

- Inputs: signed 18-bit Q3.15 (matches `LsmDiffDemodSlicer.i_sym_out`).
- Internal sum-of-products: signed 38-bit Q8.30 (= 2 * 18 + 1 +
  rounded up).
- Clamp limit: `MAX_TIMING_ADJ_Q15 ≈ 8519` (= 0.26 * 32768).
- Loop gain: `TED_GAIN_Q16 ≈ 106721` (= 1.628 * 65536, Q1.16 to
  fit the 1.63 value with one sign bit).
- Output `timing_adj_out`: signed 16-bit Q4.12 -- matches the
  `LsmTimingInterp.sample_point` format so the feedback can be
  added directly.

### Gardner TED DSP / pipeline

3 DSP48E1 (2 for the dot product, 1 for the loop-gain multiply).
3-cycle pipeline from `symbol_strobe` to `timing_adj_strobe`.

### Gardner TED tests

[maia-hdl/test/test_lsm_gardner_ted.py](../../maia-hdl/test/test_lsm_gardner_ted.py).
4 tests:

- **`test_constants`** -- Q-format quantisation sanity check.
- **`test_zero_input_no_adjustment`** -- after a few cycles of
  zero input, prev_sym becomes 0 and the dot product is zero.
- **`test_constant_phase_input_zero_adjustment`** -- with the
  same (i, q) repeated, prev_sym matches sym so the (prev - sym)
  factor is 0 -> output is 0.
- **`test_against_python_reference`** -- 32 deterministic
  pseudo-random inputs through the float-reference Python
  Gardner, HDL output within 6 Q12 ULPs.

Result: 4/4 pass.

## 6E.6b -- PLL update HDL (DONE)

Decision-directed PLL update via small-angle linearisation. New
file: [maia-hdl/p25_hdl/lsm_pll_update.py](../../maia-hdl/p25_hdl/lsm_pll_update.py).

### The linearisation trick

For each dibit, the linearised phase-error proxy collapses to a
4-way mux on `(q +/- i)`:

```text
dibit 00: raw =  q - i
dibit 01: raw = -(q + i)
dibit 10: raw =  q + i
dibit 11: raw =  i - q
```

Then clamp at `+/- 0.4243` (= 0.3 / (sqrt(2)/2)), multiply by the
combined gain `(sqrt(2)/2) * PLL_GAIN ≈ 0.0707`, subtract from
the pll integrator, and clamp the integrator at `+/- pi/3`.

### PLL update fixed-point format

- pll register: signed 16-bit Q2.13 (range +/- 4 rad, ULP 1.2e-4).
- Combined gain: Q1.16 ~= 4634.
- Clamp limit Q15: ~= 13903.
- pi/3 in Q13: ~= 8580.

### Bug found and fixed: round-half bias

First run of the integration test against a Python reference
showed accumulated drift in the integrator. Root cause: the
arithmetic right shift `(product >> step_shift)` floors toward
negative infinity for signed values, which biases negative
results downward. Fixed by adding a half-ULP bias before the
shift -- the standard "round half up" convention -- which is
unbiased for symmetric input distributions like the one this
loop sees.

### PLL update DSP / pipeline

1 DSP48E1 (the gain multiply). 3-cycle pipeline.

### PLL update tests

[maia-hdl/test/test_lsm_pll_update.py](../../maia-hdl/test/test_lsm_pll_update.py).
5 tests:

- `test_constants` -- Q-format check.
- `test_zero_input_no_change` -- (0, 0) input -> pll stays 0.
- `test_each_dibit_drives_correct_sign` -- one test per dibit
  verifying the 4-way mux drives pll in the right direction.
- `test_pll_clamps_at_pi_over_3` -- saturate the input and let
  the loop integrate, assert pll clamps at +/- pi/3.
- `test_against_python_reference` -- 64 deterministic
  pseudo-random inputs, HDL pll trace within 16 Q13 ULPs of the
  float-reference Python PLL.

Result: 5/5 pass.

## 6E.6c -- PLL rotate HDL (DONE)

Single complex rotation by an angle from the PLL register. New
file: [maia-hdl/p25_hdl/lsm_pll_rotate.py](../../maia-hdl/p25_hdl/lsm_pll_rotate.py).

### Sin/cos LUT

1024 entries, packed as `(sin << 16) | cos` 32-bit words, both
halves Q1.15 signed. Address bits = `((pll + 16384) >> 5) & 0x3FF`,
where 16384 is the centre offset (so pll == 0 lands at index 512)
and 32 is the per-entry step in Q2.13 units. The LUT covers pll
values in [-2, 2) rad; with the PLL clamp at +/- pi/3 the actual
indices used are roughly 244..780, leaving headroom for any
future widening.

LUT pre-computation in Python at module construction time
(`_build_sin_cos_lut`) -- the resulting list is passed to
`amaranth.lib.memory.Memory` as `init=...`, which Vivado infers
as a single BRAM18 with synchronous-read port.

### PLL rotate pipeline

3 cycles from `strobe_in` to `strobe_out`:

- stage 1: latch (i, q, address)
- stage 2: LUT data registered out of the synchronous-read port
- stage 3: 4 multiplies + 2 sums, round-half-up shift right by
  15, saturate to 18-bit signed, latch outputs

4 DSP48E1 per rotation. The integrated demod loop instantiates
two rotate blocks (one for the midpoint, one for the symbol
sample), so 8 DSP for the rotation step.

### PLL rotate tests

[maia-hdl/test/test_lsm_pll_rotate.py](../../maia-hdl/test/test_lsm_pll_rotate.py).
3 tests:

- `test_rotation_by_zero_is_identity` -- pll == 0 should preserve
  the input within 2 ULPs (the LUT entry for cos(0) is 32767 not
  exactly 32768, costing one ULP).
- `test_rotation_against_python_reference` -- 40 (i, q, pll)
  triples spanning +/- pi/3, HDL output within 200 Q15 ULPs of
  the float reference. The wider tolerance vs the other tests
  reflects the LUT's no-interpolation quantisation budget.
- `test_rotation_preserves_magnitude` -- rotation is unitary, so
  |output|^2 should equal |input|^2 within 1 % (the LUT
  precision).

Result: 3/3 pass.

## 6E.6d -- LsmDemodLoop top-level integration (DONE, 100 % accuracy)

[maia-hdl/p25_hdl/lsm_demod_loop.py](../../maia-hdl/p25_hdl/lsm_demod_loop.py).
Wires the front end (`LsmTimingInterp` from 6E.4 + `LsmDiffDemodSlicer`
from 6E.5) and the loop blocks (Gardner from 6E.6a + PLL update
from 6E.6b + two rotates from 6E.6c) into a closed-loop demod
that produces dibits from post-RRC IQ.

### LsmDemodLoop dataflow

```text
re/im 31.25kSPS  ->  LsmTimingInterp  ->  LsmDiffDemodSlicer  ->  rotate_mid
                          ^                    |                       |
                          |                    +-----> rotate_sym ----> dibit
                          |                                |
                          |                            slicer
                          |                                |
                          +<-- LsmGardnerTed <- (rot mid + rot sym + sliced dibit)
                                                  |
                                                  v
                                          LsmPllUpdate  --> pll
```

Two rotate blocks (one for `(i_mid_demod, q_mid_demod)`, one for
`(i_sym, q_sym)`) so the dataflow stays simple. Time-multiplexing
one rotate over two cycles would save ~32 Kbit (one BRAM18) at
the cost of a scheduler -- not worth it given headroom.

### Sample-point feedback into LsmTimingInterp

`LsmTimingInterp` got two new inputs in 6E.6d:
`timing_adj_in[16]` and `timing_adj_strobe_in`. When the strobe
fires, `timing_adj_in` is added to `sample_point`. The strobe is
gated against `strobe_in` so an input strobe and a Gardner update
in the same cycle don't fight (the input strobe wins; the
Gardner update is dropped, which is fine because Gardner fires
~250 us after each symbol decision and the sample stream is at
31.25 kSPS).

### Three bugs found during 6E.6d integration

The 6E.6a..c standalone tests all passed, but the integration
test failed at first with 25 % match rate. Three bugs in cascade
were responsible.

#### Bug 1: `sample_point` warmup offset wraps signed 16-bit

Original `LsmTimingInterp.sample_point` was `signed(16)` with
init `SPS_Q12 ≈ 26667`. To make the streaming HDL match Rust's
pre-loaded-buffer behaviour, the first decision needs to fire
~7 strobes later than the natural cadence so the lookahead FIFO
has enough samples. That makes the init `SPS_Q12 + 7 * ONE_Q12 =
55339`, which **overflows signed 16-bit** and silently wraps to
`-10197` -- the very first strobe fires a decision with garbage
FIFO contents.

Fix: widen `sample_point` and `sp_dec` to `signed(18)` (Q5.12,
range +/- 32). The 18-bit register fits the warmup-offset init
comfortably and gives headroom for future BP_INDEX changes.

#### Bug 2: Streaming FIFO indexing was off by ~5 strobes

`LsmTimingInterp` reads the lookahead FIFO **pre-shift** (the
shift schedules into the next clock edge). For the lerp at
`fifo[BP=5]` to equal the same sample Rust uses (`buf[6]` at
its first decision), we need the first decision to fire after
`bp_first + BP_INDEX + 2 = 13` HDL strobes -- i.e., 7 strobes
after the natural sps cadence (which would fire at strobe 6).

The 6E.4 standalone tests passed despite this because the Python
reference there was hand-rolled to mirror the HDL FIFO logic
exactly (same off-by-7), so HDL and Python agreed with each
other but neither matched Rust.

Fix: bump the `sample_point` init by `(BP_INDEX + 2) * ONE_Q12`
(combined with the width fix above so the larger init doesn't
overflow). Updated the 6E.4 Python reference to match. The 4
existing 6E.4 tests still pass after the change.

#### Bug 3: Q1.15 input range vs post-RRC magnitude

The post-RRC IQ in `demod_loop_synthetic.json` has magnitude
~2.4 due to the LPF + RRC gain (the Rust pipeline runs in f32
with no representation limit). HDL inputs are Q1.15 (range
+/- 1). Without an AGC stage in 6E.6, `to_fixed(saturate=True)`
clipped every sample to +/- 32767 -- the diff demod saw a
hard-limited square wave instead of a clean sinusoid.

Fix: pre-scale the test input by 0.34 (matching what an AGC
would converge to: target_magnitude / observed_magnitude ≈
1.0 / 2.94). Documented in the test as the expected workaround
until 6E.6.5 lands AGC.

### Integration test result

```text
[demod_loop] 239/239 dibits match (100.0 %)
[demod_loop]   dibit 00: 60/60 (100.0 %)
[demod_loop]   dibit 01: 59/59 (100.0 %)
[demod_loop]   dibit 10: 60/60 (100.0 %)
[demod_loop]   dibit 11: 60/60 (100.0 %)
[demod_loop]   final pll = -195 (-0.024 rad)
[demod_loop]   HDL vs Rust hard_dibit: 238/239 (100.0 %)
```

The HDL demod loop decodes the synthetic LSM signal at the same
accuracy as the Rust reference. PLL converges to ~0 (steady-state
-0.024 rad). Test threshold is set to 95 % to give a 5 % cushion
against future small numeric drift.

### Combined LSM HDL test result through 6E.6

```text
test.test_lsm_decimator + test.test_lsm_fir + test.test_lsm_timing_interp
+ test.test_lsm_diff_demod_slicer + test.test_lsm_gardner_ted
+ test.test_lsm_pll_update + test.test_lsm_pll_rotate + test.test_lsm_demod_loop
Ran 34 tests in 23.578s
OK
```

### DSP / BRAM budget through 6E.6

| Block | DSP48E1 | BRAM18 | Notes |
|---|---|---|---|
| `LsmDecimator2` | 0 | 0 | one register pair |
| `LsmFir` (LPF, 83 taps) | 2 | 0 | distributed RAM for shift register |
| `LsmFir` (RRC, 105 taps) | 2 | 0 | distributed RAM |
| `LsmTimingInterp` | ~6 | 0 | 4 lerps + 2 cur-offset Mux variants |
| `LsmDiffDemodSlicer` | 8 | 0 | 4 mults x 2 (mid + sym) |
| `LsmPllRotate` x 2 | 8 | 2 | 4 mults each, 1 BRAM each |
| `LsmGardnerTed` | 3 | 0 | 2 dot-product + 1 gain |
| `LsmPllUpdate` | 1 | 0 | gain mult |
| **Total** | **~30** | **2** | full LSM chain (no AGC, no BCH FEC) |

DSP budget: Z7020 has 220. ~14 % usage for LSM, leaves
plenty of room for the existing C4FM chain (~36 DSP for control +
traffic) and BCH FEC in 6E.7.

BRAM budget: Z7020 has 140 BRAM18. ~1.4 % usage so far. BCH FEC
in 6E.7 will be the big BRAM consumer.

### Files committed for 6E.6

| File | Purpose |
|---|---|
| [maia-hdl/p25_hdl/lsm_gardner_ted.py](../../maia-hdl/p25_hdl/lsm_gardner_ted.py) | `LsmGardnerTed` |
| [maia-hdl/p25_hdl/lsm_pll_update.py](../../maia-hdl/p25_hdl/lsm_pll_update.py) | `LsmPllUpdate` (small-angle linearisation) |
| [maia-hdl/p25_hdl/lsm_pll_rotate.py](../../maia-hdl/p25_hdl/lsm_pll_rotate.py) | `LsmPllRotate` (sin/cos LUT) |
| [maia-hdl/p25_hdl/lsm_demod_loop.py](../../maia-hdl/p25_hdl/lsm_demod_loop.py) | `LsmDemodLoop` top-level wrapper |
| [maia-hdl/p25_hdl/lsm_timing_interp.py](../../maia-hdl/p25_hdl/lsm_timing_interp.py) | `timing_adj_in` input + 18-bit width fix |
| [maia-hdl/test/test_lsm_gardner_ted.py](../../maia-hdl/test/test_lsm_gardner_ted.py) | 4 tests |
| [maia-hdl/test/test_lsm_pll_update.py](../../maia-hdl/test/test_lsm_pll_update.py) | 5 tests |
| [maia-hdl/test/test_lsm_pll_rotate.py](../../maia-hdl/test/test_lsm_pll_rotate.py) | 3 tests |
| [maia-hdl/test/test_lsm_demod_loop.py](../../maia-hdl/test/test_lsm_demod_loop.py) | 1 integration test (100 % match) |

### Deferred to follow-up: AGC

The Rust loop's AGC normalises the diff demod magnitude before
the PLL update + Gardner can use the linearised formulas
correctly. Without it, the integration test had to pre-scale the
input by hand. On real hardware the input level varies with RF
conditions, so an AGC will be needed for robustness.

A clean follow-up sub-phase (`6E.6.5`) would add:

1. CORDIC vector mode block to compute `mag = sqrt(i^2 + q^2)`
   from `(i_cur, q_cur)`. ~16 iterations, 1 DSP, ~16 cycles.
2. Reciprocal LUT or Newton-Raphson divider to compute
   `1 / mag`. 256-entry LUT covering [1/500, 1] = 1 BRAM.
3. Slewed update of `sample_gain`, clamped at 500.
4. Multiply all four `(mid, cur)` IQ values by `sample_gain`.

Total cost: ~6 DSP + 1-2 BRAM. Slot it between
`LsmTimingInterp` and `LsmDiffDemodSlicer` -- the rest of the
loop doesn't change.

### What 6E.0 does NOT do

- **No HDL.** No new amaranth modules yet. 6E.0 only puts the test
  scaffolding in place; 6E.1 starts the actual HDL port.
- **No live-RF golden.** All four fixtures are synthetic. A captured-IQ
  fixture from the Fishball iq_dma ring is queued for after 6E.6 (the
  full demod chain) so the HDL test can be driven by the same data the
  Rust port is currently running on.
- **No BCH FEC golden.** The 6E.7 (BCH codebook + popcount) golden will
  be added when that sub-phase starts -- the format will likely be
  different (data word + corrupted codeword + expected decode), so it
  doesn't fit the IQ-stage emitter shape.
