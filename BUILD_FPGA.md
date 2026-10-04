# Fishball radio core — FPGA build guide

## Overview

Builds the radio core's bitstream for the Fishball Z7020 board (Zynq-7020 + AD9361): the
Amaranth source in `scanner-hdl/radio_core/`, the Vivado project `maia-hdl/projects/fishball7020_p25/`.
The core and its register map are described in `doc/changes/079_general_radio_core.md`.

- `build_fpga.bat --p25` runs the whole flow on Windows and calls Vivado directly, bypassing the
  ADI HDL Linux makefiles (they need `flock`).
- Run it through `./build_fpga_p25_pretty.sh`, which logs to `bake.log` and prints a line per
  phase.
- Run Vivado only inside this tree: the ADI and Maia TCL use relative paths.

## Prerequisites

### Vivado ML Standard (Free)

- **Version:** 2023.2, the version every bake has used.
- Download: https://www.xilinx.com/support/download.html
- During install, select **Zynq-7000** under SoCs (saves ~45 GB).
- AMD account required (free).

### Docker Desktop

- Required for Verilog generation (Amaranth runs inside a `python:3.11-slim` container on ext4
  to avoid NTFS issues with pip editable installs).
- The first run downloads the image and installs Python packages (~2 min).
- Later runs reuse the `maia-hdl-build` Docker volume cache.

### Git Submodules

```bash
git submodule update --init --recursive
```

This pulls `maia-hdl/adi-hdl/` (Analog Devices HDL library) and
`maia-hdl/XilinxUnisimLibrary/` (Xilinx simulation primitives).

## Build pipeline

```text
scanner-hdl/radio_core/*.py       [Amaranth source: the radio core]
maia-hdl/maia_hdl/*.py      (DDC, spectrometer, DMA, registers, CDC)
         |
         | Staleness gate (tools/check_verilog_stale.ps1)
         |   regenerated in Docker if any .py is newer than the .v
         v
maia-hdl/ip/p25-core/default/p25_core.v     [generated Verilog]
scanner/core-pac/core.svd, src/lib.rs       [register SVD and the scanner's PAC]
         |
         | Vivado IP packaging (package_ip.tcl)
         v
maia-hdl/ip/p25-core/default/component.xml  [Vivado IP]
         |
         | Block design assembly (system_project.tcl)
         v
Block design: PS7 + AD9361 LVDS + clocking + ADI libraries + p25_core
         |
         | Synthesis, then opt -> place -> phys_opt -> route
         | A route hook writes utilization_hier.rpt; a timing failure stops the build
         v
system_top.bit  +  system_top.xsa   [hand-off to the Tezuka firmware]
```

The Maia SDR core (`ip/maia-sdr/maia_iio/maia_sdr.v`) is packaged on every build: the pluto base
block design that the P25 project sources needs it before the P25 project puts the radio core in
its place. The base's IIO DMA chain (`axi_dmac`, `util_cpack2`, `util_upack2`) stays, so libiio
tools still work.

## Quick start

```sh
./build_fpga_p25_pretty.sh      # the whole flow, Verilog regenerated when stale
```

Run the HDL tests first, from `scanner-hdl/` in `.venv-hdl`: `python -m pytest test/`.

Output: `maia-hdl/projects/fishball7020_p25/fishball_p25.sdk/system_top.xsa`, also copied to
`tezuka_fw/board/tezuka/fishball7020/bitstream/p25/` when tezuka_fw is at its usual path. Commit
the XSA before an image build consumes it.

## Build steps (what the script does)

### Step 1: ADI HDL submodule

Checks that `maia-hdl/adi-hdl/` is populated. If not, runs
`git submodule update --init --recursive`.

### Step 2: Generate Verilog (with automatic staleness detection)

Runs the Amaranth elaboration inside a Docker container (python:3.11-slim on ext4) to avoid
NTFS/CIFS issues with pip editable installs. It does not just check whether the `.v` exists: it
compares mtimes on every build, so an edit to the Amaranth source never gets stranded in an old
Verilog file (`doc/changes/009_build_verilog_staleness.md`).

