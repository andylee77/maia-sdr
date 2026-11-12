# Fishball scanner — project rules

A P25 and DMR trunking scanner, with an ATSC TV mode, on the Fishball Z7020 board (Zynq-7020 with
an AD9361 or AD9363), built on Maia SDR's gateware. Fork `andylee77/maia-sdr`, branch
`fishball-p25`.

## Current work

- **The plan:** `doc/ROADMAP.md` (095): the goals, the capabilities each needs, the features by
  module, the order of work and the questions open for Andy. Step 2, the Now page as a trunked
  monitoring station's front, is built from the layout he picked (096,
  `doc/changes/096_now_tuners_first.md`). Step 3 is built: the Aliases page with the
  RadioReference import (097, `doc/changes/097_aliases_page.md`) and SDRTrunk's playlist, in and
  out, on Systems (098, `doc/changes/098_sdrtrunk_playlist.md`). Now lists the talkgroups heard
  in the last week and nothing else (099, Andy's answer to the roadmap's Q4). Step 4 is built:
  Settings and Diagnostics are split into the radio's own (the rail's two) and each module's
  (tabs of it), with the event log kept by part (100,
  `doc/changes/100_settings_diagnostics_split.md`). Step 5 is built: Files, the recordings as a
  ZIP archive, the history's database and the TV recordings to take away (101,
  `doc/changes/101_files.md`; `tools/scanner_files.py` from the command line). Step 6, Access,
  is begun: a password, an administrator and a listener, tokens, and a guard on every route and
  socket (102, `doc/changes/102_access.md`). A unit is open until its administrator's password
  is set; unit A's is Andy's to set. The bounds on stalled clients are in (103,
  `doc/changes/103_bounds.md`). HTTPS is the one way in once a password is set, under a CA and a
  certificate the unit makes itself (104, `doc/changes/104_https_own_ca.md`; Q9, "Yes, keep
  it"); no browser has been tried with the CA installed, which is Andy's to do. libiio's
  daemon (`iiod`) is off unless switched on, in Settings, Access, and the bench switches it on
  when it needs it (105, `doc/changes/105_iiod_switch.md`; Q10, "Yes, build it"; the firmware's
  `iio-switch`, tezuka_fw 008). SSH takes a key and no password (107,
  `doc/changes/107_ssh_keys_only.md`; tezuka_fw 009), and with that step 6 is built. 096 to 105 are in
  unit A's SD image since 106 (`doc/changes/106_image_rust.md`, which also holds the scanner's
  dependencies to the image's Rust). 095's review of the UI and its API:
  `doc/changes/095_structure_and_review.md` (what it fixed, and what is open by
  step).
- **The radio core (079):** the gateware plan for every mode is in
  `doc/changes/079_general_radio_core.md`, "Every mode's needs". Bake A (change 082, core 2.0.0)
  is in; its burst-timing gate waits for Andy (`tools/burst_timing.py`). Bake D (change 085,
  core 2.1.0: the 8-VSB receiver in the PL, live ATSC TV) is on unit A. Bakes B and C wait for
  their evidence (079, "The bakes").
- **Live ATSC TV (085):** `doc/changes/085_bake_d_8vsb_receiver.md`. The block
  (`scanner-hdl/radio_core/vsb/`) matches 083's model (`scanner/src/protocol/atsc/pl`) bit for
  bit; the PS's half is `protocol::atsc::live` and `services::atsc::live`;
  `tools/atsc_live_check.py` names stations live and watches a channel. Known limit: a channel
  where every codeword fails holds the live thread at a core.
- **The ATSC player (084):** `doc/changes/084_atsc_player.md`: our own decoders in Rust for the
  browser (the transport stream, AC-3, MPEG-2 video), and the stream for VLC. The Watch page (091) plays
  any named channel live (085), or a recorded one without the block. H.262 (2000) and its corrigenda
  are in `C:\Users\Andy\Projects\_shared\ITU\H.262\`.
- **The cleanup:** `doc/CLEANUP_INVENTORY.md`, section 14's batches.
- **Data mode:** a study with Andy's decisions, `scanner/doc/DATA_MODE.md`.
- **The UI replacement:** `scanner/doc/UI_BRIEF.md`. The module rail and the new Now page are in
  (090, `doc/changes/090_module_rail_now.md`; modules in `scanner/src/ui/static/js/modules.js`),
  TV's Watch page with its three views (091, `doc/changes/091_tv_watch.md`), the last TV scan
  kept on the flash (092), the Scanner's Activity and Systems pages (093,
  `doc/changes/093_activity_systems.md`), and Channels, Diagnostics and Settings (094,
  `doc/changes/094_channels_diagnostics_settings.md`): every page is in the rail's layout.
  Checks: `runs/ui_check/modules_check.mjs`, `now_rail.mjs`, `scanner_pages.mjs`,
  `aliases_page.mjs`, `playlist_page.mjs`, `files_page.mjs`, `tool_pages.mjs` (the modules' and the
  radio's Diagnostics and Settings, 100) and `tv_watch.mjs`; on a unit, `tv_diag_live.mjs` (it takes the
  radio for live TV, then gives it back). `now_rail.mjs`'s writes need a talkgroup the daemon
  has heard: run `python runs/ui_check/seed_heard.py` before the PC daemon starts. Access (102):
  `access_api.py` and `access_page.mjs` set passwords, so they run against the PC daemon only
  and leave it open; never set a password on a unit. A tool or check reaches a unit that has a
  password over HTTPS, with a token in `SCANNER_TOKEN` and the file of the unit's CA
  (`http://<unit>:8080/ca.crt`) in `SCANNER_CA`, for a page check in `NODE_EXTRA_CA_CERTS`
  (`tools/scanner_token.py`, `runs/ui_check/unit.mjs`). The bounds (103): `bounds_api.py`
  against the PC daemon (`--slow` holds a download a minute). HTTPS (104): `https_api.py`, the
  PC daemon only. Start the PC daemon with `--listen-https 127.0.0.1:8453` too; the checks that
  set passwords take its plain address (`access_api.py`, `https_api.py`) or its HTTPS one
  (`access_page.mjs`). Run the page checks with `node`: VS Code's Electron run as Node refuses
  the unit's CA. libiio's switch (105): start the PC daemon with
  `--iio-switch <repo>/runs/ui_check/fake_iio_switch.cmd` too, the firmware's command's
  stand-in; `iiod_api.py` tries the switch there, and with `--unit` a unit's real one (it leaves
  it as it found it).

