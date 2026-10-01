# Fishball scanner — project rules

A P25 and DMR trunking scanner on the Fishball Z7020 board (Zynq-7020 with an AD9361 or
AD9363), built on Maia SDR's gateware. Fork `andylee77/maia-sdr`, branch `fishball-p25`.

## Current work

Change 076 builds a fresh crate, `scanner/`, that replaces `p25-httpd/`. The design and its
status log are in `scanner/doc/DESIGN.md`; work phase by phase as its section 15 says.

- `p25-httpd` stays the production binary, with fixes only, until the cutover.
- Docs the refactor needs go in the crate (`scanner/doc/`). The old docs stay where they are.

## Layout

| Path | What |
|------|------|
| `p25-httpd/` | Production daemon on the Zynq PS (Rust): decoders, trunking, audio, history, API, web UI (`src/httpd/ui/`, plain ES modules embedded at compile time) |
| `p25-httpd/p25-json/`, `p25-httpd/p25-pac/` | API types; register PAC generated from the gateware's SVD |
| `scanner/` | The 076 crate (from phase 1) |
| `maia-hdl/p25_hdl/` | P25 gateware (Amaranth); Vivado project `maia-hdl/projects/fishball7020_p25/` |
| `maia-hdl/maia_hdl/`, `maia-hdl/hwval_hdl/` | Maia gateware; hardware-validation gateware |
| `bench/` | `fbench` CLI and board agent (`doc/HW_VALIDATION_SUITE.md`) |
| `tools/` | Host scripts: captures, analysis, SDRTrunk reference harness |
| `runs/` | Gitignored run output and captures (`runs/dmr/` holds the DMR reference captures) |
| `doc/` | `P25_ADDRESS_MAP.md` (registers, DMA rings), `API_CONSUMERS.md`, `ROADMAP.md`; `doc/changes/NNN_*.md`, one per change |
| `maia-httpd/`, `maia-wasm/`, `maia-kmod/` | Upstream Maia; the unit uses `maia-kmod`'s rxbuffer driver for DMA |

## Checks

Every commit builds and passes these, run from `p25-httpd/` (later also `scanner/`):

- **Host tests:** `cargo test`. The golden-vector emitters are `#[ignore]`; run them with
  `cargo test lsm::golden_dump:: -- --ignored` only when the HDL test vectors must change.
- **ARM check** (the host check skips the `cfg(target_os = "linux")` code):

  ```sh
  V=/c/Users/Andy/Projects/MAIA_SDR/maia-sdr/.venv-hdl
  PATH="$V/Scripts:$V/Lib/site-packages/ziglang:$PATH" cargo-zigbuild check --target armv7-unknown-linux-gnueabihf.2.31
  ```

  The Tezuka toolchain rejects `///` on fn params and recently stabilised features.
- **DMR reference:** `DMR_CAPTURE_DIR=C:/Users/Andy/Projects/MAIA_SDR/maia-sdr/runs/dmr cargo test --release dmr::`
  keeps 24,984+ of 24,996 lines matching SDRTrunk, and the follower test keeps the 20:57 call.
- **P25 replay corpus:** `python bench/fbench.py run rf.p25_corpus`, with unit B transmitting
  into unit A. Andy wires the bench link when a test needs it; ask first.
- **Live:** on unit A, Clay County P25 (CC 860.9625 MHz) and Clay Electric DMR (site `cec_gcs`,
  CC 454.36875 MHz).

## Builds

- **ARM binary:** `$V/Scripts/cargo-zigbuild.exe zigbuild --release --target armv7-unknown-linux-gnueabihf`,
  with `CARGO_ZIGBUILD_ZIG_PATH` set to `$V/Lib/site-packages/ziglang/zig.exe`.
- **SD image:** `./build_tezuka_p25_pretty.sh` (about 30 minutes cached). It builds tezuka_fw
  `fishball-dev` with `fishball_p25_7020_defconfig`, and its p25-httpd package rsyncs this
  checkout, so don't run cargo while it runs. It exits 0 on failure: grep `tezuka_build.log`
  for `[ERROR]`. Images land in `tezuka_fw/output_images/sdimg/`.
- **Gateware:** `./build_fpga_p25_pretty.sh` (Vivado 2023.2, only inside this tree; it
  regenerates the Verilog, SVD and PAC). Not needed for 076.
- Never run `cargo fix`.

## Units

| Unit | Address | Board | Notes |
|------|---------|-------|-------|
| A | `192.168.120.50` (Ethernet) | AD9361, external antenna | The unit in use |
| B | `192.168.12.1` (USB) | AD9363, internal antenna | |

- **SSH:** write the full command literally, never through a variable:
  `ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR root@192.168.120.50 '...'`.
  Copy with `scp -O` (dropbear has no sftp-server).
- **Deploy a test binary:** stream it to `/tmp/p25-httpd.new`, `/etc/init.d/S60p25-httpd stop`,
  copy it over `/usr/bin/p25-httpd` and `chmod +x`, then start. The rootfs is in RAM, so a reboot
  returns to the SD image. Log: `/var/log/p25-httpd.log`. Web UI on port 8080 (HTTPS 8443).
- **State on a unit:** `/mnt/jffs2/p25-*` (settings, sites, plans, crystal calibration) and
  `/mnt/sd` (`p25-history.sqlite`, recordings).
- **Shared units:** before a restart, deploy, retune or site switch, run `ListAgents` and message
  any other session that may be using the unit.
- **Bench link:** A eth0 `10.25.0.1` ↔ B eth0 `10.25.0.2`, B transmitting into A through pads.

## Rules

- Commit locally on `fishball-p25`; ask before any push. No `Co-Authored-By` lines.
- Each change gets `doc/changes/NNN_*.md` and an entry in `CHANGELOG_FORK.md`. Leave the
  upstream `CHANGELOG.md` alone.
- Code: a short module header saying what the module owns, and comments that state intent. No
  dates, phase history or dev notes in code; git and `CHANGELOG_FORK.md` hold the history.
- SDRTrunk (`C:\Users\Andy\Projects\SDRTrunk\sdrtrunk`) is the reference for the P25 and DMR
  DSP and protocol constants. Don't change one without evidence;
  `tools/sdrtrunk_dmr_reference.py` decodes our captures with SDRTrunk itself.
- Gateware does not change in 076.
- Windows and Git Bash: `sed -i` strips CRLF, and a heredoc loses backslashes. Edit files with
  the editor tools.

## Related

- Firmware: `C:\Users\Andy\Projects\Tezuka\tezuka_fw`, branch `fishball-dev`. Init script:
  `board/tezuka/common/overlay_p25/etc/init.d/S60p25-httpd`.
- Shared docs (board schematics, build systems, Vivado install): `C:\Users\Andy\Projects\_shared\`.
