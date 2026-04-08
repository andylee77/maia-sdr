# Maia SDR — Project Rules

## Project Overview

This is the **Maia SDR** project — an open-source FPGA-based SDR platform for the ADALM Pluto
and compatible boards (including Fishball Z7020). It provides a web-based spectrum analyzer with
real-time waterfall display and IQ recording.

- **Fork:** `andylee77/maia-sdr` (forked from `F5OEO/maia-sdr`, originally `maia-sdr/maia-sdr`)
- **Active branch:** `fishball-dev`
- **Target hardware:** Fishball Z7020 (Zynq-7020 + AD9361), ADALM Pluto, Pluto+

## Components

| Component | Language | Purpose |
|-----------|----------|---------|
| `maia-hdl/` | Python (Amaranth) + Vivado | FPGA gateware |
| `maia-httpd/` | Rust | HTTP daemon on Zynq ARM |
| `maia-wasm/` | Rust → WASM | Web UI (waterfall, WebGL2) |
| `maia-kmod/` | C | Linux kernel module for DMA |

## Key Directories

- `maia-hdl/maia_hdl/` — HDL source modules
- `maia-hdl/projects/fishball7020_iio/` — Our target board project
- `maia-hdl/projects/` — Vivado project definitions per board
- `maia-httpd/src/` — Main httpd source
- `maia-httpd/maia-json/` — JSON API types
- `maia-httpd/maia-pac/` — FPGA register PAC (from SVD)
- `maia-wasm/src/` — WASM source (waterfall, UI, WebSocket)

## Build Environment

### FPGA (maia-hdl)
- **Vivado:** 2023.2 (Xilinx)
- **Amaranth:** Python virtual environment

### Firmware (maia-httpd + maia-wasm)
- Built by **Tezuka firmware** Buildroot (not built standalone here)
- Tezuka's build pulls from this repo's `fishball-dev` branch
- Cross-compiled for ARM (Zynq-7000) inside the Tezuka Docker build

### Submodules
- `maia-hdl/adi-hdl` → `analogdevicesinc/hdl` (Analog Devices HDL library)
- `maia-hdl/XilinxUnisimLibrary` → Xilinx simulation primitives

## Git Workflow

- **`main`** branch tracks upstream — keep clean for syncing
- **`fishball-dev`** is the active development branch
- All changes go on `fishball-dev`, committed with descriptive messages
- Each change gets a doc in `doc/changes/NNN_description.md`
- Update `CHANGELOG_FORK.md` with each change and build
- Do NOT modify the upstream `CHANGELOG.md`

## Conventions

- FPGA projects per board live in `maia-hdl/projects/<board>/`
- Our target board project: `maia-hdl/projects/fishball7020_iio/`
- Rust workspace: `maia-httpd/` is a Cargo workspace with sub-crates `maia-json` and `maia-pac`
- WASM build: `maia-wasm/` uses wasm-pack
- Build outputs go to build-specific directories (gitignored by component)
- Use `doc/changes/` for detailed technical docs about each change

## Related Repositories

| Repo | Location | Purpose |
|------|----------|---------|
| `andylee77/tezuka_fw` | `C:\Users\Andy\Projects\Tezuka\tezuka_fw` | Firmware (Buildroot) |
| `andylee77/sdrtrunk` | `C:\Users\Andy\Projects\SDRTrunk\sdrtrunk` | SDRTrunk fork |
| Upstream `F5OEO/maia-sdr` | remote `upstream` | F5OEO's fork |
| Original `maia-sdr/maia-sdr` | — | Daniel Estévez's original |

## Shared Documentation

- `C:\Users\Andy\Projects\_shared\` — Cross-project docs (build systems, environment, hardware refs)
- `C:\Users\Andy\Projects\MAIA_SDR\work_docs\` — Work documentation
- `C:\Users\Andy\Projects\MAIA_SDR\build_scripts\` — Build scripts
