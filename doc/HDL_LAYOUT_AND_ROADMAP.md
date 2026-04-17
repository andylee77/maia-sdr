# Fishball P25 HDL — Layout & Future-Phase Roadmap

## Document metadata

| Field | Value |
|-------|-------|
| Date | 2026-04-16 |
| Branch | `fishball-p25` |
| HEAD at review | `70206ec` (Phase 10 — AGC noise-floor gate, traffic API parity, dashboard overhaul) |
| Scope | `maia-hdl/p25_hdl/`, `maia-hdl/maia_hdl/` (surface only), `maia-hdl/ip/p25-core/`, `maia-hdl/projects/fishball7020_p25/`, `maia-hdl/adi-hdl/` (inventory only) |
| Purpose | Detailed current-state layout + future-phase plan, with polyphase channelizer as the centrepiece |
| Method | Three parallel surveys (current P25 HDL, Maia base surface, channelizer landscape + Z7020 budget) synthesised into one document |
| Confidence | Architectural claims are high-confidence. Specific DSP/LUT/BRAM numbers are estimates from docstrings + design rules — verify against a real Vivado utilisation report before sizing a build |

## How to read this document

- **Part I (§1–§8)** catalogues the current HDL in enough detail to hand to an engineer who has never seen it.
- **Part II (§9–§10)** inventories the Maia platform surface (what we are building on) and does the gap analysis against the 10-channel goal.
- **Part III (§11–§14)** is the roadmap: polyphase channelizer design space, Z7020 budget projection, phased plan, risks.
- **Part IV (§15)** is the concrete first-steps recommendation.

Numbered headings are deliberately sparse so the document reads as a reference rather than a tutorial. Every module has a file link; every architectural decision has a cited source where one exists.

---

# PART I — Current HDL architecture

## 1. Module inventory

The following modules live under [maia-hdl/p25_hdl/](../maia-hdl/p25_hdl/) and constitute the P25 gateware. `sync` is the main 62.5 MHz processing domain; `clk3x` is the 3× edge domain used by the DDC; `s_axi_lite` is the PS register interface domain.

| # | Module | File | Purpose | Domain | Reset | Notes |
|---|--------|------|---------|--------|-------|-------|
| 1 | `P25Core` | [p25_top.py](../maia-hdl/p25_hdl/p25_top.py) | Top-level IP, dual chain (control+traffic), 6 DMAs, 8 register banks | mixed | domain | Elaboration entry point |
| 2 | `P25DDC` | [p25ddc.py](../maia-hdl/p25_hdl/p25ddc.py) | v2 fork of Maia DDC: unit-DC-gain coefficients (peak=131071), strengthened stage-1 filter | `clk3x` | domain | Two instances: control + traffic |
| 3 | `C4FMDemod` | [c4fm_demod.py](../maia-hdl/p25_hdl/c4fm_demod.py) | Complex differential `z[n]·conj(z[n-1])` → FM discriminator | `sync` | domain | Queued for retirement |
| 4 | `SymbolTimingRecovery` | [symbol_timing.py](../maia-hdl/p25_hdl/symbol_timing.py) | Gardner TED + PI loop + 4800 sym/s slicer | `sync` | domain | Shared between C4FM and legacy dibit path |
| 5 | `DibitPacker` | [dibit_packer.py](../maia-hdl/p25_hdl/dibit_packer.py) | 32 dibits → 64-bit DMA word; overflow as pulse | `sync` | domain | Instantiated ×3 (control C4FM, control LSM, traffic LSM) |
| 6 | `IQPacker` | [iq_packer.py](../maia-hdl/p25_hdl/iq_packer.py) | 2× (re, im) s16 → 64-bit word | `sync` | domain | Instantiated ×2 (control + traffic IQ DMA taps) |
| 7 | `LsmDecimator2` | [lsm_decimator.py](../maia-hdl/p25_hdl/lsm_decimator.py) | Strobe-gated /2 decimator; gate is `lsm_enable` | `sync` | domain | First stage of LSM chain |
| 8 | `LsmFir` | [lsm_fir.py](../maia-hdl/p25_hdl/lsm_fir.py) | Real-coeff complex FIR; LPF (83 taps) + RRC (105 taps) cascade | `sync` | domain | 2 DSP per instance; frozen coefficients |
| 9 | `LsmDcBlocker` | [lsm_dc_blocker.py](../maia-hdl/p25_hdl/lsm_dc_blocker.py) | 1-pole leaky integrator per channel; runtime bypass | `sync` | domain | Phase 6G.1 addition |
| 10 | `LsmTimingInterp` | [lsm_timing_interp.py](../maia-hdl/p25_hdl/lsm_timing_interp.py) | 4-way Lagrange interpolation + symbol-rate slicer | `sync` | domain + `reset_in` | Phase 6E.4 |
| 11 | `LsmAgc` | [lsm_agc.py](../maia-hdl/p25_hdl/lsm_agc.py) | SDRTrunk-faithful AGC; Q9.11 gain; `mag_update_threshold` gate | `sync` | domain | Phase 10 addition |
| 12 | `LsmDiffDemodSlicer` | [lsm_diff_demod_slicer.py](../maia-hdl/p25_hdl/lsm_diff_demod_slicer.py) | Per-symbol diff demod; raw slicer output unused (rotated slicer dominates) | `sync` | domain | Phase 6E.5 |
| 13 | `LsmGardnerTed` | [lsm_gardner_ted.py](../maia-hdl/p25_hdl/lsm_gardner_ted.py) | Gardner TED on interpolated samples | `sync` | domain | Phase 6E.6a |
| 14 | `LsmPllRotate` | [lsm_pll_rotate.py](../maia-hdl/p25_hdl/lsm_pll_rotate.py) | CORDIC-LUT phase rotation; ±π/4 constellation; 2× instances (mid, curr) | `sync` | domain | Phase 6E.6 |
| 15 | `LsmCordicAtan2` | [lsm_cordic_atan2.py](../maia-hdl/p25_hdl/lsm_cordic_atan2.py) | Fixed-point 16-stage CORDIC atan2 | `sync` | domain | Fed by PLL update |
| 16 | `LsmPllUpdate` | [lsm_pll_update.py](../maia-hdl/p25_hdl/lsm_pll_update.py) | PLL accumulator via atan2 + proportional feedback; ±π/3 bound | `sync` | domain + `reset_in` | Phase 6E.6b; Phase 8A reset support |
| 17 | `LsmPllUpdateLinearised` | [lsm_pll_update.py](../maia-hdl/p25_hdl/lsm_pll_update.py) | Alternative PLL using linearised arctan; lower DSP | `sync` | domain | Not currently instantiated |
| 18 | `LsmSyncNidExtract` | [lsm_sync_nid_extract.py](../maia-hdl/p25_hdl/lsm_sync_nid_extract.py) | 48-bit hard-sync correlator + NID window extractor | `sync` | domain | Phase 6E.7 |
| 19 | `LsmNidBchFec` | [lsm_nid_bch_fec.py](../maia-hdl/p25_hdl/lsm_nid_bch_fec.py) | ML BCH(63,16,23) decoder; SDRTrunk-bit-exact under t | `sync` | domain | Phase 6E.7; see §12 of CODE_REVIEW for runtime-t note |
| 20 | `LsmNidPipeline` | [lsm_nid_pipeline.py](../maia-hdl/p25_hdl/lsm_nid_pipeline.py) | Integrates sync + BCH; start/done handshake | `sync` | domain + `reset_in` | Phase 6E.8 |
| 21 | `LsmDemodLoop` | [lsm_demod_loop.py](../maia-hdl/p25_hdl/lsm_demod_loop.py) | Closed-loop demod: interp → AGC → diff → Gardner → PLL → slicer | `sync` | domain + `reset_in` | Phase 6E.6d; Phase 10 AGC |
| 22 | `LsmDemod` | [lsm_demod.py](../maia-hdl/p25_hdl/lsm_demod.py) | Top LSM demod: DC blockers → loop → NID pipeline | `sync` | domain + `reset_in` | Phase 6E.8 + 6G.1 DC blocker |

