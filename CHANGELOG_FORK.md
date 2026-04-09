# Maia SDR -- Changelog (andylee77 fork)

Tracking log for the `andylee77/maia-sdr` fork.
Upstream: [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr) (originally [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr))

---

## [2026-04-09] Phase 5: Build Pipeline, Register CDC, Ring DMA, First Hardware Boot

**Branch:** fishball-p25

First successful hardware boot of the P25 FPGA bitstream on the Fishball Z7020,
followed by a same-day refactor of the DMA path from one-shot to a ring buffer
architecture. Also fixed multiple build pipeline issues from the standalone
repo migration, corrected the SVD register map, added DDC FIR coefficient
initialization, and fixed a register clock-domain-crossing bug.

### Build Pipeline Fixes

- Fixed P25 Verilog generation to use Docker (ext4 filesystem avoids NTFS pip issues)
- Fixed CMD escaping in `build_fpga.bat` for Docker invocation
- Added missing ADI library builds: `util_clkdiv`, `util_rfifo`, `util_wfifo`
- Fixed stale TCL/Makefile paths left over from standalone repo migration
- Added `.gitattributes` enforcing LF line endings on `.sh` files
- Added skip-if-built logic for incremental ADI library builds (and ADI library packages)
- `build_hdl.sh` now auto-generates `p25.svd` when `--p25` is set and regenerates `p25-pac/src/lib.rs` via a downloaded `svd2rust` binary, so SVD/PAC stay in sync with Amaranth HDL without manual steps

### Tezuka Firmware Fixes

- Added XSA cache invalidation in `build.sh` (detects when source XSA is newer than cached package and forces a package rebuild)
- Added p25-httpd / maia-httpd source change detection for auto-rebuild
- Removed phantom AXI UART Lite from `fishball-p25.dtsi` (was at 0x42C00000, inherited from the Maia DTSI but not present in the P25 FPGA design -- caused a kernel panic in `uartlite_probe`)

### Register Map Fix

- SVD register offsets were wrong: FPGA uses bank select bits [4:3] of word address giving byte offsets 0x00/0x20/0x40/0x60, but SVD had 0x00/0x08/0x20/0x30
- Fixed `RegisterMap` in `p25_top.py`, regenerated SVD and PAC

### Register Clock-Domain-Crossing Fix

- `demod_registers` and `traffic_registers` were instantiated in the `s_axi_lite` clock domain (100 MHz) but their Wpulse outputs fed modules running in the `sync` domain (62.5 MHz)
- The unsynchronized Wpulse edges were missed ~38% of the time due to clock skew, causing the DMA start signal to be silently dropped
- Wrapped both with `RegisterCDC`, matching the existing `sdr_registers_cdc` pattern already used for the DDC bank

### DDC FIR Coefficient Initialization

- Added 3-stage FIR filter coefficient loading to p25-httpd (`fpga.rs`, `configure_ddc()`)
- Previously only NCO frequency and enable bits were programmed
- P25 channel filter: 128x decimation (16x4x2), 8 MSPS -> 62.5 kSPS (13 samples/symbol)
  - Stage 1: 48 taps, 200 kHz cutoff, /16
  - Stage 2: 32 taps, 50 kHz cutoff, /4
  - Stage 3: 64 taps, 8 kHz cutoff, /2
- Coefficients: Kaiser window (designed via `scipy.signal.firwin`), 18-bit quantized, >137 dB stopband

### Ring-Buffer DMA Rework

- Replaced one-shot `DmaStreamWrite` with a new `DmaStreamRingWrite` module (ported from the never-merged upstream IQ stream branch)
- 8 sub-buffers x 4 KB = 32 KB ring per DMA channel
- Sub-buffer completion raises an interrupt on the AXI B-channel write response
- New `demod_status.last_buffer` field (3 bits) tracks which sub-buffer was just written
- Continuous operation -- no PS-side restart per buffer
- `demod_control` / `traffic_demod_control` simplified: start/stop Wpulses removed, only `demod_enable` (RW level) remains
- `demod_status` / `traffic_demod_status` grow the `last_buffer` field; all other bank offsets unchanged (`ddc_frequency`=0x2C, `ddc_control`=0x30, `demod_status`=0x40, etc.)
- SVD and PAC regenerated

### DMA Address Layout

- FPGA hardcodes physical buffer addresses `0x17000000` (dibit) and `0x18000000` (traffic)
- Reduced from 1 MB each to 32 KB each to match the new ring size
- Device tree updated: `reg = <0x17000000 0x8000>` and `<0x18000000 0x8000>`

### Hardware Test Results

- Board booted with P25 bitstream, FPGA registers accessible
- Product ID: 0x70323566 ("p25f")
- AD9361 configured: 858.1 MHz center / 8 MSPS
- Dibit counter incrementing (DSP chain active)
- Web UI served on port 8080
- SDRTrunk confirmed P25 signal at 860.9625 MHz (NAC:2209, WACN:781824, System:2208)

### Unified C4FM/LSM Differential Demod

- Local control channel turned out to be LSM (CQPSK), not C4FM. The original FM-only cross-product discriminator could not produce LSM dibits 1 and 3
- Refactored `C4FMDemod` to compute the full complex differential product `z[n] * conj(z[n-1])` (4 DSP48E1 multiplies instead of 2). Outputs `diff_re_out` and `diff_im_out`; `disc_out` retained as a legacy alias for `diff_im_out`
- Refactored `SymbolTimingRecovery` to take both `diff_re_in` and `diff_im_in` and use a sign-bit slicer that maps the four quadrants of the differential plane to the four P25 dibit values, matching SDRTrunk's `P25P1DemodulatorLSM.toDibit` exactly. Works for both C4FM and LSM with no path divergence
- `p25_top.py` wires the new IQ pair through both control and traffic chains
- Updated `test_c4fm_demod.py` and `test_symbol_timing.py` to drive and verify the new I/Q differential interface; all 16 P25 HDL tests pass

