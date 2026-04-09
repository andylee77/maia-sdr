# Fishball P25 -- FPGA Bitstream Build Guide

## Overview

Builds the P25 FPGA gateware for the Fishball Z7020 board (Zynq-7020 + AD9361).
The build script (`build_fpga.bat --p25`) runs on Windows and calls Vivado
directly, bypassing the ADI HDL Linux Makefile system (which requires `flock`).

## Prerequisites

### Vivado ML Standard (Free)

- **Version**: 2023.2 (preferred) or 2025.2
- Download: https://www.xilinx.com/support/download.html
- During install, select **Zynq-7000** under SoCs (saves ~45 GB)
- AMD account required (free)

### Python 3.9+

- Amaranth HDL 0.5.x (`pip install amaranth`)
- numpy, scipy

### Git Submodules

```bash
git submodule update --init --recursive
```

This pulls `maia-hdl/adi-hdl/` (Analog Devices HDL library) and
`maia-hdl/XilinxUnisimLibrary/` (Xilinx simulation primitives).

## Build Pipeline

```
Amaranth HDL (maia-hdl/p25_hdl/) --> Verilog (p25_core.v)
                                        |
                             Vivado IP Package (component.xml)
                                        |
                Vivado Block Design (PS7 + AD9361 + p25_core)
                                        |
             Synthesis -> Implementation -> Bitstream -> XSA
```

## Quick Start

```
build_fpga.bat --p25
```

Output: `output/system_top.xsa`

## Build Steps (what the script does)

### Step 1: ADI HDL Submodule

Checks that `maia-hdl/adi-hdl/` is populated. If not, runs
`git submodule update --init --recursive`.

### Step 2: Generate Verilog

- **P25 core**: `python3 -m p25_hdl.p25_top --config default` -> `maia-hdl/ip/p25-core/default/p25_core.v`
- **Maia SDR core**: `python3 -m maia_hdl.maia_sdr --config default` -> needed by pluto base design

### Step 3: Package IP Cores

Runs Vivado in batch mode to package both IP cores as Vivado-compatible IPs
with `component.xml`.

### Step 4: Build ADI Library IPs

Builds the ADI HDL library cores required by the pluto base design:

- `axi_ad9361` -- AD9361 interface
- `util_clkdiv`, `util_rfifo`, `util_wfifo` -- clock/FIFO utilities
- `util_cdc`, `util_axis_fifo`, `axi_dmac` -- CDC and DMA
- `util_cpack2`, `util_upack2` -- data packing

### Step 5: Stage Build Project

Copies project files into the build location at
`maia-hdl/projects/fishball7020_p25/` so that relative paths
in the pluto base design (`../pluto/system_bd.tcl`) resolve correctly.

Key files staged:

- `system_project.tcl` -- generated, targets xc7z020clg400-1
- `system_bd.tcl` -- block design with P25 core
- `system_top.v` -- board-level Verilog wrapper
- `system_constr.xdc` -- Fishball Z7020 pin constraints

### Step 6: Vivado Synthesis + Implementation

Runs `vivado -mode batch -source system_project.tcl` which:

1. Creates the block design (PS7 + AD9361 + clocking + p25_core)
2. Synthesizes (~10 min)
3. Implements: opt -> place -> phys_opt -> route (~10 min)
4. Generates bitstream + XSA (~2 min)

### Step 7: Copy Output

Copies `system_top.xsa` to `output/` and optionally to Tezuka firmware.

## Block Design Architecture

```
+-----------------------------------------------------------+
|                  Zynq Z7020 (Fishball)                    |
|                                                           |
|  +----------+    +----------------------------------+     |
|  |  ARM PS  |    |        FPGA Fabric (PL)          |     |
|  |          |    |                                    |    |
|  | S_AXI_HP1+----+ m_axi_dibit + m_axi_traffic      |    |
|  |          |    | (P25 dibit DMA channels)           |    |
|  |          |    |                                    |    |
|  | AXI-Lite +----+ s_axi_lite @ 0x7C460000           |    |
|  |          |    | (P25 register control)              |    |
|  |          |    |                                    |    |
|  |  IRQ[13] +----+ interrupt_out                      |    |
|  +----------+    +----------------------------------+     |
+-----------------------------------------------------------+
```

**HP Port allocation:**

