# 064 — Second traffic decode chain (core 0.3.0)

**Date:** 2026-09-27. **Branch:** fishball-p25. **Bake required:** yes (core 0.3.0).
**Tezuka rebuild required:** yes, for the new device-tree carve-out (chain 2's dibit ring).
**p25-httpd:** not changed here; the PS work is listed at the end.

The gateware gets a second, independent traffic decode chain, `traffic2_*`, so the PS can
follow two P25 voice calls at once (left speaker = chain 1, right speaker = chain 2). The
register map is a strict superset of 0.2.0: every existing register keeps its name, offset,
access and field layout, so the deployed p25-httpd runs unchanged on the new bitstream (chain
2 stays idle until the PS enables it).

## Design (maia-hdl/p25_hdl)

Chain 2 is a block-for-block copy of the traffic chain, without its two diagnostic taps:

```text
rxiq_cdc ─(1 sync-cycle register)─> traffic2_ddc (P25DDC, NCO + 3-stage FIR, 50 kSPS)
  -> traffic2_lsm_decimator (/2, 25 kSPS, strobe gated by traffic2_lsm_enable)
  -> traffic2_lsm_lpf (121-tap) -> traffic2_lsm_rrc (42-tap)
  -> traffic2_lsm_demod (LsmDemod 25 kSPS, incl. the 059 PLL/timing no-signal hold)
       -> traffic2_lsm_dibit_packer -> traffic2_lsm_dibit_dma (0x1D00_0000, m_axi_traffic2_lsm_dibit)
       -> NID event latch + status/debug registers (bank 0x140)
```

Choices:

- **Own DDC.** Chain 2 needs its own NCO to sit on a different voice channel. It shares the
  AD9361 LO, sample rate and DDC preset with the control chain and chain 1 (same IF window).
- **No diagnostic taps.** No post-DDC IQ ring and no pre-diff IQ ring on chain 2. Nothing in
  those taps is needed to decode, and chain 1's two taps are not even wired in the block
  design today (`m_axi_traffic_iq`, `m_axi_traffic_pre_diff_iq`).
- **Input register.** `traffic2_ddc` takes `rxiq_cdc.{strobe,re,im}_out` through one `sync`
  register instead of tapping the net directly. That net already drives two DDCs, the
  wideband IQ packer and the spectrometer, and the 0.2.0 worst setup path was
  `rxiq_cdc/strobe_out_reg` → spectrometer FFT (sync → clk3x, +0.255 ns). The extra 16 ns of
  latency is irrelevant.
- **Seeds live in chain 2's own bank.** Chain 2's AGC/PLL/timing seeds are words 8–10 of a
  16-word `traffic2_lsm` bank, next to `traffic2_lsm_reset`. Seed writes and the reset pulse
  therefore cross the same RegisterCDC in order, so chain 2 does not need the read-back fence
  that `write_traffic_seeds` uses between bank 8 and bank 6. Sharing chain 1's seed registers
  was rejected: the two chains sit on different channels and are retuned independently.
- **Idle by default.** Every chain 2 control bit resets to 0 (`traffic2_lsm_enable`,
  `traffic2_lsm_dibit_dma_enable`, `traffic2_enable_input`, DC block, AGC). With the enable at
  0 the decimator strobe is gated, the chain produces no dibits, the DMA issues no bursts and
  interrupt bit 8 never sets.
- **DMA: a new AXI master (option a).** `m_axi_traffic2_lsm_dibit` is one more slave port
  (S06) on the existing HP1 SmartConnect: one `ipx::associate_bus_interfaces` line in
  `package_ip.tcl` and one `ad_mem_hp1_interconnect` line in `system_bd.tcl`. No diagnostic
  feature was retired.

## Register map additions

Bank decoder: word-address bits [6:3] select a 32-byte bank. Banks 9, 10 and 11 were vacant.
Chain 2 uses bank 9 for its DDC and banks 10–11 as one 16-word bank (decoded on bits [6:4]).
Bank 15 is still vacant. Field layouts are chain 1's with `traffic_` → `traffic2_`.

| Offset | Register | Fields (bits) | Access | Reset |
|---|---|---|---|---|
| `0x00C` | `interrupts` (existing) | **new:** `traffic2_lsm_dibit_dma` [8] | Rsticky | 0 |
| `0x120` | `traffic2_ddc_coeff_addr` | `traffic2_coeff_waddr` [9:0] | RW | 0 |
| `0x128` | `traffic2_ddc_coeff` | `traffic2_coeff_wren` [0] (Wpulse), `traffic2_coeff_wdata` [18:1] | W / RW | 0 |
| `0x12C` | `traffic2_ddc_decimation` | `traffic2_decimation1` [6:0], `traffic2_decimation2` [12:7], `traffic2_decimation3` [19:13] | RW | 0 |
| `0x130` | `traffic2_ddc_frequency` | `traffic2_frequency` [27:0] | RW | 0 |
| `0x134` | `traffic2_ddc_control` | `traffic2_operations_minus_one1` [6:0], `…2` [12:7], `…3` [19:13], `traffic2_odd_operations1` [20], `traffic2_odd_operations3` [21], `traffic2_bypass2` [22], `traffic2_bypass3` [23], `traffic2_enable_input` [24] | RW | 0 |
| `0x140` | `traffic2_lsm_control` | `traffic2_lsm_enable` [0], `traffic2_lsm_dibit_dma_enable` [1], `traffic2_lsm_reset` [2] (Wpulse), `traffic2_lsm_dc_block_enable` [3], `traffic2_lsm_agc_enable` [4] | RW | 0 |
| `0x144` | `traffic2_lsm_status` | `bch_busy` [0], `in_nid_window` [1], `nid_event` [2] (Rsticky), `nid_valid` [3], `n_errors` [10:4], `sync_distance` [17:11], `traffic2_lsm_dibit_overflow` [18] (Rsticky) | R | 0 |
| `0x148` | `traffic2_lsm_nid` | `nac` [11:0], `duid` [15:12] | R | 0 |
| `0x14C` | `traffic2_lsm_drop_count` | `drop_count` [15:0], `traffic2_lsm_dibit_last_buffer` [18:16] | R | last_buffer 7 |
| `0x150` | `traffic2_lsm_dibit_next` | `next_address` [31:0] | R | `0x1D00_0000` |
| `0x154` | `traffic2_lsm_debug` | `pll_dbg` [15:0] (Q2.13), `sample_point_dbg` [31:16] | R | 0 |
| `0x158` | `traffic2_lsm_agc_debug` | `agc_gain_dbg` [15:0], `agc_mag_dbg` [31:16] | R | 0 |
| `0x15C` | `traffic2_lsm_agc_config` | `mag_update_threshold` [15:0] (also the 059 hold gate) | RW | 256 |
| `0x160` | `traffic2_lsm_agc_seed` | `agc_seed` [19:0] (Q9.11) | RW | 0 |
| `0x164` | `traffic2_lsm_pll_seed` | `pll_seed` [15:0] (Q2.13) | RW | 0 |
| `0x168` | `traffic2_lsm_timing_seed` | `timing_seed` [17:0] (Q5.12) | RW | 0 |

`version` reads `0x0000_0300` (0.3.0). `p25-httpd/p25-pac/p25.svd` is regenerated: against
0.2.0 it adds 16 registers and one field and changes only the two `<version>` strings. The
build flow (`build_hdl.bat --p25` in Docker) also regenerated `p25-pac/src/lib.rs` with
svd2rust 0.33.5. Every public item of the 0.2.0 PAC (166 fns, 57 modules, 267 types) is still
there; the only removed lines are private `_reservedNN` padding fields.

## DMA and device tree

| Ring | Base | Geometry | Master | DT reserved-memory | rxbuffer node |
|---|---|---|---|---|---|
| `traffic2_lsm_dibit_dma` | `0x1D00_0000` | 8 × 4 KB = 32 KB (same as chain 1) | `m_axi_traffic2_lsm_dibit` → HP1 S06 | `p25_traffic2_lsm_dibit_dma: p25-traffic2-lsm-dibit-dma@1d000000` (`reg = <0x1d000000 0x8000>`, `no-map`) | `p25-traffic2-lsm-dibit` (`maia-sdr,rxbuffer`, `buffer-size = <0x1000>`) |

`0x1D00_0000` is the retired Phase 10.6 `lsm_iq_dma` range, free since 2026-04-23. The
packing is identical to the other dibit rings (32 dibits per 64-bit word). `P25Config`
gains the `traffic2_lsm_dibit_dma_*` constants, and `validate()` now also asserts that no two
rings overlap.

`tezuka_fw/board/tezuka/fishball7020/dts/fishball-p25.dtsi` got the two nodes above plus a
comment line, all additive (the file also carries the user's uncommitted May edits; not
committed here). The node is harmless on a 0.2.0 bitstream (nothing writes there).

## Compatibility

- **0.2.0 PS on the 0.3.0 bitstream:** works unchanged. All 0.2.0 registers and interrupt
  bits 0–7 are where they were; chain 2 is idle, so bit 8 stays 0.
- **0.3.0 PS on a 0.2.0 bitstream:** the PS must gate every chain 2 access on
  `core_version >= 0.3.0`. On 0.2.0, banks 9–11 are vacant and a read of a vacant bank is
  never answered (no RVALID; the AXI4-Lite bridge waits for `rdone` forever), which stalls
  the CPU. The simulation confirms this for bank 15 on 0.3.0.
- **Old device tree:** a 0.3.0 PS should open `p25-traffic2-lsm-dibit` only on 0.3.0 and treat
  a missing node as "chain 2 unavailable" rather than failing `IpCore::take`.

## Resources and timing

Bake 2026-09-27 (`build_fpga_p25_pretty.sh`, Vivado 2023.2, default strategies): **timing
met, WNS +0.021 ns, WHS +0.018 ns**, 0 failing endpoints. Bitstream and
`fishball_p25.sdk/system_top.xsa` written. The XSA was not copied into tezuka_fw.

Whole design (placed):

| Resource | 0.2.0 | 0.3.0 | Δ |
|---|---|---|---|
| Slice LUTs | 23191 (43.6 %) | 28957 (54.4 %) | +5766 |
| Slice registers | 34242 (32.2 %) | 42906 (40.3 %) | +8664 |
| Slices | 10693 (80.4 %) | 12009 (90.3 %) | +1316 |
| Block RAM tiles | 53.5 (38.2 %) | 61 (43.6 %) | +7.5 (+2 RAMB36, +11 RAMB18) |
| DSP48E1 | 127 (57.7 %) | 172 (78.2 %) | +45 |

Where the increase goes (routed, hierarchical):

| Block | LUT | FF | RAMB36 | RAMB18 | DSP |
|---|---|---|---|---|---|
| `traffic2_ddc` | 394 | 575 | 0 | 10 | 11 |
| `traffic2_lsm_decimator` | 0 | 34 | 0 | 0 | 0 |
| `traffic2_lsm_lpf` | 1219 | 3937 | 0 | 0 | 2 |
| `traffic2_lsm_rrc` | 613 | 1387 | 0 | 0 | 2 |
| `traffic2_lsm_demod` | 3024 | 1629 | 1 | 0 | 30 |
| packer + DMA + IRQ sync | 35 | 167 | 0 | 0 | 0 |
| register banks + CDCs (9, 10–11) | 291 | 515 | 0 | 0 | 0 |
| **p25_core total** | 14335 → 19878 (+5543) | 20252 → 28525 (+8273) | 24 → 25 | 25 → 35 | 105 → 150 |
| HP1 SmartConnect (slave port S06) | 1547 → 1762 (+215) | 2461 → 2852 (+391) | 6 → 7 | 6 → 7 | 0 |

The chain 2 blocks match chain 1's (`traffic_lsm_demod` 3006 LUT / 30 DSP, `traffic_lsm_lpf`
1221 LUT / 3939 FF). The LPF is the largest FF user: its 121-tap delay line is 3.9k FFs
because the variable-index read keeps Vivado from inferring SRLs.