## Layout

| Path | What |
|------|------|
| `scanner/` | The daemon on the Zynq PS (Rust): the radio core's driver, the demodulators, trunking, audio, history, ATSC, API, web UI (`src/ui/`, plain ES modules embedded at compile time). Design: `scanner/doc/DESIGN.md` |
| `scanner/core-pac/` | The register PAC, generated from the radio core's SVD by the FPGA build |
| `scanner/player/` | The ATSC player (`atsc-player`, MIT, no dependencies, built to WebAssembly for the Watch page): the transport stream, AC-3, MPEG-2 video. FFmpeg is only its tests' reference (084) |
| `scanner-hdl/` | The fork's gateware (Amaranth): `radio_core/` (the radio core 2.1.0, product "rad1", its 8-VSB block in `vsb/`), `hwval_hdl/` (hardware validation), their tests (`test/`, `test_cocotb/`, the Verilator benches in `test_verilator/`) and `generate_svd.py`. It imports upstream `maia_hdl` |
| `maia-hdl/` | Upstream Maia's gateware, unchanged apart from the fork's Vivado projects (`projects/fishball7020_p25/`, `fishball7020_hwval/`) and IP packaging (`ip/p25-core/`, `ip/hwval-core/`), which stay there because ADI's scripts use paths relative to them |
| `bench/` | `fbench` CLI and board agent (`doc/HW_VALIDATION_SUITE.md`); its runs go to `runs/bench/` |
| `tools/` | Host scripts: the SDRTrunk reference harnesses, DDC filter design, the API field reference, TV checks, `scanner_files.py` (downloads of what is on a unit's card, checked) |
| `runs/` | Gitignored run output and captures (`runs/dmr/` holds the DMR reference captures) |
| `doc/` | `ROADMAP.md`, `CLEANUP_INVENTORY.md`; `doc/changes/NNN_*.md`, one per change |
| `maia-httpd/`, `maia-wasm/`, `maia-kmod/` | Upstream Maia; the unit uses `maia-kmod`'s rxbuffer driver for DMA |

- **The radio core's registers, rings and frames:** 082's "The register map", "The rings", the
  spectrum frame's header and the capture, and `scanner/core-pac/core.svd`; the lane packet in
  079's "The lane packet".
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

  The Tezuka toolchain rejects `///` on fn params and recently stabilised features: the image
  builds with Rust 1.86 (`rust-version` in `scanner/Cargo.toml`), older than this PC's, which
  runs the host tests and this check. A host test (`tests/image_rust.rs`, 106) holds every
  dependency to it, and `scanner/.cargo/config.toml` has cargo pick versions it can build; the
  scanner's own code is held to it only by the image build.
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
- **The 8-VSB block's whole fields,** when `scanner-hdl/radio_core/vsb/` or the model changes:
  `MAIA_HDL_SLOW_TESTS=1 VSB_VECTORS=<set> python -m pytest test/test_vsb_whole.py` from
  `scanner-hdl/`, Verilator in Docker (`test_verilator/Dockerfile`), on each of the six sets in
  `runs/atsc/vectors/` (written by `pl::vectors`, 085's "Tests"); every symbol and packet word
  must match the model.
- **Bench host tests,** when `bench/` changes: `python -m pytest` from `bench/`.
- **Player tests,** when `scanner/player/` changes: `cargo test` from `scanner/player/`, with
  `ATSC_PLAYER_DATA=C:/Users/Andy/Projects/MAIA_SDR/maia-sdr/runs/atsc/captures_20261004` for the
  comparisons with FFmpeg.
- **SDRTrunk playlist reference,** when `scanner/src/services/config/sdrtrunk/` changes:
  `python tools/sdrtrunk_playlist_reference.py C:/Users/Andy/SDRTrunk/playlist/default.xml C:/Users/Andy/SDRTrunk/playlist/playlist.xml`
  reads both as SDRTrunk's own reader does (1,088 and 23 lines the same), and with
  `--unit http://<unit>:8080` holds a unit's exported playlist the same way.
- **P25 replay corpus:** `python bench/fbench.py run rf.p25_corpus`, with unit B transmitting
  into unit A. Andy wires the bench link when a test needs it; ask first.
- **Live:** on unit A, Clay County P25 (site `clay_county_public_safety_simulcast`, CC
  860.9625 MHz) and Clay Electric DMR (site `clay_elec_site_2`, CC 454.36875 MHz).

## Builds

- **ARM binary:** `$V/Scripts/cargo-zigbuild.exe zigbuild --release --target armv7-unknown-linux-gnueabihf.2.31`,
  with `CARGO_ZIGBUILD_ZIG_PATH` set to `$V/Lib/site-packages/ziglang/zig.exe`. For anything
  measured on a unit, add the image's flags: `RUSTFLAGS="-C target-cpu=cortex-a9 -C target-feature=+neon,+vfp3"`.
- **SD image:** `./build_tezuka_p25_pretty.sh` (about 30 minutes cached).
  - It builds tezuka_fw `fishball-dev` with `fishball_p25_7020_defconfig`.
  - Its scanner package rsyncs this checkout, so don't run cargo while it runs.
  - It exits 0 on failure: grep `tezuka_build.log` for `[ERROR]`.
  - Images land in `tezuka_fw/output_images/`.
- **The ATSC player's module:** `tools/build_player.sh` after a change to `scanner/player/`; commit
  `scanner/src/ui/static/player/atsc_player.wasm` and its `.source` with it (a host test fails
  until then).
- **Gateware:** `./build_fpga_p25_pretty.sh` (Vivado 2023.2, only inside this tree).
  - It regenerates the Verilog, the SVD and `scanner/core-pac`.
  - A timing failure is an error, and a route hook writes `utilization_hier.rpt`.
  - Commit the XSA before an image consumes it.
- Never run `cargo fix`.

## Units

| Unit | Address | Board | Image | Notes |
|------|---------|-------|-------|-------|
| A | `192.168.120.50` (Ethernet) | AD9361, external antenna | radio core 2.1.0 (`2026-11-11-access2`: 096 to 106) | The unit in use. On 2026-11-10 its feed was on the radio antenna, not the TV antenna (Andy): TV decodes poorly there (7 stations named, 16 on the directional), so ask him for the TV antenna before any TV reception work |
| B | `192.168.12.1` (USB) | AD9363, internal antenna | not in use; it needs the current image before anything runs on it | |

- **SSH:** by key only since 107 (the image's key is this PC's `~/.ssh/id_ed25519`; the root
  password opens only the serial consoles). Write the full command literally, never through a
  variable:
  `ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR root@192.168.120.50 '...'`.
  Copy with `scp -O` (dropbear has no sftp-server).
- **Deploy a test build:**
  - stream it to `/tmp/scanner.new`;
  - `/etc/init.d/S60scanner stop`, then wait until `pidof scanner` is empty;
  - copy it over `/usr/bin/scanner`, `chmod +x`, then start.
  - An SD image from before tezuka_fw's 007 starts the scanner with certificate files, which a
    scanner from 104 on does not take: copy `tezuka_fw/package/scanner/S60scanner` over
    `/etc/init.d/S60scanner` with the binary.
  - An SD image from before tezuka_fw's 008 has no switch for libiio's daemon: the scanner says
    so, and `iiod` runs always. To try the switch there, copy `overlay_base/etc/init.d/S23udc`
    (without its CRs) and `overlay_p25/usr/sbin/iio-switch` to the unit.
  - The stop unmounts the card: mount `/dev/mmcblk0p1` on `/mnt/sd` before writing there.
  - The rootfs is in RAM, so a reboot returns to the SD image.
  - Log: `/var/log/scanner.log`. Web UI on port 8080 (HTTPS 8443, under the unit's own CA).
- **State on a unit:**
  - `/mnt/jffs2/scanner/` (`radio.json`, `systems.json`, `state/`; `tls/`, the unit's CA and
    certificate; `access.json` once a password is set); `/mnt/jffs2/iio-switch`, the firmware's
    choice for libiio's daemon (`on`; anything else is off);
  - `/mnt/sd/scanner-history.sqlite` and `/mnt/sd/p25_recordings/` (UTC day folders, locked ones
    under `locked/`; FAT32 allows a directory about 8,000 recordings, 086).
  - A's flash still holds p25-httpd's old files and the firmware's old certificates; nothing
    reads them.
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
