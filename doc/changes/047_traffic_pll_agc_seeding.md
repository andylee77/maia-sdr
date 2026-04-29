# 047 — Traffic LSM PLL + AGC seeding from control chain

**Date:** 2026-04-26  
**BUILD_TAG:** `2026-04-26-traffic-pll-agc-seeding`  
**Driver:** `doc/diagnostics/2026-04-25/CHANNELIZER_REDESIGN.md` Option D
(plus the operator's AGC-seeding extension).

## Problem

Per-call retune of the single traffic LSM chain costs 700–3400 ms before
the first IMBE — the start of every call is missed. SDRTrunk on the same
Pluto + same site sees ~0 ms because every channel is always-locked.
Park-chain doesn't help: the Costas PLL on noise drifts, so the
accumulator value at next-call time is no better than a fresh reset.

## Fix

Both LSM chains share the same crystal trim; the operator's PPM-sweep
test (CHANNELIZER_REDESIGN.md) shows the converged Costas accumulator is
the same on every channel. So we copy the control-chain PLL accumulator
into the traffic-chain PLL on every retune. Same idea for AGC gain: the
control-chain AGC is converged on real signal continuously, so its gain
register is a much better starting point for the traffic chain than
GAIN_INIT.

### HDL

- `LsmPllUpdate` + `LsmPllUpdateLinearised` (`p25_hdl/lsm_pll_update.py`):
  add `seed_in` (signed Q2.13). On `reset_in`, load `seed_in` into
  `pll_reg` instead of zero. Zero = legacy cold start.
- `LsmAgc` (`p25_hdl/lsm_agc.py`): add `seed_in` (unsigned Q9.11). On
  `reset_in`, load `Mux(seed_in != 0, seed_in, GAIN_INIT)` into `gain`.
- `LsmDemodLoop` + `LsmDemod`: plumb `pll_seed_in` + `agc_seed_in`
  through to the submodules.
- `p25_top.py`:
  - Add `traffic_pll_seed[15:0]` field to `traffic_lsm_control` (RW,
    init 0). Bit positions [20:5].
  - Add `traffic_agc_seed[15:0]` field to `traffic_lsm_agc_config` (RW,
    init 0, Q9.7). Bit positions [31:16].
  - Wire `traffic_lsm_demod.pll_seed_in` from `traffic_pll_seed`
    directly. Wire `agc_seed_in` from `Cat(Const(0,4), traffic_agc_seed)`
    to recover Q9.11 from the Q9.7 register field.
- Control chain unchanged — it stays cold-start so it still tracks
  whatever's actually on the control frequency.

### SVD + PAC

`generate_p25_svd.py` regenerates the SVD; `svd2rust` regenerates
`p25-pac/src/lib.rs`. Two new field accessors: `traffic_pll_seed()` on
`traffic_lsm_control`, and `traffic_agc_seed()` on `traffic_lsm_agc_config`.

### PS

`p25-httpd/src/hardware/fpga.rs`:

- New `set_traffic_lsm_seeds(pll_q213, agc_q97)` writes both seed fields,
  preserving sibling fields via `modify`.
- `retune_traffic_chain` reads `lsm_debug.pll_dbg` + `lsm_agc_debug
  .agc_gain_dbg`, calls `set_traffic_lsm_seeds(...)`, then pulses
  `traffic_lsm_reset` (which latches the seeds in the same cycle the
  reset clears the post-CORDIC pipeline).

## Why the Q9.7 round-trip is fine for AGC seed

The control AGC gain is internally Q9.11 (20 bits), but
`lsm_agc_debug.agc_gain_dbg` already truncates to Q9.7 (16 bits) for the
register-bank fit. We seed the traffic AGC from that 16-bit value, padded
back to Q9.11 with bottom 4 fractional bits = 0. That's a 0.05% gain
quantization — corrected by the AGC in <10 symbol periods (~2 ms).
Negligible vs the PLL settle time we're trying to eliminate.

## Why the seeds latch atomically with the reset pulse

`Wpulse` fields and `RW` fields share the same 32-bit register and same
write strobe in the AXI bridge. PS writes `traffic_lsm_control` once
with `traffic_pll_seed = X, traffic_lsm_reset = 1` — both land in the
register on the same sync edge. The demod's `reset_in` goes high on the
next cycle, and on that same cycle reads the (already-valid)
`pll_seed_in` value. Same for the AGC, which reads `agc_seed_in` on
the same `reset_in` edge — `traffic_agc_seed` was written one AXI cycle
earlier inside `set_traffic_lsm_seeds`, well before the reset pulse.

## Validation

- HDL: `pytest test/test_lsm_pll_update.py test/test_lsm_agc.py
  test/test_lsm_demod_loop.py test/test_lsm_demod.py` → 30 passed.
- PS: `cargo check` (workspace) + `cargo test` (workspace) → 64 passed,
  1 ignored, 0 failed (same baseline as `2026-04-25-...`).
- Smoke: `P25Core(...)` instantiates + elaborates to 57 subfragments;
  `write_svd` round-trips; PAC regen reports `traffic_pll_seed` at bits
  [20:5] of `traffic_lsm_control` and `traffic_agc_seed` at bits [31:16]
  of `traffic_lsm_agc_config`.

## Operator validation plan

After bake + flash:

1. Confirm `/api/system.build` returns `2026-04-26-traffic-pll-agc-seeding`.
2. On a Clay County TG 300 call, measure `first_imbe_ms` per call. Target:
   < 50 ms. Pre-fix: 700–3400 ms.
3. If achieved, ship and close. If not (e.g. AGC settle is the new
   bottleneck), measure traffic AGC gain trajectory in the first few
   symbols vs the seeded value and decide whether to add a full Q9.11
   seed register on the control side (avoids the Q9.7 round-trip).

## 2026-04-26 follow-up: seed on the nco_skip path too

**BUILD_TAG bump:** `2026-04-26-seed-on-nco-skip` (PS-only; no rebake).

After flashing the initial seeding build, on-target measurement showed
`first_imbe_ms = 3457` on the one followed call (TG 300, call_id 57) —
no improvement vs the pre-fix range. Telemetry showed `nco_skips: 11`
and `last_retune_secs_ago: 267`. The call was a same-freq re-engage:
the chain had been parked on the previous traffic frequency for ~7 s
when call_id 57 arrived. Phase 2f's `nco_skip` path zeroed all FPGA
work in that case, so the seed-write + reset never fired. The Costas
PLL had random-walked on noise during the 7-second idle and had to
cold-acquire on its own.

The seeding model wants a seeded start on **every grant**, not just on
true retunes. The DDC-write + 2 ms FIR-flush sleep are the only things
worth gating on freq-change; seeds + reset are cheap (two register
writes, ~µs).

Fix: in `app/grant_follower.rs`, the nco_skip branch (`pre_state ==
"Idle" && post_state != "Idle"`, no DDC retune) now also reads
`lsm_debug` + `lsm_agc_debug`, calls `set_traffic_lsm_seeds(...)`, and
pulses `traffic_lsm_reset`. The framer-reset block is unchanged. The
event-log line flips `lsm_reset: false` → `lsm_reset: true`.

`retune_traffic_chain` (true-retune path) was already doing the
seed-and-reset since the initial 047 work, so no change there.

## Out of scope (deferred)

- Channel-reuse attribution (multi-TG one freq sticky-lock). Lifecycle
  fix, independent of seeding.
- Software channelizer (Option A from CHANNELIZER_REDESIGN.md). Reserved
  as the fallback if seeding alone doesn't hit the < 50 ms target.