Worst paths per clock (setup / hold, ns):

| Clock group | 0.2.0 | 0.3.0 | Worst 0.3.0 path |
|---|---|---|---|
| sync → clk3x | +0.255 / +0.111 | **+0.021** / +0.056 | `wideband_spec_registers/spec_control/field_spec_enable_reg` → `wideband_spec/fft/twiddle1/cmult/im_out_reg[17]/CE`, 1 LUT, 86 % route |
| sync (62.5 MHz) | +1.062 / +0.006 | +0.908 / +0.018 | `traffic_lsm_demod/.../timing/s1_b_cur_im_reg` → `agc/sq_reg_reg` DSP (chain 1's known AGC cone) |
| clk3x (187.5 MHz) | +0.568 / +0.034 | +0.302 / +0.042 | `common_edge_3x/pulse_del_reg` → spectrometer `twiddle0/cmult/dsp` |
| rx_clk | +0.568 / +0.055 | +0.353 / +0.057 | AD9361 interface (placement) |

No chain 2 path is among the worst. The limiting path is the same spectrometer cone as in 0.2.0:
a sync-domain enable/strobe fanning out to clk3x clock enables across the FFT. It lost
0.23 ns because the fuller device (90 % of slices) spreads the FFT out. The design meets
timing, but with 21 ps of margin the next change can fail on this cone. Cheap fixes if it
does, none of them tried here:

- Register the spectrometer's `strobe_in` / `re_in` / `im_in` in `sync` in `p25_top` (as
  done for `traffic2_ddc`), plus a `max_fanout` on the CE driver inside the spectrometer.
