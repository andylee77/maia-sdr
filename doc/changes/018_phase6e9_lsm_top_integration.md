# 018 -- Phase 6E.9: Wire LsmDemod into p25_top.py alongside C4FM

**Date:** 2026-04-10
**Phase:** 6E.9 (HDL top-level integration of the LSM chain)
**Branch:** fishball-p25
**Status:** DONE -- elaborates to clean Verilog, full LSM HDL suite at 49/49

---

## Goal

Plumb the standalone `LsmDemod` Elaboratable from Phase 6E.8 into
`P25Core` so the top-level FPGA design produces both C4FM and LSM
dibit streams from the same control DDC output, and surfaces the
recovered LSM NID events as AXI registers the PS can poll.

After this sub-phase, the only remaining HDL work for Phase 6E is the
Vivado bake (6E.10): regen `p25_core.v`, push through synth/PAR, and
validate on hardware against the Clay County NAC 0x8A1 simulcast site.

---

## Architectural decisions

Four design choices were locked in with the user before any code
changed (memory note `project_phase6e_entry_point.md` flagged them as
TBD). All four picked the recommended option:

1. **LSM dibit transport: dedicated parallel `lsm_dibit_dma`**
   ring at `0x1A00_0000` (32 KB, 8 sub-buffers x 4 KB), mirroring the
   existing C4FM `dibit_dma` layout. This lets the PS drain both rings
   in parallel and run both decoders against the same RF capture --
   essential for A/B comparing the two demods on bring-up. Cost is
   one extra HP1 master at ~1.28 KB/s, well below 0.001 % of the HP1
   budget.

2. **LSM front-end packaging: top-level submodules in
   `P25Core.elaborate()`**, not wrapped in a new Elaboratable. The
   four blocks (`LsmDecimator2`, `LsmFir(LPF)`, `LsmFir(RRC)`,
   `LsmDemod`) live as siblings of the existing C4FM submodules. This
   matches the prevailing top-level style and keeps every block
   visible in the Vivado hierarchy and amaranth-sim waveforms during
   bring-up.

3. **Channel scope: control channel only.** The traffic channel
   stays C4FM-only -- LSM-on-traffic is a follow-up phase. The
   immediate goal is decoding the Clay County control channel, and
   adding LSM to a chain that hasn't yet been exercised against
   simulcast traffic captures would just double the resource cost
   for speculative value.

4. **Enable gating: dedicated `lsm_enable` bit in the new
   `lsm_control` register.** The LSM chain is independent of the
   C4FM chain's enable, so it can be turned off in the field for
   power or debugging without losing C4FM dibits. The enable gates
   the strobe at the very front of `LsmDecimator2`, so when 0 every
   downstream block goes quiescent (no PLL drift, no BCH sweeps, no
   spurious dibits).

---

## Pipeline

```text
control DDC (62.5 kSPS, 16-bit signed I+Q)
       |
       v
   LsmDecimator2  (/2)              -> 31.25 kSPS
       |
       v
   LsmFir(LPF_TAPS_31250)           -- 83-tap baseband LPF, 31.25 kSPS
       |
       v
   LsmFir(RRC_TAPS_31250)           -- 105-tap matched filter, alpha=0.2
       |
       v
   LsmDemod                          -- timing recovery + diff demod +
       |                                Costas-style PLL rotate +
       |                                slicer + sync detect + BCH FEC
       |
       +--> dibit_out / symbol_strobe -> lsm_dibit_packer -> lsm_dibit_dma
       |
       +--> nid_event_strobe + (NAC, DUID, n_errors, valid,
                                sync_distance, drop_count, busy,
                                in_window, pll_dbg, sample_point_dbg)
              -> latched into the new `lsm` AXI register bank
```

The pipeline is the HDL counterpart of `LsmPipeline::process_iq()` in
[`p25-httpd/src/lsm/mod.rs`](../../p25-httpd/src/lsm/mod.rs):
`StreamingDecimator2 -> StreamingFir(LPF) -> StreamingFir(RRC) ->
demod_lsm_with_state`.

---

## What's new

### DDR carve-out

`P25Config.lsm_dibit_dma_address = 0x1A00_0000`, 32 KB total ring
(8 sub-buffers x 4 KB), aligned to 32 KB. `validate()` asserts the
alignment.

### AXI register bank

