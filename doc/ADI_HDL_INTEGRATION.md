# ADI HDL Integration in Maia SDR

Reference document describing how the Analog Devices HDL library (`analogdevicesinc/hdl`) is pulled in, configured, and wired into the Maia SDR FPGA block design on the Fishball Z7020 target. This doc is the first stop when you need to understand where an ADI IP core sits in the design, which clock domain it runs in, or why the build sources a particular TCL script.

Scope: Zynq-7020 + AD9361 projects (`fishball7020_iio`, `fishball7020_p25`). The shared base block design lives under `maia-hdl/projects/pluto/system_bd.tcl` — both fishball projects source it and then diff their config on top.

## 1. Submodule Pinning

| Field | Value |
|---|---|
| Submodule path | `maia-hdl/adi-hdl` |
| Upstream | `https://github.com/analogdevicesinc/hdl` |
| Pinned commit | `01ef62972d689ab9c69e130625a5cb07683a89b4` |
| Parent of local patch | `cf81ab15cdb1376982b53827a9aa3db01297eecc` |
| Release base | `dev_prj_2018_r1` (tag + 2325 commits) |
| Working tree | one local cherry-pick on top of fork HEAD |

### Local patches on top of the fork HEAD

The submodule currently carries **one local cherry-pick** from ADI
upstream on top of the fork pin:

| SHA | Upstream origin | Purpose |
|---|---|---|
| `01ef62972` | cherry-pick of `analogdevicesinc/hdl` main `92534dc1d` (2025-05-20) | `axi_ad9361_tx: Use incrementing cnt to improve timing margin` — structural fix to the `dac_rate_cnt_reg` counter in `library/axi_ad9361/axi_ad9361_tx.v`. The old load-and-decrement counter was marginal at 250 MHz `rx_clk` under placement pressure on 7-series; the replacement count-up-and-reset version removes the `dac_datarate_s` routing delay from the critical data path. Applied in commit `86b7536` (P25DDC fork v2 timing closure). See `doc/changes/042_p25ddc_fork_timing_fixes.md` Fix 3 for the full rationale and instructions for re-applying the fix if the submodule is updated past either SHA. |

If the submodule is ever updated to a newer origin/main snapshot
that already includes commit `92534dc1d`, the local cherry-pick
becomes redundant and can be dropped. Detect this with:

```bash
cd maia-hdl/adi-hdl
git log --oneline HEAD -- library/axi_ad9361/axi_ad9361_tx.v | grep 'incrementing cnt'
```

Defined in `.gitmodules` at the repo root. Init with:

```bash
git submodule update --init maia-hdl/adi-hdl
```

`build_fpga.bat` runs this automatically in Step 1 if the submodule is empty.

A second submodule, `maia-hdl/XilinxUnisimLibrary`, is referenced but **not currently initialized** by the build — it is unused in the active flow.

## 2. Library Path Wiring

The ADI HDL flow is entirely TCL-driven. Everything is rooted at a single variable, `$ad_hdl_dir`, set by sourcing `adi-hdl/scripts/adi_env.tcl`.

### 2.1 TCL Source Chain

From `maia-hdl/projects/fishball7020_iio/system_project.tcl` (first four lines):

```tcl
set IGNORE_VERSION_CHECK 1
source ../../adi-hdl/scripts/adi_env.tcl
source $ad_hdl_dir/projects/scripts/adi_project_xilinx.tcl
source $ad_hdl_dir/projects/scripts/adi_board.tcl
```

The three scripts it pulls in provide:

- **`adi_env.tcl`** — Sets `$ad_hdl_dir` from the relative path of the sourcing project, checks Vivado version (expects 2023.2; `IGNORE_VERSION_CHECK=1` bypasses this for 2025.2).
- **`adi_project_xilinx.tcl`** — Defines `adi_project`, `adi_project_files`, `adi_project_run` (project creation, source list, synth/impl/bitstream driver).
- **`adi_board.tcl`** — Defines the connection DSL: `ad_ip_instance`, `ad_ip_parameter`, `ad_connect`, `ad_cpu_interconnect`, `ad_mem_hp2_interconnect`, etc.

Every `system_bd.tcl` call in the Maia block design is one of these DSL primitives. If you need to know what a line does, the authoritative reference is `adi-hdl/projects/scripts/adi_board.tcl`.

### 2.2 Environment Variables

`build_fpga.bat` sets two env vars for Vivado:

