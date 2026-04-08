# Maia SDR — Fishball Z7020 Fork

> Fork of [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr) (originally
> [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr) by Daniel Estévez)

This fork adds support for the **Fishball Z7020** board (Zynq-7020 + AD9361) and
related hardware variants (LibreSDR, 2R2T configurations). It builds on the
upstream Maia SDR project which provides an open-source FPGA-based SDR platform
with a web-based spectrum analyzer, real-time waterfall display, and IQ recording.

## What this fork adds

* **Fishball Z7020 board support** — Vivado project, clock configuration, and
  FPGA bitstream targeting the Zynq-7020 + AD9361 platform.
* **LibreSDR support** — CMOS-mode clock fixes and LVDS ADI HDL fixes.
* **2R2T support** — Dual RX/TX chain configuration.
* **maia_iio_lite** — Reduced-memory IIO variant for 512 MB boards (recorder
  buffer halved from 0x1000_0000 to 0x0800_0000).
* **Web UI improvements** — Double-click to hide panel, CORS fixes.
* **Build scripts** — Windows/Linux build helpers for HDL, FPGA synthesis,
  and simulation (`build_hdl.bat/.sh`, `build_fpga.bat`, `sim_hdl.bat/.sh`).

## Target hardware

| Board | Status |
| ------- | -------- |
| Fishball Z7020 (Zynq-7020 + AD9361) | Primary target |
| LibreSDR (1R1T / 2R2T) | Supported |
| ADALM Pluto / Pluto+ | Upstream support preserved |

## Firmware

Firmware is built via the [Tezuka firmware](https://github.com/andylee77/tezuka_fw)
Buildroot system, which pulls from this repo's `fishball-dev` branch. The firmware
is cross-compiled for the Zynq-7000 ARM core inside the Tezuka Docker build
environment.

## Branch strategy

* **`main`** — Tracks upstream, kept clean for syncing.
* **`fishball-dev`** — Active development branch. All changes land here.

## Project structure

| Component | Language | Purpose |
| --------- | -------- | ------- |
| [maia-hdl](maia-hdl) | Python (Amaranth) + Vivado | FPGA gateware |
| [maia-httpd](maia-httpd) | Rust | HTTP daemon on Zynq ARM |
| [maia-wasm](maia-wasm) | Rust → WASM | Web UI (waterfall, WebGL2) |
| [maia-kmod](maia-kmod) | C | Linux kernel module for DMA |

### Key directories

* `maia-hdl/projects/fishball7020_iio/` — Fishball board Vivado project
* `maia-hdl/maia_hdl/` — HDL source modules
* `maia-httpd/src/` — HTTP daemon source
* `maia-wasm/src/` — WASM UI source

## Build environment

### FPGA (maia-hdl)

* **Vivado 2023.2** (Xilinx)
* **Amaranth** HDL via Python virtual environment
* Build scripts: `build_hdl.bat/.sh` (Amaranth → Verilog + SVD),
  `build_fpga.bat` (Vivado synthesis), `sim_hdl.bat/.sh` (simulation)

### Firmware (maia-httpd + maia-wasm)

* Built by Tezuka Buildroot — not built standalone in this repo
* Cross-compiled for ARM (Zynq-7000)

## Upstream

* Upstream repo: [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr)
* Original project: [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr)
* Project website: [maia-sdr.org](https://maia-sdr.org)

See [CHANGELOG_FORK.md](CHANGELOG_FORK.md) for a detailed log of fork-specific changes.

## License

maia-hdl is licensed under the
[MIT license](http://opensource.org/licenses/MIT). maia-httpd and maia-wasm are
licensed under either of the
[Apache License, Version 2.0](http://www.apache.org/licenses/LICENSE-2.0)
or the MIT license at your option. maia-kmod is licensed under the
[GPL, version 2](https://www.gnu.org/licenses/old-licenses/gpl-2.0.en.html).
