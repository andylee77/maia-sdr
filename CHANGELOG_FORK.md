# Maia SDR — Changelog (andylee77 fork)

Tracking log for the `fishball-dev` branch of the Maia SDR fork.
Upstream: [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr) (originally [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr))

---

## [2026-03-08] Project Setup

### Fork & Repository
- Forked `F5OEO/maia-sdr` → `andylee77/maia-sdr`
- Created `fishball-dev` branch for Fishball Z7020 development
- Set up upstream tracking: `upstream` → `F5OEO/maia-sdr`, `origin` → `andylee77/maia-sdr`
- Working copy: `C:\Users\Andy\Projects\MAIA_SDR\maia-sdr`

### Build Infrastructure
- In-repo build scripts created (see `doc/changes/001_build_scripts.md`):
  - `build_hdl.bat/.sh` — Amaranth → Verilog + SVD via Docker
  - `build_fpga.bat` — Full Vivado FPGA synthesis (Windows native)
  - `sim_hdl.bat/.sh` — HDL simulation via Docker (Amaranth + cocotb)
  - `clean.bat` — Clean all build artifacts
- Docker volume `maia-hdl-build` for persistent Python venv cache
- `.gitignore` updated for build artifacts
- Work documents at `C:\Users\Andy\Projects\MAIA_SDR\work_docs\`
- Tezuka firmware pointed to this fork's `fishball-dev` branch (maia-httpd.mk, maia-wasm.mk)

### Components
- **maia-hdl** — FPGA gateware (Amaranth/Python, Vivado 2023.2)
- **maia-httpd** — HTTP daemon (Rust, cross-compiled for ARM)
- **maia-wasm** — Web UI (Rust/WebAssembly, WebGL2 waterfall)
- **maia-kmod** — Kernel module (C, DMA buffer management)

---

## Pending / Future

- [ ] Document IQ streaming patches to maia-httpd
- [ ] FPGA ring buffer implementation for high-bandwidth IQ
- [ ] Fishball 7020 Vivado project build and verification
- [ ] Persist httpd patches into fork cleanly
- [ ] Test FPGA gateware changes on hardware
