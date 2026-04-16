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

### P25 Radio (fishball-p25 / bisect-safety branches)

- **P25DDC v2 filter chain** -- SDRTrunk-faithful Parks-McClellan
  equiripple cascade with a 248-tap stage 3 FIR for tight
  adjacent-channel rejection, unit-DC-gain coefficient convention,
  and explicit per-stage output scaling. Fixes the LsmDecimator2
  fold-back bug at 8 MHz rf_bandwidth. See
  [doc/changes/041_p25ddc_fork.md](doc/changes/041_p25ddc_fork.md).
- **Dual LSM demod chains** -- Independent control and traffic chains
  each running: DC blocker, per-symbol `LsmAgc` (SDRTrunk port),
  `LsmPll` carrier recovery, `LsmTimingInterp` Gardner symbol
  timing, diff-demod slicer, BCH(63,16,23) NID FEC, and universal
  status-dibit strip.
- **C4FM demod chain** -- Parallel C4FM decoder (dormant on LSM
  sites, used for C4FM-only systems like FP&L or St Johns Interop).
- **Grant-follower retune sequence** -- Atomic freeze/NCO-write/
  FIR-pipeline-flush/LSM-reset/thaw for traffic-channel retunes,
  with boot PPM correction and explicit 2 ms FIR cascade flush
  wait. Traffic chain uses the same PLL/AGC configuration as the
  control chain for full symmetry.
- **P25 HTTP daemon** -- Control channel TSBK parser, grant
  follower, traffic manager with call-end semantics, integrated
  mbelib IMBE vocoder producing PCM to a WebSocket audio
  broadcast channel.
- **Live dashboard** -- Radio/Debug/Logs tabs, real-time talkgroup
  activity, frequency map, call log, IMBE + vocoder stats,
  PL HDL register read-back, live retune control, board info
  panel, and browser audio player (AudioWorklet + fallback).
- **Runtime-tunable sync threshold** and per-decoder override
  via `/api/sync_tune` for tightening on noisy sites.
- **Full FEC** -- Golay(23,12), trellis Viterbi, CRC-16, BCH(63,16,23),
  Reed-Solomon (24,12,13).

**Timing closure**: the P25DDC v2 redesign required three
independent fixes to close Vivado timing at WNS +0.267 ns,
0 failing endpoints. See
[doc/changes/042_p25ddc_fork_timing_fixes.md](doc/changes/042_p25ddc_fork_timing_fixes.md)
for the PulseSynchronizer CDC fix, XDC waiver set (ASYNC_REG
synchronizer chains + RegisterCDC data lanes + ADI AD9361 TX
counter + sys_rstgen reset fanout + software reset recovery),
and the local cherry-pick of ADI upstream commit `92534dc1d`
(`axi_ad9361_tx: Use incrementing cnt to improve timing margin`)
onto our pinned adi-hdl submodule.

**Sites tested on-target**: Clay County LSM (NAC `0x8A1`,
860.9625 MHz) as primary target, with Duval County LSM
(NAC `0x3BA`, 855.4875 MHz, same WACN `0xBEE00`) as secondary.
Both validated at 8 MHz rf_bandwidth with full voice decode
and live audio playback. FP&L (935 MHz C4FM) and St Johns
Interop (774 MHz C4FM) are documented as future cross-band
test targets.

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
- **P25:** `fishball-dev` branch, includes `p25-httpd`, `mbelib` vocoder,
  and the P25 device-tree overlay for the P25 Vivado bitstream
- Flash + boot + `/api/system.build` reports the currently-running
  firmware BUILD_TAG so rolling p25-httpd updates can be verified
  without rebaking the FPGA

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