Test coverage is present for every module in [maia-hdl/test/](../maia-hdl/test/) with the naming convention `test_<module>.py`. See §7 for coverage gaps.

## 2. Module hierarchy

From the top:

```
P25Core (p25_top.py)
├── Axi4LiteRegisterBridge                       [s_axi_lite]
├── Registers × 8 banks                          [s_axi_lite via DomainRenamer]
│     ├── control_registers           (0x00)
│     ├── sdr_registers               (0x20)    control DDC config
│     ├── demod_registers             (0x40)    control C4FM status
│     ├── traffic_registers           (0x60)    traffic DDC + C4FM
│     ├── iq_registers                (0x80)    control IQ DMA
│     ├── lsm_registers               (0xA0)    control LSM
│     ├── traffic_lsm_registers       (0xC0)    traffic LSM
│     └── traffic_iq_registers        (0xE0)    traffic IQ DMA
│
├── CONTROL CHAIN                                [sync]
│   ├── P25DDC            (control)              [clk3x]
│   ├── C4FM sub-chain    (C4FMDemod → SymbolTimingRecovery → DibitPacker → dibit_dma)
│   ├── IQ tap            (IQPacker → iq_dma)
│   └── LSM sub-chain     (LsmDecimator2 → LsmFir ×2 → LsmDemod → DibitPacker → lsm_dibit_dma)
│                             LsmDemod = DcBlocker ×2 → LsmDemodLoop → LsmNidPipeline
│
├── TRAFFIC CHAIN                                [sync]   (mirrors control)
│   ├── P25DDC            (traffic, shared coeff RAM)
│   ├── C4FM sub-chain    (traffic_dma)
│   ├── IQ tap            (traffic_iq_dma)
│   └── LSM sub-chain     (traffic_lsm_dibit_dma)
│
└── CDC & clock glue
    ├── RxIQCDC                 (sampling → sync)
    ├── RegisterCDC × N         (s_axi_lite ↔ sync per bank)
    ├── ClkNxCommonEdge         (sync → clk3x edge alignment)
    └── PulseSynchronizer × 6   (DMA IRQ pulses: sync → s_axi_lite)
```

The **critical observation** for future phases: everything interesting happens twice (control + traffic), and both instances are structurally identical. Scaling to N channels is *almost* mechanical — the current code just doesn't parameterise the duplication.

## 3. Data flow — receive chain

```
                      AD9361 @ ~8 MSPS (LVDS)
                             │
                             ▼
                      ┌──────────────┐
                      │   RxIQCDC    │   sampling → sync
                      └──────┬───────┘
                             │
                 ┌───────────┴───────────┐
                 │                       │
                 ▼                       ▼
        ┌────────────────┐      ┌────────────────┐
        │  Control DDC   │      │  Traffic DDC   │
        │   (P25DDC v2)  │      │   (P25DDC v2)  │
        │  ÷128 → 62.5   │      │  ÷128 → 62.5   │
        │     kSPS       │      │     kSPS       │
        └──┬─────────┬───┘      └──┬─────────┬───┘
           │         │             │         │
     ┌─────┤         ├─ IQ tap ────┤         ├─ IQ tap
     │     │         │             │         │
     ▼     ▼         ▼             ▼         ▼
 C4FM    LSM      IQPacker     C4FM     LSM     IQPacker
  │      │           │          │       │          │
  ▼      ▼           ▼          ▼       ▼          ▼
 dibit  dibit       iq        dibit   dibit       iq
  DMA    DMA        DMA        DMA     DMA        DMA
 0x1700 0x1A00    0x1900     0x1800  0x1B00     0x1C00
```

The LSM sub-chain expanded:

```
[DDC out 62.5 kSPS]
        │
        ▼
  LsmDecimator2 (strobe-gated by lsm_enable)
        │ 31.25 kSPS
        ▼
  LsmFir  (LPF, 83 taps)
        │
        ▼
  LsmFir  (RRC, 105 taps)
        │
        ▼
  LsmDemod
    ├── LsmDcBlocker (I, Q; runtime bypass)
    ├── LsmDemodLoop
    │     ├── LsmTimingInterp   (4-way Lagrange)
    │     ├── LsmAgc             (Phase 10 gate)
    │     ├── LsmDiffDemodSlicer (mid + curr)
    │     ├── LsmPllRotate ×2
    │     ├── LsmGardnerTed
    │     └── LsmPllUpdate
    │           └── LsmCordicAtan2
    └── LsmNidPipeline
          ├── LsmSyncNidExtract (48-bit hard sync)
          └── LsmNidBchFec      (BCH(63,16,23) ML)
        │
        ▼ dibits @ 4800 sym/s
  DibitPacker → lsm_dibit_dma (ring 0x1A00_0000, 32 KB)
```

The C4FM sub-chain is the simpler classical path: `C4FMDemod → SymbolTimingRecovery → DibitPacker → dibit_dma`. It is queued for retirement once the LSM path is confirmed to decode C4FM on ≥3 sites; see [project_c4fm_stack_cleanup_todo](../C:/Users/Andy/.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/project_c4fm_stack_cleanup_todo.md) in auto-memory.

## 4. Clock domain architecture

### 4.1 Clocks

| Clock | Nominal freq | Source | Amaranth domain | Role |
|-------|-------------:|--------|-----------------|------|
| `clk_fpga_0` | 100 MHz | PS7 FCLKCLK[0] | `s_axi_lite` | Register bridge, control-plane logic |
| `clk_fpga_1` | 200 MHz | PS7 FCLKCLK[1] | — | Upstream of `clk3x` MMCM (not directly used) |
| `clk3x` | ~187.5 MHz | MMCM from `clk_fpga_1` | `clk3x` | DDC 3× edge processing |
| `sync` (aka `clk_out1`) | ~62.5 MHz | MMCM | `sync` | Main FPGA baseband domain — everything after DDC |
| `rx_clk` | 250 MHz | AD9361 LVDS | — | RX interface, converted to sync via ADI fabric |
| `sampling` | ~62.5 MHz | derived | `sampling` | RX IQ domain pre-CDC |

