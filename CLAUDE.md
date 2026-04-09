# Maia SDR + Fishball P25 — Project Rules

## Project Overview

This is the **Maia SDR** project with the **Fishball P25** trunking radio added on the
`fishball-p25` branch. Maia SDR provides the base FPGA platform (AD9361, IIO DMA, DDC,
register infrastructure). P25 adds C4FM demod, symbol timing, dibit DMA, and a Rust
control channel decoder.

- **Fork:** `andylee77/maia-sdr` (forked from `F5OEO/maia-sdr`, originally `maia-sdr/maia-sdr`)
- **Branches:** `fishball-dev` (Maia SDR), `fishball-p25` (P25 radio)
- **Target hardware:** Fishball Z7020 (Zynq-7020 + AD9361)

## Components

| Component | Language | Purpose |
|-----------|----------|---------|
| `maia-hdl/maia_hdl/` | Python (Amaranth) + Vivado | Maia FPGA gateware (DDC, DMA, registers) |
| `maia-hdl/p25_hdl/` | Python (Amaranth) | P25 FPGA gateware (C4FM demod, symbol timing, dibit packer) |
| `maia-httpd/` | Rust | Maia HTTP daemon on Zynq ARM |
| `p25-httpd/` | Rust | P25 HTTP daemon (control channel decoder, web UI) |
| `maia-wasm/` | Rust → WASM | Web UI (waterfall, WebGL2) |
| `maia-kmod/` | C | Linux kernel module for DMA |

## Key Directories

- `maia-hdl/maia_hdl/` — Maia HDL source modules
- `maia-hdl/p25_hdl/` — P25 HDL source modules
- `maia-hdl/p25_hdl/p25_top.py` — P25 top-level IP core
- `maia-hdl/ip/p25-core/` — P25 Vivado IP packaging
- `maia-hdl/projects/fishball7020_iio/` — Maia FPGA project
- `maia-hdl/projects/fishball7020_p25/` — P25 FPGA project
- `maia-hdl/projects/` — Vivado project definitions per board
- `maia-httpd/src/` — Maia httpd source
- `p25-httpd/src/` — P25 httpd source
- `p25-httpd/p25-pac/` — P25 FPGA register PAC (from SVD)
- `p25-httpd/p25-json/` — P25 JSON API types
- `doc/DEVPLAN.md` — P25 development roadmap
- `doc/BUILD_FPGA.md` — P25 FPGA bitstream build guide
- `doc/changes/` — Detailed change documentation (Maia + P25)
- `maia-wasm/src/` — WASM source (waterfall, UI, WebSocket)

## Build Environment

### FPGA (maia-hdl)

- **Vivado:** 2023.2 (Xilinx)
- **Amaranth:** Python virtual environment
- **Maia build:** `build_fpga.bat` (builds fishball7020_iio)
- **P25 build:** `build_fpga.bat --p25` (builds fishball7020_p25)
- **P25 build guide:** `doc/BUILD_FPGA.md`

### Firmware

- Built by **Tezuka firmware** Buildroot
- Tezuka's build mounts this repo at `/mnt/maia-sdr` in Docker
- **Maia:** `fishball-dev` branch, `fishball_maiasdr_7020_defconfig`
- **P25:** `fishball-dev` branch (P25 defconfig TBD -- Phase 5)
- Cross-compiled for ARM (Zynq-7000) inside the Tezuka Docker build

### Submodules

- `maia-hdl/adi-hdl` → `analogdevicesinc/hdl` (Analog Devices HDL library)
- `maia-hdl/XilinxUnisimLibrary` → Xilinx simulation primitives

## Git Workflow

- **`main`** branch tracks upstream — keep clean for syncing
- **`fishball-dev`** is the Maia SDR development branch
- **`fishball-p25`** is the P25 radio branch (forked from fishball-dev)
- Maia changes go on `fishball-dev`, committed with descriptive messages
- P25 changes go on `fishball-p25`
- Each significant change gets a doc in `doc/changes/NNN_description.md`
- Do NOT modify the upstream `CHANGELOG.md`

## Conventions

- FPGA projects per board live in `maia-hdl/projects/<board>/`
- Maia target project: `maia-hdl/projects/fishball7020_iio/`
- P25 target project: `maia-hdl/projects/fishball7020_p25/`
- Maia Rust workspace: `maia-httpd/` with sub-crates `maia-json` and `maia-pac`
- P25 Rust workspace: `p25-httpd/` with sub-crates `p25-json` and `p25-pac`
- WASM build: `maia-wasm/` uses wasm-pack
- Build outputs go to build-specific directories (gitignored by component)
- Use `doc/changes/` for detailed technical docs (Maia + P25 changes)
- P25 dev plan: `doc/DEVPLAN.md`
- P25 dev log: merged into root `DEVLOG.md`
- Update `CHANGELOG_FORK.md` with each change (Maia and P25)

## Related Repositories

| Repo | Location | Purpose |
|------|----------|---------|
| `andylee77/tezuka_fw` | `C:\Users\Andy\Projects\Tezuka\tezuka_fw` | Firmware (Buildroot) |
| `andylee77/sdrtrunk` | `C:\Users\Andy\Projects\SDRTrunk\sdrtrunk` | SDRTrunk (P25 reference) |
| `andylee77/fishball-p25` | archived | Original standalone P25 repo (migrated here) |
| Upstream `F5OEO/maia-sdr` | remote `upstream` | F5OEO's fork |
| Original `maia-sdr/maia-sdr` | — | Daniel Estévez's original |

## Shared Documentation

- `C:\Users\Andy\Projects\_shared\` — Cross-project docs (build systems, environment, hardware refs)
- `C:\Users\Andy\Projects\_shared\Hardware\` — Board schematics, pinouts, block diagrams
  - `schematic/` — Fishball 7020 schematic analysis (FPGA banks, AD9361 interface, peripherals)
  - `BOARD_BLOCK_DIAGRAMS.md` — Block diagrams for all supported boards
  - `PIN_COMPATIBILITY_E200_VS_FISHBALL.md` — Pin compatibility analysis
- `C:\Users\Andy\Projects\_shared\BUILD_SYSTEMS.md` — Full firmware build pipeline overview
- `C:\Users\Andy\Projects\_shared\VIVADO_2023_INSTALL_GUIDE.md` — Vivado installation guide
- `C:\Users\Andy\Projects\MAIA_SDR\work_docs\` — Work documentation (Vivado build guide, dev plans)
- `C:\Users\Andy\Projects\MAIA_SDR\build_scripts\` — Original build scripts (reference)
