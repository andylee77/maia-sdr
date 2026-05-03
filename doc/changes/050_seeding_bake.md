# 050 — LSM seed register bank (AGC / PLL / Gardner timing)

**Date:** 2026-05-03
**BUILD_TAG:** `2026-05-03-seeding-bake`
**Driver:** `memory/project_2026_05_03_traffic_ddc_preset_refresh_session.md`
("seeding bake — next session's main work").

## Problem

Cold-start First-IMBE on a traffic-chain retune is 3–5 s. The bit-exact
SW demod path (offline) hits SDRTrunk's 85–90 % decode rate on the same
RF the on-target HDL chain delivers ~57 % on. The dominant gap is the
acquisition transient on every retune — by the time the AGC/PLL/Gardner
loops converge on the new freq, several LDUs have already gone by.

The 047 bake added AGC + PLL seed inputs to `LsmAgc` / `LsmPllUpdate`,
but the corresponding CSR fields were silently dropped during the
2026-05-03 dual-DDC pivot (the old `traffic_pll_seed[15:0]` and
`traffic_agc_seed[15:0]` fields lived in bank 6's control / agc_config
registers, both of which were rebuilt). Result on the
`fa246d3 dual-DDC` bake: `pll_seed_in` and `agc_seed_in` are wired into
`LsmDemod` but nothing drives them — they sit at zero and the chains
always cold-start.

This bake:

1. Restores AGC + PLL seeding via a new dedicated CSR bank.
2. Adds a Gardner timing seed (sample_point), which the offline sweep
   identified as the third loop that needs warm-starting.

## Fix

### HDL

`LsmTimingInterp` (`p25_hdl/lsm_timing_interp.py`):

- Add `timing_seed_in` (signed 18-bit Q5.12, matches `sample_point`).
- In the `reset_in` override, load
  `Mux(seed != 0, seed, sample_point_init)` into `sample_point`.
  Mirrors the AGC/PLL warm-start pattern.

`LsmDemodLoop` + `LsmDemod`:

- Plumb `timing_seed_in` through to the `LsmTimingInterp` submodule.

`p25_top.py`:

- New register bank 8 at 0x100 (`lsm_seed_registers`, 6 RW registers):
  - `lsm_agc_seed[19:0]`            @ 0x100  (Q9.11 unsigned)
  - `lsm_pll_seed[15:0]`            @ 0x104  (Q2.13 signed)
  - `lsm_timing_seed[17:0]`         @ 0x108  (Q5.12 signed)
  - `traffic_lsm_agc_seed[19:0]`    @ 0x10C
  - `traffic_lsm_pll_seed[15:0]`    @ 0x110
  - `traffic_lsm_timing_seed[17:0]` @ 0x114
- New `lsm_seed_registers_cdc` (RegisterCDC, s_axi_lite → sync).
- New `lsm_seed_regs_select` bank-decode at `addr_bank == 0b1000`.
- Wire seed register fields into `lsm_demod` and `traffic_lsm_demod`
  `agc_seed_in` / `pll_seed_in` / `timing_seed_in` ports.

Address-map block comment in `p25_top.py` and the canonical bank table
in `doc/P25_ADDRESS_MAP.md` updated to reflect bank 8.

### PS workflow (next bake — out of scope here)

Seeds remain at zero (cold-start fallback) until the PS heartbeat is
extended. Per the session pickup memo:

1. During clean LDU flow on the control chain (sync_distance == 0,
   PLL stable >100 ms, IMBE flowing), the heartbeat snapshots
   `(pll_dbg, sample_point_dbg, gain_dbg << 4)` into a per-freq cache.
   The `<< 4` recovers the Q9.11 representation from the Q9.7 debug
   tap (`gain_dbg` is bits [10:4] of the internal Q9.11 accumulator).
2. EMA blend on each new clean snapshot, OR last-good with TTL.
3. `grant_follower::retune_traffic_chain` writes the cached seeds into
   bank 8 BEFORE pulsing `traffic_lsm_reset`.

### CDC ordering — read-back fence required on PS

The seed registers and the `traffic_lsm_reset` Wpulse cross
**independent** RegisterCDC instances (bank 8 vs bank 6). Back-to-back
AXI writes have no fabric-level guarantee that the seed value has
crossed the CDC by the time the reset pulse fires in the sync domain.

The PS must use a read-back fence in the retune sequence:

```rust
agc_seed_reg.write(|w| w.agc_seed().bits(seed_agc));
pll_seed_reg.write(|w| w.pll_seed().bits(seed_pll));
timing_seed_reg.write(|w| w.timing_seed().bits(seed_timing));
let _ = pll_seed_reg.read().bits();   // CDC fence
ctrl_reg.write(|w| w.traffic_lsm_reset().set_bit());
```

The read-back round-trip (~100 ns) is the cheapest correct way to prove
the seed values have crossed before the reset pulse.

## Empirical baseline (from offline sweep, 2026-05-03)

```
PLL bias        : -13 ± 4 Hz uniform across active voice channels
AGC             : whole-window medians corrupt; needs PTT-time sample
Gardner timing  : sweep deferred (timing_seed = 0 ships in this bake)
```

PLL seed = -13 Hz uniform → one global cached value works (no per-freq
table needed for first PS revision).

## Tests

`maia-hdl/test/test_lsm_timing_interp.py` — three new cases:

- `test_timing_seed_zero_uses_cold_start_init` — seed=0 reset →
  `sample_point` falls back to `sps_q12 + (BP_INDEX + 2) * ONE_Q12`.
- `test_timing_seed_nonzero_overrides_init` — seed=N reset →
  `sample_point` latched to N.
- `test_timing_seed_negative_value` — signed-18 round-trip catch.

All 7 tests in the file pass.

## Bake notes (Andy)

- `build_fpga.bat --p25` regenerates Verilog + SVD + svd2rust + bitstream.
- Bank 8 is at byte address 0x100; svd2rust will produce a
  `lsm_seed_registers` block alongside `lsm_registers` /
  `traffic_lsm_registers`.

## What this bake does NOT do

- PS heartbeat snapshot logic (next PS-side change after the bake).
- AGC-on-`nid_event_strobe` HDL snapshot register (deferred; the PS
  heartbeat in the websocket event stream covers the same need with
  no extra HDL).
- Per-freq seed caching (PS-side; first revision can ship a single
  global PLL seed = -13 Hz baked from the sweep).

## Files touched

- `maia-hdl/p25_hdl/lsm_timing_interp.py` — add `timing_seed_in`.
- `maia-hdl/p25_hdl/lsm_demod_loop.py` — forward `timing_seed_in`.
- `maia-hdl/p25_hdl/lsm_demod.py` — forward `timing_seed_in`.
- `maia-hdl/p25_hdl/p25_top.py` — bank 8 + CDC + wiring.
- `maia-hdl/test/test_lsm_timing_interp.py` — 3 new tests.
- `p25-httpd/src/main.rs` — BUILD_TAG bump.
- `CHANGELOG_FORK.md` — entry.