**Note on documentation:** the two sub-reviewers disagreed slightly on LVDS-side rates (one cited 15.36 MHz, the other 8 MSPS). The authoritative number for *FPGA-side processing* is the `sync` domain at ~62.5 MHz; the post-DDC IQ rate is 62.5 kSPS (÷128) and the post-LSM-decimator IQ rate is 31.25 kSPS. Confirm the LVDS interface configuration from the Vivado block design before touching the AD9361 setup.

### 4.2 CDC crossings

| # | Source → Dest | Signals | Mechanism | Location |
|---|---------------|---------|-----------|----------|
| 1 | `sampling` → `sync` | `re_in`, `im_in`, strobe | `RxIQCDC` (async FIFO) | p25_top.py RxIQCDC instance |
| 2 | `s_axi_lite` → `sync` | register writes per bank | `RegisterCDC` ×N | one per bank |
| 3 | `sync` → `s_axi_lite` | 6× DMA interrupt pulses | `PulseSynchronizer` | [p25_top.py:889-933](../maia-hdl/p25_hdl/p25_top.py#L889-L933) |
| 4 | `sync` ↔ `clk3x` | DDC input strobe + output | `ClkNxCommonEdge` + pipeline | DDC instantiation |

There is one **explicit comment** at p25_top.py:889 acknowledging that Phase-10-prep closed timing "by placement luck." The fix uses `PulseSynchronizer` correctly; the remaining risk is verifying worst negative slack on the IRQ nets after every major change. Add this to the bake checklist.

### 4.3 DomainRenamer usage

- `s_axi_lite_renamer` wraps `Axi4LiteRegisterBridge` and `control_registers` so the AXI-Lite side runs in its own domain (p25_top.py ~line 745).
- **No `DomainRenamer` wraps any LSM pipeline stage.** The Phase 8C attempt at a local clock domain for LSM reset was reverted (91.7% → 24.8% CRC regression); both control (Phase 8C.1) and traffic (2026-04-16) live in the global `sync` domain and use explicit `reset_in` pulses instead.

## 5. Reset strategy

Two reset concepts coexist:

1. **Implicit domain reset** — active on power-on; resets everything except `reset_less=True` signals.
2. **Explicit `reset_in` pulse** — a 1-cycle pulse field on the control register, routed into modules whose state must survive domain reset (the PLL accumulator, timing FIFO, sync register, BCH sweep state). Write-1-to-pulse semantics.

### 5.1 Modules that consume `reset_in`

- `LsmTimingInterp` — clears sample-point FIFO and `sample_point` register
- `LsmPllUpdate` — clears PLL accumulator `pll_reg`
- `LsmDiffDemodSlicer` — clears `prev_re`, `prev_im`
- `LsmSyncNidExtract` — clears the 48-bit sync shift register
- `LsmNidBchFec` — clears BCH sweep state
- `LsmDemodLoop` / `LsmDemod` — wire `reset_in` down to the above

### 5.2 Register wiring

- Control: `lsm_registers['lsm_control']['lsm_reset']` — `Access.Wpulse` at bit 2
- Traffic: `traffic_lsm_registers['traffic_lsm_control']['traffic_lsm_reset']` — `Access.Wpulse` at bit 3

### 5.3 Retune sequencing contract

The PS-side retune flow (applied on every traffic DDC retune and on operator-initiated control retune) **must** be:

1. Write new `ddc_frequency` + `ddc_decimation` + `ddc_control`.
2. Pulse `lsm_reset` (write 1 to Wpulse field).
3. Write `lsm_enable` + `lsm_dc_block_enable` + `lsm_agc_enable` (same or separate transactions).

Rationale: the reset pulse must land after the DDC tuning words are stable, so the post-retune LSM acquisition starts with zeroed PLL/timing/sync state on the new signal. Any other order can cause the LSM chain to briefly acquire on the *old* signal, producing transient garbage dibits on the first symbols after retune.

This contract is not currently enforced or documented anywhere but the Amaranth comments. It should be surfaced in a Rust-side helper and in `doc/P25_ADDRESS_MAP.md`.

## 6. Register map

The P25Core exposes an AXI4-Lite slave with 7-bit byte address (`128` registers × 4 bytes = 512 bytes total). Eight banks live at 32-byte strides:

| Offset | Bank | Registers | Purpose |
|--------|------|-----------|---------|
| `0x00` | `control_registers` | `product_id` (R), `version` (R), `control` (RW), `interrupts` (Rsticky ×6) | Top-level ID + IRQ status |
| `0x20` | `sdr_registers` | `ddc_coeff_addr`, `ddc_coeff` (Wpulse + data), `ddc_decimation`, `ddc_frequency`, `ddc_control` | Control DDC tuning |
| `0x40` | `demod_registers` | `demod_status` (R + Rsticky), `demod_control` (RW), `dibit_next_address` (R) | Control C4FM status |
| `0x60` | `traffic_registers` | same layout as 0x20 + 0x40 but for traffic DDC + C4FM | Traffic DDC + C4FM |
| `0x80` | `iq_registers` | `iq_dma_status` (R + Rsticky), `iq_dma_control` (RW), `iq_next_address` (R) | Control IQ DMA |
| `0xA0` | `lsm_registers` | `lsm_control`, `lsm_status`, `lsm_nid`, `lsm_drop_count`, `lsm_dibit_next`, `lsm_debug`, `lsm_agc_debug` | Control LSM chain |
| `0xC0` | `traffic_lsm_registers` | same layout as 0xA0 for traffic side | Traffic LSM chain |
| `0xE0` | `traffic_iq_registers` | same layout as 0x80 for traffic side | Traffic IQ DMA |

### 6.1 LSM bank field map (per side)

`lsm_control`:

| Bit | Field | Access | Function |
|-----|-------|--------|----------|
| 0 | `lsm_enable` | RW | Gates `LsmDecimator2` strobe (level-sensitive) |
| 1 | `lsm_dibit_dma_enable` | RW | Enables `lsm_dibit_dma` AW channel |
| 2 | `lsm_reset` | Wpulse | 1-cycle pulse clears PLL/timing/sync/BCH state |
| 3 | `lsm_dc_block_enable` | RW | Enables DC blocker inside `LsmDemod` |
| 4 | `lsm_agc_enable` | RW | Enables `LsmAgc` update path (Phase 10) |

`lsm_status`:

| Bit | Field | Access | Function |
|-----|-------|--------|----------|
| 0 | `bch_busy` | R | BCH decoder in-progress |
| 1 | `in_nid_window` | R | Inside 48-bit sync window |
| 2 | `nid_event` | Rsticky | Pulse strobe → sticky; clear on read |
| 3 | `nid_valid` | R | Latched valid flag from last NID event |
| 10:4 | `n_errors` | R | BCH Hamming distance |
| 17:11 | `sync_distance` | R | 7-bit sync distance (smaller = better lock) |
| 18 | `lsm_dibit_overflow` | Rsticky | Overflow from `lsm_dibit_packer` |

`lsm_nid`:

| Bit | Field | Access |
|-----|-------|--------|
| 11:0 | `nac` | R |
| 15:12 | `duid` | R |

Debug taps (`lsm_debug`, `lsm_agc_debug`) expose `pll_dbg`, `sample_point_dbg`, `agc_gain_dbg`, `agc_mag_dbg` for dashboard rendering.

### 6.2 DMA base addresses

| Address | Stream | Ring size | Contents |
|---------|--------|-----------|----------|
| `0x1700_0000` | control C4FM dibit | 32 KB (8 × 4 KB) | C4FM dibit pairs |
| `0x1800_0000` | traffic C4FM dibit | 32 KB | as above, traffic |
| `0x1900_0000` | control IQ | 256 KB (8 × 32 KB) | post-DDC IQ pairs (62.5 kSPS) |
| `0x1A00_0000` | control LSM dibit | 32 KB | LSM dibits |
| `0x1B00_0000` | traffic LSM dibit | 32 KB | as above, traffic |
| `0x1C00_0000` | traffic IQ | 256 KB | traffic post-DDC IQ |

All DMAs route to PS7 `S_AXI_HP1` via the block-design SmartConnect. Aggregate sustained bandwidth is well under the HP interface capacity (dibit streams at ~1 KB/s each; IQ streams at ~250 KB/s each).

## 7. Test infrastructure

Every module listed in §1 has a matching `test_<name>.py` under [maia-hdl/test/](../maia-hdl/test/), all using `amaranth.sim`. Notable characteristics:

- **Per-module coverage is strong** for the LSM chain — AGC, TED, PLL rotate, CORDIC, NID pipeline all have dedicated tests.
- `test_p25ddc.py` covers the v2 fork specifically (unit-DC-gain coefficients + the three macc_trunc stages).
- `MAIA_HDL_SLOW_TESTS=1` gating exists but no P25 test currently uses it.

### 7.1 Gaps (non-exhaustive)

1. **No integration test for `P25Core`.** The only route to full-IP verification is on-target or Vivado-side RTL simulation. An `amaranth.sim` harness that drives a captured IQ stream through the top-level (both chains, all 6 DMAs) and validates dibit output would catch regressions like the 8C → 8C.1 CRC cliff before they hit a bake.
2. **No regression test for the 8C revert.** The `DomainRenamer`-based reset that broke CRC is not guarded by any test — a future refactor that re-introduces the pattern will reproduce the cliff silently.
3. **No test for traffic↔control symmetry.** Phase 7A.2 added traffic as an exact mirror; no automated check that they stay in sync.
4. **CDC paths rely on Amaranth stdlib synchronisers.** No custom tests for P25-specific CDC sequencing (e.g. register-write-before-DDC-reset ordering).
5. **C4FM chain coverage is thin.** Fine given the queued retirement; tests should be demoted to "legacy" rather than maintained as if they guarantee current behaviour.

## 8. Build pipeline

```
maia-hdl/p25_hdl/*.py      (Amaranth source)
        │
        │  Step A: amaranth.back.verilog
        ▼
maia-hdl/ip/p25-core/default/p25_core.v
        │
        │  Step B: Vivado IP package (component.xml, xgui/*.tcl)
        ▼
Vivado IP catalog: p25_core v0.1.0
        │
        │  Step C: projects/fishball7020_p25/system_bd.tcl
        │          wraps p25_core + axi_ad9361 + PS7
        ▼
Vivado project (synthesis → P&R → bitstream)
        │
        │  Step D: write_bitstream, write_hw_platform
        ▼
system_wrapper.bit + system_wrapper.xsa
```

### 8.1 Known gotchas

1. **`p25_core.v` regeneration is NOT automatic.** `build_fpga.bat --p25` relies on an explicit Verilog regeneration step. If the regen step is skipped (cached, incremental build), the packaged IP will be stale even though the Amaranth source was edited. A PowerShell helper ([tools/check_verilog_stale.ps1](../tools/check_verilog_stale.ps1)) exists but is not wired into the .bat flow. See [feedback_p25_verilog_regen](../C:/Users/Andy/.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/feedback_p25_verilog_regen.md).
2. **The v2 DDC coefficient convention is "peak tap at 131071", NOT unit DC gain.** The Amaranth-side filter generator ([tools/p25_ddc_filter_design.py](../tools/p25_ddc_filter_design.py)) emits both conventions; any downstream consumer must use the peak-scaled variant for the Maia DDC interface. One full bake was lost to diagnosing this in Phase 10.
3. **[build_fpga.bat:509](../build_fpga.bat#L509) IP clarification** — `192.168.120.50` is the board's Ethernet interface, `192.168.2.1` is RNDIS-over-USB. Both are valid; the .bat now lists RNDIS as primary with an Ethernet note. See `CODE_REVIEW_2026_04_16.md §1.4` (was mis-labelled "stale" in the original review).

### 8.2 Timing closure

- Target: all sync-domain logic closed at 62.5 MHz; `clk3x` path closed at ~187.5 MHz; DDC DSP pipelines must close internally.
- Current margin: tight on the CDC IRQ path post-Phase-10-prep (placement-luck comment at p25_top.py:889). Re-run `report_timing_summary` on every bake and watch `WNS` on the IRQ nets.
- Known waivers: `set_false_path` on async GPIO from PS7 to PL and on the AD9361 SPI/control GPIO; all CDC synchroniser flops are flagged `ASYNC_REG=TRUE`.

---

# PART II — Platform surface and gap analysis

## 9. Maia base HDL surface

The P25 fork sits on top of the Maia platform, which provides most of the non-P25-specific infrastructure. Contents relevant to future phases:

### 9.1 DDC building blocks

- [maia_hdl/ddc.py](../maia-hdl/maia_hdl/ddc.py) — single-input, single-output DDC: NCO mixer + 3-stage FIR decimator. Runs in `clk3x` with async I/O to sync.
- [maia_hdl/fir.py](../maia-hdl/maia_hdl/fir.py) — `Macc`, `FIR2DSP`, `FIR4DSP` — BRAM-coefficient FIR primitives. Each `Macc` = 1 DSP48E1 slice, 4-cycle latency.
- [maia_hdl/mixer.py](../maia-hdl/maia_hdl/mixer.py) — NCO-driven complex multiplier; 28-bit signed frequency tuning.
- [maia_hdl/cmult.py](../maia-hdl/maia_hdl/cmult.py) — complex multiplier; 3-4 DSP per multiply.

**Reuse verdict for channelizer:** `P25DDC` is a subclass of the Maia DDC. Instantiating N of them is trivial in Amaranth. The *input fanout* is the architectural question (see §11).

### 9.2 DMA infrastructure

- [maia_hdl/dma.py](../maia-hdl/maia_hdl/dma.py) — `DmaStreamRingWrite` (AXI-Stream → DDR ring, IRQ on sub-buffer wrap), `DmaBRAMWrite` (local BRAM → DDR burst), `DmaStreamWrite` (linear, non-ring).

**Reuse verdict:** `DmaStreamRingWrite` is the correct per-channel DMA pattern. Z7020 HP1 aggregate bandwidth at 64-bit × ~150 MHz is ~1.2 GB/s — enough for ≥10 simultaneous dibit rings plus a spectrometer stream.

### 9.3 Register infrastructure

- [maia_hdl/register.py](../maia-hdl/maia_hdl/register.py) — `Register`, `Registers`, `RegisterMap` with typed access (`R`, `RW`, `W`, `Wpulse`, `Rsticky`).
- [maia_hdl/axi4_lite.py](../maia-hdl/maia_hdl/axi4_lite.py) — AXI-Lite slave bridge; combinational, scales to 256+ registers without timing cost.
- SVD emission: generator in Maia tooling → `.svd` → `p25-pac/` Rust crate via `svd2rust`.

**Reuse verdict:** trivially scales. N-channel designs can either use N identical banks at stride offsets or one unified bank with per-channel offset arithmetic.

### 9.4 AD9361 interface

- Uses `adi-hdl/library/axi_ad9361/` (Verilog, from the pinned ADI `hdl_2023_r2` branch).
- Fishball is configured 2R2T LVDS with one RX channel wired to the RF port; RX2 is deserialised but currently unused.
- Width slice from 16-bit (sign-extended) to 12-bit at the Maia DDC input loses 4 MSBs — intentional headroom for AGC, but may want to revisit for channelizer-grade SNR.

**Reuse verdict:** single-chip, 2-RX ceiling. True multi-antenna requires an external RF front-end or a hardware change — not possible in pure HDL.

### 9.5 FFT + spectrometer

- [maia_hdl/fft.py](../maia-hdl/maia_hdl/fft.py) — Radix-2 DIF FFT, configurable order, Blackman-Harris window. Spectrometer typical: order 10 (1024 points) or 12 (4096 points).
- [maia_hdl/spectrum_integrator.py](../maia-hdl/maia_hdl/spectrum_integrator.py) — magnitude-squared integrator; peak or average.
- [maia_hdl/spectrometer.py](../maia-hdl/maia_hdl/spectrometer.py) — wires FFT + integrator + `DmaBRAMWrite`.

**Reuse verdict:** the block is production-tested and well-specified. P25 currently does not instantiate it (see `project_wideband_fft_display_todo` in auto-memory). Restoring it alongside `p25_core` is the cleanest path to a waterfall display. It is also *potentially* the front-end for an FFT-based polyphase channelizer (§11 option 2), but post-FFT per-channel synthesis is non-trivial.

### 9.6 Recorder + packer

- [maia_hdl/recorder.py](../maia-hdl/maia_hdl/recorder.py) — dual-clock IQ recorder with width-packing modes (16/12/8-bit), linear DMA.
- [maia_hdl/packer.py](../maia-hdl/maia_hdl/packer.py) — `Pack16IQto32` etc.

**Reuse verdict:** instantiate per channel for on-FPGA archive capture. Bandwidth-trivial.

### 9.7 Common utilities

- `PulseSynchronizer`, `FFSynchronizer` (Amaranth stdlib)
- `RegisterCDC`, `RxIQCDC` (Maia)
- `ClkNxCommonEdge` — produces common_edge signal for N× sub-clock domains
- `SkidBuffer`, `AsyncFifo18_36` — standard handshake/FIFO primitives

**Reuse verdict:** foundational. Zero concern for N-channel scale.

### 9.8 ADI HDL inventory

Submoduled at `maia-hdl/adi-hdl/`. Used by Maia:

- `axi_ad9361` — RX/TX LVDS + SPI (mandatory)
- `axi_dmac` — ADI's DMA controller (used by Maia's non-P25 IIO path; P25 uses Maia's own `DmaStreamRingWrite`)
- `util_wfifo`, `util_rfifo` — LVDS ↔ sampling CDC
- `util_cpack2`, `util_upack2` — lane aggregation/deaggregation
- `util_clkdiv`, `util_reduced_logic`, `util_vector_logic` — glue

**Not present:** no polyphase channelizer, no multi-channel DDC farm, no parallel FIR bank primitive. If we want a polyphase channelizer we write it ourselves (or port one).

## 10. Gap analysis — 10-channel target vs current

The stated final goal is ~10 concurrent traffic channels (from project memory `project_fishball_p25`). The current state is **1 control + 1 traffic = 2 channels**. Gap:

| Axis | Current | Target | Gap |
|------|---------|--------|-----|
| Simultaneous traffic chains | 1 | ~10 | +9 chains |
| DDC instances | 2 | 11 (1 control + 10 traffic) | +9 DDCs |
| LSM demod chains | 2 | 11 | +9 LSM chains |
| Dibit DMA rings | 3 | 11 | +8 rings |
| IQ DMA taps | 2 | likely 2–3 (diagnostic) | no scale |
| Register banks | 8 | ~17 | +9 (one per traffic chain) |
| NID BCH decoder instances | 2 | 11 or 1 arbitrated | design decision |
| Total DSP | ~60 est | ~150–180 est | see §12 |
| Critical path | CDC IRQ (placement-luck) | TBD | re-verify |

A naive "instantiate everything 10 times" approach is the simplest path (see §11 option 1) and the gap analysis above treats it that way. More clever architectures (§11 options 2–4) reduce some axes but introduce integration cost.

### 10.1 Secondary goals that compound with the primary one

- **Wideband FFT spectrometer restore** — project memory `project_wideband_fft_display_todo`.
- **C4FM stack retirement** — frees ~2 DSP + ~500 LUT per chain.
- **Soft-sync correlator** — project memory notes current sync is hard-only; soft-sync helps low-SNR sites.
- **Direct traffic tune mode** — `project_direct_traffic_tune_todo` — park on a fixed frequency without control-channel grant.
- **Encryption flag from HDU** — Phase 7C.2 (deferred).
- **NTP-on-boot** — firmware-level, not HDL.

---

# PART III — Future-phase roadmap

## 11. Polyphase channelizer — design space

### 11.1 Reference: SDRTrunk's software channelizer

SDRTrunk's `PolyphaseChannelManager` (Java) implements a **non-maximally-decimated polyphase filter bank + IFFT** channelizer. Salient parameters (from the survey):

- Input: 4–5 MSPS complex
- M (channels): `floor(sample_rate / 25 kHz)`, enforced even → ~200 channels at 5 MSPS
- Channel spacing: 25 kHz (2× oversampled from P25's 12.5 kHz)
- Prototype filter: 9 taps per channel, sinc-windowed for perfect reconstruction
- IFFT size: M/2 (≈100–256 typically)
- Output rate per channel: 25 kHz (2× P25 nominal)
- Dispatch: batches 1024 polyphase outputs per IFFT call (~20 ms at 5 MSPS)

Key characteristics:

- **Excellent DSP efficiency** when you want all M channels visible simultaneously.
- **Bin-boundary problem**: a 12.5 kHz P25 channel can straddle two polyphase bins, forcing a synthesis-filter stage that spans adjacent bins and reassembles them via complex weighting. SDRTrunk does this in Java; in hardware it translates to +1–2 DSP per synthesised channel and a non-trivial control-flow FSM.
- **Latency**: ~45 ms end-to-end in SDRTrunk. HDL implementation will be much lower (no Java GC, pipeline not batch).

**ADI HDL does not ship a polyphase channelizer primitive.** Nothing in `adi-hdl/library/` matches. Maia does not have one either.

### 11.2 Architecture options

Four viable architectures. Each trades DSP-per-channel against integration complexity.

#### Option A — Brute-force parallel DDC bank

Instantiate N independent `P25DDC` instances, each fed the same wideband input, each tuned to a different NCO frequency.

```
         AD9361 wideband ───────┬─────┬─────┬─── ...
                                │     │     │
                                ▼     ▼     ▼
                              DDC#1 DDC#2 DDC#3 ...  (N copies)
                                │     │     │
                                ▼     ▼     ▼
                              LSM#1 LSM#2 LSM#3 ...
                                │     │     │
                                ▼     ▼     ▼
                              DMA   DMA   DMA  ...
```

Pros:

- Minimal architectural change — literally N copies of today's chain.
- Any channel frequency; no grid lock.
- Latency is identical to current per-channel latency.
- Reuses 100% of existing Amaranth source.

Cons:

- DSP scales linearly (each DDC ~13–18 DSP; 10 channels ≈ 130–180 DSP, ~60–82% of Z7020).
- Every DDC computes its full FIR cascade even for idle channels — no amortisation.
- Ceiling at ~10 channels on Z7020; 16 channels won't fit.

**When to pick:** if the goal is ≤8 channels and schedule pressure outweighs DSP efficiency. Highest confidence of shipping on time.

#### Option B — FFT-based polyphase channelizer

Single wide-FFT + uniform channel extraction. The Maia FFT block is the natural front-end.

```
AD9361 wideband ──► FFT (N-point) ──► bin-combining synthesis ──► N channel streams
                                                                     │    │    │
                                                                     ▼    ▼    ▼
                                                                   LSM  LSM  LSM ...
```

Pros:

- O(log N) DSP cost for the FFT itself vs O(N) for option A.
- Restores the spectrometer "for free" — the FFT is dual-use for waterfall display.
- Scales well beyond 10 channels.

Cons:

- **Channels are grid-locked to FFT bins.** At 8 MSPS / 4096 points you get ~1953 Hz bins — a P25 channel spans 6–7 bins and each extracted channel needs a synthesis filter to combine them.
- **Synthesis filter design is non-trivial.** 4-tap Hann is a starting point but likely inadequate for dense grids; expect to need 8–16-tap Kaiser or similar, pushing DSP cost back up.
- **Batch latency.** FFT adds ~512 μs to the path. Acceptable for P25 (symbol period is 208 μs, frame is 30 ms) but must be budgeted.
- **Integration cost is high.** The post-FFT synthesis is entirely new Amaranth code; none of the current LSM-side modules need to change, but the glue is substantial.

**When to pick:** if the 10-channel goal is firm *and* a wider site (20+ channels) is foreseeable. The FFT block is already in Maia, so this is the *most future-proof* option.

#### Option C — Hybrid: coarse DDC + fine DDC bank

One wide coarse DDC extracts a sub-band; N fine DDCs extract individual channels within that sub-band.

```
AD9361 ─► Coarse DDC (÷16) ─► 500 kSPS sub-band ─┬─► Fine DDC#1 ─► LSM#1
                                                  ├─► Fine DDC#2 ─► LSM#2
                                                  ...
                                                  └─► Fine DDC#N ─► LSM#N
```

Pros:

- Fine DDCs only need modest decimation (÷8) → cheaper per instance than a full Maia DDC.
- Cumulative DSP cost between options A and B.
- Flexible: if channels shift, retune the coarse DDC.
- No grid lock within the sub-band.

Cons:

- Channels must fit within one coarse sub-band; widely-scattered channels (e.g. 851 MHz + 858 MHz together) break the model.
- Two-level PS tuning logic.
- New "small DDC" module to write and test (Maia's DDC is a single parameterised thing; dropping stages requires fork or refactor).

**When to pick:** if the target sites all cluster channels in a <1 MHz window. Realistic for most P25 trunked systems, but needs a survey to confirm.

#### Option D — Hardware FFT + software demod

Ship the wideband FFT to PS via DMA; do channel selection + C4FM/LSM/symbol-timing entirely in Rust on the A9.

```
AD9361 ─► FFT ─► DMA ring ─► PS Rust (per-channel select + demod)
```

Pros:

- Minimal FPGA resource use (just the FFT).
- Arbitrary channel count, limited only by A9 CPU.
- Trivially extensible.

Cons:

- **Completely replaces the current HDL demod chain** — throws away the LSM gateware work.
- A9 CPU headroom at 10× software LSM demod chains is unknown; project memory notes libiio CPU overhead is already non-trivial.
- Higher latency and jitter than hardware demod.
- DMA bandwidth is non-trivial (4 MB/s sustained for 32-bit FFT bins at 1 MSPS output rate).

**When to pick:** if the hardware LSM chain turns out to be insufficient for some edge case and we need to fall back to software. Not a forward path; a contingency.

### 11.3 Recommendation

**Option A (brute-force DDC bank) for the 10-channel goal** — combined with a **parallel Option B prototype** for the future.

Rationale:

- Option A is the lowest-risk path to a working 10-channel receiver because it reuses everything we already have.
- The DSP budget (~150–180 DSP for 10 channels) fits on Z7020 with slack, provided we retire C4FM and share the NID BCH decoder across chains (see §12).
- Option B is the right *eventual* architecture but carries real synthesis-filter design risk. Prototyping it in parallel — initially as a 2-channel hand-built polyphase validated against an Option A baseline — de-risks the eventual migration.
- Don't pick Option C unless a site survey confirms tight channel clustering. Don't pick Option D unless hardware LSM proves unworkable.

The concrete phased plan in §13 follows this recommendation.

## 12. Z7020 resource budget

**Confidence note:** the numbers in this section are derived from the per-module docstring estimates in the current code plus the sub-reviewers' design-rule sizing. Treat them as *order-of-magnitude* until a Vivado utilisation report from the current bitstream is in hand. The §12 estimates disagreed slightly between sub-reviewers (estimates ranged from 34 to 60 DSP for the current P25 core, depending on whether spectrometer was included); resolve by running `report_utilization` post-place-and-route on the current bitstream and updating this document.

### 12.1 Z7020 capacity (hard)

| Resource | Count |
|----------|------:|
| DSP48E1 slices | 220 |
| BRAM36 tiles | 140 (≈ 280 × BRAM18) |
| LUT (6-input) | 53,200 |
| Flip-flops | 106,400 |
| Slices | 13,300 |

### 12.2 Estimated current (Phase 10) utilisation

| Component | DSP | BRAM | LUT | FF |
|-----------|----:|-----:|----:|---:|
| AD9361 + ADI glue + CDC | 0 | 8 | ~2,000 | ~1,500 |
| 2× P25DDC | ~30 | ~12 | ~6,000 | ~3,000 |
| 2× LSM chain (incl. FIRs, AGC, PLL, NID BCH) | ~16 | ~6 | ~6,000 | ~3,000 |
| 2× C4FM + SymbolTiming + DibitPacker | ~10 | 0 | ~3,000 | ~1,500 |
| 6× DMA (ring write) | 0 | ~3 | ~2,000 | ~1,200 |
| Registers + AXI-Lite bridge | 0 | 0 | ~1,500 | ~800 |
| **Total (estimate)** | **~60** | **~29** | **~22,000** | **~11,000** |
| **% of Z7020** | **~27%** | **~21%** | **~41%** | **~10%** |

### 12.3 Phase roadmap budget projection

Target phases (see §13 for full definitions):

- **P11** — polyphase channelizer front-end (Option A: 9 more DDCs; or Option B: FFT + synthesis filters)
- **P12** — LSM demod chain replicated to match channel count, with shared NID BCH arbitration
- **P13** — wideband spectrometer restored (reuse Maia block)
- **P14** — soft-sync correlator upgrade (optional)
- **P15** — C4FM stack retirement (subtractive)

**Option A (brute-force) projection:**

| Phase | Added DSP | Added BRAM | Added LUT | Running DSP % | Running LUT % | Notes |
|-------|----------:|-----------:|----------:|--------------:|--------------:|-------|
| P10 (baseline) | — | — | — | 27% | 41% | estimate |
| P11 (+9 DDCs) | ~135 | ~54 | ~27,000 | 88% | 92% | NOT FEASIBLE as stated |
| P15 (retire C4FM, −10 chains) | −20 | 0 | −5,000 | 79% | 82% | — |
| P12 (shared NID BCH, not per-chain) | 0 | 0 | 0 | 79% | 82% | savings baked in |
| P13 (spectrometer FFT-2048) | +8 | +6 | +2,500 | 83% | 87% | fits |
| P14 (soft-sync) | +8 | +2 | +1,500 | 86% | 90% | tight |

**Option A is too aggressive at 10 channels as a pure scale-out.** The estimate breaks 100% LUT even after C4FM retirement. Two mitigations:

1. Target **6–8 channels**, not 10, on Z7020. Budgets then fit with healthy slack.
2. **Share the NID BCH decoder + FIR cascade across chains.** Each channel's raw dibit stream can be arbitrated into one BCH decoder (since NID decode is infrequent relative to symbol rate). FIR coefficients are identical across chains — the coefficient RAM can be shared. Both savings combined can drop the per-chain cost from ~18 DSP + ~3000 LUT to ~10 DSP + ~2000 LUT.

**Option B (FFT-based) projection:**

| Phase | Added DSP | Added BRAM | Added LUT | Running DSP % | Running LUT % | Notes |
|-------|----------:|-----------:|----------:|--------------:|--------------:|-------|
| P10 (baseline) | — | — | — | 27% | 41% | estimate |
| P11 (FFT + 10× synthesis filters, 4-tap) | +56 | +22 | +11,000 | 52% | 61% | Fits |
| P12 (LSM ×10, shared NID BCH) | +30 | +10 | +15,000 | 65% | 89% | Tight |
| P15 (retire C4FM) | −10 | 0 | −3,000 | 60% | 83% | — |
| P13 (spectrometer — reuse FFT from P11) | 0 | 0 | +500 | 60% | 84% | Free |
| P14 (soft-sync) | +8 | +2 | +1,500 | 64% | 86% | Fits |

**Option B is viable for 10 channels.** The key enabler is reusing the FFT block for both the channelizer front-end and the waterfall display — this is what makes the DSP budget close. LUT is still the binding constraint; FIR synthesis filter depth must be kept modest, and all register/glue logic must be tight.

### 12.4 Conclusion on budget

- **10-channel Option A does not fit Z7020** at reasonable synthesis-filter quality without aggressive sharing.
- **10-channel Option B fits with slack**, given a 4-tap synthesis filter and FFT-2048. If synthesis quality demands 8-tap Kaiser, LUT budget pushes into the 95%+ zone — verify with a real synthesis run before committing.
- **A hardware upgrade to Zynq-7030** (53 kLUT → 78 kLUT, 220 DSP → 400 DSP) opens option A cleanly and gives Option B plenty of slack. If the final product targets 16+ channels, this is the path.

## 13. Phased roadmap

The phase numbers below extend the existing P1–P10 sequence. Each phase is a single bake/flash boundary.

### Phase 11 — Polyphase channelizer front-end (Option B prototype)

**Scope:** Instantiate Maia's FFT block alongside `p25_core`. Wire it into a *diagnostic* DMA (spectrum display only — not yet routing channels). Write and test a single synthesis filter stage that extracts one channel from the FFT output and feeds it into the existing LSM chain.

**Acceptance:** the synthesised "extract one channel" path decodes a known-good P25 IQ recording with CRC parity comparable to the current DDC-based chain on the same recording.

**Risk:** synthesis filter design — you may need 8+ tap Kaiser rather than 4-tap Hann; budget for iteration.

**Effort:** 3–4 weeks Amaranth + Vivado; 1 bake + on-target validation.

### Phase 12 — N-channel parameterisation

**Scope:** Refactor `p25_top.py` so that the number of traffic chains is a parameter. Add a shared NID BCH arbitrator so only one BCH decoder runs across all chains. Keep the structure compatible with both Option A (per-chain DDC) and Option B (per-chain synthesis filter). Validate at N=2 (current) and N=4.

**Acceptance:** N=4 synthesises and closes timing; all 4 chains decode independently in a multi-call simulation.

**Risk:** register map explosion — need to commit to either "N identical banks" or "one unified bank with per-chain offsets". Recommend the latter for SVD sanity.

**Effort:** 2–3 weeks, mostly refactor.

### Phase 13 — 8-channel synthesis deployment

**Scope:** Raise N from 4 to 8 (Option B FFT-extracted channels). Validate on-target on Clay County or FP&L — measure end-to-end decode rate when multiple simultaneous calls are active.

**Acceptance:** 8 concurrent calls successfully decode on at least one production site.

**Risk:** timing closure at 8 — the 62.5 MHz `sync` domain has headroom, but the `clk3x` domain (DDC) may tighten. Also: register bank scale may bump AXI-Lite address width.

**Effort:** 2 weeks, then field validation.

### Phase 14 — Full 10-channel + spectrometer

**Scope:** Raise N to 10 (or the practical ceiling if timing/LUT won't close). Fully wire the spectrometer to its own DMA with dashboard waterfall.

**Acceptance:** 10 concurrent calls decoded; waterfall renders live in dashboard.

**Risk:** budget overrun — see §12. Have a pre-decided fallback (reduce to 8, or step down synthesis filter depth) documented before bake.

**Effort:** 2 weeks.

### Phase 15 — C4FM retirement + soft-sync upgrade

**Scope:** Remove `C4FMDemod`, `SymbolTimingRecovery` (C4FM-dedicated instances), legacy dibit DMAs, and dashboard PS-C4FM column. Add `LsmSoftSyncCorrelator` alongside the hard-sync extractor; evaluate on low-SNR sites.

**Acceptance:** decoded-call count on a known low-SNR site improves relative to Phase 14 baseline; resource budget drops.

**Risk:** low; removal is mostly additive-by-subtraction. Soft-sync is optional — if the LUT budget won't close with it, defer.

**Effort:** 1 week C4FM retirement + 2 weeks soft-sync.

### Phase 16+ — Direct traffic tune, HDU encryption parse, and beyond

Out of HDL scope primarily — these are Rust-side features on the PS. Flagged here because they're in the project memory backlog; they will not change the HDL layout but may add one or two registers.

## 14. Risks and open questions

### 14.1 Architecture risks

- **Synthesis-filter design (Option B P11).** The 4-tap Hann starting point may be inadequate for dense P25 grids. If adjacent-channel rejection is weak, neighbouring calls bleed into each other and decode rate plummets. Mitigation: design the prototype filter in Python against real IQ captures *before* committing to Amaranth.
- **Z7020 ceiling.** The budget projection in §12.3 has Option B at ~85% LUT by Phase 14. Any late surprise pushes it over. Mitigation: keep the Zynq-7030 upgrade path clear — architecture decisions in P11-P12 should not preclude it.
- **CDC timing closure regression.** The current placement-luck comment at p25_top.py:889 is concerning. Every phase that adds DMAs also adds IRQ nets; verify WNS per bake.
- **Shared NID BCH arbiter (P12).** Arbitrated access is fine if NID decode is infrequent (which it is — once per data unit, ~30 Hz). But if multiple chains hit the arbiter simultaneously and queue depth overflows, NID decodes are dropped silently. Mitigation: size the arbitration queue and add an overflow register bit.

### 14.2 Dependency risks

- **Maia FFT block.** Pulled from `maia_hdl/` — any upstream Maia change could break it. Fishball pins Maia via branch; confirm the pin doesn't drift during channelizer work.
- **`p25_core.v` regeneration gotcha.** Every phase will compound this. Consider wiring the Verilog staleness check into `build_fpga.bat` as a hard failure, not a warning.

### 14.3 Unknown-unknown risks

- **Z7020 utilisation baseline.** The entire §12 budget is estimates. We need a real `report_utilization` from the current P25 bitstream *before* sizing P11. Highest-priority action.
- **Clock relationships on Fishball.** Sub-reviewers disagreed on LVDS-side rates. Confirm from the Vivado block design what the actual sampling rates are; update §4 of this document.
- **AD9361 RF-bandwidth setting for 10-channel operation.** Current is 4–8 MHz depending on profile. A 10-channel receiver needs the RF bandwidth to span all channels simultaneously; 15–20 MHz may be required. Verify AD9361 filter can actually deliver that cleanly.
- **Soft-sync correlator resource cost.** Sub-reviewer estimated 8 DSP; realistic range 4–16 depending on window size. Pilot before committing to P15.

## 15. Recommendations — concrete next steps

In order, first-to-last:

1. **Run a real Vivado utilisation report on the current P25 bitstream.** Update §12 of this document with the actual DSP / BRAM / LUT / FF counts. Everything else depends on this.
2. **Verify the sample-rate architecture.** Read the block design; confirm the `sampling` domain rate and the `clk3x` rate. Update §4.
3. **Do a prototype-filter design pass in Python before any Amaranth.** For the P11 FFT + synthesis filter option, design the prototype FIR and the synthesis filter in `scipy.signal`, test adjacent-channel rejection against real P25 IQ captures (the iq_dma captures from current bitstream are perfect for this). Decide on tap count.
4. **Wire `check_verilog_stale.ps1` into `build_fpga.bat`** as a hard-failing pre-step, not an advisory. Eliminates the most costly recurring bake bug.
5. **Add a top-level integration test for `P25Core`** (a `test_p25_top.py` that drives a saved IQ recording end-to-end and asserts dibits land in the ring buffer). Acts as guardrail during the heavy refactor in P12.
6. **Lock the choice between Option A and Option B** before starting P11. Document the decision + rationale in a new `doc/changes/NNN_channelizer_architecture.md`.
7. **Commit to a per-channel register layout convention** (unified bank with channel-indexed offsets, not N identical banks). Do this in P12 refactor; it makes the SVD sane.
8. **Keep the C4FM retirement (P15) as a *subtractive* phase** — no rename-and-delete style refactors; just remove. This is the lowest-risk phase of all and provides the resource headroom P13/P14 need.

## 16. Out-of-scope items (recorded for completeness)

- **Any Rust-side work.** TrafficManager refactor, HDU encryption parse, NTP-on-boot, dashboard work — orthogonal.
- **AD9361 RF configuration** beyond confirming that a 15+ MHz BW mode closes cleanly.
- **Alternative FPGA platforms** (Zynq UltraScale, RFSoC, etc.) — out of scope unless the 10-channel goal provably cannot fit Z7020.
- **JTAG/debug infrastructure upgrades** — ChipScope/ILA placement is a separate bring-up task.

## 17. Appendix — notes on verification confidence

The three sub-reviewers' findings are cross-referenced below. Where they disagreed, both views are included and the discrepancy is flagged.

| Claim | Source(s) | Confidence | Action |
|-------|-----------|------------|--------|
| 22 P25-specific modules in `p25_hdl/` | Survey 1 | High | — |
| Current DSP utilisation ~27% | Survey 3 (estimate) | Low | Run `report_utilization` |
| 10-channel Option A exceeds LUT | Survey 3 (estimate) | Medium | Re-calc once baseline verified |
| Option B fits at 10 channels | Survey 3 (estimate with 4-tap synth) | Medium | Depends on prototype filter |
| `clk3x` ≈ 187.5 MHz | Survey 1 | Medium | Confirm from BD |
| Sampling rate from AD9361 | Surveys 1 + 2 disagree (8 MSPS vs 15.36 MHz) | Low | Confirm from BD |
| No polyphase primitive in `adi-hdl` | Survey 2 | High | — |
| Maia FFT is reusable for channelizer | Survey 2 + 3 | High for waterfall, Medium for channelizer | Prototype P11 |
| CDC IRQ closure is placement-luck | Survey 1 | High (in-source comment) | Fixed with PulseSynchronizer; re-verify WNS |

Anything marked Medium or Low confidence should be re-verified against current code or a Vivado report before acting on it. The recommended first action (§15 item 1) resolves most of the Low-confidence budget items.
