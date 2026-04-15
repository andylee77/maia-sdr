# Phase 8 — LSM runtime reset + clean re-lock on retune

Date: 2026-04-14

## Summary

Phase 8 fixes the Phase 7 traffic-audio quality problem diagnosed
in [doc/changes/037](037_phase8_hdl_lsm_review.md): after every
traffic-channel retune the LSM chain's PLL accumulator was still
holding the phase-error it settled to on the old carrier, and the
chain was never gated off between calls. The result was corrupted
post-retune dibits, runaway false TDU_LC NIDs (~60 % of the BCH
output, flat-tail sync distance histogram), and 1-in-20 intelligible
calls on a Clay County LSM site.

Phase 8 has three sub-phases, all landed in this change:

1. **Phase 8A** — HDL runtime reset plumbing. A new `reset_in` port
   cascades from `LsmDemod` down through every stateful LSM
   submodule and clears their persistent state (PLL accumulator,
   timing interpolator sample-point + IQ FIFO, diff-slicer history,
   sync shift register, BCH sweep state, NID FSM, drop counter) to
   init on a single 1-cycle pulse. Wired to two new W1P register
   fields, `lsm_control.lsm_reset` and
   `traffic_lsm_control.traffic_lsm_reset`.
2. **Phase 8B** — PS integration. New `fpga.rs` helpers
   (`pulse_traffic_lsm_reset`, `retune_traffic_chain`,
   `pause_traffic_chain`) give the follower an atomic
   freeze-reset-thaw primitive for the retune path and an explicit
   pause for Idle/timeout / encryption tear-down. The follower task
   in `main.rs` now calls `retune_traffic_chain` on every grant
   retune and `pause_traffic_chain` on every teardown. BUILD_TAG
   bumped to `2026-04-14-phase8-lsm-runtime-reset`.
3. **Phase 8C** — Local clock domains. `LsmDemod` (both chains)
   moved into per-chain local clock domains (`lsm_ctrl_dom`,
   `lsm_traffic_dom`) clocked from `sync` with reset wired to
   `~lsm_enable` / `~traffic_lsm_enable`. Disabling the chain now
   forces a synchronous reset of every non-`reset_less` register,
   which covers all the pipeline state (FSM registers, stage
   strobes, output latches). `reset_less=True` accumulators are
   left alone by the domain reset, so the 8A explicit reset path
   remains load-bearing for those and is called from the `retune`
   sequence AFTER the chain has been re-enabled.

## Root cause recap

See doc/changes/037 for the full investigation. Short version:

- Two independent gaps in the Phase 7A.2 HDL port:
  - `traffic_lsm_enable` was set once at boot and never toggled —
    the LSM chain has been running continuously since boot,
    accumulating noise-derived state between calls.
  - Stateful LSM registers (`pll_reg`, `sample_point`, sync
    shift register, etc.) were all declared with `reset_less=True`
    so the PS had no runtime mechanism to clear them back to init.
- Symptom on Clay County LSM: post-retune PLL runs against the
  stale phase reference for ~hundreds of milliseconds while it
  re-converges, slicer emits corrupted dibits, sync correlator
  matches noise at various Hamming distances, BCH "corrects" the
  noise to its nearest codeword, `DUID=0xF=TDU_LC` wins (large
  basin of attraction in Hamming space) → ~60 % phantom TDU_LC
  rate, flat sync-distance histogram tail, 1-in-20 intelligible
  calls.

The control side never had the problem because the control DDC
never retunes; its PLL locked once at boot and stayed locked.

## Phase 8A — HDL runtime reset plumbing

### Modules modified

Every stateful LSM submodule got a new `reset_in: Signal()`
input and a trailing `with m.If(self.reset_in): m.d.sync += [...]`
override block that clears the persistent state registers to
their init values. Amaranth's last-assignment-wins semantics in
`m.d.sync` makes this a clean override of any normal-path update
that would fire on the same cycle.

| Module | State cleared on `reset_in` |
|---|---|
| `LsmPllUpdateLinearised` | `pll_reg`, stage1/2 pipeline regs, `pll_out`, `pll_strobe` |
| `LsmPllUpdate` (CORDIC) | same as above + `pending_skip`, `clamped_angle_q`, `stage1_skip`, `stage2_skip` |
| `LsmTimingInterp` | `sample_point` → warmup init, IQ FIFO entries, stage-1 latches, output regs |
| `LsmDiffDemodSlicer` | `prev_middle_*`, `prev_current_*`, per-decision `*_full` accumulators, output regs |
| `LsmSyncNidExtract` | `sync_reg`, `reg_fill`, `nid_word`, `dibit_count`, `latched_distance`, FSM→IDLE via per-state `m.next = "IDLE"` |
| `LsmNidBchFec` | `counter`, `received_q`, `best_dist` → 0x7F, `best_data`, output regs, FSM→IDLE |
| `LsmNidPipeline` | `latched_sync_distance`, NID event latches, `nid_drop_count` |
| `LsmDemodLoop` | plumbs `reset_in` into timing / diff_demod / pll_update; clears local `dibit_out`, `symbol_strobe`, rotation debug taps |
| `LsmDemod` | plumbs `reset_in` into demod_loop + nid_pipeline. DC blocker is NOT reset — its state is a slow ADC-offset estimate that does not change between retunes and re-converges in ~20 ms |

