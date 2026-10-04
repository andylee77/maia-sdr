# Fishball scanner

A P25 and DMR trunking scanner, with an ATSC TV mode, on the Fishball Z7020 board (Zynq-7020 with
an AD9361 or AD9363). It is a fork of [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr), itself
from [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr) by Daniel Estévez, and builds on
Maia SDR's gateware.

## What it does

The unit runs in one mode at a time:

- **Scanner:**
  - P25 Phase 1 trunking, on simulcast (LSM) and C4FM control channels;
  - DMR Tier III trunking;
  - calls followed on the traffic lanes, decoded (IMBE, AMBE+2) and played live in the
    browser;
  - recordings, and a history of calls on the SD card.
- **ATSC TV:**
  - a TV channel finder (pilot, carrier to noise, ATSC 3.0);
  - station names and programmes from PSIP, decoded from 8-VSB in software.
- **Data:** a scanner for 315, 433 and 902-928 MHz devices. A study so far
  ([scanner/doc/DATA_MODE.md](scanner/doc/DATA_MODE.md)).

Everything is controlled from the web UI or the HTTP and WebSocket API
([scanner/doc/API.md](scanner/doc/API.md)). SDRTrunk is the reference for the P25 and DMR signal
processing and protocol constants.

## How it is split

- **The radio core (FPGA):**
  - three lanes, each a Maia DDC with its own NCO and filters, cut into tagged packets on one DMA
    ring;
  - a 4096-bin wideband spectrometer;
  - a raw IQ capture ring.

  It holds no demodulator. Its design and plans are in
  [doc/changes/079_general_radio_core.md](doc/changes/079_general_radio_core.md).
- **The scanner (ARM, Rust):**
  - every demodulator (LSM, C4FM, DMR, 8-VSB);
  - framing, FEC, the vocoders, trunking, history and recordings;
  - the API and the web UI.

## Layout

| Path | What |
|------|------|
| [scanner/](scanner/) | The daemon on the unit ([scanner/README.md](scanner/README.md), design in [scanner/doc/DESIGN.md](scanner/doc/DESIGN.md)) |
| [scanner-hdl/](scanner-hdl/) | The fork's gateware (Amaranth): the radio core (`radio_core/`) and hardware-validation gateware (`hwval_hdl/`), with their tests. Their Vivado projects and IP packaging are in `maia-hdl/projects/fishball7020_*` and `maia-hdl/ip/` |
| [bench/](bench/) | `fbench`, the two-unit test bench, and its board agent ([doc/HW_VALIDATION_SUITE.md](doc/HW_VALIDATION_SUITE.md)) |
| [tools/](tools/) | Host scripts: the SDRTrunk reference harnesses, filter design, API field reference, TV checks ([tools/README.md](tools/README.md)) |
| [doc/](doc/) | [ROADMAP.md](doc/ROADMAP.md), and one document per change in [doc/changes/](doc/changes/) |
| `p25-httpd/` | The daemon before the scanner. It leaves the repo once the bench stops reading its register map |
| `maia-hdl/`, `maia-httpd/`, `maia-wasm/`, `maia-kmod/` | Upstream Maia SDR, unchanged apart from the fork's Vivado projects and IP packaging in `maia-hdl/`. The unit uses `maia-kmod`'s DMA driver |

## Building

- **FPGA:**
  - Command: `./build_fpga_p25_pretty.sh`.
  - Tools: Vivado 2023.2 and Docker for the Amaranth step. Builds run inside this tree only.
  - It writes the XSA and the register SVD, and the scanner's PAC in `scanner/core-pac/`.
  - Guide: [BUILD_FPGA.md](BUILD_FPGA.md).
- **SD image:**
  - Command: `./build_tezuka_p25_pretty.sh`.
  - It builds [Tezuka firmware](https://github.com/andylee77/tezuka_fw) (branch `fishball-dev`,
    `fishball_p25_7020_defconfig`), which takes the scanner from this checkout.
- **The scanner alone:** `cargo test` on the host. For the ARM binary, `cargo-zigbuild` (see
  [scanner/README.md](scanner/README.md)).

## Branches

- **`fishball-p25`:** this work.
- **`main`:** tracks upstream, for syncing.

[CHANGELOG_FORK.md](CHANGELOG_FORK.md) logs the fork's changes.

## License

maia-hdl, scanner-hdl and the scanner are licensed under the [MIT license](http://opensource.org/licenses/MIT).
maia-httpd and maia-wasm are licensed under either of the
[Apache License, Version 2.0](http://www.apache.org/licenses/LICENSE-2.0) or the MIT license at your
option. maia-kmod is licensed under the
[GPL, version 2](https://www.gnu.org/licenses/old-licenses/gpl-2.0.en.html).