`tools/check_verilog_stale.ps1` compares the mtime of each generated `.v` file with the newest
`*.py` under its source directories and returns `MISSING`, `STALE` or `FRESH`. On `MISSING` or
`STALE`, `build_fpga.bat` runs `build_hdl.bat --verilog-only [--p25]` in Docker.

| Generated file | Source directories compared | Why |
|----------------|-----------------------------|-----|
| `maia-hdl/ip/maia-sdr/maia_iio/maia_sdr.v` | `maia-hdl/maia_hdl/*.py` | Maia's core is built from `maia_hdl` only |
| `maia-hdl/ip/p25-core/default/p25_core.v` | `scanner-hdl/radio_core/*.py` **and** `maia-hdl/maia_hdl/*.py` | The radio core imports the DDC, spectrometer, registers, DMA and CDC from `maia_hdl` |

Inside the container:

- `python -m maia_hdl.maia_sdr --config maia_iio` -> `maia-hdl/ip/maia-sdr/maia_iio/maia_sdr.v`
- `python -m radio_core.p25_top --config default` -> `maia-hdl/ip/p25-core/default/p25_core.v`
- with `--p25`, the core's SVD and its `svd2rust` PAC go to `scanner/core-pac/`.

### Step 3: Package IP cores

Runs Vivado in batch mode to package both cores as Vivado IPs (`component.xml`).

### Step 4: Build ADI library IPs

Builds the ADI HDL library cores the pluto base design needs. Each is skipped if its
`component.xml` already exists.

- `util_clkdiv` (in `xilinx/`): the AD9361 clock divider
- `util_rfifo`, `util_wfifo`: the AD9361 read and write FIFOs
- `axi_ad9361`: the AD9361 LVDS interface
- `util_axis_fifo`, `util_cdc`: CDC and AXI-Stream FIFO
- `axi_dmac`: the DMA controller
- `util_cpack2`, `util_upack2`: packing and unpacking

### Step 5: Vivado synthesis and implementation

`vivado -mode batch -source system_project.tcl`:

1. creates the block design (PS7 + AD9361 + clocking + `p25_core`);
2. synthesizes (~10 min);
3. implements: opt -> place -> phys_opt -> route (~10 min); the route hook
   (`utilization_hier.tcl`) writes `fishball_p25.runs/impl_1/utilization_hier.rpt`;
4. writes the bitstream and the XSA (~2 min).

### Step 6: Copy output

Copies `system_top.xsa` to the Tezuka firmware's bitstream directory when it is there.

## Block design

```text
+-------------------------------------------------------------+
|                    Zynq Z7020 (Fishball)                    |
|                                                             |
|  +----------+      +----------------------------------+     |
|  |  ARM PS  |      |         FPGA fabric (PL)          |     |
|  |          |      |                                  |     |
|  | S_AXI_HP1+------+ m_axi_lanes, m_axi_wideband_spec, |     |
|  |          |      | m_axi_wideband_iq (the three rings)|    |
|  |          |      |                                  |     |
|  | AXI-Lite +------+ s_axi_lite @ 0x7C46_0000 (1 KB)   |     |
|  |          |      |                                  |     |
|  |  IRQ_F2P +------+ interrupt_out (concat In11)       |     |
|  +----------+      +----------------------------------+     |
+-------------------------------------------------------------+
```

| Port | AXI masters | Purpose |
|------|-------------|---------|
| HP1 | `m_axi_lanes`, `m_axi_wideband_spec`, `m_axi_wideband_iq` | The lane ring, the spectrum ring and the raw IQ capture, through an `axi_interconnect` at the sync clock (62.5 MHz, about 500 MB/s) |
| HP2 | `adc_dma`, `dac_dma` | IIO DMA (AD9361 streaming through libiio) |
| HP0, HP3 | — | Unused (the hwval build puts its memory testers there) |

## Opening in the Vivado GUI

```text
vivado maia-hdl\projects\fishball7020_p25\fishball_p25.xpr
```

Then: Flow Navigator -> Open Block Design -> `system.bd`.