The CORDIC atan2 submodule and `LsmPllRotate` / `LsmGardnerTed`
don't get explicit reset paths — they have no long-lived
accumulators that matter across calls, and by the time the PS
pulses reset the upstream `strobe_in` has been off for ~tens of
microseconds (an AXI write latency), so their internal pipelines
have drained naturally.

### `p25_top.py` — new W1P register fields

`lsm_control.lsm_reset` (bit 2) and
`traffic_lsm_control.traffic_lsm_reset` (bit 3) are new `Wpulse`
fields. The Register framework gives us a clean 1-sync-cycle
pulse per PS write, which is wired directly into
`LsmDemod.reset_in`. Readback is 0 (write-only in SVD, `R` has no
method for these fields, so `modify()` on any other bit in the
same word will never accidentally pulse the reset).

### Unit tests

- `test_lsm_pll_update.py::TestLsmPllUpdateReset` adds three new
  cases that cover both `LsmPllUpdateLinearised` and
  `LsmPllUpdate` (CORDIC):
  - `test_linearised_reset_clears_pll` — drive pll into saturation
    (dibit 10 with positive IQ), pulse reset, verify `pll_out = 0`.
  - `test_cordic_reset_clears_pll` — same for the CORDIC form.
  - `test_cordic_reset_then_reconverge_matches_cold_start` — after
    a saturation + reset, drive a random input sequence and verify
    the post-reset trajectory is **bit-exact** to a cold-start run
    on the same sequence. This is the acceptance criterion for
    the 8A retune path: the post-reset PLL behaves identically to
    a just-instantiated PLL.
- `test_lsm_demod.py::TestLsmDemod::test_reset_in_clears_pll_and_sample_point`
  is the end-to-end integration-level reset smoke test. Drives
  `LsmDemod` with the synthetic golden IQ for 200 samples, drains
  the pipeline for 128 cycles, pulses `reset_in` for one sync
  cycle, then verifies `pll_dbg == 0` and
  `sample_point_dbg == warmup_init` after another 32 drain
  cycles.

### SVD + PAC regen

- `python3 generate_p25_svd.py` — emits `p25.svd` with the new
  W1P fields.
- `svd2rust -i p25.svd --target none -o src/` — emits the PAC
  with `lsm_reset()` / `traffic_lsm_reset()` writer methods. No
  reader methods for these bits because they're `Wpulse` and the
  HDL reads them back as 0.

## Phase 8B — PS integration

### `p25-httpd/src/fpga.rs` new helpers

- `pulse_lsm_reset()` — control side reset pulse (not currently
  used; symmetry with traffic side).
- `pulse_traffic_lsm_reset()` — traffic side reset pulse, fires
  a W1P write into `traffic_lsm_control.traffic_lsm_reset`.
- `retune_traffic_chain(freq_hz, sample_rate_hz) -> Result<()>` —
  atomic freeze-reset-thaw primitive. Order:
  1. `set_traffic_lsm_enable(false)` — domain reset asserts
     (8C), non-reset_less state clears, pipeline stops.
  2. `set_traffic_demod_enable(false)` — C4FM chain also off.
  3. `set_traffic_ddc_frequency(...)` — new NCO word.
  4. `set_traffic_lsm_enable(true)` — domain reset deasserts,
     pipeline state starts from init.
  5. `pulse_traffic_lsm_reset()` — 8A explicit reset clears the
     `reset_less=True` accumulators (`pll_reg`, `sample_point`,
     sync register, diff-slicer prev, BCH sweep state). This
     MUST come after step 4 because `m.d.<domain>` assignments
     don't fire while the domain is held in reset.
  6. `set_traffic_demod_enable(true)` — C4FM chain back on.
- `pause_traffic_chain()` — two-register quiesce:
  `set_traffic_lsm_enable(false)` + `set_traffic_demod_enable(false)`.
  Used by Idle/timeout + encryption teardown.

### `p25-httpd/src/main.rs`

- Boot init: `traffic_lsm_enable` now starts **off** at boot
  (the old code set it on once and forgot it). Readback check
  inverted: expects `traffic_lsm_enable=false` at boot.
- Retune path (follower task `handle_grant_event` → `retune` arm):
  ad-hoc `set_traffic_ddc_frequency` + `set_traffic_demod_enable(true)`
  replaced with a single `core.retune_traffic_chain(...)` call.
  Event log entry now includes `"lsm_reset": true`.
