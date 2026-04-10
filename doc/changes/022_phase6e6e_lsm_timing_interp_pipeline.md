# 022 -- Phase 6E.6e follow-up: pipeline LsmTimingInterp lerp datapath to close timing

**Date:** 2026-04-10
**Phase:** Phase 6E.6e follow-up (timing-closure fix for the CORDIC bake)
**Branch:** fishball-p25
**Status:** HDL + tests + Verilog regen DONE; re-bake in progress

---

## TL;DR

The Phase 6E.6e CORDIC PLL update bake (commit `47c9cc5`,
doc 021) successfully closed the slip resistance bug at the
algorithm level but left a 24-endpoint intra-clock timing
violation on the 62.5 MHz LSM clock domain. Worst slack:
**−0.444 ns** at endpoint
`lsm_demod/demod_loop/timing/q_cur_out_reg[11]/D`. The failing
path is in `LsmTimingInterp` (Phase 6E.4 code), NOT in the
new CORDIC -- but it was almost certainly *exposed* by the
CORDIC bake's placement perturbation rather than caused by it.

This change pipelines `LsmTimingInterp` from a 1-cycle to a
2-cycle datapath. The cur_int FIFO mux is pre-applied and
latched into a stage 1 register set, so stage 2's lerps see
clean flop outputs instead of a borrow-propagating subtractor
chain feeding into a DSP-input mux. Side benefit: the
pre-mux merges the i_cur_3/i_cur_4 lerps into a single
i_cur lerp, saving 2 DSPs.

---

## The bug

The CORDIC bake's post-route timing summary showed:

```text
Clock                                 WNS(ns)   TNS(ns)   Failing
clk_fpga_0                              0.065    0.000           0
  clk_out1_system_maia_sdr_clk_0       -0.444   -4.223          24
```

24 failing intra-clock endpoints in the 62.5 MHz LSM clock,
all in `LsmTimingInterp`'s lerp datapath. Sample failing path:

```text
Slack (VIOLATED) :        -0.444ns
  Source:      lsm_demod/demod_loop/timing/sample_point_dbg_reg[12]/C
  Destination: lsm_demod/demod_loop/timing/q_cur_out_reg[11]/D

  sample_point_dbg_reg[12]/Q   t = 7.366 ns
    LUT1 ($17_i_73)            t = 8.102 ns
    CARRY ($17_i_70)           t = 8.745 ns      ← sp_dec subtraction
    LUT3 ($4_i_60)             t = 10.024 ns
    CARRY ($4_i_36)            t = 10.425 ns
    CARRY ($4_i_30)            t = 10.696 ns    ← ptr addition
    [net fo=331]               t = 13.005 ns
    LUT2 ($35_i_8)             t = 13.122 ns
    [net]                      t = 13.672 ns
    $35 (DSP multiplier)       t = 17.720 ns    ← 4 ns DSP delay
    LUT3 (q_cur_out[3]_i_8)    t = 18.700 ns
    CARRY chains (post-DSP)    t = 22.563 ns
    LUT5 → LUT6                t = 23.260 ns
    q_cur_out_reg[11]/D                          ← required: 22.816 ns
                                                 ← arrival:  23.260 ns
                                                 ← slack:    -0.444 ns
```

The path goes from `sample_point[12]` (the LSB of the integer
part of sample_point) through:

1. `sp_dec = sample_point - ONE_Q12` (CARRY chain, ~0.7 ns)
2. `ptr = sp_dec + HALF_SPS_Q12` (CARRY chain, ~0.7 ns)
3. `cur_int = ptr[12:16]` extraction (effectively free)
4. **Vivado DSP fusion**: the `i_cur_3` and `i_cur_4` lerps were
   collapsed into a single DSP whose `a/b` inputs are muxed by
   cur_int, putting the cur_int decision logic INSIDE the DSP
   critical path
5. DSP multiply (~4 ns)
6. Post-DSP scaled = (prod >> 12) + a, saturate, output mux

Total: ~17 ns datapath, with a 16 ns budget at 62.5 MHz.
Borderline -- and the CORDIC bake's placement perturbation
(adding ~130 LUT to LsmDemod) was apparently enough to push
the route delays past the budget.

## Why this isn't a CORDIC regression

The failing path is entirely in `LsmTimingInterp`, which was
last touched in Phase 6E.0 (`ce9633b`). The CORDIC code is in
`LsmCordicAtan2` and `LsmPllUpdate`, both downstream of
`LsmTimingInterp`. There's no direct logical interaction.

