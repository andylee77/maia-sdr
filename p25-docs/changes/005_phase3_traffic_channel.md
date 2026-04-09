# 005 -- Phase 3: Traffic Channel + Cleanup

**Date:** 2026-04-08
**Phase:** 3 (Second DDC + traffic manager + spectrometer/recorder removal)

---

## Summary

Added a second DDC + C4FM demod chain for traffic channel voice following. Removed the spectrometer and recorder (carried over from Maia but unused in P25). Implemented the Rust traffic manager with NCO calculation, grant lifecycle, and timeout management.

## FPGA Changes

### Second DDC + Demod Chain (`p25_top.py`)

- `traffic_ddc`: Independent DDC instance sharing IQ input with control DDC
- `traffic_c4fm` + `traffic_timing` + `traffic_packer`: Full demod pipeline
- `traffic_dma`: DmaStreamWrite to DDR for PS traffic dibit consumption
- Independent NCO frequency register for PS-controlled retuning on voice grants
- Traffic register bank at 0x30: DDC frequency/decimation/control + demod status/control

### Removed: Spectrometer + Recorder

- Removed `Spectrometer` and `Recorder16IQ` instances
- Removed `PulseSynchronizer` (was only for spectrometer interrupt CDC)
- Removed `clk2x` clock domain (was only used by spectrometer)
- Removed recorder register bank
- Removed spectrometer/recorder from interrupt register
- Freed: HP1, HP2, ~16 DSP48E1, ~5K LUT, ~12 BRAM

### Simplified Register Map

| Offset | Bank | Purpose |
|--------|------|---------|
| 0x00 | control | product_id, version, reset, interrupts |
| 0x08 | sdr | Control DDC: coefficients, decimation, frequency, control |
| 0x20 | demod | Control demod: status, start/stop, next_address |
| 0x30 | traffic | Traffic DDC + demod: frequency, decimation, control, status |

### Simplified AXI Port Map

| Port | HP | Purpose |
|------|-----|---------|
| s_axi_lite | - | CPU registers (0x7C460000) |
| m_axi_dibit | HP1 (shared) | Control channel dibit DMA |
| m_axi_traffic | HP1 (shared) | Traffic channel dibit DMA |

### Updated Resource Budget

| Component | DSP48 | LUT (est.) | BRAM |
|-----------|-------|------------|------|
| AD9361 interface | 0 | ~2000 | 2 |
| DDC x2 | 26 | ~6000 | 12 |
| C4FM Demod x2 | 4 | ~1000 | 0 |
| Symbol Timing x2 | 6 | ~2000 | 0 |
| Dibit Packer x2 + DMA | 0 | ~1600 | 2 |
| AXI + misc | 0 | ~3000 | 0 |
| **Total** | **~36** | **~15600** | **~16** |
| **Z7020 capacity** | **220** | **53200** | **140** |
| **Utilization** | **16%** | **29%** | **11%** |

## Rust Changes

### Traffic Manager (`traffic_manager.rs`)

- `TrafficState`: Idle -> Acquiring -> Active lifecycle
- `handle_grant()`: Computes 28-bit NCO word from frequency offset and sample rate
- `sync_acquired()`: Transitions from acquiring to active
- `check_timeouts()`: 200ms acquire timeout, 3s call inactivity timeout
- NCO formula: `word = (target_freq - rx_lo) / sample_rate * 2^28`
- 3 new unit tests: NCO calculation, negative offset, grant lifecycle

## Vivado Changes

- `system_bd.tcl`: Removed HP1 spectrometer and HP2 recorder connections
- Both DMA ports share HP1 via `ad_mem_hp1_interconnect`
- `package_ip.tcl`: Removed clk2x interface, recorder bus association, added m_axi_traffic

## Test Results

30 tests passing (14 Python + 16 Rust)
