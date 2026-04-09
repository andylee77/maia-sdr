# 004 -- P25 Phase 1: FPGA Gateware + Vivado Integration

**Date:** 2026-04-08
**Phase:** 1A (DSP modules + sim tests) + 1B (DMA integration + Vivado prep)
**Branch:** fishball-p25

---

## Summary

Implemented the complete FPGA DSP pipeline: C4FM demodulator, Gardner symbol
timing recovery, and dibit packer with DMA. Integrated into p25_top.py with
DmaStreamWrite for PS dibit delivery. Updated Vivado project files and generated
the SVD + Rust PAC.

## Phase 1A: DSP Modules

### C4FM Demodulator (`maia-hdl/p25_hdl/c4fm_demod.py`)

- Cross-product FM discriminator: `disc[n] = re[n-1]*im[n] - im[n-1]*re[n]`
- 2-stage pipeline (latch + multiply), 18-bit signed output
- 2 DSP48E1 cost

### Symbol Timing Recovery (`maia-hdl/p25_hdl/symbol_timing.py`)

- Gardner timing error detector at 13 samples/symbol
- PI loop filter: Kp=185, Ki=1 (Q0.16), BW ~48 Hz, damping 0.707
- Sign-of-midpoint error approximation (avoids full multiplier)
- 4-level slicer: P25 dibit mapping per TIA-102.BAAA (01=+3, 00=+1, 10=-1, 11=-3)
- Timing adjustment via +/-1 counter reload

### Dibit Packer (`maia-hdl/p25_hdl/dibit_packer.py`)

- Shift register packs 32 dibits into 64-bit words
- AXI4-Stream handshaking (data_valid / stream_ready)
- Overflow detection for backpressure stalls

### Simulation Tests (14 passing)

- C4FM: DC zero, positive/negative frequency, 4-level ordering, strobe gating
- Symbol timing: known sequence, symbol rate, slicer levels, strobe gating
- Dibit packer: 32-dibit pack, multi-word, no-strobe, backpressure, overflow

## Phase 1B: Integration + Vivado

### p25_top.py Changes

- Demod chain wired: DDC -> C4FMDemod -> SymbolTimingRecovery -> DibitPacker -> DmaStreamWrite
- DmaStreamWrite for dibit DMA via AXI HP3 (reuses maia_hdl.dma)
- Demod register bank at 0x40: start/stop Wpulse, dibit_count, demod_overflow, next_address
- Address space: 6 bits (4 register banks)

### Vivado Project (`maia-hdl/projects/fishball7020_p25/`)

- `system_bd.tcl`: Replaces maia_sdr with p25_core, enables HP3 for dibit DMA
- `system_top.v`: Board pin mapping (identical to Maia fishball_iio)
- `package_ip.tcl`: Added m_axi_dibit bus interface

### SVD + Rust PAC

- SVD generated via `maia-hdl/generate_p25_svd.py` -> `p25-httpd/p25-pac/p25.svd`
- PAC generated via `svd2rust --target none` (no cortex-m dependency)
- All demod registers accessible: `demod_status().dibit_count()`, `demod_control().start()`, etc.

## Sample Rate Discovery

Tezuka firmware configures the AD9361 for 8 MSPS (not 6.144 MSPS):

- BBPLL: 1024 MHz / 4 = 256 MHz ADC
- HB chain: 32x decimation -> 8.000000 MSPS exact
- Analog LPF: 4 MHz, usable RF BW ~8 MHz
- DDC: 128x -> 62.5 kSPS -> ~13 samples/symbol

## Clay County Test System

Target system identified for hardware testing:

- Control channel: 860.9625 MHz (LCN 11)
- 11 LCNs spanning 855.24 - 860.96 MHz (5.725 MHz span)
- All channels fit within 8 MHz BW -- single LO at 858.1 MHz, no retuning
- WACN: BEE00, System: 8A0, NAC: 8A1