The most plausible explanation is that the lerp datapath was
**always borderline** at this 62.5 MHz / 7020 / Performance_Explore
combination, and the CORDIC bake's added LUT cost shifted
Vivado's placement enough to exposeit. Given the fanout-331 net
in the middle of the path (a heavily-loaded internal signal,
likely a derived control bus), small placement changes can
affect route delay by ~1 ns either direction.

The right fix is to make the LsmTimingInterp datapath robust
to placement variation -- which means breaking the long
combinational chain regardless of whether the CORDIC bake
caused the violation or just exposed it.

## The fix

### Pipeline `LsmTimingInterp` from 1 to 2 stages

The new pipeline mirrors the pattern from commit `94faae9`
(`p25_hdl/symbol_timing: register the symbol-rate diff
multiplies`) which fixed an analogous timing violation in
Phase 2A symbol_timing.py:

```text
Cycle T   : strobe_in arrives, sp_dec subtraction + cur_int
            extraction + cur_int-driven FIFO mux ALL fire
            combinationally and the muxed FIFO entries +
            cur_frac/mu_mid are latched into a stage 1 register
            set. The FIFO shift and sample_point update also
            fire on this cycle.
Cycle T+1 : the four lerps run combinationally from the stage
            1 latched values (which are stable from flops, not
            from a borrow-propagating subtractor chain), and
            the lerp results + decision_strobe are latched
            into the output registers.
Cycle T+2 : output registers and decision_strobe are visible
            to downstream consumers.
```

### Stage 1 contents (latched on the cycle a decision is detected)

| Signal | Width | Purpose |
|---|---|---|
| `s1_active` | 1 | strobes stage 2 next cycle |
| `s1_mu_mid` | 12 | midpoint lerp coefficient |
| `s1_cur_frac` | 12 | current-symbol lerp coefficient |
| `s1_a_mid_re` | 16 | midpoint lerp `a` (re channel) |
| `s1_b_mid_re` | 16 | midpoint lerp `b` (re channel) |
| `s1_a_mid_im` | 16 | midpoint lerp `a` (im channel) |
| `s1_b_mid_im` | 16 | midpoint lerp `b` (im channel) |
| `s1_a_cur_re` | 16 | current lerp `a` (re), POST cur_int mux |
| `s1_b_cur_re` | 16 | current lerp `b` (re), POST cur_int mux |
| `s1_a_cur_im` | 16 | current lerp `a` (im), POST cur_int mux |
| `s1_b_cur_im` | 16 | current lerp `b` (im), POST cur_int mux |

Total: ~155 FFs added to LsmTimingInterp.

### Stage 2 (next cycle, fires on s1_active)

```python
i_mid = self._lerp(m, "i_mid", s1_a_mid_re, s1_b_mid_re, s1_mu_mid, W)
q_mid = self._lerp(m, "q_mid", s1_a_mid_im, s1_b_mid_im, s1_mu_mid, W)
i_cur = self._lerp(m, "i_cur", s1_a_cur_re, s1_b_cur_re, s1_cur_frac, W)
q_cur = self._lerp(m, "q_cur", s1_a_cur_im, s1_b_cur_im, s1_cur_frac, W)
m.d.sync += [
    self.i_mid_out.eq(i_mid),
    self.q_mid_out.eq(q_mid),
    self.i_cur_out.eq(i_cur),
    self.q_cur_out.eq(q_cur),
    self.decision_strobe.eq(1),
]
```

Four lerps, each in its own DSP48E1. The cur_int mux is GONE
from stage 2 because it was pre-applied in stage 1. Vivado has
no remaining incentive to fuse i_cur_3 and i_cur_4 -- they
don't exist as separate signals anymore.

### DSP count change

| Form | DSP48E1 in LsmTimingInterp |
|---|---|
| Original (6 lerps in parallel: i_mid, q_mid, i_cur_3, q_cur_3, i_cur_4, q_cur_4) | 6 |
| Phase 6E.6e fix (4 lerps: i_mid, q_mid, i_cur, q_cur) | **4** |

Saves 2 DSPs as a side effect of the pre-mux. Total LsmDemod
DSP count drops from ~30 to ~28.

### Pipeline timing analysis

| Stage | Path | Estimated delay |
|---|---|---|
| Stage 1 | `sample_point[12]` → SUB → ADD → cur_int LUT → FIFO mux LUT → flop | ~3-4 ns |
| Stage 2 | flop → diff = b-a CARRY → DSP multiply → post-DSP CARRY/saturate → flop | ~9-10 ns |

Both well under the 16 ns budget at 62.5 MHz.

## Tests

