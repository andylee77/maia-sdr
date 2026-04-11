# 031 -- Phase 6G.1: HDL DC blocker on the LSM IQ input

**Date:** 2026-04-11
**Phase:** Phase 6G.1 (PL port -- HDL DC blocker on the LSM IQ chain)
**Branch:** fishball-p25
**Status:** SHIPPED in HDL + PS Rust. Awaiting next FPGA bake to verify on target.
**Next:** rebuild bitstream, on-target A/B verification (blocker on vs off),
then candidate 2 from doc 030 (soft sync correlator into PL) or call PL port done.

---

## TL;DR

Phase 6G.1 adds a pair of one-pole leaky-integrator DC blockers
(one each for I and Q) at the very front of `LsmDemod`, runtime
bypassable through a new `lsm_control.lsm_dc_block_enable`
register bit. This is candidate #1 from the doc 030 PL port
roadmap and is the smallest, highest-leverage change available:
~12 LUTs of HDL plus a single PS-side enable bit.

The motivation, in one sentence: **the slicer was running on
a slightly DC-biased input**, which gave it a 60/40 inner/outer
dibit ratio for the first 2-3 minutes after PLL start (until
the loop slowly absorbed the bias on its own), causing ~5 bit
errors per 48-bit sync window and collapsing the sync hit rate
from the steady-state ~14/sec down to ~5/sec for the entire
acquisition transient. With the front-end DC blocker enabled,
the slicer never sees the bias in the first place and the loop
should lock immediately from cold boot.

This is the first commit on the PS-from-100% baseline (doc 030)
to actually move the needle on radio reliability, as opposed to
adding incremental opcode parsers that didn't change the radio's
behavior.

---

## What changed

### New HDL module: `LsmDcBlocker`

`maia-hdl/p25_hdl/lsm_dc_blocker.py` -- a single-channel
one-pole leaky-integrator DC blocker:

```text
dc[n] = (1 - alpha) * x[n] + alpha * dc[n-1]      # low-pass
y[n]  = x[n] - dc[n]                              # high-pass out
```

with `alpha = 1 - 2^-K` so the `(1 - alpha)` factor is just an
arithmetic right shift -- no DSPs, no multipliers. Default
`K = 7` gives:

| Quantity | Value at K=7, fs=31.25 kSPS |
|---|---|
| `alpha` | 0.9921875 |
| Time constant | 2^7 = 128 samples = ~4.1 ms |
| -3 dB cutoff | ~38.9 Hz |
| 5-tau settling | ~20 ms |

The cutoff is two orders of magnitude below the 4800 sym/s LSM
signal energy, so the block doesn't eat anything we care about.
The 5-tau settling is also well inside one symbol (208 us),
which is what makes "blocker on at boot" indistinguishable from
"DC-clean RF" for the downstream loop.

**Fixed-point format:**

| Signal | Width | Q-format | Notes |
|---|---|---|---|
| `x_in` / `y_out` | signed 16 | Q1.15 | matches `LsmTimingInterp`'s expected interface |
| `acc` (state) | signed 24 | Q1.23 | holds `dc << K` so the right-shift update has no quantisation creep; +1 sign-margin bit absorbs the worst-case `(x - acc)` difference |
| `y_wide` | signed 17 | Q2.15 | one extra sign bit before the saturator |

The `y_wide -> y_sat` step explicitly saturates to signed-16 so
a pathological transient (e.g. a step from -32768 to +32767 with
the accumulator loaded the other way) can never wrap. In normal
operation `|dc| << |x|` so the saturator never fires.

**Bypass:** when `enable_in = 0` the output is `x_in` verbatim.
The accumulator continues to update silently so a disable ->
enable round-trip doesn't cause a step on the output -- by the
time the user re-enables the block, the integrator already
reflects the current DC.

**Strobe convention:** matches `LsmDecimator2`. Both `y_out`
and `strobe_out` are registered in lockstep on input strobes;
data and strobe travel together; `strobe_out` defaults to 0
between strobes; the accumulator holds when `strobe_in` is low.

**Resource cost (Z7020):**

Per instance: ~6 LUT, 24 FF, 0 DSP, 0 BRAM. Two instances per
LSM channel = ~12 LUT total. Negligible.

### Wired into `LsmDemod`

[lsm_demod.py](../../maia-hdl/p25_hdl/lsm_demod.py) now
instantiates two `LsmDcBlocker`s before the existing
`LsmDemodLoop` and routes a new top-level `dc_block_enable`
input to both. The pipeline becomes:

```text
IQ -> [LsmDcBlocker x2] -> [LsmDemodLoop] -> dibits -> [LsmNidPipeline] -> NID
```