- Encrypted tear-down: replaces
  `core.set_traffic_demod_enable(false)` with
  `core.pause_traffic_chain()`.
- Call-timeout handler: same replacement.
- `BUILD_TAG` bumped to `2026-04-14-phase8-lsm-runtime-reset`.

`cargo check --workspace` passes with only pre-existing warnings.

## Phase 8C — Local clock domains

### Implementation

`p25_top.py` `elaborate()`:

```python
lsm_ctrl_dom = ClockDomain("lsm_ctrl_dom")
lsm_traffic_dom = ClockDomain("lsm_traffic_dom")
m.domains += [lsm_ctrl_dom, lsm_traffic_dom]
lsm_ctrl_renamer = DomainRenamer({'sync': 'lsm_ctrl_dom'})
lsm_traffic_renamer = DomainRenamer({'sync': 'lsm_traffic_dom'})
```

Then the two `LsmDemod` submodule instantiations are wrapped:

```python
m.submodules.lsm_demod = lsm_ctrl_renamer(self.lsm_demod)
m.submodules.traffic_lsm_demod = lsm_traffic_renamer(self.traffic_lsm_demod)
```

And the two new domains get their clock + reset wired once
`lsm_enable` / `traffic_lsm_enable` are in scope:

```python
m.d.comb += [
    ClockSignal("lsm_ctrl_dom").eq(ClockSignal("sync")),
    ResetSignal("lsm_ctrl_dom").eq(~lsm_enable),
]
# ... same for lsm_traffic_dom ~traffic_lsm_enable
```

Since both domains use the same `sync` clock, there is no real
async CDC — signals that cross between `sync` (the decimator,
register bank, DMA) and `lsm_*_dom` are same-clock, phase-aligned,
and safe to wire combinationally.

### What 8C actually gets us

