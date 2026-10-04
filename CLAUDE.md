# Fishball scanner — project rules

A P25 and DMR trunking scanner, with an ATSC TV mode, on the Fishball Z7020 board (Zynq-7020 with
an AD9361 or AD9363), built on Maia SDR's gateware. Fork `andylee77/maia-sdr`, branch
`fishball-p25`.

## Current work

- **The radio core (079):** the gateware plan for every mode is in
  `doc/changes/079_general_radio_core.md`, "Every mode's needs". Bake A (change 082) comes next.
- **The cleanup:** `doc/CLEANUP_INVENTORY.md`, section 14's batches.
- **Data mode:** a study with Andy's decisions, `scanner/doc/DATA_MODE.md`.
- **The UI replacement:** `scanner/doc/UI_BRIEF.md`.

## Layout

| Path | What |
|------|------|
| `scanner/` | The daemon on the Zynq PS (Rust): the radio core's driver, the demodulators, trunking, audio, history, ATSC, API, web UI (`src/ui/`, plain ES modules embedded at compile time). Design: `scanner/doc/DESIGN.md` |
| `scanner/core-pac/` | The register PAC, generated from the radio core's SVD by the FPGA build |
| `p25-httpd/` | The daemon before the scanner. It stays only until the bench reads the radio core's map, then leaves (`doc/CLEANUP_INVENTORY.md` §3) |
| `scanner-hdl/` | The fork's gateware (Amaranth): `radio_core/` (the radio core 1.0.0, product "rad1"), `hwval_hdl/` (hardware validation), their tests (`test/`, `test_cocotb/`) and `generate_svd.py`. It imports upstream `maia_hdl` |
| `maia-hdl/` | Upstream Maia's gateware, unchanged apart from the fork's Vivado projects (`projects/fishball7020_p25/`, `fishball7020_hwval/`) and IP packaging (`ip/p25-core/`, `ip/hwval-core/`), which stay there because ADI's scripts use paths relative to them |
| `bench/` | `fbench` CLI and board agent (`doc/HW_VALIDATION_SUITE.md`); its runs go to `runs/bench/` |
| `tools/` | Host scripts: the SDRTrunk reference harnesses, DDC filter design, the API field reference, TV checks |
| `runs/` | Gitignored run output and captures (`runs/dmr/` holds the DMR reference captures) |
| `doc/` | `ROADMAP.md`, `CLEANUP_INVENTORY.md`; `doc/changes/NNN_*.md`, one per change |
| `maia-httpd/`, `maia-wasm/`, `maia-kmod/` | Upstream Maia; the unit uses `maia-kmod`'s rxbuffer driver for DMA |

- **The radio core's registers and rings:** 079's "Registers" and "Rings and the device tree", and
  `scanner/core-pac/core.svd`.
- **Retired docs, tools and gateware:** `C:\Users\Andy\Projects\MAIA_SDR\_archive\`, with a
  README per batch.

## Checks

Every commit builds and passes these, run from `scanner/`:

- **Host tests:** `cargo test`.
- **ARM check** (the host check skips the `cfg(target_os = "linux")` code):

  ```sh
  V=/c/Users/Andy/Projects/MAIA_SDR/maia-sdr/.venv-hdl
  PATH="$V/Scripts:$V/Lib/site-packages/ziglang:$PATH" cargo-zigbuild check --target armv7-unknown-linux-gnueabihf.2.31
  ```

  The Tezuka toolchain rejects `///` on fn params and recently stabilised features.
- **DMR reference:** `DMR_CAPTURE_DIR=C:/Users/Andy/Projects/MAIA_SDR/maia-sdr/runs/dmr cargo test --release dmr::`
  keeps 24,984+ of 24,996 lines matching SDRTrunk, and the follower test keeps the 20:57 call.