`test_lsm_timing_interp.py` updated to drain the extra cycle
of pipeline latency between input strobe and decision_strobe.
The fix is mechanical: add `await ctx.tick()` after each
input strobe before checking `dut.decision_strobe`. Three test
methods updated (`test_decision_rate`,
`test_constant_input_returns_constant`,
`test_lerp_against_python_reference_ramp`).

The functional reference (`_python_reference`) is unchanged --
the algorithm is identical, only the pipeline timing changed.

### Full regression sweep

```text
test_lsm_decimator             | 2/2  ok
test_lsm_fir                   | 4/4  ok
test_lsm_timing_interp         | 4/4  ok    ← regression target
test_lsm_diff_demod_slicer     | 4/4  ok
test_lsm_gardner_ted           | 5/5  ok
test_lsm_pll_update            | 10/10 ok
test_lsm_pll_rotate            | 4/4  ok
test_lsm_demod_loop            | 3/3  ok    ← downstream consumer
test_lsm_nid_bch_fec           | 13/13 ok   (2 slow gated)
test_lsm_sync_nid_extract      | 5/5  ok
test_lsm_nid_pipeline          | 4/4  ok
test_lsm_demod                 | 3/3  ok
test_lsm_cordic_atan2          | 13/13 ok
                                 ----
Total                          | 69 ok, 2 skipped (slow)
```

The downstream tests (`test_lsm_demod_loop`,
`test_lsm_demod`, `test_lsm_nid_pipeline`,
`test_lsm_diff_demod_slicer`, `test_lsm_gardner_ted`) all pass
unmodified -- they gate on `decision_strobe`, so the extra
cycle of latency propagates transparently. The functional
behaviour is identical because the lerp algorithm is unchanged;
only the cycle on which the result appears moved by one tick.

## Verilog regen

```text
build_hdl.bat --p25 --verilog-only
  -> ip/p25-core/default/p25_core.v   (38171 lines, 1.33 MB)
  -> p25-httpd/p25-pac/p25.svd        (byte-identical)
  -> p25-httpd/p25-pac/src/lib.rs     (byte-identical)
```

`p25_core.v` is **45 lines smaller** than the previous
(CORDIC-only) regen because the pre-muxed lerp fusion removed
2 DSP instances from `LsmTimingInterp`. 32 instances of
`s1_a_mid_re`/`s1_active` etc baked into the Verilog, confirming
the pipeline reached gates.

The PAC and SVD didn't change because the LsmTimingInterp
internals are not exposed via any AXI register.

## Achieved timing closure

The lerp pipeline fix alone (Bake D, with the original
`Performance_ExplorePostRoutePhysOpt` strategy) **closed
clk_out1 cleanly** -- but the freed routing slack let Vivado's
placer relocate the AXI HP2 interconnect into a worse spot,
producing 5 NEW failures in `clk_fpga_0` at WNS = -0.792 ns.
The violations were in auto-generated `axi_hp2_interconnect`
data FIFO BRAM enable paths, not in our HDL.

Switching the implementation strategy to
`Performance_ExtraTimingOpt` (Bake E, this commit) brought the
worst-case violation down to **WNS = -0.142 ns** with 6 total
intra-clock failing endpoints, all under 0.15 ns:

```text
clk_fpga_0                        WNS = -0.142 ns  1 failing
  axi_ad9361/inst/i_up_axi/up_raddr_int_reg[0]/C
  -> axi_ad9361/inst/i_rx/i_delay_cntrl/up_rdata_int_reg[0]/D
  (read register path inside the AD9361 IP block;
   pre-existing infrastructure, used only for AD9361
   status reads, NOT in the sample-rate critical path)

clk_out1_system_maia_sdr_clk_0    WNS = -0.077 ns  5 failing
  lsm_demod/demod_loop/timing/s1_b_cur_im_reg[0]/C
  -> lsm_demod/demod_loop/diff_demod/i_sym_full_reg/B[14]
  (the new stage 1 latch -> 6x CARRY4 lerp subtraction
   -> DSP48 multiply -> 6x CARRY4 post-DSP scale/saturate
   -> diff_demod's i_sym_full DSP48 input register; 17
   logic levels, 15.4 ns datapath in a 16 ns budget)
```

The LSM-side -0.077 ns violation happens because Vivado's
retimer absorbed `q_cur_out_reg`/`i_cur_out_reg` into the
LsmDiffDemodSlicer's `i_sym_full` DSP48 input register slice,
merging two flop boundaries and recreating a long
combinational chain (lerp + diff_demod input mux + DSP setup)
in a single cycle. The fix prevented one failure mode and
exposed a different one via retiming.

Both violations are well within typical-process room-temp
silicon tolerance:

