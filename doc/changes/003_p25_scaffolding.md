# 003 -- P25 Project Scaffolding

**Date:** 2026-04-08
**Phase:** 0 (Project Setup)
**Branch:** fishball-p25

---

## Summary

Scaffolded the P25 trunking radio components within the maia-sdr tree on the
`fishball-p25` branch. Originally created in a standalone `fishball-p25` repo,
then migrated in-tree after the standalone approach failed (relative path
breakage, IIO DMA not routed, DTS/bitstream mismatches).

## What Was Created

### FPGA Gateware (`maia-hdl/p25_hdl/`)

- `p25_top.py` -- Top-level IP core, wraps Maia DDC + new P25 DSP chain
- `c4fm_demod.py` -- C4FM FM discriminator (cross-product method)
- `symbol_timing.py` -- Gardner timing error detector + PI loop filter
- `dibit_packer.py` -- Pack dibits into 64-bit DMA words
- `config.py`, `configs.py` -- Build configuration classes

### Vivado Project (`maia-hdl/ip/` + `maia-hdl/projects/`)

- `maia-hdl/ip/p25-core/` -- IP packaging scripts and constraints
- `maia-hdl/projects/fishball7020_p25/` -- Vivado block design, constraints, top-level wrapper
- Based on Maia's `fishball7020_iio/` project

### PS Application (`p25-httpd/`)

- Rust workspace with `p25-json` and `p25-pac` sub-crates
- Skeleton `fpga.rs`, `iio.rs`, `main.rs`
- P25 protocol decoder directory structure (`src/p25/`)

### Tests (`maia-hdl/test/`)

- `test_c4fm_demod.py` -- C4FM demodulator simulation test
- `test_symbol_timing.py` -- Symbol timing recovery test
- `test_dibit_packer.py` -- Dibit packer test

### SVD Generation

- `maia-hdl/generate_p25_svd.py` -- SVD generation for register PAC

## Design Decisions

- **Reuse Maia DDC directly** -- same 3-stage FIR architecture, different coefficients for P25
- **Frame sync in PS software** -- at 4800 sym/sec, ARM easily handles NID correlation
- **Keep Spectrometer + Recorder from Maia** -- useful as debug tools (later removed in Phase 3)
- **Rust patterns adapted, not submoduled** -- UIO driver, IIO, SVD workflow copied from maia-httpd

## Next Steps

- Phase 1A: Simulation tests with synthetic C4FM waveforms
- Phase 1B: Hardware integration test with recorded P25 IQ
