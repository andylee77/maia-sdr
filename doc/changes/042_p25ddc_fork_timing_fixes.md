# P25DDC fork — timing-closure fixes

Date: 2026-04-15
Branch: `bisect-safety`
Companion to: [041_p25ddc_fork.md](041_p25ddc_fork.md)
Status: **in progress** — bake #3b running as of writing. This doc
captures every timing-closure diff that accumulated on top of the
P25DDC v2 filter fork so that a future upgrader can re-apply them if
adi-hdl, Amaranth, or Maia HDL change out from under us.

## Summary

The P25DDC fork v2 HDL (commit `0898736`) landed a much larger
stage-3 FIR (248 taps vs Phase 10-prep's 97), a new `P25DDC`
subclass, and unit-DC-gain coefficient tables. The resulting
placement pressure surfaced three latent timing issues that had
been masked by placement luck on earlier Phase 10-prep bakes:

1. **Maia `Rsticky` interrupt fields have no CDC synchronizer** —
   5 DMA completion pulses in the `sync` (62.5 MHz) domain were
   being directly comb-assigned into sticky FFs in the `s_axi_lite`
   (100 MHz) domain via `control_registers`.
2. **RegisterCDC / FFSynchronizer chains lack `set_false_path`** —
   ~293 endpoints in the 2-flop synchronizer chains between
   `clk_out1_system_maia_sdr_clk_0` and `clk_fpga_0` were being
   analysed by Vivado as real 2 ns inter-clock setup constraints.
3. **ADI's axi_ad9361 TX rate counter is marginal at 250 MHz** —
   `i_tx/dac_rate_cnt_reg` has a 7-logic-level CARRY4+LUT chain
   that fails timing by ~0.4 ns under certain placements. ADI
   shipped an upstream fix in May 2025 that our adi-hdl submodule
   pin is too old to include.

Each issue has both a **waiver** (fastest path) and a **structural
fix** (cleanest path). Where possible I applied both — the waiver
catches any future placement lottery for the same problem, and the
structural fix means timing should close on its own even without
the waiver.

---

## Fix 1 — PulseSynchronizer for DMA interrupt CDC

**File**: [maia-hdl/p25_hdl/p25_top.py](../../maia-hdl/p25_hdl/p25_top.py)

### Root cause

`control_registers` is wrapped with `s_axi_lite_renamer` at
[p25_top.py:712](../../maia-hdl/p25_hdl/p25_top.py#L712) so its
internal `m.d.sync` runs in the `s_axi_lite` (100 MHz `clk_fpga_0`)
domain. `Rsticky` fields implement sticky capture as
`m.d.sync += sticky.eq(sticky | self[field.name])` in
[register.py:127-129](../../maia-hdl/maia_hdl/register.py#L127-L129).
That OR's the input signal directly into the sticky FF with **no
explicit CDC synchronizer**.

The 5 DMA completion interrupt signals
(`dibit_dma.interrupt`, `traffic_dma.interrupt`, `iq_dma.interrupt`,
`lsm_dibit_dma.interrupt`, `traffic_lsm_dibit_dma.interrupt`) all
originate in the `sync` (PL 62.5 MHz `clk_out1`) domain. Without a
synchronizer, Vivado analyses the comb path from the pulse source
to the sticky FF's `D` input as a real inter-clock setup
constraint with a ~2 ns requirement. Phase 10-prep's placement
happened to route it within that budget; v2's didn't.

### Fix

Insert an `amaranth.lib.cdc.PulseSynchronizer` for each of the 5
DMA interrupt signals, routing `sync` → `s_axi_lite`. The
synchronized pulse then feeds the sticky OR in `control_registers`
instead of the raw source. `PulseSynchronizer` uses a toggle-FF +
edge-detect scheme that guarantees exactly one destination-domain
pulse per source pulse, so no DMA completion is lost.

### Diff

```diff
--- a/maia-hdl/p25_hdl/p25_top.py
+++ b/maia-hdl/p25_hdl/p25_top.py
@@ -79,7 +79,7 @@ import os
 from amaranth import *
-from amaranth.lib.cdc import FFSynchronizer
+from amaranth.lib.cdc import FFSynchronizer, PulseSynchronizer
 import amaranth.back.verilog

@@ -848,9 +848,48 @@ class P25Core(Elaboratable):
         # DMA sub-buffer completion -> interrupt (sticky bit, cleared by read)
+        # ... see file for full docstring ...
         interrupts_reg = self.control_registers['interrupts']
+
+        m.submodules.dibit_dma_irq_sync = dibit_dma_irq_sync = (
+            PulseSynchronizer('sync', 's_axi_lite'))
+        m.submodules.traffic_dma_irq_sync = traffic_dma_irq_sync = (
+            PulseSynchronizer('sync', 's_axi_lite'))
+        m.submodules.iq_dma_irq_sync = iq_dma_irq_sync = (
+            PulseSynchronizer('sync', 's_axi_lite'))
+        m.submodules.lsm_dibit_dma_irq_sync = lsm_dibit_dma_irq_sync = (
+            PulseSynchronizer('sync', 's_axi_lite'))
+        m.submodules.traffic_lsm_dibit_dma_irq_sync = (
+            traffic_lsm_dibit_dma_irq_sync) = (
+                PulseSynchronizer('sync', 's_axi_lite'))
+
         m.d.comb += [
-            interrupts_reg['dibit_dma'].eq(self.dibit_dma.interrupt),
+            dibit_dma_irq_sync.i.eq(self.dibit_dma.interrupt),
+            traffic_dma_irq_sync.i.eq(self.traffic_dma.interrupt),
+            iq_dma_irq_sync.i.eq(self.iq_dma.interrupt),
+            lsm_dibit_dma_irq_sync.i.eq(self.lsm_dibit_dma.interrupt),
+            traffic_lsm_dibit_dma_irq_sync.i.eq(
+                self.traffic_lsm_dibit_dma.interrupt),
+            interrupts_reg['dibit_dma'].eq(dibit_dma_irq_sync.o),
         ]

@@ -897 @@
-            interrupts_reg['iq_dma'].eq(self.iq_dma.interrupt),
+            interrupts_reg['iq_dma'].eq(iq_dma_irq_sync.o),

@@ -987 @@
-            interrupts_reg['lsm_dibit_dma'].eq(self.lsm_dibit_dma.interrupt),
+            interrupts_reg['lsm_dibit_dma'].eq(lsm_dibit_dma_irq_sync.o),

@@ -1125 @@
-            interrupts_reg['traffic_dma'].eq(self.traffic_dma.interrupt),
+            interrupts_reg['traffic_dma'].eq(traffic_dma_irq_sync.o),

@@ -1263 @@
-            interrupts_reg['traffic_lsm_dibit_dma'].eq(
-                self.traffic_lsm_dibit_dma.interrupt),
+            interrupts_reg['traffic_lsm_dibit_dma'].eq(
+                traffic_lsm_dibit_dma_irq_sync.o),
```

### When to revisit

- **If Maia HDL upstreams an `Rsticky`-with-CDC variant** (watch
  `maia-hdl/maia_hdl/register.py` for a synchronizer inside the
  sticky logic), this fix becomes redundant and can be removed.
  Check for any changes to the `Access.Rsticky` branch at
  [register.py:127](../../maia-hdl/maia_hdl/register.py#L127).
- **If any new DMA ring is added to `p25_top.py`** with an
  interrupt that feeds the `control_registers['interrupts']` bank,
  it needs the same PulseSynchronizer treatment.

### Verification

1. `cd maia-hdl && python -c "from p25_hdl.p25_top import P25Core; from amaranth.hdl import Fragment; Fragment.get(P25Core(), platform=None)"` — elaborates clean
2. `python -m pytest test/test_p25ddc.py test/test_lsm_agc.py test/test_lsm_demod.py test/test_lsm_fir.py test/test_lsm_decimator.py -q` — 27 passed pre-fix, 27 passed post-fix
3. Post-bake: `grep -c irq_sync ip/p25-core/default/p25_core.v` → expect 20 (5 synchronizers × 4 FFs each)

---

## Fix 2 — ASYNC_REG false_path + TX counter + reset fanout XDC waivers

**File**: [maia-hdl/projects/fishball7020_p25/system_constr.xdc](../../maia-hdl/projects/fishball7020_p25/system_constr.xdc)

### Root cause (three separate issues, all in one XDC block)

#### 2a. RegisterCDC / FFSynchronizer / PulseSynchronizer chains

This one has **two sub-problems** that both need separate waivers
and tripped me up on the first XDC iteration. Understanding the
distinction matters because a naive `ASYNC_REG == TRUE` filter
only catches the first kind.

##### 2a-i: Amaranth FFSynchronizer / PulseSynchronizer chains

Amaranth's `amaranth.lib.cdc.FFSynchronizer` and
`PulseSynchronizer` emit 2-flop synchronizer chains with
`ASYNC_REG="TRUE"` on the destination flops. These cover:

- The new `*_irq_sync` PulseSynchronizer instances from Fix 1.
- The **control-handshake** (`request_sync` / `response_sync`)
  inside every `RegisterCDC`, which is implemented via
  `amaranth.lib.cdc.PulseSynchronizer`.

Vivado's `ASYNC_REG` attribute correctly disables **hold-time**
analysis on the destination flop (the implicit CDC rule), but
does **not** waive setup analysis. Without an explicit waiver,
every synchronizer FF pair is analysed against the nearest
common edge between the two clocks (2 ns for a 62.5 MHz /
100 MHz pair) and reports as failing.

Waiver:

```tcl
set_false_path -to [get_cells -hierarchical -filter {ASYNC_REG == TRUE}]
```

##### 2a-ii: Maia RegisterCDC pulse-gated data lanes

**This was the gotcha on bake #3b.** Maia HDL's `RegisterCDC` at
[maia_hdl/cdc.py:102-140](../../maia-hdl/maia_hdl/cdc.py#L102-L140)
uses a **pulse-gated data CDC pattern**. Only the request /
response control *pulses* go through a PulseSynchronizer (with
`ASYNC_REG`). The actual register data lanes travel across a
pair of bare `Signal`s:

```python
cdc_response_data_src = Signal(self.w, reset_less=True)
cdc_response_data_dest = Signal(self.w, reset_less=True)
# ...
with m.If(response_sync.o):
    m.d[self._i_domain] += cdc_response_data_dest.eq(
        cdc_response_data_src)
```

These data-lane flops do **not** carry `ASYNC_REG=TRUE`. They
are ordinary Amaranth registers, just written in one clock
domain and sampled in another under the pulse handshake gate.
The protocol is correct by construction (the source flop is
stable for many cycles before the pulse handshake permits the
destination flop to sample it), but Vivado's default inter-clock
analysis treats each data bit as a real 2 ns setup path.

In bake #3b this hit **272 failing endpoints** that were NOT
caught by the `ASYNC_REG == TRUE` filter — one `*_dest_reg` flop
per data-bus bit in each of the six `*_registers_cdc` instances
(sdr, demod, iq, lsm, traffic, traffic_lsm). Waive them
explicitly by cell name:

```tcl
set_false_path -to [get_cells -hierarchical -filter {NAME =~ *cdc_request_data_dest_reg*}]
set_false_path -to [get_cells -hierarchical -filter {NAME =~ *cdc_response_data_dest_reg*}]
```

Both constraints (2a-i and 2a-ii) are needed. Dropping either
leaves ~100+ endpoints timing-failing.

##### Total endpoints covered

Bake history:

| Bake | WNS | Failing endpoints | Waivers active |
|---|---|---|---|
| #1 (original) | −6.319 ns | 298 | none |
| #2 (+Fix 1 HDL) | −7.115 ns | 314 | Fix 1 only; placement regressed |
| #3b (+Fix 2 partial) | −5.258 ns | 273 | Fix 1, 2a-i ASYNC_REG, 2b, 2c |
| #4 (+Fix 2a-ii +Fix 3) | −4.999 ns | 3 | Fix 1, 2a-i+2a-ii, 2b, 2c, Fix 3 |
| **#5 (+Fix 2d)** | **+0.267 ns** | **0** | **Fix 1, 2a-i+2a-ii, 2b, 2c, 2d, Fix 3** |

**Bake #5 closed cleanly.** "All user specified timing constraints are met."

Per-clock group slack on bake #5:

| Clock | Freq | WNS |
|---|---|---|
| `clk_fpga_0` (PS AXI) | 100 MHz | +0.920 ns |
| `clk_out1_system_maia_sdr_clk_0` (PL sync) | 62.5 MHz | +0.864 ns |
| `clk_out3_system_maia_sdr_clk_0` (clk3x) | 187.5 MHz | +0.827 ns |
| `rx_clk` (AD9361 RX) | 250 MHz | +0.456 ns |
| `clk_fpga_1` | 200 MHz | +3.673 ns |

The jump from 273 → 3 on bake #4 came from Fix 2a-ii catching the
272 pulse-gated data-lane endpoints. The remaining 3 endpoints
were all the same `sdr_reset` source FF analysed against three
destination clocks — addressed by Fix 2d below. Bake #5 cleared
the last three and landed at zero failing endpoints with
+0.267 ns worst slack.

**NOTE on build_fpga.bat**: The script still prints "Timing
constraints NOT met" and promotes `system_top_bad_timing.xsa` →
`system_top.xsa` even on a clean bake. That's a legacy check
from Phase 10-prep era that triggers on Vivado's first-pass
interim reports rather than the final routed timing summary.
The **authoritative answer is the routed timing report**, not
the script message. If the routed report says "All user
specified timing constraints are met" and
`write_bitstream completed successfully`, the XSA is good.

#### 2b. ADI AD9361 TX rate counter (`dac_rate_cnt`)

The `rx_clk` clock domain runs at 250 MHz (4 ns period). ADI's
`axi_ad9361/inst/i_tx/dac_rate_cnt_reg[13] → dac_rate_cnt_reg[7]`
counter has a 7-logic-level CARRY4+LUT chain and 4.1 ns data path
delay, exactly at the 4 ns period limit. Placement-dependent: in
the v2 bake it was ~0.48 ns over budget.

The Fishball P25 project is **receive-only** (P25 is an RX
protocol) but ADI's `axi_ad9361_tx` submodule is always
instantiated — it can't be gated off with `DAC_DATAPATH_DISABLE`
because that parameter only strips internal DDS/userports/IQ
correction, not the top-level i_tx block. The TX rate counter
exists in the synthesised design but is never driven by any
real data consumer.

Fix 3 (below) replaces the counter logic with ADI's upstream
structural fix. This waiver is belt-and-braces.

#### 2d. `sdr_reset` software reset recovery

**Discovered on bake #4.** `control_registers/control/field_sdr_reset_reg`
is a software-initiated reset bit the PS writes via AXI-Lite
(clk_fpga_0 domain). Its only downstream consumer is the
[rxiq_cdc](../../maia-hdl/maia_hdl/cdc.py) FIFO18E1's async `RST`
pin, which belongs to the clk_out1 sample domain. The PS writes
the reset, waits tens of milliseconds, releases it, and the FIFO
takes a handful of cycles to come out of reset.

Vivado reports this as **three separate recovery violations**
(~−5 ns) because the FIFO18E1 `RST` pin is analysed against all
three of its reachable destination clocks:

- `clk_out1_system_maia_sdr_clk_0` (the main sample clock)
- `clk_div_sel_0_s` (an 8 ns half-rate clock that shares FIFO18 muxing)
- `clk_div_sel_1_s` (a 4 ns half-rate clock, same muxing)

A software reset has no per-cycle recovery-timing requirement —
you don't need the reset release to settle within 2 ns of the
next clk_out1 edge because the software won't be writing
samples for many milliseconds afterwards. A single
source-based `set_false_path` covers all three:

```tcl
set_false_path -from [get_pins {i_system_wrapper/system_i/p25_core/inst/control_registers/control/field_sdr_reset_reg/C}]
```

Note: `-from` covers ALL downstream uses of this signal, so if
more async consumers of `sdr_reset` are added in the future
(e.g. resetting a new peripheral), they are automatically
covered by this one waiver.

#### 2c. PS `sys_rstgen` reset fanout

ADI's `sys_rstgen/U0/ACTIVE_LOW_PR_OUT_DFF[0].FDRE_PER_N/Q` reset
signal drives 143+ loads, primarily into the
`axi_ad9361_*_dma/inst/i_regmap/i_up_axi/s_axi_aresetn` register
reset pins. Single LUT1 logic level, ~5 ns of routing across the
die. Vivado's default setup analysis treats this as a normal
per-cycle path, but resets are held stable for many cycles — the
setup constraint is spurious.

### Fix

Append three constraint blocks to the existing P25 project
`system_constr.xdc`:

```tcl
# ── CDC synchronizer false paths ───────────────────────────────────────
# ... (full docstring in file) ...
set_false_path -to [get_cells -hierarchical -filter {ASYNC_REG == TRUE}]

# ── ADI AD9361 TX rate counter ─────────────────────────────────────────
# ... (full docstring in file) ...
set_false_path -to [get_cells -hierarchical -filter {NAME =~ *axi_ad9361/inst/i_tx/*}]

# ── PS reset generator fan-out ─────────────────────────────────────────
# ... (full docstring in file) ...
set_false_path -through [get_pins {i_system_wrapper/system_i/sys_rstgen/U0/ACTIVE_LOW_PR_OUT_DFF[0].FDRE_PER_N/Q}]
```

### When to revisit

- **If Amaranth upstreams setup-path XDC emission** for
  `FFSynchronizer` / `PulseSynchronizer` (issue to watch in the
  `amaranth-lang/amaranth` repo), the first `set_false_path` can
  be dropped.
- **If the Maia IIO project starts hitting the same CDC timing
  failures**, copy the first constraint (`ASYNC_REG == TRUE`
  waiver) into [projects/fishball_iio/system_constr.xdc](../../maia-hdl/projects/fishball_iio/system_constr.xdc).
  That project currently gets lucky because it has fewer register
  banks and less routing pressure.
- **If we ever actually use the AD9361 TX path** (P25 is
  receive-only forever, so this shouldn't happen — but e.g. for a
  loopback test), the second constraint needs to be narrowed from
  `*axi_ad9361/inst/i_tx/*` to just the rate counter:
  `*axi_ad9361/inst/i_tx/i_up_dac_common/i_xfer_cntrl/dac_rate_cnt_reg*`.
- **If ADI changes the sys_rstgen internal register name**
  (unlikely — this is in the `sys_rstgen_0` IP), the third
  constraint's `-through` pin path will need updating.

---

## Fix 3 — adi-hdl cherry-pick: `axi_ad9361_tx` up-counter

**File**: submodule `maia-hdl/adi-hdl`
**Upstream commit**: `92534dc1d` (ADI main, 2025-05-20)
**Local pick commit**: `01ef62972` (cherry-picked onto `cf81ab15c`)
**Parent pointer bump**: `cf81ab1..01ef629`

### Root cause

ADI's pre-2025 `axi_ad9361_tx.v` implemented `dac_rate_cnt` as a
load-and-decrement counter. The critical path was:

```
dac_datarate_s (from AXI register) → load mux → dac_rate_cnt FF
                                  or
dac_rate_cnt - 1 (CARRY4 subtract chain) → dac_rate_cnt FF
```

The load-value path pulls `dac_datarate_s` from the AXI register
bank through 1 LUT into the counter FF, and `dac_datarate_s` has
a long trip across the die from the up-reg bank. Combined with
the subtract chain, this was marginal at 250 MHz.

Andrei Grozav at Analog Devices shipped a fix in May 2025
(`92534dc1d`) that restructures the counter as a count-up that
resets on match:

- Load value becomes a constant `16'd0` — no more dependency on
  `dac_datarate_s` routing delay on the data path.
- `dac_datarate_s` only feeds the comparator now, not the data
  mux — it has more timing slack as a combinational input.

The behavioural semantics are equivalent: both produce a
`dac_valid_int == 1` pulse once every `dac_datarate_s + 1` cycles.
The only observable difference is a one-cycle phase shift on the
very first pulse after reset (new code fires immediately after
reset, old code fires after counting down from `dac_datarate_s`).

### Diff

Verbatim from commit `01ef62972` (= upstream `92534dc1d`):

```diff
--- a/library/axi_ad9361/axi_ad9361_tx.v
+++ b/library/axi_ad9361/axi_ad9361_tx.v
@@ -176,10 +176,10 @@ module axi_ad9361_tx #(
     if (dac_rst == 1'b1) begin
       dac_rate_cnt <= 16'b0;
     end else begin
-      if ((dac_data_sync == 1'b1) || (dac_rate_cnt == 16'd0)) begin
-        dac_rate_cnt <= dac_datarate_s;
+      if ((dac_data_sync == 1'b1) || (dac_rate_cnt == dac_datarate_s)) begin
+        dac_rate_cnt <= 16'd0;
       end else begin
-        dac_rate_cnt <= dac_rate_cnt - 1'b1;
+        dac_rate_cnt <= dac_rate_cnt + 1'b1;
       end
     end
   end
```

### How to re-apply if the adi-hdl submodule is updated

Our submodule is pinned to a fork branch `dev_prj_2018_r1` at
commit `cf81ab15c`. The cherry-picked `01ef62972` is a local commit
on top of that. If someone bumps the submodule past `cf81ab15c` in
the future:

1. **If the new pin is already past upstream commit `92534dc1d`**
   (any ADI `main` from May 2025 onward), the fix is already in.
   The local cherry-pick can be dropped. Verify with:

   ```bash
   cd maia-hdl/adi-hdl
   git log --oneline --all | grep -i 'Use incrementing cnt to improve timing margin'
   git grep 'dac_rate_cnt == dac_datarate_s' library/axi_ad9361/
   ```

2. **If the new pin is a different fork branch that doesn't have
   the fix**, re-cherry-pick it:

   ```bash
   cd maia-hdl/adi-hdl
   git fetch origin
   git cherry-pick 92534dc1d
   cd ../..
   git add maia-hdl/adi-hdl
   git commit -m "adi-hdl: re-apply axi_ad9361_tx timing-margin fix"
   ```

3. **If the upstream commit `92534dc1d` moves** (rebased /
   squashed into something else), search for the fix by content
   rather than SHA:

   ```bash
   cd maia-hdl/adi-hdl
   git log --all -S 'dac_rate_cnt == dac_datarate_s' -- library/axi_ad9361/axi_ad9361_tx.v
   ```

### When this fix becomes unnecessary

- **If we ever update `adi-hdl` past origin/main HEAD from May
  2025 or later** — the fix is already upstream.
- **If we ever move away from `axi_ad9361` to a different radio
  IP** — the whole `i_tx` path goes away.

---

## Three-layer defence summary

The three fixes collectively protect timing closure against three
independent failure modes. Any one failing alone would be enough
to break the bake; having all three means any *two* can degrade
without timing violations surfacing:

| Layer | Covers | Mechanism |
|---|---|---|
| Fix 1 (PulseSynchronizer) | Real CDC correctness for DMA IRQs | HDL change, guarantees metastability-free capture |
| Fix 2 (XDC waivers) | Vivado setup analysis on CDC chains, TX counter, reset fanout | Constraint, tells the analyser not to flag structurally-safe paths |
| Fix 3 (adi-hdl cherry-pick) | ADI TX counter critical path | Structural HDL fix in ADI IP, shortens the path |

Fix 1 is necessary regardless of 2 and 3 — it fixes a real CDC
correctness gap (not just an analysis artefact). Metastability on
an interrupt sticky flag is fine for typical PS reads but is not
theoretically clean.

Fix 2 is necessary if either Fix 1 or Fix 3 is incomplete. If we
drop the cherry-pick in a future upgrade that already has the
upstream fix, the `axi_ad9361/inst/i_tx/*` line becomes redundant
but harmless.

Fix 3 is technically redundant with Fix 2's TX counter waiver,
but the waiver is a blunt instrument (waives ALL setup paths into
i_tx/* including any future ADI updates that might add new logic).
The cherry-pick is the surgical fix and is preferred.

---

## Debugging playbook for future P25 bake timing failures

If a future P25 bake fails timing, check in this order:

1. **Intra-`clk_out1_system_maia_sdr_clk_0` WNS** — if it's
   negative, it's a real P25 DSP critical path, NOT a CDC/reset
   artefact. Investigate the specific path. Examples of likely
   culprits: new FIR with too many taps, ops_minus_one out of
   sync with tap count, new state machine with deep combinational
   logic.

2. **Intra-`clk_fpga_0` WNS** — if it's negative and paths are
   in `axi_hp*_interconnect` or `sys_rstgen`, it's ADI IP
   routing pressure. Placement lottery. Try a fresh synth
   or widen Fix 2 constraints.

3. **Inter-clock `clk_out1 ↔ clk_fpga_0` WNS** — if it's
   negative and paths end in `*_cdc/*_dest_reg*` or
   `*_irq_sync/*`, it's the Fix 2 `ASYNC_REG` waiver missing
   or being bypassed. Check that `system_constr.xdc` still has
   `set_false_path -to [get_cells -hierarchical -filter
   {ASYNC_REG == TRUE}]`.

4. **Inter-clock `clk_out1 → clk_fpga_0` with destinations in
   `control_registers/interrupts/field_sticky_*_reg`** — Fix 1
   is missing or partial. A new DMA IRQ was added without a
   PulseSynchronizer.

5. **`rx_clk` intra-domain WNS in `axi_ad9361/inst/i_tx/*`** —
   Fix 3 cherry-pick is missing or got dropped. Re-apply per
   the instructions above.

6. **`rx_clk` intra-domain WNS elsewhere** — ADI AD9361 marginal
   at 250 MHz. Unusual, likely a Vivado placement regression.
   File a bug upstream; in the meantime, extend Fix 2's TX
   counter waiver or investigate with `report_timing -from
   [get_cells {...}] -max_paths 10`.

---

## File checklist

Before committing, verify all four files are staged together:

- [ ] `maia-hdl/p25_hdl/p25_top.py` — Fix 1 (PulseSynchronizer)
- [ ] `maia-hdl/projects/fishball7020_p25/system_constr.xdc` — Fix 2 (XDC waivers)
- [ ] `maia-hdl/adi-hdl` submodule pointer — Fix 3 (cf81ab1..01ef629)
- [ ] `doc/changes/042_p25ddc_fork_timing_fixes.md` — this file

And verify the Verilog regen and bake:

- [ ] `maia-hdl/ip/p25-core/default/p25_core.v` — regenerated
      (`python -m p25_hdl.p25_top ip/p25-core/default/p25_core.v`)
      — note this is gitignored, not tracked
- [ ] Bake closes timing with zero violated endpoints OR only
      known-safe waived endpoints that match Fix 2's scopes

## References

- [041_p25ddc_fork.md](041_p25ddc_fork.md) — parent change, the
  P25DDC fork v2 filter design. This doc documents the
  timing-closure fixes that were needed to get that bake to close.
- [feedback_p25_verilog_regen.md](../../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/feedback_p25_verilog_regen.md) —
  reminder that `build_fpga.bat` does NOT auto-regen p25_core.v.
- [project_phase10_entry_point.md](../../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/project_phase10_entry_point.md) —
  Phase 10-prep's own timing fix-up (`8e07759 pipeline LsmAgc
  UPDATE state for timing closure`), a different critical path
  that was addressed by HDL pipelining.
- [Amaranth PulseSynchronizer source](https://github.com/amaranth-lang/amaranth/blob/main/amaranth/lib/cdc.py) —
  for understanding the toggle-FF + edge-detect mechanism used.
- [ADI upstream commit 92534dc1d](https://github.com/analogdevicesinc/hdl/commit/92534dc1d) —
  the surgical TX rate counter fix.