## Timing

A timing failure is an error for `--p25` (and `--hwval`): the build exports no usable XSA and
never promotes `system_top_bad_timing.xsa`. No waiver is added to close it; 079's study names
the paths with the least slack and their fix.

The route hook's report stops at depth 4, which ends at the core's top. For the core's own
blocks, run `report_utilization -hierarchical -cells [get_cells i_system_wrapper/system_i/p25_core/inst]`
on `fishball_p25.runs/impl_1/system_top_routed.dcp` in a Vivado batch session.

## Troubleshooting

### Vivado not found

The script searches the standard install locations. Set `VIVADO_DIR_OVERRIDE` to override:

```text
set VIVADO_DIR_OVERRIDE=D:\Xilinx\Vivado\2023.2
build_fpga.bat --p25
```

### "No parts matched xc7z020clg400-1"

The Zynq-7000 device family is not installed. Re-run the Vivado installer -> Add Design Tools
or Devices -> SoCs -> Zynq-7000.

### "couldn't read ... mref/cs12_cs8/xgui/..."

A stale Vivado project. Delete the generated `.Xil`, `fishball_p25.cache`, `.gen`, `.hw`,
`.ip_user_files`, `.srcs` and `.xpr` in `maia-hdl/projects/fishball7020_p25/` and rebuild. The
build deletes the tracked `fishball_p25.sdk/system_top.xsa` on its way; don't commit that
deletion.

### ADI library build fails

Clean and rebuild:

```text
del /s /q maia-hdl\adi-hdl\library\*\component.xml
build_fpga.bat --p25
```

### IP version mismatch

Clean the generated IP and rebuild:

```text
rd /s /q maia-hdl\ip\p25-core\default
build_fpga.bat --p25
```

### An Amaranth edit does not show up on the hardware

Step 2 catches this: if any `radio_core/*.py` or `maia_hdl/*.py` is newer than `p25_core.v`, the
Verilog is regenerated before synthesis, and the log says:

```text
[Step 2] Checking Verilog generation status...
[WARN] p25_core.v is STALE -- radio_core or maia_hdl has newer changes.
```

If it says `[OK] p25_core.v is current` when you expected a regeneration, compare the mtimes, or
ask the helper directly:

```bat
powershell -NoProfile -ExecutionPolicy Bypass -File tools\check_verilog_stale.ps1 ^
    -VerilogFile "maia-hdl\ip\p25-core\default\p25_core.v" ^
    -SourceDirs  "scanner-hdl\radio_core;maia-hdl\maia_hdl"
```

To force a regeneration, delete `maia-hdl\ip\p25-core\default\p25_core.v` and rebuild.

## Output files

| File | Location | Purpose |
|------|----------|---------|
| `system_top.xsa` | `maia-hdl/projects/fishball7020_p25/fishball_p25.sdk/` | Hardware specification for the Tezuka firmware (also copied to `tezuka_fw/board/tezuka/fishball7020/bitstream/p25/`) |
| `system_top.bit` | `maia-hdl/projects/fishball7020_p25/fishball_p25.runs/impl_1/` | Raw bitstream |
| `utilization_hier.rpt` | the same `impl_1/` | Hierarchical utilization, from the route hook |
| `p25_core.v` | `maia-hdl/ip/p25-core/default/` | Generated Verilog, regenerated by Step 2 when the source is newer |
| `core.svd`, `src/lib.rs` | `scanner/core-pac/` | The core's register map, and the scanner's PAC from it |
| `maia_sdr.v` | `maia-hdl/ip/maia-sdr/maia_iio/` | Generated Maia core Verilog |

## Build helper scripts

