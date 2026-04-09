# Maia SDR + Fishball P25

> Fork of [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr) (originally
> [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr) by Daniel Estevez)

This fork targets the **Fishball Z7020** board (Zynq-7020 + AD9361). The
`fishball-p25` branch adds an FPGA-based **P25 Phase 1 trunking radio** on top
of Maia SDR's platform. The `fishball-dev` branch carries Maia SDR changes only.

## What this fork adds

### Maia SDR (fishball-dev branch)

- **Fishball Z7020 board support** -- Vivado project, clock configuration, FPGA bitstream
- **LibreSDR / 2R2T support** -- CMOS-mode clock fixes, dual RX/TX chain
- **maia_iio_lite** -- Reduced-memory IIO variant for 512 MB boards
- **Web UI improvements** -- Double-click to hide panel, CORS fixes
- **Build scripts** -- Windows/Linux build helpers (`build_hdl.bat/.sh`, `build_fpga.bat`, `sim_hdl.bat/.sh`)

### P25 Radio (fishball-p25 branch)

- **P25 FPGA gateware** -- C4FM demodulator, Gardner symbol timing recovery, dibit packer with DMA
- **Dual DDC chains** -- Control channel + traffic channel with independent NCO tuning
- **P25 HTTP daemon** -- Control channel decoder, TSBK parser, traffic manager, web dashboard
- **Full FEC** -- Golay(23,12), trellis Viterbi, CRC-16
- **Web dashboard** -- Real-time talkgroup activity, frequency map, WebSocket events

## Project structure

| Component | Language | Purpose |
| --------- | -------- | ------- |
| [maia-hdl/maia_hdl](maia-hdl/maia_hdl) | Python (Amaranth) | Maia FPGA gateware (DDC, DMA, registers) |
| [maia-hdl/p25_hdl](maia-hdl/p25_hdl) | Python (Amaranth) | P25 FPGA gateware (C4FM, symbol timing, dibit packer) |
| [maia-httpd](maia-httpd) | Rust | Maia HTTP daemon on Zynq ARM |
| [p25-httpd](p25-httpd) | Rust | P25 HTTP daemon (decoder, traffic manager, web UI) |
| [maia-wasm](maia-wasm) | Rust -> WASM | Web UI (waterfall, WebGL2) |
| [maia-kmod](maia-kmod) | C | Linux kernel module for DMA |

### Key directories

- `maia-hdl/projects/fishball7020_iio/` -- Maia Vivado project
- `maia-hdl/projects/fishball7020_p25/` -- P25 Vivado project
- `maia-hdl/ip/p25-core/` -- P25 Vivado IP packaging
- `doc/DEVPLAN.md` -- P25 development roadmap
- `doc/BUILD_FPGA.md` -- P25 FPGA build guide
- `doc/changes/` -- Detailed change documentation

## Target hardware

| Board | Status |
| ------- | -------- |
| Fishball Z7020 (Zynq-7020 + AD9361) | Primary target |
| LibreSDR (1R1T / 2R2T) | Supported (Maia only) |
| ADALM Pluto / Pluto+ | Upstream support preserved |

## Build environment

### FPGA (maia-hdl)

- **Vivado 2023.2** (Xilinx)
- **Amaranth** HDL via Python virtual environment
- Build scripts: `build_hdl.bat/.sh` (Amaranth -> Verilog + SVD),
  `build_fpga.bat` (Maia), `build_fpga.bat --p25` (P25)

### Firmware (maia-httpd / p25-httpd)

- Built by [Tezuka firmware](https://github.com/andylee77/tezuka_fw) Buildroot
- Cross-compiled for ARM (Zynq-7000) inside the Tezuka Docker build environment
- **Maia:** `fishball-dev` branch, `fishball_maiasdr_7020_defconfig`
- **P25:** `fishball-dev` branch (P25 defconfig + device tree TBD -- Phase 5)

## Branch strategy

- **`main`** -- Tracks upstream, kept clean for syncing
- **`fishball-dev`** -- Maia SDR development branch
- **`fishball-p25`** -- P25 radio branch (forked from fishball-dev, includes Maia + P25)

## Upstream

- Upstream repo: [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr)
- Original project: [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr)
- Project website: [maia-sdr.org](https://maia-sdr.org)

See [CHANGELOG_FORK.md](CHANGELOG_FORK.md) for a detailed log of fork-specific changes.
See [DEVLOG.md](DEVLOG.md) for developer reference (repo layout, build systems, session log).

## License

maia-hdl is licensed under the
[MIT license](http://opensource.org/licenses/MIT). maia-httpd, p25-httpd, and
maia-wasm are licensed under either of the
[Apache License, Version 2.0](http://www.apache.org/licenses/LICENSE-2.0)
or the MIT license at your option. maia-kmod is licensed under the
[GPL, version 2](https://www.gnu.org/licenses/old-licenses/gpl-2.0.en.html).