- **Non-`reset_less` state clears automatically on disable.** FSM
  state in `LsmSyncNidExtract`, `LsmNidBchFec`, CORDIC; stage
  pipeline strobes in `LsmPllUpdate`; output latches
  (`dibit_out`, `symbol_strobe`, `nid_event_strobe`, `nid_drop_count`
  since it's non-reset_less, etc.) — all of these go back to
  init the moment `lsm_enable` drops low. No explicit
  `reset_in` pulse needed for them.
- **Per-call drop-count semantics.** `nid_drop_count` was a
  persistent counter; now it resets on every call, so a non-zero
  value at Idle→Active means "this call dropped N" rather than
  "cumulative drops since boot". Intentional.
- **Phase 7G groundwork.** When the channelizer + multi-channel
  follower lands (Phase 7G), each LDU slot will want its own
  clock domain with enable = reset. 8C's two-domain split is the
  prototype for that pattern.

### What 8C does NOT do

`reset_less=True` registers — `pll_reg`, `sample_point`, the IQ
lookahead FIFO, `prev_middle_*` / `prev_current_*`, `sync_reg`,
`reg_fill`, `nid_word`, `dibit_count`, `latched_distance`, BCH
`counter` / `received_q` / `best_dist` / `best_data` — are NOT
affected by domain reset (that's the defining property of
`reset_less=True`). They still need the 8A explicit reset pulse
to clear them.

This is why the `retune_traffic_chain` sequence in 8B pulses the
reset AFTER re-enabling the chain: the explicit reset logic
(`with m.If(self.reset_in): m.d.sync += [...]`) lives inside
each submodule's elaborate and runs in the per-chain clock
domain, so it can only fire while that domain's reset is
deasserted. Pulsing before re-enable would be swallowed by the
active domain reset.

The spec in [doc/changes/037](037_phase8_hdl_lsm_review.md)
originally described 8C as giving "disable = full reset"
semantics, which is aspirational — in practice it's "disable =
reset the pipeline state, explicit pulse clears the persistent
accumulators". The two together match the acceptance criterion.

## Acceptance criteria

Phase 8A:

1. **Unit tests pass.** ✔
   `test_lsm_pll_update.py` (13 tests, incl. 3 new reset cases),
   `test_lsm_demod.py` (3 tests, incl. 1 new reset case), plus
   all 76 other LSM tests. Linux WSL Python sim.
2. **Vivado bake.** Deferred — gateware elaborates cleanly via
   `python3 -m p25_hdl.p25_top` (55 602 lines of Verilog,
   554 references to the new `lsm_ctrl_dom` / `lsm_traffic_dom`
   domains). Bake is the next on-target cycle.
3. **`traffic_lsm_reset` is visible in the PAC.** ✔
   svd2rust generated `TrafficLsmResetW` write-only bit 3. No
   reader method — matches the W1P / `write_with_zero` semantics.
4. **Pulsing reset from `devmem` on hardware clears `pll_dbg` and
   `sample_point_dbg` within one heartbeat cycle.** Deferred to
   on-target verification.

Phase 8B:

1. **Between calls, traffic LSM chain is quiescent.** Boot init
   now leaves `traffic_lsm_enable = 0`. Encrypted tear-down and
   Idle/timeout call `pause_traffic_chain()`. Expected
   behaviour: no new NID events observed in `/api/traffic.stats`
   between calls. Verify on-target.
2. **After a retune, PLL starts from 0 and converges within
   ~50-100 ms.** Verify via `lsm_debug.pll_dbg` trace on-target.
3. **DUID ratio on `/api/traffic` rebalances.** Expected: `tdu_lc`
   ≤ 1-2 per call, `ldu1 ≈ ldu2`, each at ~5 per second during
   an active call. Verify on-target.
4. **`/api/nid_capture` sweep post-BCH DUID histogram dominated
   by real frames.** Verify on-target via
   `tools/p25_nid_analyze.py sweep --side traffic`.

Phase 8C:

1. **Both `LsmDemod` instances elaborate into independent clock
   domains.** ✔ Verilog diff against the 8A+8B baseline shows
   the new `lsm_ctrl_dom_clk` / `lsm_ctrl_dom_rst` /
   `lsm_traffic_dom_clk` / `lsm_traffic_dom_rst` nets.
2. **Existing cocotb + amaranth-sim tests still pass.** ✔ 30
   LSM tests run clean. `DomainRenamer` is used at the
   `p25_top` wrapper layer only, so individual submodule tests
   (which instantiate the submodule in the default `sync`
   domain) are unaffected.
3. **Vivado bake WNS ≥ 0 and BRAM/DSP/LUT usage unchanged within
   2 %.** Deferred to the on-target bake.
4. **Toggling `traffic_lsm_enable` produces equivalent clean-up
   to pulsing `traffic_lsm_reset`.** Partially: toggle clears
   the non-`reset_less` state (pipeline regs, FSM state, output
   latches), pulse also clears the `reset_less` accumulators.
   Complete parity would require removing `reset_less=True`
   from the accumulators, which is a deliberate deferred decision
   (cost: ~extra LUT/FF on an already-tight Z7020 place-and-route).

## Files touched

### Gateware (maia-hdl)

- `p25_hdl/lsm_pll_update.py` — +reset_in on both classes
- `p25_hdl/lsm_timing_interp.py` — +reset_in
- `p25_hdl/lsm_diff_demod_slicer.py` — +reset_in
- `p25_hdl/lsm_sync_nid_extract.py` — +reset_in (incl. FSM override)
- `p25_hdl/lsm_nid_bch_fec.py` — +reset_in (incl. SWEEP→IDLE)
- `p25_hdl/lsm_nid_pipeline.py` — propagate reset_in
- `p25_hdl/lsm_demod_loop.py` — propagate reset_in
- `p25_hdl/lsm_demod.py` — +reset_in port
- `p25_hdl/p25_top.py` — new W1P fields, lsm_ctrl_dom /
  lsm_traffic_dom local clock domains, DomainRenamer on the
  two LsmDemod instances, reset_in wiring
- `test/test_lsm_pll_update.py` — +3 reset tests
- `test/test_lsm_demod.py` — +1 end-to-end reset test

### Firmware (p25-httpd)

- `p25-pac/p25.svd` — regen
- `p25-pac/src/lib.rs` — regen via svd2rust
- `src/fpga.rs` — new `pulse_lsm_reset`, `pulse_traffic_lsm_reset`,
  `retune_traffic_chain`, `pause_traffic_chain` helpers
- `src/main.rs` — boot init flips `traffic_lsm_enable` to off;
  retune path uses `retune_traffic_chain`; encrypted/timeout
  paths use `pause_traffic_chain`; `BUILD_TAG` bumped

## Deferred / next

- **Vivado bake.** `./build_fpga.bat --p25`, flash, verify on-
  target. Expect WNS ≥ 0 — we're adding synchronous reset wires
  to registers that already had a D path, not changing critical
  paths. The DomainRenamer swap is a pure rename, no new routing
  at the physical level since `lsm_*_dom.clk` is tied to
  `sync.clk`.
- **On-target verification.** Replay the Clay County LSM capture,
  re-run the `tools/p25_nid_analyze.py sweep --side traffic`
  analysis, confirm the post-BCH DUID histogram flips from
  ~60 % TDU_LC to the expected LDU1 ≈ LDU2 dominance with
  ≤ 1-2 TDU_LC per call.
- **Remove `reset_less=True` from the persistent accumulators.**
  If 8C's on-target behaviour turns out clean and the extra LUT
  cost is trivial, we can retire the explicit 8A reset_in path
  entirely and simplify the HDL back to "disable = full reset".
  Follow-up pass, not blocking.