New bank 5 `lsm` at word `0x28` / byte `0x7C46_00A0`. Six registers
in an 8-slot bank (3-bit reg field), 2 slots free for future use:

| Offset | Register | Notable fields |
|--------|----------|----------------|
| `0x00` | `lsm_control`     | `lsm_enable` (RW), `lsm_dibit_dma_enable` (RW) |
| `0x04` | `lsm_status`      | `bch_busy` (R), `in_nid_window` (R), `nid_event` (Rsticky), `nid_valid` (R), `n_errors[6:0]` (R), `sync_distance[6:0]` (R), `lsm_dibit_overflow` (Rsticky) |
| `0x08` | `lsm_nid`         | `nac[11:0]` (R), `duid[3:0]` (R) |
| `0x0C` | `lsm_drop_count`  | `drop_count[15:0]` (R), `lsm_dibit_last_buffer[2:0]` (R) |
| `0x10` | `lsm_dibit_next`  | `next_address[31:0]` (R) |
| `0x14` | `lsm_debug`       | `pll_dbg[15:0]` (R, signed Q2.13), `sample_point_dbg[15:0]` (R, signed Q4.10 = top 16 bits of the 18-bit Q4.12 source) |

The five "latched" NID-event fields (`nid_valid`, `n_errors`,
`sync_distance`, `nac`, `duid`) live in `Signal()`s in
`P25Core.elaborate()` that are updated on every `nid_event_strobe`
pulse and read out via `R` fields. Combined with the `nid_event`
sticky bit (which clears on read), the PS-side polling protocol is:

```text
loop:
    s = read(lsm_status)
    if s.nid_event:                # implicitly clears the sticky
        nid  = read(lsm_nid)
        drop = read(lsm_drop_count)
        # (s.n_errors, s.valid, s.sync_distance, nid.nac, nid.duid)
        # all describe the same NID event because none of the latched
        # fields update again until the next nid_event_strobe.
```

### Interrupts

Bank 0 `interrupts` register grows a fourth Rsticky bit at offset 3:
`lsm_dibit_dma`, fed by `lsm_dibit_dma.interrupt`. Bits 0..2 are
unchanged (`dibit_dma`, `traffic_dma`, `iq_dma`).

NID events themselves are PS-polled via `lsm_status.nid_event` rather
than IRQ-driven, because at one NID per ~14 ms a 60 Hz dashboard poll
already catches every event without burning IRQ overhead.

### LSM dibit ring DMA

A second `DibitPacker` and `DmaStreamRingWrite` instance, identical
in shape to the existing C4FM pair, named `lsm_dibit_packer` and
`lsm_dibit_dma`. The DMA word format is bit-identical to the C4FM
ring so the kernel-side DMA helper code reads both rings the same
way. New AXI master `m_axi_lsm_dibit` is added to `ports()` for
Vivado IP packaging in 6E.10.

---

## Files changed

```text
maia-hdl/p25_hdl/p25_top.py       +220 / -75   imports, top-level comment
                                                rewrite (C4FM-only history
                                                collapsed), new submodules,
                                                new bank, new wiring, new
                                                bank decoder + CDC fanout
maia-hdl/p25_hdl/config.py        +30          lsm_dibit_dma_* fields +
                                                num_buffers / total_size
                                                properties + validate()
                                                alignment assertion
doc/P25_ADDRESS_MAP.md            +60          lsm_dibit_dma row, lsm bank
                                                row, lsm bank detail table,
                                                why-parallel-DMA blurb,
                                                IRQ row, HP1 row
doc/changes/018_phase6e9_lsm_top_integration.md  this document
doc/changes/015_phase6e_lsm_hdl_port.md          phase ladder updated
DEVLOG.md                                        session entry
CHANGELOG_FORK.md                                phase entry
```

No new test files. The Phase 6E.8 LSM HDL suite (49 tests) and the
older P25 HDL tests (`test_c4fm_demod`, `test_dibit_packer`,
`test_symbol_timing` -- 17 tests) all still pass unchanged. The
6E.9 work is purely top-level wiring; the building blocks have
already been exhaustively tested in their own benches.

---

## Verification

### Elaboration smoke test