- **P25 recordings,** when a receiver changes: the 313 SDRTrunk recordings decode as before
  (`P25_LSM_WAVS`, `P25_LSM_OUT`, `cargo test --release lsm_wavs -- --ignored --nocapture`, then
  `tools/p25_lsm_compare.py`; 079's status log has the counts).
- **HDL tests,** when `scanner-hdl/` or `maia-hdl/` changes: `python -m pytest test/` from
  `scanner-hdl/` (and from `maia-hdl/` for upstream's) in `.venv-hdl`. The long sweeps run with
  `MAIA_HDL_SLOW_TESTS=1`. Three upstream model tests (`test_cpwr`, `test_floating_point`,
  `test_packer`) fail on Windows with numpy 2's integer types; the hardware they model is
  unchanged.
- **Bench host tests,** when `bench/` changes: `python -m pytest` from `bench/`.
- **P25 replay corpus:** `python bench/fbench.py run rf.p25_corpus`, with unit B transmitting
  into unit A. Andy wires the bench link when a test needs it; ask first.
- **Live:** on unit A, Clay County P25 (CC 860.9625 MHz) and Clay Electric DMR (site `cec_gcs`,
  CC 454.36875 MHz).

## Builds

- **ARM binary:** `$V/Scripts/cargo-zigbuild.exe zigbuild --release --target armv7-unknown-linux-gnueabihf.2.31`,
  with `CARGO_ZIGBUILD_ZIG_PATH` set to `$V/Lib/site-packages/ziglang/zig.exe`. For anything
  measured on a unit, add the image's flags: `RUSTFLAGS="-C target-cpu=cortex-a9 -C target-feature=+neon,+vfp3"`.
- **SD image:** `./build_tezuka_p25_pretty.sh` (about 30 minutes cached).
  - It builds tezuka_fw `fishball-dev` with `fishball_p25_7020_defconfig`.
  - Its scanner package rsyncs this checkout, so don't run cargo while it runs.
  - It exits 0 on failure: grep `tezuka_build.log` for `[ERROR]`.
  - Images land in `tezuka_fw/output_images/`.
- **Gateware:** `./build_fpga_p25_pretty.sh` (Vivado 2023.2, only inside this tree).
  - It regenerates the Verilog, the SVD and `scanner/core-pac`.
  - A timing failure is an error, and a route hook writes `utilization_hier.rpt`.
  - Commit the XSA before an image consumes it.
- Never run `cargo fix`.

## Units

| Unit | Address | Board | Image | Notes |
|------|---------|-------|-------|-------|
| A | `192.168.120.50` (Ethernet) | AD9361, external antenna | radio core 1.0.0 (`2026-10-04-radio-core-atsc1`) | The unit in use |
| B | `192.168.12.1` (USB) | AD9363, internal antenna | core 0.3.0: today's scanner does not run on it | |

- **SSH:** write the full command literally, never through a variable:
  `ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR root@192.168.120.50 '...'`.
  Copy with `scp -O` (dropbear has no sftp-server).
- **Deploy a test build:**
  - stream it to `/tmp/scanner.new`;
  - `/etc/init.d/S60scanner stop`, then wait until `pidof scanner` is empty;
  - copy it over `/usr/bin/scanner`, `chmod +x`, then start.
  - The stop unmounts the card: mount `/dev/mmcblk0p1` on `/mnt/sd` before writing there.
  - The rootfs is in RAM, so a reboot returns to the SD image.
  - Log: `/var/log/scanner.log`. Web UI on port 8080 (HTTPS 8443).
- **State on a unit:**
  - `/mnt/jffs2/scanner/` (`radio.json`, `systems.json`, `state/`);
  - `/mnt/sd/scanner-history.sqlite` and `/mnt/sd/p25_recordings/`.
  - A's flash still holds p25-httpd's old files; nothing reads them.
- **Shared units:** before a restart, deploy, retune or site switch, run `ListAgents` and message
  any other session that may be using the unit.
- **Bench link:** A eth0 `10.25.0.1` ↔ B eth0 `10.25.0.2`, B transmitting into A through pads.

## Rules

- Commit locally on `fishball-p25`; ask before any push. No `Co-Authored-By` lines.
- Each change gets `doc/changes/NNN_*.md` and an entry in `CHANGELOG_FORK.md`. Leave the
  upstream `CHANGELOG.md` alone.
- Code: a short module header saying what the module owns, and comments that state intent. No
  dates, phase history or dev notes in code; git and `CHANGELOG_FORK.md` hold the history.
- SDRTrunk (`C:\Users\Andy\Projects\SDRTrunk\sdrtrunk`) is the reference for the P25 and DMR DSP
  and protocol constants. Don't change one without evidence;
  `tools/sdrtrunk_dmr_reference.py` decodes our captures with SDRTrunk itself.
- Gateware changes pass 079's gates:
  - bit-exact against a model where there is DSP;
  - timing met with no waiver;
  - a hierarchical utilization report down to the core's blocks;
  - then the checks on unit A.
- Windows and Git Bash: `sed -i` strips CRLF, and a heredoc loses backslashes. Edit files with
  the editor tools.

## Related

- **Firmware:** `C:\Users\Andy\Projects\Tezuka\tezuka_fw`, branch `fishball-dev`.
  - The scanner package and its init script: `package/scanner/` (`S60scanner`).
  - The P25 board's device tree: `board/tezuka/fishball7020/dts/fishball-p25.dtsi`.
- **Shared docs** (board schematics, build systems, Vivado install): `C:\Users\Andy\Projects\_shared\`.