| File | Purpose |
|------|---------|
| `build_fpga_p25_pretty.sh` | The entry point: `build_fpga.bat --p25` with its log in `bake.log` and a line per phase |
| `build_fpga.bat` | The whole flow: staleness-checked Verilog regeneration -> IP packaging -> ADI library builds -> Vivado synthesis and implementation -> XSA export -> Tezuka copy |
| `build_hdl.bat` / `build_hdl.sh` | Docker-based Amaranth -> Verilog generation, called by `build_fpga.bat`. With `--p25` it also writes the SVD and runs `svd2rust` into `scanner/core-pac/` |
| `tools/check_verilog_stale.ps1` | Compares a generated `.v` file's mtime with its `*.py` sources: `MISSING`, `STALE` or `FRESH` |
| `sim_hdl.bat` / `sim_hdl.sh` | Docker-based HDL simulation; the tests are usually run from `.venv-hdl` instead |

## Tezuka firmware build

After the bitstream is built, build the SD image with the Tezuka firmware (`andylee77/tezuka_fw`,
branch `fishball-dev`):

```sh
./build_tezuka_p25_pretty.sh      # tezuka_fw build.bat --p25, logged to tezuka_build.log
```

It builds:

- the bitstream from the XSA (`package/fishball_fpga_p25`);
- the scanner, cross-compiled from this checkout (`package/scanner`, init script `S60scanner`);
- maia-kmod's DMA driver (`S50maia-kmod`);
- the Linux kernel with the P25 device tree (`fishball-p25.dtsi`).

Copy `tezuka_fw/output_images/` to a FAT32 SD card and boot.

### Build notes

- **Docker required:** Buildroot can't build on NTFS; `build.bat` handles Docker.
- **The scanner's source** is this repo, mounted read-only at `/mnt/maia-sdr`
  (`SITE_METHOD = local`). Don't run cargo while the image builds: the package's rsync fails.
- **The wrapper exits 0 on failure:** grep `tezuka_build.log` for `[ERROR]`.
- **Rebuilds:** `build.sh` rebuilds the bitstream package when the XSA is newer, the scanner
  when its sources, manifests or `core-pac` changed (`make scanner-dirclean`), and the kernel
  when a device tree changed.
- **Shell scripts** must have Unix (LF) line endings; `.gitattributes` enforces it for `*.sh`.

## Device tree