```text
$ python -c "
from p25_hdl.p25_top import P25Core
from p25_hdl.config import P25Config
from maia_hdl.pluto_platform import PlutoPlatform
import amaranth.back.verilog as v
top = P25Core(P25Config())
print('SVD len:', len(top.svd()))
print('ports:', len(top.ports()))
verilog = v.convert(top, platform=PlutoPlatform(), ports=top.ports())
print('verilog len:', len(verilog))
"
SVD len: 22286
ports: 108
verilog len: 1509238
OK
```

This exercises the full Amaranth elaboration of every new module
(`LsmDecimator2`, two `LsmFir` instances, `LsmDemod` with all 11 of
its submodules, `DibitPacker`, `DmaStreamRingWrite`, the new
`Registers` bank, the new `RegisterCDC`), the SVD generator over the
new bank, and Amaranth's Verilog backend over the entire `P25Core`
hierarchy. Every connection lands; no warnings about unconnected
fields or duplicate driver conflicts.

For comparison, before 6E.9 the SVD was ~17 KB and the Verilog was
~1.27 MB. Adding the LSM chain bumps the Verilog by ~240 KB
(~19 %), consistent with `LsmDemod` being the largest single block
in the design.

### Test suite

```text
$ python -m unittest \
    test.test_lsm_decimator test.test_lsm_fir \
    test.test_lsm_timing_interp test.test_lsm_diff_demod_slicer \
    test.test_lsm_gardner_ted test.test_lsm_pll_update \
    test.test_lsm_pll_rotate test.test_lsm_demod_loop \
    test.test_lsm_nid_bch_fec test.test_lsm_sync_nid_extract \
    test.test_lsm_nid_pipeline test.test_lsm_demod
Ran 49 tests in 135.259s
OK (skipped=2)

$ python -m unittest \
    test.test_c4fm_demod test.test_dibit_packer test.test_symbol_timing
Ran 17 tests in 0.302s
OK
```

All 49 LSM HDL tests + 17 older P25 HDL tests pass. The two skips are
the slow BCH sweeps from 6E.7 still gated behind
`MAIA_HDL_SLOW_TESTS=1`.

### Resource estimate (Z7020, post-6E.9)

The C4FM chain numbers below are unchanged from before 6E.9; the LSM
column sums what 6E.8 estimated for `LsmDemod` + the front-end blocks
that 6E.9 instantiates around it.

| Component | DSP48 | BRAM18 | LUT | FF |
|---|---|---|---|---|
| C4FM chain (control + traffic, unchanged) | ~10 | 0 | ~2000 | ~1500 |
| Maia DDC + DMA infra (unchanged) | ~30 | ~10 | ~5000 | ~2500 |
| **LSM front end (decimator + LPF + RRC)** | ~2 | 0 | ~150 | ~250 |
| **LSM demod (`LsmDemod`)** | ~30 | 2 | ~3940 | ~1730 |
| **LSM dibit packer + DMA** | 0 | 0 | ~100 | ~50 |
| **LSM register bank** | 0 | 0 | ~80 | ~150 |
| **6E.9 grand total**       | **~72** | **~12** | **~11270** | **~6180** |

Z7020 has 220 DSP48E1, 140 BRAM18, ~53 K LUTs, ~106 K FFs. The
6E.9 design fits in roughly **33 % DSP, 9 % BRAM, 21 % LUT, 6 % FF**
-- comfortable margins for the Vivado bake in 6E.10 even after PAR
overhead. The biggest single consumer remains `LsmDemod`, dominated
by its three multiplier-rich blocks (timing interp, two PLL rotates,
diff demod slicer).

---

## Architectural notes (6E.9 specific)

### Why latch the NID fields in `Signal()`s instead of writing them direct

The natural HDL move would be to feed `nac_out`, `duid_out`, etc.
directly from `LsmDemod` outputs into the `R` fields of the `lsm_nid`
register. That works for the bch_busy / in_nid_window / drop_count
fields (which are continuous live signals from the BCH decoder), but
it does not work for the per-event fields, because:

- `nac_out`, `duid_out`, `n_errors_out`, `valid_out`,
  `sync_distance_out` change over the BCH decoder's lifetime (they
  reflect the **best candidate so far** during the sweep, not the
  final answer).
- The PS reads each register independently. If the BCH decoder
  finishes between two PS reads, the PS could see a half-old /
  half-new tuple.

Latching all five into local `Signal()`s on `nid_event_strobe` (the
"BCH done, here's the final answer" pulse from `LsmNidPipeline`)
guarantees the PS sees a coherent snapshot per event. The
`nid_event` Rsticky bit then tells the PS *which* snapshot is current.