### p25-httpd Diagnostic Logging

Added structured tracing and a `/api/dibit_dump` endpoint to make hardware bring-up debuggable from the web UI without devmem on the target.

- `target=p25_irq` -- IRQ arrival counter (first 10, then every 64th)
- `target=p25_reader` -- per-wakeup buffer count, byte total, dibit histogram
- `target=p25_stats` -- FPGA register state (`dibit_count`, `overflow`, `last_buffer`, `next_addr`) every 2 s
- `target=p25_decoder` -- periodic dibit histogram + sync correlator stats, near-sync events (Hamming distance ≤ 12), state transitions, NID Golay decode results
- `GET /api/dibit_dump` -- JSON: `total_dibits`, per-value histogram with percentages, sync correlator stats (`hits`, `near_misses`, `best_distance`), and the last 2048 dibits packed as hex
- Dashboard: two new cards ("Dibit Histogram", "Sync Correlator") that poll `/api/dibit_dump` and surface the histogram + best Hamming distance live

---

## [2026-04-08] P25 Migration into maia-sdr

**Branch:** fishball-p25

Migrated the P25 trunking radio from the standalone `fishball-p25` repo into
the maia-sdr tree. The standalone approach failed due to relative path breakage,
IIO DMA not routed, and DTS/bitstream mismatches.

### P25 FPGA Gateware (Phases 0-1)

- `maia-hdl/p25_hdl/` -- Amaranth HDL modules: C4FM demod, Gardner symbol timing, dibit packer
- `maia-hdl/ip/p25-core/` -- Vivado IP packaging (TCL + constraints)
- `maia-hdl/projects/fishball7020_p25/` -- Vivado project (block design, constraints, top-level wrapper)
- `maia-hdl/test/` -- 14 Amaranth simulation tests (C4FM, symbol timing, dibit packer)
- `maia-hdl/generate_p25_svd.py` -- SVD generation for P25 register PAC
- Dual DDC + demod chains: control channel + traffic channel with independent NCO
- Spectrometer + recorder removed (unused in P25), freeing ~16 DSP48, ~5K LUT, ~12 BRAM
- Resource usage: 36 DSP48 (16%), 15.6K LUT (29%), 16 BRAM (11%) on Z7020

### P25 Firmware (Phases 2-3)

- `p25-httpd/` -- Rust workspace with `p25-json` and `p25-pac` sub-crates
- Control channel decoder: frame sync, NID extraction, TSDU de-interleave, TSBK parser
- FEC: Golay(23,12), trellis Viterbi, CRC-16-CCITT
- 6 TSBK opcodes: GRP_V_CH_GRANT, GRP_V_CH_GRANT_UPDT, IDEN_UP, NET_STS_BCST, RFSS_STS_BCST, ADJ_STS_BCST
- Traffic manager: grant lifecycle, NCO word calculation, timeout management
- Web dashboard: REST API (6 endpoints), WebSocket events, embedded SPA
- 16 Rust unit tests passing

### Build Infrastructure

- `build_fpga.bat --p25` builds P25 FPGA (fishball7020_p25)
- P25 Tezuka config: `fishball_p25_7020_defconfig`

See `doc/changes/003_p25_scaffolding.md` through `doc/changes/007_p25_traffic_channel.md` for details.

---

## [2026-04-07] Upstream Sync

**Branch:** fishball-dev

Rebased `fishball-dev` onto `upstream/main`, picking up 10 new commits from
F5OEO: Vivado 2023 env, WASM broadcast channel, interpolation coefficients,
overclock simplification, FIFO redesign, clock simplification, x32 interpolator,
RX2 fix, DAC FIR support, RX with FIFO.

Also analyzed the upstream `refactor` branch (53 commits, 18K lines, not yet
merged to main) -- includes board-based project layout, common TCL library,
DVB-S2 receiver, IQ burst capture, frequency sweeper, multi-board sync.

See `doc/changes/002_upstream_sync.md` for full analysis.

---

## [2026-03-08] Project Setup

**Branch:** fishball-dev

### Fork & Repository

- Forked `F5OEO/maia-sdr` -> `andylee77/maia-sdr`
- Created `fishball-dev` branch for Fishball Z7020 development
- Set up upstream tracking: `upstream` -> `F5OEO/maia-sdr`, `origin` -> `andylee77/maia-sdr`

### Build Infrastructure (Initial)

- In-repo build scripts (see `doc/changes/001_build_scripts.md`):
  - `build_hdl.bat/.sh` -- Amaranth -> Verilog + SVD via Docker
  - `build_fpga.bat` -- Full Vivado FPGA synthesis (Windows native)
  - `sim_hdl.bat/.sh` -- HDL simulation via Docker (Amaranth + cocotb)
  - `clean.bat` -- Clean all build artifacts
- Docker volume `maia-hdl-build` for persistent Python venv cache
- Tezuka firmware pointed to this fork's `fishball-dev` branch

---

## Pending / Future

- [ ] Phase 5 (remaining): Live control channel decode, traffic following test, SD card clean boot
- [ ] Voice frame extraction (LDU1/LDU2 -> IMBE frames)
- [ ] Audio codec (mbelib, codec2, or DVSI)
- [ ] Document IQ streaming patches to maia-httpd
- [ ] FPGA ring buffer implementation for high-bandwidth IQ