- Enable post-route `phys_opt_design`. It was skipped this run because WNS was positive
  after placement.
- Use `rerun_with_strategy.tcl` in the project dir.

## Tests (maia-hdl/test)

`test_p25_top.py` (new, 23 tests, ~50 s). The 0.2.0 register map is frozen in
`golden_vectors/p25_core_0.2.0.svd`.

- **Config:** default validates; chain 2 ring at 0x1D00_0000 with chain 1's geometry; no
  two rings overlap; an overlapping ring is rejected.
- **Register map (SVD):**
  - Every 0.2.0 register keeps name, offset, access and fields; new fields only use
    previously unused bits.
  - The only additions are the 16 `traffic2_*` registers, at new offsets, with chain 1's
    field layout.
  - Interrupt bit 8; offsets unique, word-aligned and below 0x200.
  - `p25-pac/p25.svd` equals the generated SVD.
  - `build_fpga.bat` packages the same version as `p25_top._version`.
- **Elaboration:** converts to Verilog; the `m_axi_traffic2_lsm_dibit_*` ports exist next to
  every 0.2.0 master; chain 2's modules, including `signal_hold`, are present;
  `package_ip.tcl` associates the new master and `system_bd.tcl` wires it (and the six 0.2.0
  HP1 masters) to HP1.