### Why pre-decimator strobe gating instead of post-DDC

The `lsm_enable` bit gates the strobe at the input to
`LsmDecimator2`, **not** at the input to `LsmDemod` or anywhere
deeper in the chain. This is intentional: when the LSM chain is
disabled, none of the front-end FIRs should be running their MAC
sequences (each LPF MAC is 85 cycles, each RRC MAC is 107 cycles,
which would be wasteful work and would build up wrong filter state).
Gating at the very front means every block downstream sees an
all-zero strobe stream and goes truly quiescent.

### Why `sample_point_dbg[2:]` instead of widening `lsm_debug`

`LsmDemod.sample_point_dbg` is signed 18-bit (Q4.12). Combining it
with `pll_dbg` (signed 16-bit Q2.13) inside one 32-bit register
register would need 34 bits, one over budget. The two dropped LSBs
cost ~0.25 sample of fractional resolution in the Gardner timing
trace, which is irrelevant for a dashboard plot at any reasonable
zoom level. Splitting them into two separate registers would burn an
extra slot in the bank for very little benefit.

---

## Phase ladder status (post-6E.9)

- 6A: Python LSM demod -- DONE (`e1980aa`, doc 011)
- 6B: NID BCH FEC -- DONE (`46630d6`, doc 012)
- 6C: IQ DMA path in FPGA gateware -- DONE (`9f35f34`, doc 013)
- 6D: Rust LSM port to PS -- DONE (`7d69bac` + `b386c5b`, doc 014)
- 6E.0-6E.6: HDL front end + demod loop -- DONE (`ce9633b`, doc 015)
- 6E.6.5: AGC in HDL -- deferred follow-up
- 6E.7: BCH FEC in HDL -- DONE (doc 016)
- 6E.8: LsmDemod top-level (sync detect + NID pipeline) -- DONE (doc 017)
- 6E.8.5: Soft sync detector -- deferred follow-up
- **6E.9: wire LsmDemod into p25_top.py alongside C4FM -- DONE (THIS commit, doc 018)**
- 6E.10: regen `p25_core.v` + Vivado bake + on-target validation

---

## Notes for the next session (6E.10)

The 6E.10 work is the standard regen-Verilog + Vivado bake + on-target
validation cycle, with one twist: the `p25_core.v` regen now produces
a much larger file (~1.5 MB up from ~1.27 MB) and Vivado will take
correspondingly longer to synthesize. Steps:

1. `./build_hdl.bat --verilog-only --p25` to regen `p25_core.v`. The
   build script's mtime check should trip on the modified
   `p25_top.py` and any of the `lsm_*.py` files that were touched
   in 6E.0-6E.8 but not yet baked.
2. Update `p25-httpd/p25-pac/p25.svd` from `top.svd()` and regen the
   PAC via `svd2rust`. The new `lsm` bank will appear as a new
   register cluster in the PAC; existing PAC consumers in
   `p25-httpd/src/` are not affected.
3. `./build_fpga.bat --p25` for the full bake. Expect Vivado synth
   time to grow ~20 % vs the pre-6E.9 baseline.
4. Update `system_bd.tcl` to wire `m_axi_lsm_dibit` to HP1 via
   `ad_mem_hp1_interconnect` (idempotent), and add the new master
   to `package_ip.tcl`'s `ipx::associate_bus_interfaces`.
5. Tezuka firmware needs the kernel device tree to include the
   `0x1A00_0000`/32 KB carve-out alongside the existing
   `0x17/0x18/0x19` carve-outs. Coordinate with the Tezuka build
   in the same way Phase 6C did for `iq_dma`.
6. On-target smoke: with `lsm_enable=1` and `lsm_dibit_dma_enable=1`,
   point the control DDC at the Clay County NAC 0x8A1 simulcast
   (860.9625 MHz) and confirm `lsm_status.nid_event` is firing
   regularly with `n_errors <= 11`, `nac == 0x8A1`, and
   `nid_drop_count == 0`.

The "AGC deferred" caveat from 6E.8 is still in force: real-RF
capture quality will depend on RF level until 6E.6.5 lands. For
bring-up, the Clay County signal is strong and SDRTrunk's reference
shows clean decode without explicit AGC, so 6E.10 should still be
able to confirm correctness on hardware before AGC work begins.