| Var | Value | Used by |
|---|---|---|
| `ADI_HDL_DIR` | `%PROJECT_DIR%\maia-hdl\adi-hdl` | Optional override for `$ad_hdl_dir` |
| `ADI_IGNORE_VERSION_CHECK` | `1` | Allows Vivado 2025.2 to run the 2023.2-targeted flow |
| `ADI_LIB` | `%MAIA_HDL%\adi-hdl\library` | Shell-side convenience, not read by TCL |

### 2.3 Vivado Version Compatibility

- **Target:** Vivado 2023.2 (ADI's official support matrix).
- **In practice:** 2025.2 is what Andy runs; `ADI_IGNORE_VERSION_CHECK=1` lets the elaboration pass. No IP-level incompatibilities observed on this commit of adi-hdl.

## 3. Build Entry Points

`build_fpga.bat` is the single Windows entry point. It orchestrates three Vivado batch invocations:

| Step | Script | Purpose |
|---|---|---|
| 3 | `maia-hdl/ip/maia-sdr/package_ip.tcl` (and `p25-core/package_ip.tcl` for `--p25`) | Wrap Amaranth-generated Verilog (`maia_sdr.v`, `p25_core.v`) into a packaged Vivado IP with `component.xml` |
| 4 | Per-IP `*_ip.tcl` under `adi-hdl/library/` | Pre-build each ADI utility + AXI core so Vivado can resolve them during block design elaboration |
| 5 | `maia-hdl/projects/fishball7020_*/system_project.tcl` | Create project, source block design, run synth → impl → bitstream |

Step 4 build order matters: `util_axis_fifo` and `util_cdc` must exist before `axi_dmac`. `build_fpga.bat` enforces this explicitly.

Post-bitstream, `system_project.tcl` sources:

```tcl
source $ad_hdl_dir/library/axi_ad9361/axi_ad9361_delay.tcl
```

This emits `axi_ad9361_delay.log` — the RX IDDR timing margin analysis for the LVDS capture path. Check it when debugging AD9361 interface reliability.

## 4. ADI IP Cores Instantiated

All ADI instantiations for the fishball target live in the shared `maia-hdl/projects/pluto/system_bd.tcl`. There are no per-project additions to the ADI side — the fishball projects diff only via switches (`fishball`, `maia_iio`, `LVDS_ENABLE`).

### 4.1 AXI Cores (PS-facing)

| Instance | Core | Lite Base | Role |
|---|---|---|---|
| `axi_ad9361` | `axi_ad9361` | `0x79020000` | AD9361 LVDS RX/TX interface + SPI control |
| `axi_ad9361_adc_dma` | `axi_dmac` | `0x7C400000` | S2MM DMA — stream → DDR (IQ capture) |
| `axi_ad9361_dac_dma` | `axi_dmac` | `0x7C420000` | MM2S DMA — DDR → stream (cyclic TX) |

Maia's own `maia_sdr` IP sits at `0x7C460000` and is **not** an ADI core — it is Amaranth-generated Verilog wrapped as a custom Vivado IP.

### 4.2 Utility Cores (Datapath Glue)

| Instance | Core | Purpose |
|---|---|---|
| `util_ad9361_divclk` | `util_clkdiv` | Divide `axi_ad9361/l_clk` by 4 (LVDS 2R2T) or 2 (CMOS 1R1T) to produce `sampling_clk` |
| `util_ad9361_divclk_sel` | `util_reduced_logic` | OR of `adc_r1_mode`/`dac_r1_mode` — selects divider ratio |
| `util_ad9361_divclk_reset` | `proc_sys_reset` | Reset sync into the divided-clock domain |
| `util_ad9361_adc_fifo` | `util_wfifo` | 4-channel CDC FIFO, `l_clk` → `sampling_clk`, 16-bit lanes |
| `axi_ad9361_dac_fifo` | `util_rfifo` | 4-channel CDC FIFO, `sampling_clk` → `l_clk` |
| `util_ad9361_adc_pack` | `util_cpack2` | Pack four 16-bit ADC lanes into one 64-bit AXI-Stream word |
| `util_ad9361_dac_upack` | `util_upack2` | Unpack a 64-bit AXI-Stream word into four 16-bit DAC lanes |
| `adc_i_slice`, `adc_q_slice` | `xlslice` | Extract the lower 12 bits of I and Q from the 16-bit FIFO output (fed to Maia DDC) |

### 4.3 `axi_ad9361` Configuration (fishball)

Set in `system_bd.tcl` around line 500:

| Parameter | Value | Meaning |
|---|---|---|
| `CMOS_OR_LVDS_N` | `0` | LVDS mode (fishball hardware) |
| `MODE_1R1T` | `0` | 2R2T (both I/Q channels live) |
| `ADC_INIT_DELAY` | `30` | IDELAY taps for RX capture (fishball-specific; libre uses 21) |
| `TDD_DISABLE` | `1` | Frequency-division duplex only |
| `DAC_DDS_DISABLE` | `1` | No internal DDS — Maia drives TX from DMA |

### 4.4 `axi_dmac` Configuration

**ADC DMA (S2MM, capture):**

| Parameter | Value |
|---|---|
| `DMA_TYPE_SRC` | `2` (AXI-Stream slave) |
| `DMA_TYPE_DEST` | `0` (AXI-MM master) |
| `CYCLIC` | `0` (one-shot) |
| `SYNC_TRANSFER_START` | `0` |
| `DMA_DATA_WIDTH_SRC` | `64` |

**DAC DMA (MM2S, transmit):**

| Parameter | Value |
|---|---|
| `DMA_TYPE_SRC` | `0` (AXI-MM master) |
| `DMA_TYPE_DEST` | `1` (AXI-Stream master) |
| `CYCLIC` | `1` (ring buffer for continuous TX) |
| `DMA_DATA_WIDTH_DEST` | `64` |

Both DMAs hit PS DDR through `S_AXI_HP2` via `ad_mem_hp2_interconnect`. Interrupts land on PS IRQ `ps-12` (DAC) and `ps-13` (ADC).

## 5. AD9361 — Chip Internals and LVDS Interface

This section covers everything from the antenna pins on the AD9361 package to the 12-bit I/Q lanes arriving at Maia's DDC. There are two distinct parts to the story: what happens **inside** the AD9361 silicon (pure analog + on-chip DSP, not touchable from the FPGA) and what happens on the **LVDS interface** between the chip and the PL — the latter is what `axi_ad9361` owns.

The single picture to keep in mind:

```
┌───────── AD9361 package ─────────┐          ┌──── Zynq-7020 PL ────┐
│                                  │          │                      │
│  RF ─ LNA ─ VGA ─ mixer ─ LPF    │  LVDS    │  axi_ad9361          │
│                     ↑            │  6 pairs │  (IBUFDS+IDELAY+     │
│                 RX RFPLL         │  DDR     │   IDDR+deser)        │
│                                  │ ───────▶ │          ↓           │
│  Σ-Δ ADC → HB3 → HB2 → HB1       │ DATA_CLK │  l_clk  ≈ 61.44 MHz  │
│  → RX FIR  (on-chip decimators)  │ ≈61.44MHz│          ↓           │
│                                  │          │  util_ad9361_adc_fifo│
│  BBPLL ──── on-die clock tree    │          │  (l_clk → sampling)  │
│                                  │          │          ↓           │
│  SPI regs ◀──────── SPI ─────────│◀──────── │  axi-lite @0x79020000│
└──────────────────────────────────┘          │          ↓           │
                                               │  Maia DDC @15.36 MHz │
                                               └──────────────────────┘
```

Everything left of the LVDS boundary is internal to the chip — you can only see it through the SPI control plane. Everything right of the boundary is HDL you can edit.

### 5.1 Inside the AD9361 — RX Signal Path

The AD9361 is a complete zero-IF RF transceiver in a single package. For each of the two RX channels (RX1, RX2) the on-die path is:

```
RF input (differential, balun-matched)
    │
    ▼
LNA + input-impedance match             ← noise figure < 3 dB
    │
    ▼
Mixer-mode VGA (coarse gain)            ← first step of MGC / AGC
    │
    ▼
Quadrature demodulator (I and Q mixer)  ← LO from RX RFPLL
    │       ↑
    │    cos(2π·f_LO·t), sin(2π·f_LO·t)
    ▼
Baseband VGA + TIA (per I, per Q)       ← second gain stage
    │
    ▼
Programmable analog LPF                 ← corner ≈ rf_bandwidth / 2
    │                                    (5th-order Chebyshev, set
    │                                     via SPI register 0x1F5)
    ▼
12-bit Σ-Δ ADC  (I)   +   12-bit Σ-Δ ADC  (Q)
    │     (oversampled, internal rate up to ~640 MSPS)
    ▼
Digital decimation chain (per channel):
    HB3  →  HB2  →  HB1  →  RX FIR (programmable, up to 128 taps)
    │      │      │         │
    ÷2/3   ÷2     ÷2       ÷1, 2, 4
    ▼
Output sample rate  =  DATA_CLK rate  (what the FPGA sees on LVDS)
```

None of this — LNA, mixer, LO, LPF, Σ-Δ ADC, on-chip decimators — is exposed on the FPGA pins. The PL can only influence it indirectly, through the **SPI control plane** (section 5.8), which writes configuration registers (frequencies, gains, filter corner, decimator ratios, FIR taps).

A consequence worth internalizing: when you hear "the ADC runs at X MSPS", there are two possible interpretations. The Σ-Δ modulator is running in the hundreds of MSPS range internally; the rate the *FPGA* actually sees is the rate *after* HB3/HB2/HB1/RX-FIR, which is whatever libiio asked for. On the Fishball configuration that's typically 61.44 MSPS.

### 5.2 The AD9361 Clock Tree

Two PLLs inside the chip matter to the block design:

| PLL | Purpose | Range |
|---|---|---|
| **BBPLL** | Baseband master clock — drives the Σ-Δ ADC, on-chip decimators, and `DATA_CLK` generation | 715–1430 MHz |
| **RX RFPLL** | RF synthesizer — produces the quadrature LO fed to the mixer | 70 MHz – 6 GHz |

The BBPLL is the one the HDL cares about, because its division ratios determine the `DATA_CLK` frequency, which becomes `l_clk` on the PL side. The RX RFPLL is invisible to the HDL — it's entirely configured by libiio attributes like `out_altvoltage0_RX_LO_frequency`.

BBPLL frequency itself is a derived number. What you actually set via libiio is the user sample rate; the driver solves the constraint BBPLL → HB3 → HB2 → HB1 → RX-FIR → `DATA_CLK = sample_rate` for legal divider ratios and writes the result over SPI. Two takeaways:

- You don't pick the BBPLL frequency directly. You pick the sample rate and let the driver solve the chain.
- Anything upstream of `DATA_CLK` (the raw Σ-Δ rate, the HB3/HB2/HB1 intermediate rates) is hidden from the FPGA — it only matters for noise-folding math, not for HDL.

### 5.3 LVDS Interface — Physical Layer

Fishball wires the AD9361 to the PL in **LVDS 2R2T mode** (both RX and both TX channels carried). LVDS mode uses fewer pins than CMOS and is less sensitive to board skew, but is DDR-encoded — both rising and falling edges of `DATA_CLK_P` carry payload.

Signals, as seen by the FPGA:

| Signal | Count | Dir | Purpose |
|---|---|---|---|
| `rx_clk_in_p/n` | 1 pair | in | `DATA_CLK` — forwarded sample clock from AD9361 |
| `rx_frame_in_p/n` | 1 pair | in | Frame marker — tells receiver which sub-slot is on the bus |
| `rx_data_in_p/n[5:0]` | 6 pairs | in | 6 DDR data lanes → 12 bits per `DATA_CLK` half-cycle |
| `tx_clk_out_p/n` | 1 pair | out | `FB_CLK` — sample clock to AD9361 |
| `tx_frame_out_p/n` | 1 pair | out | TX frame marker |
| `tx_data_out_p/n[5:0]` | 6 pairs | out | TX DDR data lanes |
| `enable` | 1 SE | out | ENSM state machine step |
| `txnrx` | 1 SE | out | TDD direction (held in FDD here) |
| `gpio_*`, `spi_*` | several SE | both | AGC ctl/status, reset, SPI control plane |

**Bit accounting in LVDS 2R2T.** 6 data lanes × DDR = 12 bits per half-cycle of `DATA_CLK`. Exactly one 12-bit signed sample sits on the bus per half-cycle. Four consecutive half-cycles carry the full tuple `{I1, Q1, I2, Q2}` (both RX channels). So:

- `DATA_CLK` frequency = **per-channel sample rate**
- But `adc_valid` for a single channel (say `i0`) asserts only once per **four** half-cycles of `l_clk`
- The aggregate AD9361 payload throughput is `f_sample × 4 channels × 12 bits`

The `rx_frame_in` signal is how the receiver knows which slot carries which channel — it toggles in a deterministic pattern that lets `axi_ad9361`'s deserializer align to the channel boundary.

### 5.4 `axi_ad9361` — How the PL Recovers the Streams

`axi_ad9361` is ADI's wrapper around the full LVDS-PHY + deserializer + AD9361 register access. Functionally, pin to AXI-Stream-ish output:

1. **`IBUFDS_DIFF_OUT`** on each LVDS pair → single-ended differential receivers for clock, frame, and 6 data lanes.
2. **`IDELAYE2`** on each data lane → adjustable per-lane tap delay to center the data eye under `DATA_CLK`. This is what `ADC_INIT_DELAY = 30` in section 4.3 sets — it compensates for fishball's PCB trace skew. `axi_ad9361_delay.tcl` (post-route analysis) reports the resulting margin.
3. **`BUFIO + BUFR`** on `rx_clk_in` → distribute the recovered `DATA_CLK` as `l_clk` into the surrounding logic region. `BUFIO` feeds the IDDR primitives directly (lowest skew), `BUFR` feeds the general-purpose fabric.
4. **`IDDR`** on each data lane → one flop on the rising edge, one on the falling edge of `l_clk`; gives 12 bits at SDR once deserialized.
5. **Frame-aligned demux** — uses `rx_frame_in` to split the interleaved {I1, Q1, I2, Q2} stream into four independent lanes `adc_data_{i0,q0,i1,q1}` with their own `adc_valid` / `adc_enable`.
6. **AXI-Lite register bank** at `0x79020000` → internal SPI master, DAC test tones, AGC thresholds, RSSI readback, RX/TX calibration triggers. The Linux AD9361 IIO driver talks to all AD9361 internals through this port.

Outputs that matter to Maia's side of the block design:

| Output | Width | Clock | Meaning |
|---|---|---|---|
| `l_clk` | 1 | — | Recovered `DATA_CLK`, BUFR-distributed. Runs at `DATA_CLK_P` ≈ sample rate in LVDS 2R2T |
| `adc_data_i0` | 16 | `l_clk` | RX1 I, sign-extended 12→16 |
| `adc_data_q0` | 16 | `l_clk` | RX1 Q, sign-extended 12→16 |
| `adc_data_i1` | 16 | `l_clk` | RX2 I (unused on fishball — single-antenna build) |
| `adc_data_q1` | 16 | `l_clk` | RX2 Q (unused) |
| `adc_valid_*` | 1 each | `l_clk` | Per-channel sample-valid strobe |
| `adc_enable_*` | 1 each | `l_clk` | Per-channel enable from the AD9361 driver state |
| `adc_r1_mode` | 1 | — | Driver-asserted when AD9361 is in 1R1T (halves the channel count) |
| `dac_r1_mode` | 1 | — | Same, TX side |

The samples from `axi_ad9361` are **already in the `l_clk` domain as parallel 12-bit words** — the LVDS deserialization is done. What the rest of the block design sees is a clean four-lane synchronous stream, not raw DDR bits.

### 5.5 From `l_clk` to `sampling_clk` — Why the Divide-by-4

`l_clk` ticks at `DATA_CLK_P` — say 61.44 MHz for a 61.44 MSPS configuration. But a given channel's `adc_valid` asserts only once every **four** `l_clk` cycles (because the four channels take turns on the shared bus). Downstream Maia logic does not want to see three idle cycles out of four; it wants a clock that ticks once per useful sample.

That's what `util_ad9361_divclk` produces. The divider table is:

| PHY | Mode | Divide by | Selected when |
|---|---|---|---|
| LVDS | 2R2T | 4 | `LVDS_ENABLE` at build + `adc_r1_mode`=0 at runtime |
| LVDS | 1R1T | 2 | `LVDS_ENABLE` at build + `adc_r1_mode`=1 at runtime |
| CMOS | 2R2T | 2 | `LVDS_ENABLE` not set + `adc_r1_mode`=0 |
| CMOS | 1R1T | 1 | `LVDS_ENABLE` not set + `adc_r1_mode`=1 |

The mux between the two ratios is driven by `adc_r1_mode`/`dac_r1_mode` through `util_reduced_logic`. That handles the 1R1T ↔ 2R2T case at runtime — but note that `MODE_1R1T` on `axi_ad9361` is also a compile-time parameter that sets the deserializer framing, and on Fishball it is hard-set to 2R2T, so in practice `adc_r1_mode` stays 0 and the divider is always `/4`.

Numbers for the canonical Fishball configuration:

```
BBPLL ≈ 983 MHz     (driver-computed, typical)
on-chip decimation ratio chosen so that DATA_CLK = 61.44 MHz
  ↓
DATA_CLK_P   = l_clk            ≈ 61.44 MHz   (LVDS interface clock)
sampling_clk = l_clk / 4        ≈ 15.36 MHz   (per-channel sample clock)

payload:  15.36 Msamples/s × 12 bits (I) + 12 bits (Q)  per RX channel
```

**15.36 MSPS is the rate Maia's DDC actually sees at its input**, regardless of whatever final P25 sample rate the pipeline wants. Getting from 15.36 MSPS down to the P25 working rate is the job of Maia's *custom* DDC — it is not the AD9361's on-chip decimation. That custom DDC is the stage flagged as "weak" in the `project_p25_ddc_stage1_filter_weak` investigation and is the target of the P25DDC fork.

### 5.6 Clock Domains on the PL Side

Pulling it all together, here is every clock in the RX data path and where it comes from:

| Clock | Source | Typical Freq | Used by |
|---|---|---|---|
| `axi_ad9361/l_clk` | AD9361 `DATA_CLK_P`, recovered via IBUFDS + BUFR | ~61.44 MHz | ADI capture, wfifo write side |
| `util_ad9361_divclk/clk_out` (= `sampling_clk`) | `l_clk` / 4 (LVDS) or / 2 (CMOS) | ~15.36 MHz | ADI FIFO read side + Maia DDC input |
| `sys_cpu_clk` | PS7 FCLK0 | 100 MHz | AXI-Lite control, DMA AXI-MM |
| `maia_sdr_clk/clk_out1` | MMCM from `sys_cpu_clk` | ~250 MHz | Maia core `clk` |
| `maia_sdr_clk/clk_out2` | MMCM | ~500 MHz | Maia `clk2x_clk` |
| `maia_sdr_clk/clk_out3` | MMCM | ~750 MHz | Maia `clk3x_clk` |

`l_clk` is forwarded from the AD9361 and is not phase-related to anything else in the PL. `sampling_clk` is derived from `l_clk` through `util_clkdiv` but the BUFR routing introduces enough delay that the downstream logic treats the boundary as asynchronous. Everything from `sys_cpu_clk` downward is generated by the PS/MMCM and is unrelated to either `l_clk` or `sampling_clk`.

### 5.7 CDC — Why the FIFO Is Mandatory

Because `l_clk` (recovered, AD9361-sourced) and `sampling_clk` (divided, BUFR-delayed) are treated as asynchronous, samples must cross a proper CDC boundary.

`util_ad9361_adc_fifo` (a `util_wfifo` instance) is a 4-channel asynchronous FIFO that handles this. Each channel has independent `din_clk` = `l_clk` and `dout_clk` = `sampling_clk` ports; pointer-synchronization uses 2-flop synchronizers per ADI's standard. Without it, samples would get destroyed by metastability at the clock crossing and the design would not close timing.

The TX path uses the mirror-image `util_rfifo` to bridge `sampling_clk` → `l_clk` for the DAC.

### 5.8 The 12-bit Slice

The AD9361 delivers 12-bit signed samples sign-extended into 16 bits across `axi_ad9361`'s outputs. The ADI CDC FIFO is 16-bit wide; Maia's custom DDC expects 12-bit. The two `xlslice` instances (`adc_i_slice`, `adc_q_slice`) drop the upper four bits, re-flattening to 12-bit. Both slices use `DIN_FROM=11`, `DOUT_WIDTH=12`. If you ever widen Maia's input width to 16-bit (e.g., to preserve headroom for a software AGC), this is the first place to change.

### 5.9 SPI Control Plane

Parallel to the data path, the PL talks to the AD9361's configuration register map through an SPI master **inside** `axi_ad9361`. The SPI pins (`spi_csn`, `spi_clk`, `spi_mosi`, `spi_miso`) run from the FPGA to the AD9361; the AXI-Lite registers at `0x79020000` are the PL-side doorway.

In normal operation nothing in the HDL directly pokes this. The flow is:

1. PS-side Linux boots the `ad9361` IIO kernel driver.
2. Driver reaches `axi_ad9361` through the AXI-Lite port.
3. Every libiio attribute write (LO frequency, gain, `rf_bandwidth`, `sampling_frequency`, calibration, FIR loading) becomes one or more SPI transactions on the wire.
4. The AD9361 reconfigures its analog front-end, PLLs, filters, and on-chip decimators accordingly, then signals completion back through GPIO / status registers.

This is why `iio_attr -u ip:192.168.2.1 ...` from your Windows host can change AD9361 state without any bitstream rebuild — it all flows through this SPI path, which is entirely in ADI's hands. It is also why the `reference_iio_attr_local.md` shortcut works for fast gain/BW diagnosis: you are talking straight to the AD9361 registers through stable, well-tested plumbing.

### 5.10 Bandwidth Math — How Many MHz of Signal Does This Board See

Three numbers govern the usable RF window, and they are easy to confuse. Keep them straight:

**1. Bit accounting on the LVDS bus (where `/4` comes from).**

LVDS mode uses 6 differential data pairs, DDR on both edges of `DATA_CLK`, giving 12 bits per full `DATA_CLK` period — exactly **one 12-bit AD9361 sample** per period.

- 2R2T mode packs 4 samples per IQ set `{I1, Q1, I2, Q2}` → needs 4 `DATA_CLK` periods → `fs_complex = DATA_CLK / 4`
- 1R1T mode packs 2 samples per IQ set `{I, Q}` → needs 2 `DATA_CLK` periods → `fs_complex = DATA_CLK / 2`

Fishball is compile-time-locked to 2R2T, so the divider is always `/4`, and with `DATA_CLK ≈ 61.44 MHz` the per-channel complex sample rate is `fs_complex ≈ 15.36 MHz`.

**2. "I at 15 MSPS + Q at 15 MSPS ≠ 30 MSPS signal."**

`I` and `Q` are not two independent data streams. They are the two components of *one* complex sample `z = I + jQ`. Summing them is double-counting. The correct statement is:

- `fs_complex = 15.36` Mcomplex-samples/s per channel
- Raw payload on the wire: `15.36 M × 24 bit ≈ 368 Mbit/s` per channel
- **Signal bandwidth represented: 15.36 MHz**, not 30.72 MHz

**3. Complex-vs-real Nyquist — why the usable window equals `fs`, not `fs/2`.**

| Sampling style | Usable bandwidth | Reason |
|---|---|---|
| Real-valued (one ADC) | `fs / 2` | classical Nyquist — can't distinguish `+f` from `−f` |
| Complex IQ (quadrature pair) | **`fs`** | I and Q together encode the sign of frequency around the LO |

So `fs_complex = 15.36 MHz` means the maximum theoretical RF window is:

```
[ f_LO − 7.68 MHz ,  f_LO + 7.68 MHz ]   =  15.36 MHz total
```

centered on whatever RX LO the AD9361 is tuned to.

**4. The analog LPF is the real ceiling.**

The `fs_complex` window is a theoretical cap from the bus. What the RF front-end *actually* lets through is set by the AD9361's on-die analog LPF corner, controlled via the libiio `rf_bandwidth` attribute (SPI register `0x1F5`). The LPF corner is roughly `rf_bandwidth / 2` per side.

```
usable_RF_BW  =  min( rf_bandwidth, fs_complex )
```

Concretely:

| `rf_bandwidth` | Analog LPF corner | Usable RF window | vs Nyquist |
|---|---|---|---|
| 4 MHz | ~2 MHz | ±2 MHz (4 MHz total) | 3.8× oversampled |
| 8 MHz | ~4 MHz | ±4 MHz (8 MHz total) | 1.9× oversampled |
| 15.36 MHz | ~7.68 MHz | ±7.68 MHz (15.36 MHz total) | at Nyquist |
| > 15.36 MHz | > 7.68 MHz | **aliases** — LPF lets in energy that folds into the passband | above Nyquist |

**5. Takeaways for Fishball + Maia.**

- The maximum real RF bandwidth this bitstream can capture is **15.36 MHz**, clamped by the compile-time `/4` divider on a 61.44 MHz `DATA_CLK`.
- The current P25 configuration runs at `rf_bandwidth = 4 MHz`, which means you are heavily oversampled — plenty of margin for processing gain and noise shaping, but the actual spectrum window is only 4 MHz wide because the analog LPF throws the rest away before the ADC.
- Widening `rf_bandwidth` past ~8 MHz in this bitstream has two compounding risks: (a) adjacent-site energy leaks past the analog LPF, and (b) the Maia custom DDC's weak stage-1 filter (see `project_p25_ddc_stage1_filter_weak`) can't reject it. This is why the 2026-04-15 8 MHz sweep collapsed control CRC from 80% to 43%.
- Going above 15.36 MHz of real bandwidth requires **rebuilding the bitstream** for 1R1T (doubles `fs_complex`, loses RX2) or driving `DATA_CLK` higher (within AD9361 LVDS limits). Neither is a runtime knob.

## 6. DMA / Memory Path

The Maia block design talks to PS DDR through **two** PS7 HP slaves:

| HP Port | Masters | Purpose |
|---|---|---|
| `S_AXI_HP1` | `maia_sdr/m_axi_spectrometer` | Direct, single-master, high-throughput spectrum frame writes |
| `S_AXI_HP2` | `axi_ad9361_adc_dma`, `axi_ad9361_dac_dma`, `maia_sdr/m_axi_recorder` | Shared via SmartConnect (`ad_mem_hp2_interconnect`) |

On `fishball7020_p25`, Maia's `m_axi_recorder` carries a third IQ DMA ring added in Phase 6C — the SmartConnect arbiter behind HP2 handles three masters instead of two. See `doc/changes/013_phase6c_iq_dma.md` for the arbitration analysis.

The IIO capture path used by libiio on the target is the S2MM DMA into a DDR buffer. No Maia custom logic sits in that path — it is pure ADI plumbing, which is why the libiio-based known-good baseband capture still works on the P25 build even when the Maia DDC / P25 pipeline is offline. This is load-bearing for debugging (see `feedback_keep_libiio_path.md` in auto-memory).

## 7. Maia IP Core — Connection to the ADI Side

Although `maia_sdr` is not an ADI core, its wiring into the ADI infrastructure is part of the integration story.

Variant selection in `system_bd.tcl`:

```tcl
if {[info exists maia_iio]} {
    if {[info exists fishball]} {
        ad_ip_instance maia_sdr_maia_iio maia_sdr
    } else {
        ad_ip_instance maia_sdr_maia_iio_lite maia_sdr
    }
} else {
    ad_ip_instance maia_sdr_default maia_sdr
}
```

Clock connections:

```tcl
ad_connect maia_sdr/sampling_clk     util_ad9361_divclk/clk_out
ad_connect sys_cpu_clk               maia_sdr/s_axi_lite_clk
ad_connect maia_sdr_clk/clk_out1     maia_sdr/clk
ad_connect maia_sdr_clk/clk_out2     maia_sdr/clk2x_clk
ad_connect maia_sdr_clk/clk_out3     maia_sdr/clk3x_clk
```

AXI master connections:

```tcl
ad_connect maia_sdr_clk/clk_out1     sys_ps7/S_AXI_HP1_ACLK
ad_connect maia_sdr/m_axi_spectrometer sys_ps7/S_AXI_HP1
ad_mem_hp2_interconnect sys_cpu_clk  maia_sdr/m_axi_recorder
```

Interrupt: Maia drives PS IRQ `ps-11` for spectrum-frame-done / IQ-DMA events.

## 8. Interrupt Map (ADI-owned lines)

| IRQ | Source | Purpose |
|---|---|---|
| `ps-11` | `maia_sdr/interrupt_out` | Spectrometer / IQ DMA complete |
| `ps-12` | `axi_ad9361_dac_dma/irq` | DAC DMA (cyclic end) |
| `ps-13` | `axi_ad9361_adc_dma/irq` | ADC DMA (transfer complete) |

All three are concatenated into `sys_concat_intc` (xlconcat) and fed to PS7 `IRQ_F2P`.

## 9. Local Modifications

The adi-hdl submodule working tree is clean. Maia does **not** patch any ADI source file. All customization happens at the TCL configuration layer (`ad_ip_parameter` calls in `system_bd.tcl`) and through Maia's own custom IP cores that sit beside the ADI cores on the block design.

If you ever find yourself wanting to change behavior inside an ADI core, prefer to do it via `ad_ip_parameter` first. A local fork of adi-hdl should be a last resort because it breaks the `git submodule update` upgrade path.

## 10. Files You Will Actually Touch

| Task | File |
|---|---|
| Change AD9361 config (bandwidth disable, IQ correction, init delay) | `maia-hdl/projects/pluto/system_bd.tcl` (~ line 500) |
| Change DMA width / cyclic / sync behavior | same file (~ lines 726, 746) |
| Retarget HP port / interconnect | same file (~ lines 829–851) |
| Change `sampling_clk` divider | `util_ad9361_divclk` block (~ line 572) |
| Post-route AD9361 timing check | `adi-hdl/library/axi_ad9361/axi_ad9361_delay.tcl` (sourced from `system_project.tcl` line 20) |
| Bump ADI HDL version | `.gitmodules` + `git submodule update --remote` inside `maia-hdl/adi-hdl` |

## 11. Related Documentation

- `doc/changes/001_build_scripts.md` — build script architecture, how `build_fpga.bat` drives Vivado
- `doc/changes/013_phase6c_iq_dma.md` — HP2 SmartConnect arbitration with the third IQ DMA ring (P25)
- `doc/P25_ADDRESS_MAP.md` — full DDR and AXI-Lite address map (incl. ADI core bases)
- `doc/BUILD_FPGA.md` — end-user FPGA bitstream build guide
- Upstream ADI documentation: <https://analogdevicesinc.github.io/hdl/>
  - `axi_ad9361`: <https://analogdevicesinc.github.io/hdl/library/axi_ad9361/index.html>
  - `axi_dmac`: <https://analogdevicesinc.github.io/hdl/library/axi_dmac/index.html>
  - `util_cpack2` / `util_upack2`: <https://analogdevicesinc.github.io/hdl/library/util_cpack2/index.html>
