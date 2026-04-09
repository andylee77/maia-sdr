# 001 -- Project Scaffolding

**Date:** 2026-04-08
**Phase:** 0 (Project Setup)

---

## Summary

Created the `fishball-p25` repository with full project scaffolding for an FPGA-based P25 Phase 1 trunking radio targeting the Fishball Z7020 board.

## What Was Created

### FPGA Gateware (`p25_hdl/`)

- `p25_top.py` -- Top-level IP core, wraps Maia DDC + new P25 DSP chain
- `c4fm_demod.py` -- C4FM FM discriminator (cross-product method)
- `symbol_timing.py` -- Gardner timing error detector + PI loop filter
- `dibit_packer.py` -- Pack dibits into 64-bit DMA words
- `config.py`, `configs.py` -- Build configuration classes

### Vivado Project (`ip/` + `projects/`)

- `ip/p25-core/` -- IP packaging scripts and constraints
- `projects/fishball7020_p25/` -- Vivado block design, constraints, top-level wrapper
- Based on Maia's `fishball7020_iio/` project

### PS Application (`p25-httpd/`)

- Rust workspace with `p25-json` and `p25-pac` sub-crates
- Skeleton `fpga.rs`, `iio.rs`, `main.rs`
- P25 protocol decoder directory structure (`src/p25/`)

### Tests (`test/`)

- `test_c4fm_demod.py` -- C4FM demodulator simulation test
- `test_symbol_timing.py` -- Symbol timing recovery test
- `test_dibit_packer.py` -- Dibit packer test

### Submodule

- `ext/maia-sdr/` -> `andylee77/maia-sdr` (`fishball-dev` branch)
- Provides `maia_hdl` package: DDC, DMA, registers, spectrometer, recorder

### Build & Config

- `Makefile` -- Top-level build targets
- `pyproject.toml` -- Python project config
- `generate_p25_svd.py` -- SVD generation for register PAC
- `.gitignore` -- Editor, Python, Vivado, Rust, simulation artifacts

## Design Decisions

- **Reuse Maia DDC directly** -- same 3-stage FIR architecture, different coefficients for P25
- **Frame sync in PS software** -- at 4800 sym/sec, ARM easily handles NID correlation
- **Keep Spectrometer + Recorder from Maia** -- useful as debug tools
- **Rust patterns adapted, not submoduled** -- UIO driver, IIO, SVD workflow copied from maia-httpd

## Next Steps

- Phase 1A: Simulation tests with synthetic C4FM waveforms
- Phase 1B: Hardware integration test with recorded P25 IQ