- **Full-core simulation** (AXI4-Lite through the real bridge, bank decoder and CDCs):
  - version register;
  - chain 2 reset values: agc_config 256, last_buffer 7, next_address 0x1D00_0000;
  - read-back of every new RW register plus chain 1 and bank 8 registers, with no aliasing
    between them;
  - chain 2 DDC / seed / threshold / enable registers drive chain 2's ports and not chain
    1's; `traffic2_lsm_reset` pulses only chain 2's `reset_in` and chain 1's reset only
    chain 1's;
  - DMA: gated by `traffic2_lsm_dibit_dma_enable`, first burst at 0x1D00_0000, next address
    advances by 128 B per accepted AW. 32 injected B responses complete a sub-buffer:
    `last_buffer` 7 → 0 in 0x14C (chain 1's stays 7), `interrupts` = bit 8 only,
    `interrupt_out` high, read-to-clear;
  - datapath: with chain 2's DDC programmed and only `traffic2_lsm_enable` set, strobes reach
    the LPF, RRC and the demod's symbol clock; chain 1's decimator stays idle.

A dibit burst needs 512 symbols, about 6 minutes of full-core pysim, so dibit content through
the DMA is not simulated. The packer → DMA stream wiring is the same as chain 1's.

LSM and p25 suites (`test_lsm_*`, `test_p25ddc`, `test_p25_top`, `test_dibit_packer`,
`test_iq_packer`): 156 passed, 2 skipped, 2 failed. The two failures are the known ones in
modules this change does not touch: `test_lsm_agc::test_reset_in_restores_gain_to_init` and
`test_lsm_nid_bch_fec::test_error_correction_at_t1_t6_t11` (both also listed in 059).

## Bench (2026-09-27)

Tezuka image with the 0.3.0 bitstream and the new device tree on unit A; the deployed
p25-httpd (065, single chain) unchanged. Mode B replay corpus (`rf.p25_corpus -p mode=B`, B
transmitting, run `run_20260927_193729_rf.p25_corpus`):

| | 0.2.0 (baseline, 12:02) | 0.3.0 |
|---|---|---|
| Followable clear transmissions | 219 | 219 |
| IMBE frames recovered of SDRTrunk's | 33795 / 34038 (99.3 %) | 33795 / 34038 (99.3 %) |
| Missed transmissions | 0 | 0 |
| Relay underruns | 0 | 0 |
| Focus tone dropouts | 1 | 1 |

Identical: the second chain costs chain 1 nothing. (The verdict reads "fail" in both runs
only because of the single tone dropout, threshold 0.)

## What p25-httpd must implement

Nothing in `p25-httpd/src` changed here; change 066 implements this list. To use chain 2:

1. **`hardware/core_version.rs`:** `CoreVersion::TRAFFIC2 = 0.3.0` and
   `has_traffic2_chain()`, with host tests like the 059 ones.
2. **`IpCore::take`:** open `RxBuffer::new("p25-traffic2-lsm-dibit")` only when
   `has_traffic2_chain()`; keep it as `Option<RxBuffer>`; a missing node disables chain 2
   with a log line instead of failing startup. Never touch 0x120–0x168 on an older core
   (the bus stalls, see Compatibility).
3. **Chain 2 accessors** (PAC names `traffic2_*`, same shape as the `traffic_*` ones). A
   `TrafficChain { One, Two }` parameter on the existing `traffic_*` methods would avoid
   duplicating them.

   - control: `set_traffic2_lsm_enable` (with epoch report), `traffic2_lsm_enabled`,
     `set_traffic2_lsm_dibit_dma_enable`, `set_traffic2_lsm_dc_block_enable`,
     `set_traffic2_lsm_agc_enable`, `pulse_traffic2_lsm_reset`,
     `traffic2_lsm_control_readback`;
   - status: `traffic2_lsm_status` (`LsmStatusSnapshot`, `traffic2_lsm_dibit_overflow`),
     `traffic2_lsm_nid`, `traffic2_lsm_drop_count`, `traffic2_lsm_dibit_last_buffer`,
     `traffic2_lsm_dibit_next_address`, `traffic2_lsm_debug`, `traffic2_lsm_agc_debug`,
     `traffic2_lsm_agc_threshold` / `set_traffic2_lsm_agc_threshold`.

4. **DDC:** `configure_traffic2_ddc(freq, preset)` (FIR loads through
   `traffic2_ddc_coeff_addr` / `traffic2_ddc_coeff`, decimation and control through
   `traffic2_ddc_decimation` / `traffic2_ddc_control`, same polyphase ordering and addresses
   0/256/512 as chain 1), `set_traffic2_ddc_frequency` (epoch report),
   `set_traffic2_ddc_enable`, `retune_traffic2_chain(freq, fs, should_reset, seeds)`,
   `pause_traffic2_chain`. Startup: same init as chain 1 (DDC with the active preset, input
   enable, DC block and AGC on, threshold 256, dibit DMA enable).
5. **Seeds:** `write_traffic2_seeds` / `read_traffic2_seeds` on 0x160 / 0x164 / 0x168, then
   `pulse_traffic2_lsm_reset`. Same Q-formats as chain 1. The read-back fence is not needed
   (same bank as the reset), though it is harmless.
6. **Ring reader:** `DibitRing::Traffic2` in `dibit_dma`, `dibit_ring_geometry`,
   `dibit_ring_snapshot` (next address 0x150, `last_buffer` 0x14C [18:16], enable 0x140 [0];
   all plain-R reads, never the read-to-clear 0x144 / 0x0C), the legacy cursor and
   `DmaChannel::Traffic2LsmDibit` in `read_dma_buffers`; a second `ChainEpochSink` for
   chain 2's hardware actions.
7. **Interrupt:** `interrupts().traffic2_lsm_dibit_dma()` (bit 8) →
   `notify_traffic2_lsm_dibit_dma` / `waiter_traffic2_lsm_dibit_dma()`, a
   `traffic2_lsm_dibit` counter in `IrqStats` and `/api/irq_stats`. On 0.2.0 the bit reads 0.
8. **Pipeline and policy:** a second dibit reader + frame decoder + IMBE path feeding the
   right speaker; the grant follower assigns calls to a free chain (the 063 left/right
   talkgroup mapping and priority pre-emption decide which); chain 2 fields in `/api/traffic`.

## Files changed

- `maia-hdl/p25_hdl/p25_top.py`: chain 2, banks 9 and 10–11, interrupt bit 8, version 0.3.0.
- `maia-hdl/p25_hdl/config.py`: `traffic2_lsm_dibit_dma_*`, ring list, overlap check.
- `maia-hdl/ip/p25-core/package_ip.tcl`, `maia-hdl/projects/fishball7020_p25/system_bd.tcl`:
  the new master.
- `build_fpga.bat`: `IP_CORE_VERSION=0.3.0` for `--p25`.
- `p25-httpd/p25-pac/p25.svd`, `p25-httpd/p25-pac/src/lib.rs`: regenerated.
- `maia-hdl/test/test_p25_top.py`, `maia-hdl/test/golden_vectors/p25_core_0.2.0.svd`: new.
- `doc/P25_ADDRESS_MAP.md`: carve-out, banks 8–11, IRQ bits and HP1 masters brought up to date.
- `tezuka_fw/.../fishball-p25.dtsi` (other repo, uncommitted): carve-out + rxbuffer node.
- `maia-hdl/projects/fishball7020_p25/fishball_p25.sdk/system_top.xsa`: rebuilt by the bake.