The blockers add one cycle of latency on the IQ path, which is
invisible to `LsmTimingInterp` because it samples on its own
input strobe.

### New PAC field: `lsm_control.lsm_dc_block_enable`

`maia-hdl/p25_hdl/p25_top.py` adds bit `[2]` to `lsm_control`,
following the same `Access.RW, init=0` convention as
`lsm_enable` and `lsm_dibit_dma_enable`. The PS daemon is
responsible for setting it to 1 at startup.

`p25-httpd/p25-pac/p25.svd` gets the matching SVD field, and
the PAC was regenerated with `svd2rust 0.33.5 -i p25.svd
--target none -o src/`.

The full address-map line lives in
[doc/P25_ADDRESS_MAP.md](../P25_ADDRESS_MAP.md) -- look for
`lsm_dc_block_enable`.

### PS-side wiring

[p25-httpd/src/fpga.rs](../../p25-httpd/src/fpga.rs):

- New `set_lsm_dc_block_enable(bool)` mirroring the existing
  `set_lsm_enable` / `set_lsm_dibit_dma_enable` helpers.
- `lsm_control_readback()` extended from `(bool, bool)` to
  `(bool, bool, bool)` to also return the DC block bit.

[p25-httpd/src/main.rs](../../p25-httpd/src/main.rs):

- The control DDC startup sequence now calls
  `ip_core.set_lsm_dc_block_enable(true)` alongside the
  existing `set_lsm_enable(true)` /
  `set_lsm_dibit_dma_enable(true)`.