| Port | AXI Master | Purpose |
|------|-----------|---------|
| HP1 | m_axi_dibit + m_axi_traffic | P25 dibit DMA (control + traffic channels) |
| HP2 | m_axi_recorder + adc_dma + dac_dma | IIO DMA (AD9361 streaming via libiio) |

The P25 design keeps full IIO DMA support (`axi_dmac` RX/TX, `util_cpack2`,
`util_upack2`, 8-bit mode mux chain) so `iio_readdev` and PlutoSDR Python
scripts work alongside the P25 decoder.

## Opening in Vivado GUI

After building, open the project to view the block diagram:

```
vivado maia-hdl\projects\fishball7020_p25\fishball_p25.xpr
```

Then: Flow Navigator -> Open Block Design -> `system.bd`

## Timing Violations

The build typically completes with timing violations on PS7 cross-clock domain
paths. This is expected and matches the Maia SDR build behavior. The XSA is
exported as `system_top_bad_timing.xsa` and promoted to `system_top.xsa`.

## Troubleshooting

### Vivado not found

The script searches standard install locations. Set `VIVADO_DIR_OVERRIDE` to
override:

```
set VIVADO_DIR_OVERRIDE=D:\Xilinx\Vivado\2023.2
build_fpga.bat --p25
```

### "No parts matched xc7z020clg400-1"

Zynq-7000 SoC device family not installed. Re-run Vivado installer -> Add
Design Tools or Devices -> SoCs -> Zynq-7000.

### ADI library build fails

Clean and rebuild:

```
del /s /q maia-hdl\adi-hdl\library\*\component.xml
build_fpga.bat --p25
```

### IP version mismatch

Clean generated IP and rebuild:

```
rd /s /q maia-hdl\ip\p25-core\default
build_fpga.bat --p25
```

## Output Files

| File | Location | Purpose |
|------|----------|---------|
| `system_top.xsa` | `output/` | Hardware specification for Tezuka firmware |
| `system_top.bit` | `maia-hdl/projects/fishball7020_p25/fishball_p25.runs/impl_1/` | Raw bitstream |
| `p25_core.v` | `maia-hdl/ip/p25-core/default/` | Generated Verilog (19k lines) |

## Tezuka Firmware Build

After the FPGA bitstream is built, build the full firmware via Tezuka
(`andylee77/tezuka_fw` on `fishball-dev` branch):

```bat
cd C:\Users\Andy\Projects\Tezuka\tezuka_fw
build.bat --p25            # incremental (~3 min if cached)
build.bat --p25 --clean    # clean build (~1-3 hours)
```

Or manually inside Docker:

```bash
build.bat --interactive
# then inside Docker:
cd buildroot
make fishball_p25_7020_defconfig && make
```

This builds:

- P25 bitstream (from XSA) via `package/fishball_fpga_p25`
- p25-httpd binary (Rust cross-compile) via `package/p25-httpd`
- Linux kernel with P25 device tree (`fishball-p25.dtb`)
- Root filesystem with `S60p25-httpd` init script

Flash `output_images/` to FAT32 SD card and boot.

### Build Notes

- **Docker required**: Buildroot can't build on NTFS. `build.bat` handles
  Docker setup automatically.
- **p25-httpd source**: Mounted from the local maia-sdr repo at
  `/mnt/maia-sdr` (read-only). The Buildroot package uses `SITE_METHOD = local`.
- **Switching configs**: If switching between Maia and P25 defconfigs, use
  `--clean` to avoid stale build artifacts.
- **Rebuilding p25-httpd**: If you change Rust code and need to rebuild just
  p25-httpd without a full clean build, use interactive mode:
  `make p25-httpd-dirclean && make`
- **Shell scripts**: Must have Unix (LF) line endings. The repo has
  `.gitattributes` to enforce this for `*.sh` files.

## Device Tree

The P25 build uses `fishball-p25.dtsi` (separate from Maia's `fishball.dtsi`):

| DTS Node | Name seen by userspace | Purpose |
|----------|----------------------|---------|
| `p25-core@7c460000` | `/sys/class/uio/uio0/name` = `p25-core` | FPGA register UIO |
| `p25-dibit` | `/dev/p25-dibit` | Control channel dibit DMA ring buffer |
| `p25-traffic` | `/dev/p25-traffic` | Traffic channel dibit DMA ring buffer |

Both DMA devices use `compatible = "maia-sdr,rxbuffer"` (reuses the maia-kmod
kernel module for ARMv7 cache coherency on non-coherent AXI HP writes).