The P25 build uses `fishball-p25.dtsi` (separate from Maia's `fishball.dtsi`):

| Node | Seen by userspace | Purpose |
|------|-------------------|---------|
| `p25-core@7c460000` | UIO `p25-core` | The radio core's registers |
| `p25-lanes` | `/dev/p25-lanes` | The lane ring (0x1900_0000) |
| `p25-wideband-spec` | `/dev/p25-wideband-spec` | The spectrum ring (0x2100_0000) |
| `p25-wideband-iq` | `/dev/p25-wideband-iq` | The raw IQ capture ring (0x2200_0000) |

The rings use `compatible = "maia-sdr,rxbuffer"` (maia-kmod's driver, which keeps the ARMv7
caches coherent with the non-coherent HP writes). Their sizes and sub-buffers are in 079's
"Rings and the device tree", and must match `scanner-hdl/radio_core/config.py`.

## hwval bitstream (hardware validation)

The `hwval` image is the Tier 1 validation bitstream for the `fbench` bench
(contract: `doc/HW_VALIDATION_SUITE.md` sections 6 and 11). It reuses the P25
kernel and rootfs; only `BOOT.bin` (bitstream) and `devicetree.dtb` differ.

### Build command

```bash
./build_fpga_hwval_pretty.sh     # Git Bash; raw log in bake_hwval.log
```

or, from `cmd`, `build_fpga.bat --hwval`. `--p25` and `--hwval` are mutually
exclusive.

### Pipeline

```
scanner-hdl/hwval_hdl/*.py  (+ radio_core, maia_hdl)   [Amaranth HDL source]
         |
         | Staleness gate (hwval_hdl + radio_core + maia_hdl vs hwval_core.v;
         |   a missing hwval_regs.json / hwval_register_map.md also regenerates)
         | build_hdl.bat --verilog-only --hwval  (Docker, Step 5d):
         |   python -m hwval_hdl.hwval_top --config default hwval_core.v
         |       --svd hwval.svd --json hwval_regs.json --md hwval_register_map.md
         |   + port-contract check of module `top` against the Vivado project
         v
maia-hdl/ip/hwval-core/default/{hwval_core.v, hwval.svd, hwval_regs.json, hwval_register_map.md}
         |
         | Vivado IP packaging (ip/hwval-core/package_ip.tcl -> package_ip.ok)
         v
fishball-hwval:hwval_core_default:hwval_core:0.1.0
         |
         | maia-hdl/projects/fishball7020_hwval (system_project.tcl)
         v
fishball_hwval.sdk/system_top.xsa   (only if timing is met)
         |
         +--> tezuka_fw/board/tezuka/fishball7020/bitstream/hwval/system_top.xsa
         +--> bench/share/hwval_regs.json, doc/hwval_register_map.md
```

### Differences from the P25 build

- Timing failure is a **hard error**. `system_top_bad_timing.xsa` is never
  promoted; stale XSAs are deleted before the Vivado run.
- The project enables the `axi_ad9361` DDS, `S_AXI_HP0` and `S_AXI_HP3`
  (64-bit, memory testers at 125 MHz), puts ring v2 and the production-replica
  ring on HP1, and adds pins `y1_clk` (N18) and `ad_clkout` (R16). `rx_clk` is
  constrained at 8.138 ns and the TX path is timing-checked. Full list:
  `maia-hdl/projects/fishball7020_hwval/README.md`.
- The register map is published only after the XSA exists, so
  `bench/share/hwval_regs.json` always describes a bitstream that was built.

### Quick checks after Verilog generation

```bash
# Ports of module `top` (the build already checks these; this prints them)
awk '/^module top\(/{f=1} f&&/^endmodule/{exit} f&&/^ *(input|output|inout) /{print $0}' \
    maia-hdl/ip/hwval-core/default/hwval_core.v | grep -v 'm_axi_\|s_axi_lite_[awrb]'
# The four AXI managers
grep -oE 'm_axi_(ringv2|legacy|mt0|mt1)_(aw|ar)valid' maia-hdl/ip/hwval-core/default/hwval_core.v | sort -u
# CDC names the XDC waivers rely on (all counts must be > 0)
V=maia-hdl/ip/hwval-core/default/hwval_core.v
grep -cE '^\s*reg .*_snapshadow' $V         # snapshot shadows + census counters
grep -cE '^\s*reg .*_cdchold' $V            # DomainCrossing/ConfigSync holds
grep -c 'amaranth.vivado.false_path' $V     # FFSynchronizer first stages, census snapstage
grep -c 'fifo18e1 (' $V                     # ingest_cdc + evt FIFO18E1
```

### Tezuka side

The P25 Tezuka build (`build.bat --p25`) also builds `fishball-hwval.dtb`
(listed in `fishball_p25_7020_defconfig`) and, when
`board/tezuka/fishball7020/bitstream/hwval/system_top.xsa` exists, its
`post-image.sh` writes:

| SD path | Content |
|---|---|
| `bench/images/hwval/` | `BOOT.bin` (FSBL + hwval bitstream + U-Boot), `devicetree.dtb` (= `fishball-hwval.dtb`), `SHA256SUMS` |
| `bench/images/p25/` | the production `BOOT.bin` + `devicetree.dtb` of the same build, `SHA256SUMS` |

`fbench boot <unit> hwval|p25` swaps those pairs on the card. The first build
after `fishball-hwval.dts` was added needs a kernel rebuild
(`make linux-rebuild`) so the new DTB exists; see Tezuka
`doc/changes/005_fishball_hwval_dual_image.md`.

| DTS node (hwval) | Userspace | Purpose |
|---|---|---|
| `hwval-core@7c460000` | `/sys/class/uio/uioN/name` = `hwval-core` | register UIO, IRQ SPI 55 |
| `hwval-ringv2` | `/dev/hwval-ringv2` | ring v2 window 0x2000_0000, 16 x 1 MiB |
| `hwval-legacy` | `/dev/hwval-legacy` | legacy replica ring 0x2200_0000, 16 x 1 MiB |
| reserved `hwval-memtest@24000000` | -- | 64 MiB memtester window (no device) |
