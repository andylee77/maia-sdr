# Maia SDR — Developer Log

Reference for repo structure, build systems, and infrastructure in the `andylee77/maia-sdr` fork.

---

## Repository Layout

```
maia-sdr/
├── CHANGELOG_FORK.md         ← Fork tracking log (our changes, builds)
├── CHANGELOG.md              ← Upstream changelog (do not modify)
├── DEVLOG.md                 ← This file (developer reference)
├── README.md                 ← Upstream README
├── .gitmodules               ← Submodule definitions (adi-hdl, XilinxUnisimLibrary)
├── sourceme.first            ← Vivado/OSS-CAD environment setup (upstream)
│
├── maia-hdl/                 ← FPGA gateware
│   ├── maia_hdl/             ← Amaranth HDL modules (Python)
│   │   ├── maia_sdr.py       ← Top-level SDR module
│   │   ├── spectrometer.py   ← Spectrum analyzer
│   │   ├── recorder.py       ← IQ recorder
│   │   ├── ddc.py            ← Digital down-converter
│   │   ├── fft.py            ← FFT implementation
│   │   └── ...
│   ├── projects/             ← Vivado board projects
│   │   ├── fishball7020_iio/ ← Our target (Z7020)
│   │   ├── fishball_iio/     ← Z7010 variant
│   │   ├── pluto/            ← ADALM Pluto
│   │   ├── plutoplus/        ← Pluto+
│   │   └── ...
│   ├── ip/maia-sdr/          ← Packaged IP core
│   ├── adi-hdl/              ← [submodule] Analog Devices HDL
│   ├── XilinxUnisimLibrary/  ← [submodule] Xilinx sim primitives
│   ├── test/                 ← Amaranth unit tests
│   └── test_cocotb/          ← Cocotb simulation tests
│
├── maia-httpd/               ← Rust HTTP daemon (runs on Zynq ARM)
│   ├── src/
│   │   ├── main.rs           ← Entry point
│   │   ├── httpd.rs          ← HTTP server
│   │   ├── app.rs            ← Application state
│   │   ├── spectrometer.rs   ← Spectrum data streaming
│   │   ├── iio.rs            ← IIO subsystem interface
│   │   ├── rxbuffer.rs       ← RX buffer management
│   │   ├── ddc.rs            ← DDC control
│   │   └── httpd/            ← HTTP route handlers
│   ├── maia-json/            ← JSON API type definitions
│   ├── maia-pac/             ← FPGA register definitions (from SVD)
│   ├── Cargo.toml            ← Rust workspace manifest
│   └── Cross.toml            ← Cross-compilation config
│
├── maia-wasm/                ← Rust/WASM web frontend
│   ├── src/
│   │   ├── lib.rs            ← WASM entry point
│   │   ├── waterfall.rs      ← Waterfall display (WebGL2)
│   │   ├── ui.rs             ← UI controls
│   │   ├── websocket.rs      ← WebSocket client
│   │   └── render/           ← WebGL rendering
│   ├── assets/               ← Static web assets (HTML, CSS, icons)
│   └── Cargo.toml
│
├── maia-kmod/                ← Linux kernel module
│   ├── maia-sdr.c            ← DMA buffer kernel module
│   └── Makefile
│
└── doc/
    └── changes/              ← Detailed change documentation
```

---

## Build Systems

### Build Scripts (In-Repo)

All build scripts live at the repo root, following the same pattern as `tezuka_fw/build.bat`:

```
maia-sdr/
├── build_hdl.bat / .sh    ← Amaranth → Verilog + SVD (Docker)
├── build_fpga.bat          ← Full Vivado FPGA synthesis (Windows)
├── sim_hdl.bat / .sh       ← HDL simulation (Docker)
└── clean.bat               ← Clean all build artifacts
```

| Script | What it does | Runs where | Time |
|--------|-------------|------------|------|
| `build_hdl.bat` | Generate `maia_sdr.v` + `maia-sdr.svd` from Amaranth | Docker (python:3.11-slim) | ~2 min |
| `build_fpga.bat` | IP packaging + Vivado synthesis → XSA bitstream | Windows (native Vivado) | ~15-30 min |
| `sim_hdl.bat` | Tier 1: pytest + Tier 2: cocotb/iverilog | Docker (python:3.11-slim) | ~5 min |
| `clean.bat` | Remove Vivado artifacts, generated IP, Docker volume | Windows | instant |

**Docker volume:** `maia-hdl-build` — persistent ext4 volume caching the Python venv. Shared between `build_hdl` and `sim_hdl`. First run installs packages (~2 min); subsequent runs reuse the cache.

