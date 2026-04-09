# Maia SDR -- Changelog (andylee77 fork)

Tracking log for the `andylee77/maia-sdr` fork.
Upstream: [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr) (originally [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr))

---

## [2026-04-09] Phase 5: Build Pipeline, Register Fix, DDC Init, First Hardware Boot

**Branch:** fishball-p25

First successful hardware boot of the P25 FPGA bitstream on the Fishball Z7020.
Fixed multiple build pipeline issues from the standalone repo migration, corrected
the SVD register map, and added DDC FIR coefficient initialization.

### Build Pipeline Fixes

- Fixed P25 Verilog generation to use Docker (ext4 filesystem avoids NTFS pip issues)
- Fixed CMD escaping in `build_fpga.bat` for Docker invocation
- Added missing ADI library builds: `util_clkdiv`, `util_rfifo`, `util_wfifo`
- Fixed stale TCL/Makefile paths left over from standalone repo migration
- Added `.gitattributes` enforcing LF line endings on `.sh` files
- Added skip-if-built logic for incremental ADI library builds

### Tezuka Firmware Fixes

- Added XSA cache invalidation in `build.sh` (detects when source XSA is newer than cached package)
- Added p25-httpd/maia-httpd source change detection for auto-rebuild
- Removed phantom AXI UART Lite from P25 device tree (was at 0x42C00000, not present in P25 FPGA design)

### Register Map Fix

- SVD register offsets were wrong: FPGA uses bank select bits [4:3] of word address giving byte offsets 0x00/0x20/0x40/0x60, but SVD had 0x00/0x08/0x20/0x30
- Fixed `RegisterMap` in `p25_top.py`, regenerated SVD and PAC

### DDC FIR Coefficient Initialization

- Added 3-stage FIR filter coefficient loading to p25-httpd (`fpga.rs`)
- Previously only NCO frequency and enable bits were programmed
- P25 channel filter: 128x decimation (16x4x2), 8 MSPS -> 62.5 kSPS (13 samples/symbol)
- Coefficients: 48+32+64 taps, Kaiser window, 18-bit quantized, >137 dB stopband

### Hardware Test Results

- Board booted with P25 bitstream, FPGA registers accessible
- Product ID: 0x70323566 ("p25f")
- AD9361 configured: 858.1 MHz center / 8 MSPS
- Dibit counter incrementing (DSP chain active)
- Web UI served on port 8080
- SDRTrunk confirmed P25 signal at 860.9625 MHz (NAC:2209, WACN:781824, System:2208)

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