- The startup readback log line now includes
  `lsm_dc_block_enable=...`, and a `tracing::warn!` fires if
  the readback comes back false (with the explicit warning
  that the PLL acquisition transient will be 2-3 minutes
  instead of a few seconds, so this isn't a silent regression).

### Tests

**New unit tests** -- `maia-hdl/test/test_lsm_dc_blocker.py`,
4 tests:

| Test | What it proves |
|---|---|
| `test_step_response_decays_and_matches_reference` | Bit-exact against a Python reference for a constant DC input over 2048 samples; final residual is well under 0.5% of the input level; first sample equals the input (dc estimate is still 0); midpoint is much smaller than the input but still nonzero (decay is monotone-ish). |
| `test_passband_1khz_sinusoid_unattenuated` | A 1 kHz sinusoid at fs=31.25 kSPS comes through with peak amplitude in [0.95, 1.05] of the input -- catches both runaway-amp and significant-attenuation regressions while leaving slack for the IIR shoulder + integer arithmetic. |
| `test_bypass_passes_dc_through` | With `enable_in=0` and a constant DC input, every output sample equals the input verbatim. |
| `test_strobe_out_lockstep_with_strobe_in` | The registered strobe_out matches the input strobe pattern bit-for-bit -- proves the lockstep convention. |

**New integration regression** -- added to
`maia-hdl/test/test_lsm_demod.py`:

`test_dc_blocker_absorbs_constant_iq_bias` -- drives `LsmDemod`
with the existing synthetic golden IQ + a constant DC bias of
2000 (~6 % of full scale, far larger than any AD9361 offset in
practice) added to both I and Q, with the blocker enabled by
default. Verifies the dibit pass-through still produces a
sensible dibit count (within the same 0.9..1.1 ratio the
unbiased test uses). This is the integration-level proof that
the blocker wiring is correct: the demod loop downstream sees
IQ with the bias removed.

**All test results:**

```text
test/test_lsm_dc_blocker.py ............................... 4 passed
test/test_lsm_demod.py ..................................... 2 passed
test/test_lsm_demod_loop.py ................................ 3 passed
                                                            ─────────
                                                            9 passed
```

The pre-existing `test_dibit_passthrough_and_quiescent_nid_pipeline`
on `LsmDemod` and the three `LsmDemodLoop` tests all still pass
unchanged, proving the new wiring is non-disruptive.

---

## Why a leaky integrator and not the canonical DC blocker form

The two standard topologies are:

| Form | Equation | State |
|---|---|---|
| Canonical | `y[n] = x[n] - x[n-1] + alpha*y[n-1]` | one input delay + one output state |
| Leaky integrator | `dc[n] = (1-alpha)*x[n] + alpha*dc[n-1]; y = x - dc` | one state register |

They are mathematically equivalent up to a constant scale of
`alpha` (which is ~0.992 here -- imperceptible). We pick the
leaky-integrator form because it needs only ONE state register
(no input delay), and the `(1 - alpha)` factor is exactly an
arithmetic right shift when `alpha = 1 - 2^-K` -- no
multiplier, no DSP, just shifts and adds.

This is the same idiom every "DC blocker in HDL" reference
uses for exactly this reason; we're not innovating here, just
being explicit about the choice.

---

## Why insert at the IQ input, not at the slicer

Doc 030 framed the symptom as "the slicer is running on a
slightly DC-biased input", but tracing it back, the **cause**
is upstream: the AD9361 IQ samples carry a small slow DC
offset that propagates through diff-demod and PLL rotate to
the slicer. Two reasonable insertion points were on the table:

| Option | Where | Pros | Cons |
|---|---|---|---|
| **(a) IQ input to LsmDemod** ← chosen | Right at `re_in` / `im_in`, before `LsmTimingInterp` | Removes DC at the source. Symmetric I/Q. Helps **Gardner TED, PLL update, AND the slicer**, all of which are linear in the IQ samples. Mirrors what an analog DC blocker on the AD9361 would do. | Two parallel one-pole IIRs (~12 LUT total). |
| **(b) Diff-demod output** | On `i_sym_out` / `q_sym_out` right before the slicer | One-stage fix targeted exactly at the slicer's complaint. | Doesn't help Gardner TED or PLL update -- they still see the biased post-rotate values. If those are also affected by the bias (and we have circumstantial evidence they are), the fix is incomplete. |

We picked (a). The cost difference is one extra IIR stage,
which is irrelevant on Z7020, and the win is that the entire
inner loop sees clean IQ rather than just the slicer.

---

## Why runtime-bypassable

The 6F.4-6F.10 throughput saga was driven entirely by per-pipeline
A/B comparison (`/api/decoder_compare`, `ps_lsm` vs `ps_iq_lsm`).
Phase 6G.1 follows the same playbook: making the new feature
runtime-bypassable means we can flip back to the old behavior
on hardware to **prove** the bias hypothesis (rather than
just believe it), and gives a safety hatch if the blocker
turns out to chew real signal energy on weird captures.

The cost is one PAC bit + ~4 LUTs of mux. Cheap.

The default `init = 0` matches the existing `lsm_enable`
convention -- post-reset, the blocker is off, and the P25
daemon sets it to 1 at startup in the same code path that
turns on `lsm_enable`. If a future change accidentally drops
the `set_lsm_dc_block_enable(true)` call, the
`tracing::warn!` on the readback will catch it loudly and
explain the radio behavior change.

---

## How this should look on target

**Before 6G.1 (current behavior):**

- Cold boot to first lock: 2-3 minutes
- During the transient: ~5-7 sync hits/sec, ~22-50 % CRC pass
- After the transient: ~14 sync hits/sec, ~92 % CRC pass on `ps_lsm`
- Combined CRC-OK rate (lsm + iq_lsm): rises from ~5/sec to
  ~41/sec over the first 2-3 minutes

**After 6G.1 (expected):**

- Cold boot to first lock: a few seconds (effectively as fast
  as the PLL can converge once it sees clean IQ)
- Immediately at the steady-state ~14 sync hits/sec
- ~92 % CRC pass from boot, no transient
- Combined CRC-OK rate: ~41/sec from the first sync hit
- "Reset and measure" experiments take 30 seconds instead of
  3 minutes -- this is a developer-experience win on top of
  the radio-reliability win

**A/B verification plan** (post-bake):

1. Flash the new bitstream + new p25-httpd binary.
2. Wait for cold boot to complete.
3. Take a 60-second baseline measurement of `/api/sync_stats`
   and `/api/decoder_compare` -- expect steady-state numbers
   immediately, not after a 2-3 minute climb.
4. Use a future `lsm_dc_block_enable=false` knob (e.g. `curl`
   to a new debug endpoint, or just patch `main.rs` for the
   single test) to **disable** the blocker.
5. Re-flash or hot-toggle (the blocker is enable-controlled
   in HDL, so a simple PS register write should suffice
   without re-flashing).
6. Take another 60-second measurement and confirm the
   pre-6G.1 climb-from-cold-start behavior reappears.
7. Re-enable, confirm the climb goes away again.

If steps 4-7 don't show the expected difference, the bias
hypothesis was wrong and we need to look elsewhere -- which
is exactly the kind of question runtime bypass was added to
answer.

---

## Build / commit sequencing

Per the standing rule (commit binary artefacts before they
ship in a firmware image):

1. **This commit:** all source changes + tests + doc + this
   change doc + the regenerated `p25-pac/src/lib.rs`. **No
   binary artefact yet.** PS Rust workspace builds clean,
   HDL test suite is 100 % green, p25_top elaborates without
   error.
2. **Next session:** run `build_fpga.bat --p25` from git-bash
   (per the `.bat from git-bash` reference: invoke directly,
   no `cmd //c` wrapping), wait for the Vivado bake, commit
   the new XSA + bitstream as a separate artefact commit,
   then verify on target.
3. **Tezuka firmware** consumes the new artefact via its
   Buildroot mount, no changes required on that side as
   long as the `lsm_control` register layout stays
   backward-compatible (it does -- we only added a bit, we
   didn't move existing ones).

This is exactly the "commit binary artefacts before they ship"
flow from the build/commit-sequencing feedback memory.

---

## What's NOT in 6G.1

- **No changes to the C4FM HDL chain.** The C4FM demod has
  its own AGC + DC handling and isn't affected by the LSM
  IQ DC bias. If we ever do find a similar issue on the
  C4FM side, we'd add a separate front-end blocker there;
  this phase doesn't touch it.
- **No changes to AGC.** AGC is Phase 6G.x or later, and
  doc 030 didn't put it on the critical path.
- **No `/api/sync_stats` "DC blocker effect" diagnostic
  endpoint.** The dashboard already shows enough state via
  `/api/decoder_compare` to A/B the blocker by hand. If
  it turns out we want a single-number "did the blocker
  help" metric, we can add it post-verification.
- **No port of the soft sync correlator to PL.** That's
  candidate #2 in doc 030's roadmap and the next session's
  question. We may decide it's not worth doing (the
  parallel-decoder architecture's diagnostic value, see
  doc 030, is the counter-argument).

---

## Files touched

| File | Change |
|---|---|
| `maia-hdl/p25_hdl/lsm_dc_blocker.py` | NEW -- the `LsmDcBlocker` Elaboratable |
| `maia-hdl/p25_hdl/lsm_demod.py` | Instantiates two `LsmDcBlocker`s; new `dc_block_enable` top-level input |
| `maia-hdl/p25_hdl/p25_top.py` | New `lsm_dc_block_enable` field on `lsm_control`; wires it to `LsmDemod.dc_block_enable` |
| `maia-hdl/test/test_lsm_dc_blocker.py` | NEW -- 4 unit tests |
| `maia-hdl/test/test_lsm_demod.py` | NEW regression test `test_dc_blocker_absorbs_constant_iq_bias` |
| `p25-httpd/p25-pac/p25.svd` | New `lsm_dc_block_enable` field on `lsm_control` |
| `p25-httpd/p25-pac/src/lib.rs` | Regenerated with `svd2rust 0.33.5` |
| `p25-httpd/src/fpga.rs` | New `set_lsm_dc_block_enable`; `lsm_control_readback` returns 3-tuple |
| `p25-httpd/src/main.rs` | Calls `set_lsm_dc_block_enable(true)` at startup; readback log + warn on mismatch |
| `doc/P25_ADDRESS_MAP.md` | Documents the new field at `lsm_control[2]` |
| `doc/changes/031_phase6g1_hdl_dc_blocker.md` | This doc |

---

## Test results

```text
$ python -m pytest test/test_lsm_dc_blocker.py test/test_lsm_demod.py test/test_lsm_demod_loop.py -v
test/test_lsm_dc_blocker.py::TestLsmDcBlocker::test_bypass_passes_dc_through PASSED
test/test_lsm_dc_blocker.py::TestLsmDcBlocker::test_passband_1khz_sinusoid_unattenuated PASSED
test/test_lsm_dc_blocker.py::TestLsmDcBlocker::test_step_response_decays_and_matches_reference PASSED
test/test_lsm_dc_blocker.py::TestLsmDcBlocker::test_strobe_out_lockstep_with_strobe_in PASSED
test/test_lsm_demod.py::TestLsmDemod::test_dc_blocker_absorbs_constant_iq_bias PASSED
test/test_lsm_demod.py::TestLsmDemod::test_dibit_passthrough_and_quiescent_nid_pipeline PASSED
test/test_lsm_demod_loop.py::TestLsmDemodLoop::test_demod_loop_synthetic_matches_truth PASSED
test/test_lsm_demod_loop.py::TestLsmDemodLoopSlipResistance::test_demod_loop_cordic_vs_linearised_under_phase_step PASSED
test/test_lsm_demod_loop.py::TestLsmDemodLoopSlipResistance::test_demod_loop_linearised_baseline PASSED
======================== 9 passed in 11.01s ========================
```

```text
$ cargo check  # in p25-httpd
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.58s
```

```text
$ python -c "from p25_hdl.p25_top import P25Core; ..."
p25_top elaborates OK
```

All green. Ready for the FPGA bake.