**Key options:**
```
build_hdl.bat --config NAME        Use a specific Amaranth config (default: maia_iio)
build_hdl.bat --verilog-only       Skip SVD generation
build_hdl.bat --clean              Delete cached venv, fresh rebuild
build_hdl.bat --interactive        Open Docker shell for debugging
build_fpga.bat                     Auto-detects Vivado 2023.2 or 2025.2
sim_hdl.bat --tier1                Amaranth Python sim only (fast)
sim_hdl.bat --test NAME            Run a single test by name
clean.bat --all                    Also remove Docker build volume
```

**Environment variables:**
- `VIVADO_DIR_OVERRIDE` — Force a specific Vivado installation path
- `TEZUKA_FW` — Path to tezuka_fw repo (default: `../../Tezuka/tezuka_fw`)

### FPGA Gateware (maia-hdl)

**Tool:** Vivado 2023.2 + Amaranth HDL (Python)
**Target:** Fishball Z7020 (`projects/fishball7020_iio/`)

**Build flow:**
1. `build_hdl.bat` — Amaranth generates Verilog from Python HDL (in Docker)
2. `build_fpga.bat` — Vivado synthesizes, places, routes → bitstream (native Windows)
3. XSA copied to Tezuka firmware, packaged into BOOT.bin by Buildroot

### HTTP Daemon (maia-httpd)

**Tool:** Rust + Cross (cross-compilation to ARM)
**Built by:** Tezuka firmware Buildroot (not standalone)

In `tezuka_fw`, the package `maia-httpd.mk` does:
```
MAIA_HTTPD_SITE = https://github.com/andylee77/maia-sdr.git
MAIA_HTTPD_VERSION = fishball-dev
```

So Buildroot clones this repo and builds maia-httpd automatically.

### Web UI (maia-wasm)

**Tool:** Rust + wasm-pack → WebAssembly
**Built by:** Tezuka firmware Buildroot (not standalone)

Similar to httpd — `maia-wasm.mk` pulls from this fork and builds.

### Kernel Module (maia-kmod)

**Tool:** Linux kernel build system (Makefile)
**Built by:** Tezuka firmware Buildroot

---

## Git Remotes

| Remote | URL | Purpose |
|--------|-----|---------|
| `origin` | `https://github.com/andylee77/maia-sdr.git` | Your fork |
| `upstream` | `https://github.com/F5OEO/maia-sdr.git` | F5OEO's fork |

### Branches

| Branch | Purpose |
|--------|---------|
| `main` | Tracks upstream, clean for syncing |
| `fishball-dev` | Active development (Fishball Z7020 changes) |

### Upstream Sync

```bash
git fetch upstream
git checkout main
git merge upstream/main
git push origin main

git checkout fishball-dev
git merge main
# Resolve conflicts if any
git push
```

---

## Related Repositories

| Repo | Branch | Purpose |
|------|--------|---------|
| `andylee77/maia-sdr` | `fishball-dev` | This repo — FPGA + httpd + wasm |
| `andylee77/tezuka_fw` | `fishball-dev` | Firmware (Buildroot, pulls from this repo) |
| `andylee77/sdrtrunk` | `plutosdr` | SDRTrunk (Java SDR app, PlutoSDR support) |

---

## Work Documents (Outside Repo)

Located at `C:\Users\Andy\Projects\MAIA_SDR\work_docs\`:

| Document | Topic |
|----------|-------|
| `FIRMWARE_AUDIT.md` | Component audit of firmware |
| `FISHBALL_VIVADO_BUILD_GUIDE.md` | FPGA build instructions |
| `FPGA_REBUILD_NEEDED.md` | When FPGA rebuild is required |
| `FPGA_RING_BUFFER_IMPLEMENTATION.md` | Ring buffer design for IQ streaming |
| `FPGA_RING_BUFFER_IQ_STREAM_PLAN.md` | IQ stream architecture plan |
| `HIGH_BANDWIDTH_IQ_STREAM.md` | High-bandwidth streaming investigation |
| `MAIA_UHD_INTEGRATION.md` | UHD integration research |
| `PERSIST_PATCHED_HTTPD.md` | How to persist httpd patches in fork |
| `TEZUKA_FIRMWARE_BUILD_WITH_PATCHED_HTTPD.md` | Patched firmware build guide |
| `VIVADO_2023_INSTALL_GUIDE.md` | Vivado 2023.2 installation |

---

## Session Log

| Date | Session | What was done |
|------|---------|---------------|
| 2026-03-08 | Project setup | Forked repo, created fishball-dev, set up workspace docs |
| 2026-03-08 | Build scripts | Created in-repo build scripts (build_hdl, build_fpga, sim_hdl, clean) |
