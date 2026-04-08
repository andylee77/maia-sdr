# Upstream Sync — 2026-04-07

## Summary

Rebased `fishball-dev` onto `upstream/main` (F5OEO/maia-sdr), picking up 10 new
commits. Also documented the state of the upstream `refactor` branch which
contains significant architectural changes and new features not yet merged to main.

---

## What the rebase added (upstream/main, 10 commits)

These commits were integrated into `fishball-dev` via rebase on 2026-04-07.

### 1. Vivado 2023 environment update

- **Commit:** `c49db1f` — *Use vivado 2023*
- Updated `sourceme.first` to target Vivado 2023 toolchain

### 2. WASM broadcast channel for FFT overlay

- **Commit:** `f3d9a1c` — *Add broadcast channel to wasm in order to use fft bin in a overlay jscript*
- Added `BroadcastChannel` support to `maia-wasm/src/websocket.rs`
- Allows external JavaScript to access FFT bin data via browser broadcast API
- New dependency added to `maia-wasm/Cargo.toml`

### 3. Interpolation coefficients tooling

- **Commit:** `42f370c` — *Add interpolation coefficients - Tool to convert gnuradio filter to coe*
- New `convert_taps.py` tool in `maia-hdl/projects/pluto/` to convert GNURadio
  filter taps to Xilinx `.coe` format
- Added `firinterp32.coe` (236 coefficients) for x32 FIR interpolation

### 4. Overclock simplification

- **Commit:** `1e8cb6f` — *Overclock only few parameters in order to save size*
- Reduced overclock scripts across all board projects (e200, fishball7020,
  fishball, libre, pluto, plutoplus) to only essential parameters
- Saves FPGA resource utilization

### 5. FIFO redesign (major update)

- **Commit:** `6dad239` — *FIFO redesign - Major Update*
- Touched 21 files across all board projects
- Updated Makefiles, `system_bd.tcl`, `system_project.tcl`, and constraint files
- Affected boards: e200, fishball7020, fishball, libre, pluto, pluto_iio,
  plutoplus, plutoplus_iio
- Added new project TCL includes and updated build dependencies

### 6. Clock simplification for Pluto

- **Commit:** `a8651b6` — *Simplify clock for maia*
- Removed 38 lines of clock configuration from `pluto/system_bd.tcl`
- Simplified clock tree for the Pluto target

### 7. Interpolator x8 to x32

- **Commit:** `d3643cb` — *Change interpolator from x8 to x32*
- New `filter32.coe` coefficients for x32 interpolation on Pluto
- Updated FIR configuration in `pluto/system_bd.tcl`

### 8. RX2 fix

- **Commit:** `2347bcc` — *Fix RX2*
- Single-line fix in `pluto/system_bd.tcl` for second receiver chain

### 9. DAC with/without FIR

- **Commit:** `cedcd64` — *DAC seems working with and without FIR*
- Major rework of `pluto/system_bd.tcl` (+91 / -51 lines)
- DAC path now supports both FIR-filtered and direct output modes

### 10. RX with FIFO

- **Commit:** `fdd361b` — *Rx with fifo works*
- Significant update to `pluto/system_bd.tcl` (+178 / -58 lines)
- Updated fishball_iio constraint file
- Added Makefile dependency for Pluto project

---

## Upstream `refactor` branch analysis

The `upstream/refactor` branch has **53 commits ahead of `upstream/main`**,
totalling ~18,000 lines of new code across 131 files. This is a major
restructuring effort by F5OEO that has **not yet been merged to main**.

### Architecture changes

The refactor fundamentally reorganizes the project structure:

#### Board-based project layout

- **New structure:** `maia-hdl/projects/boards/<board>/` contains per-board
  hardware definitions (`ports.tcl`, `ps7.tcl`, `system_constr.xdc`,
  `system_top.v`, `vcxo_ctrl.tcl`)
- **Supported boards:**
  - `pluto` — ADALM Pluto
  - `plutoplus` — Pluto+
  - `libre` — LibreSDR
  - `fishball7010` — Fishball Z7010
  - `fishball7020` — Fishball Z7020
  - `e200` — E200
  - `e310` — E310
  - `nano` — Nano (7010-based)
  - `signalsdrpro` — SignalSDR Pro
- Board definitions are decoupled from project configurations

#### Common TCL library

