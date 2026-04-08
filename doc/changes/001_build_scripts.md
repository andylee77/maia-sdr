# 001 — In-Repo Build Scripts

**Date:** 2026-03-08
**Branch:** fishball-dev
**Author:** andylee77

---

## Summary

Created build scripts inside the maia-sdr repo, following the same pattern established
in the `tezuka_fw` repository (`build.bat` / `build.sh` at the repo root).

Previously, build scripts lived outside the repo at
`C:\Users\Andy\Projects\MAIA_SDR\build_scripts\` with hardcoded paths to
`C:\Users\Andy\Downloads\...`. This change makes the scripts self-contained,
portable, and version-controlled.

## Files Added

| File | Purpose |
|------|---------|
| `build_hdl.bat` | Windows launcher: Docker → Amaranth → Verilog + SVD |
| `build_hdl.sh` | Docker-internal: Python venv setup, Amaranth elaboration |
| `build_fpga.bat` | Windows: Full Vivado FPGA synthesis (IP packaging → bitstream → XSA) |
| `sim_hdl.bat` | Windows launcher: Docker → HDL simulation |
| `sim_hdl.sh` | Docker-internal: pytest (Tier 1) + cocotb/iverilog (Tier 2) |
| `clean.bat` | Windows: Clean all build artifacts (Vivado, IP, Docker) |

## Files Modified

| File | Change |
|------|--------|
| `.gitignore` | Added build artifact patterns (Vivado, generated IP, SVD, sim output) |
| `DEVLOG.md` | Documented build scripts, options, environment variables |
| `CHANGELOG_FORK.md` | Logged this change |

## Architecture

### Build Flow

```
build_hdl.bat  →  Docker (python:3.11-slim)  →  build_hdl.sh
                   ↓
                   maia_sdr.v    (Verilog netlist)
                   maia-sdr.svd  (register map)
                   ↓
build_fpga.bat →  Vivado (native Windows)
                   ↓
                   system_top.xsa  (FPGA bitstream)
                   ↓
                   (optional) copy to tezuka_fw
```

### Docker Volume

- Name: `maia-hdl-build`
- Contains: Python venv with amaranth, numpy, scipy, amaranth-yosys, pytest
- Shared between `build_hdl` and `sim_hdl` (same dependencies)
- First run: ~2 min to install packages
- Subsequent runs: instant (reuses cached venv)

### Key Design Decisions

1. **Docker instead of WSL** — More portable and reproducible. The old scripts
   used WSL directly but Docker gives the same ext4 benefits with better isolation.

2. **Separate HDL and FPGA scripts** — HDL generation is fast (~2 min) and needs
   Docker. FPGA synthesis takes ~15-30 min and needs native Vivado. Keeping them
   separate lets you iterate on HDL without re-running synthesis.

3. **Relative paths** — All scripts use `%~dp0` (bat) or `$(dirname "${BASH_SOURCE[0]}")`
   (sh) for portability. No hardcoded user-specific paths.

4. **Vivado auto-detection** — `build_fpga.bat` checks multiple common install
   locations for Vivado 2023.2 and 2025.2, with `VIVADO_DIR_OVERRIDE` for custom paths.

5. **Tezuka integration** — `build_fpga.bat` attempts to copy the XSA to the
   tezuka_fw repo if found at `../../Tezuka/tezuka_fw` or via `TEZUKA_FW` env var.

## Usage Quick Reference

```bat
REM Generate Verilog + SVD from Amaranth HDL
build_hdl.bat

REM Full FPGA build (auto-generates Verilog if missing)
build_fpga.bat

REM Run HDL tests
sim_hdl.bat --tier1

REM Clean everything including Docker cache
clean.bat --all
```