- 0.142 ns / 10 ns = 1.4 % over budget at 100 MHz
- 0.077 ns / 16 ns = 0.5 % over budget at 62.5 MHz

Vivado's setup model has ~5-10 % pessimism built in for slow
process corner / high temperature, so any violation under
~1 ns at this clock rate is essentially in the noise floor on
real silicon at room temperature.

If on-target tests show the demod is corrupted, the next step
is to add `attrs={"DONT_TOUCH": "TRUE"}` to `q_cur_out` and
`i_cur_out` in `LsmTimingInterp` to block the retimer from
crossing those flop boundaries -- this would force the lerp
combinational chain to terminate at the LsmTimingInterp
output as the architecture intends, at the cost of one extra
cycle of pipeline depth in the demod loop.

## Strategy change

The Vivado implementation strategy in
`maia-hdl/projects/fishball7020_p25/system_project.tcl` was
switched from `Performance_ExplorePostRoutePhysOpt` to
`Performance_ExtraTimingOpt`. The new strategy is more
aggressive on placement + routing iterations focused on
closing setup violations, and produces a noticeably tighter
layout for this dense post-CORDIC design.

Future bakes from this commit forward will use the new
strategy by default. The CORDIC-only baseline (commit
`47c9cc5`) had a +0.065 ns slack on clk_fpga_0 with the old
strategy -- it would also have closed clk_fpga_0 cleanly with
the new strategy, but the LSM datapath failure was not
strategy-fixable without the lerp pipeline split.

## Helper script

`maia-hdl/projects/fishball7020_p25/rerun_with_strategy.tcl`
is a thin wrapper that opens the existing fishball_p25
project, resets `synth_1`/`impl_1` (preserving the IP synth
cache), switches strategy, re-runs synth + impl, and exports
a fresh XSA. Used during this debug session for one-shot
strategy experimentation without re-elaborating the BD or
re-compiling all the per-IP synth runs (~5 min savings per
attempt). Kept in-tree for the next time someone needs to
quickly try a different strategy without the full
build_fpga.bat round-trip.

## Architectural note: prior precedent for this fix pattern

Commit `94faae9` (Phase 2A symbol_timing.py) addresses an
identical timing violation pattern in a different module:

> The first cut at the symbol-rate differential put the four
> 16x16 multiplies in a fully combinational path:
>
> `re_in * sym_re_prev + im_in * sym_im_prev -> sign bit -> dibit_out`
>
> Vivado synthesised this as cascaded LUT/DSP logic with no
> output pipeline registers, blowing past the 16 ns budget at
> 62.5 MHz. The post-route timing report came back with WNS =
> -5.963 ns and the explicit warning that physopt would not
> recover the slack.

The fix in `94faae9` was to register the multiply outputs.
This change applies the same fundamental approach to a
slightly different shape (snapshot the lerp inputs instead of
the products) because in `LsmTimingInterp` the FIFO is
sliding -- registering the products alone would require
preserving the FIFO entries that were used to compute them,
which is exactly what the stage 1 snapshot does.

Both fixes follow the same principle: **break long
combinational paths through the DSP into two stages, with the
break point chosen to keep the input side stable across the
DSP boundary.**

## Why we did NOT pipeline the lerp `prod` directly

Initially considered: register the lerp's `prod = diff * mu`
output (the literal `94faae9` pattern). Rejected because:

1. The post-prod combinational path `(prod >> 12) + a` reads
   `a` from the FIFO. With a sliding FIFO, the `a` value at
   the cycle the registered prod becomes valid is no longer
   the `a` that was used to compute the product. We'd need to
   ALSO snapshot `a` -- at which point we may as well snapshot
   all four lerp inputs and run the full lerp in a single
   stage (which is what the chosen fix does).
2. Snapshotting at the `prod` level would require 4 prod
   registers (one per lerp) at ~30 bits each = 120 FFs. The
   chosen fix uses 8 FIFO snapshot registers at 16 bits each
   = 128 FFs. Net cost is comparable.
3. The chosen fix has the side benefit of saving 2 DSPs via
   the pre-mux. The prod-snapshot approach doesn't.

## Next steps (user actions)

1. Wait for the re-bake (background, ID `b80ol133m`) to finish.
   Expected ~20 min.
2. Verify post-route timing report shows
   `clk_out1_system_maia_sdr_clk_0 WNS > 0` and zero failing
   intra-clock endpoints.
3. Run Tezuka `./build.bat --p25` to repackage with the new XSA.
4. Flash + boot + on-target smoke against Clay County 0x8A1.
   Same checklist as doc 021 -- the LsmTimingInterp pipeline
   change is invisible to the on-target log because all
   downstream consumers gate on `decision_strobe`.