- **New:** `maia-hdl/projects/common/` with shared build blocks:
  - `xilinx_ad9361.tcl` / `xilinx_ad9361_no_pack.tcl` — AD9361 integration
  - `xilinx_init.tcl` — Vivado project initialization
  - `maia.tcl` — Standard Maia SDR block design
  - `minimal.tcl` — Minimal block design (no Maia IP)
  - `rxfir.tcl` / `txfir.tcl` — RX/TX FIR filter blocks
  - `sweeper.tcl` — Frequency sweeper
  - `sync.tcl` — Multi-board synchronization
  - `cs12_cs8.tcl` — 12-bit/8-bit sample conversion with RX channel OR for 2R2T
  - `uartlite.tcl` — UART peripheral
  - Shared FIR coefficient files (`filter4_coeffs.coe`, `filter8_coeffs.coe`,
    `firinterp32.coe`)

#### Tezuka integration

- **New:** `maia-hdl/projects/tezuka/` — Dedicated Tezuka firmware project
  with its own Makefile, block design, and environment config (`tezuka_env.tcl`)
- The Tezuka project uses the common TCL library to compose board-specific builds

### New features

#### DVB-S2 receiver (Amaranth)

- **New modules** in `maia-hdl/maia_hdl/`:
  - `dvbs2rx.py` — Top-level DVB-S2 receiver
  - `dvbs2_frontend.py` — RF frontend processing
  - `dvbs2_pll.py` — PLL for carrier recovery (~1000 lines)
  - `dvbs2_bb_soft.py` — Baseband soft-decision processing (~1076 lines)
  - `dvbs2_deinterleaver.py` — Deinterleaver (~943 lines)
  - `dvbs2_documentation.md` — Design documentation
- Packaged as IP: `maia-hdl/ip/dvbs2rx/`
- Status: "First attempt" — likely experimental

#### IQ burst capture (Amaranth)

- `maia-hdl/maia_hdl/iqburst.py` — Burst IQ capture module (~241 lines)
- Packaged as IP: `maia-hdl/ip/iqburst/`
- Note: Could not be inserted on 7010 (resource limited)

#### Modulator (Amaranth)

- `maia-hdl/maia_hdl/modulator.py` — Signal modulator (~385 lines)
- Status: "Try to add a new component" — experimental

#### Raw FFT output

- `maia-hdl/maia_hdl/topfft.py` — Top-level FFT wrapper (~235 lines)
- Modified `fft.py` and `spectrometer.py` for raw FFT sample output
- Added `fftraw` output parameter
- Multiple attempts noted — may still be work-in-progress

#### Frequency sweeper

- `sweeper.tcl` + `sweeper_it.v` — Frequency sweep functionality
- 8/12/16-bit conversion format support (`cs12_cs8.tcl`, `cs12_cs8mux.v`)

#### Multi-board synchronization

- `sync.tcl` — Synchronization mechanism for multi-board setups
- 1PPS timing support
- VCXO control for Fishball 7020 (`vcxo_ctrl.v`, `vcxo_ctrl.tcl`)
- GPIO-based sync in/out with ADC GPIO bit 3

#### New board support

- **E310** — Full project with block design, constraints, overclock scripts
- **Nano** — 7010-based board
- **SignalSDR Pro** — Full project definition
- **Fishball 7020 sync** — Dedicated sync variant project

### Items of interest for fishball-dev

| Feature | Relevance | Notes |
| ------- | --------- | ----- |
| Board-based project layout | High | Our `fishball7020_iio` project would need to adapt to the new `boards/fishball7020/` structure if we merge |
| Common TCL library | High | Would simplify our Vivado project maintenance |
| Tezuka project | High | Directly relevant — our firmware build uses Tezuka |
| VCXO control for 7020 | High | `fishball7020/vcxo_ctrl.v` — clock discipline for our board |
| IQ burst capture | Medium | Useful feature but may not fit on all targets |
| DVB-S2 receiver | Low | Large feature, experimental, niche use case |
| Raw FFT output | Medium | Still WIP upstream, could be useful for analysis |
| Multi-board sync | Low-Medium | Only relevant if running multiple boards |

### Risk assessment for future merge

- **High risk:** The refactor restructures project directories significantly.
  Merging will likely conflict with our `fishball7020_iio` project changes.
- **Recommendation:** Monitor the refactor branch. When it merges to
  `upstream/main`, plan a dedicated effort to adapt `fishball-dev` to the new
  structure. Cherry-picking specific features (VCXO, IQ burst) may be easier
  than a full merge.
- **Action items:**
  - [ ] Watch for refactor → main merge in upstream
  - [ ] Evaluate VCXO control module for standalone cherry-pick
  - [ ] Test common TCL library compatibility with our build scripts
