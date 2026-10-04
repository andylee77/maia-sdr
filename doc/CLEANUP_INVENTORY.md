# Cleanup inventory

**Date:** 2026-10-04, on `fishball-p25` after abc6214. Andy approved the recommendations;
section 14 proposes the batches, and the status below says what is done. Sections 1-13 are the
findings as they stood before the cleanup.

## Status

| Batch | State | Commits |
|-------|-------|---------|
| 1. Push | The branch is pushed to efef2ce (the inventory). The later commits, the four tags, and tezuka_fw's branch and tag wait for Andy | |
| 2. Texts that mislead | Done | e384095, 6e6d7ab; tezuka_fw 8a34f2c |
| 3. Dead on arrival | Done | 9740d17 |
| 4. The bench on the radio core | Done: the map from `radio_core.bench_map`, maintenance mode, `rf.cw_ppm`, `sys.boot_log`, the texts | 9f698db, b1f77f2 |
| 5. p25-httpd out | Done, with tezuka_fw's package | 493a12d; tezuka_fw 8a34f2c |
| 6. The archive move | Done: `MAIA_SDR/_archive/cleanup_2026-10-04/` | 31ac775 |
| 7. `API_FIELDS.md` | Done: regenerated from unit A, every route sampled | (this commit) |
| 8. The old fishball-p25 folder | Open | |
| Added: `scanner-hdl/` | Done: the fork's gateware apart from Maia's (Andy's ask) | b64fa63 |

Left open in sections 8 and 9:

- `rf.p25_replay` reads p25-httpd's routes and is in no suite; a port to `/api/v1` (`receivers`,
  the hold) would bring it back.
- `rf.p25_corpus` modes A and B use routes the scanner serves; mode C and the per-item counters
  need `/api/traffic` and `/api/monitor`, which it does not. Not yet run on the radio core.
- `fbench setup agent` on both units: their agents predate the new map. Unit B needs the
  current image first.
- F20 in `HW_VALIDATION_SUITE.md`: re-check against the radio core's carve-outs.

What the inventory covers:

- what is out of date in the repo, its docs, tools and bench;
- tezuka_fw's references into this repo;
- `MAIA_SDR/_archive/` and the folders beside it;
- the old `fishball-p25` folder;
- the memory notes.

Upstream Maia files are left alone, `CHANGELOG.md` and `README.md` among them.

Each item gives what it is and when it last changed, what still refers to it, and one of these
recommendations:

| Recommendation | Meaning |
|----------------|---------|
| **keep** | Current; no change |
| **update** | Keep it, and fix what is named as wrong |
| **archive** | Move it out of the repo to `C:\Users\Andy\Projects\MAIA_SDR\_archive\<batch>\`, with a README for the batch, and fix every reference. Git history keeps it too |
| **delete** | Remove it. Nothing worth keeping outside git history |

"Inferred" marks what was not checked directly. The facts come from git, reads of the files, a
`pytest --collect-only` of the HDL tests, and unit A's API (read-only).

## 1. First: the history exists only on this PC

| Repo | Remote | Local | Tags only local |
|------|--------|-------|-----------------|
| maia-sdr | `origin/fishball-p25` at c27fd77 (2026-04-15) | **407 commits ahead** | `pre-076`, `build/p25-core-0.3.0`, `build/2026-10-01-scanner-image3`, `build/2026-05-03-forensics-sd-redirect` |
| tezuka_fw | `origin/fishball-dev` at 72ad61f (2026-04-15) | 24 commits ahead | `build/2026-10-01-scanner-image3` |

- Everything since April exists only here: the scanner, the radio core, 076-081 and the 0.3.0 LSM
  gateware's history.
- An archive move or a `git rm` still leaves the history in the local repo, but there is no second
  copy.
- **Recommended:** push both branches and the tags before the first removal. Pushes need Andy's
  go.

## 2. CLAUDE.md

Last change e020ec2 (2026-10-04); every session loads it. **Update.** What is wrong:

| Lines | Says | Now |
|-------|------|-----|
| 6-12 | "Current work": 076 builds `scanner/`; "`p25-httpd` stays the production binary ... until the cutover" | The scanner is the production binary on both units' images since 2026-10-01. Current work: 079's bakes, data mode, the UI replacement |
| 18-21 | `p25-httpd/` "Production daemon"; `p25-pac` the gateware's PAC; `scanner/` "The 076 crate (from phase 1)"; `p25_hdl` "P25 gateware" | The scanner is the daemon. The core's PAC is `scanner/core-pac`, and `p25-pac` is the 0.3.0 map only the bench still reads. `p25_hdl` is the radio core 1.0.0 ("rad1") |
| 25-26 | `runs/` holds run output; `P25_ADDRESS_MAP.md` (registers, DMA rings) | fbench writes its runs to `doc/diagnostics/` (section 5.3). The map is core 0.3.0's; the radio core's is in 079 "Step 3a" and `scanner/core-pac/core.svd` |
| 29-34 | Checks run from `p25-httpd/` (later also `scanner/`); the golden-vector emitters | The checks run in `scanner/`. The emitters serve the LSM gateware, which the radio core no longer builds |
| 50-53 | The ARM recipe | Lacks the image's RUSTFLAGS (`-C target-cpu=cortex-a9 -C target-feature=+neon,+vfp3`, tezuka `scanner.mk`) and the `.2.31` target suffix the check uses |
| 54-56 | "its p25-httpd package rsyncs this checkout" | The scanner package does |
| 58-59 | Gateware "Not needed for 076" | 079 baked the radio core; the next bakes are in 079 |
| 64-67 | The unit table | Add each unit's image: A `2026-10-04-radio-core-atsc1`, B still core 0.3.0, so **today's scanner cannot be deployed to B** |
| 72-76 | Deploy and state: `/tmp/p25-httpd.new`, `S60p25-httpd`, `/usr/bin/p25-httpd`, `/var/log/p25-httpd.log`, `/mnt/jffs2/p25-*`, `p25-history.sqlite` | `/etc/init.d/S60scanner stop` (wait until `pidof scanner` is empty; the stop unmounts the card), `/usr/bin/scanner`, `/var/log/scanner.log`, `/mnt/jffs2/scanner/`, `/mnt/sd/scanner-history.sqlite`, recordings in `/mnt/sd/p25_recordings` |
| 84 | "Each change gets `doc/changes/NNN_*.md`" | ROADMAP says a doc is written "when it is large", and 060-074 have none. One of the two should win |
| 91 | "Gateware does not change in 076" | Obsolete. The live rule is 079's gates |
| 98 | Init script `overlay_p25/etc/init.d/S60p25-httpd` | `package/scanner/S60scanner`; the overlay holds only `S50maia-kmod` and `S50p25-httpd-certificates` |

## 3. p25-httpd/

- **What:** the daemon before the scanner: about 4.2 MB of tracked source (262 files) and an 8.3 GB
  ignored `target/`. Last commit f81fb9e (2026-10-01).
- **What no longer needs it:**
  - **The scanner:** its only path dependency is `core-pac` (`scanner/Cargo.toml:17`). `p25-pac` was
    dropped in 33b9780 (2026-10-03).
  - **The image:** no defconfig selects the p25-httpd package. `fishball_p25_7020_defconfig:141`
    selects `BR2_PACKAGE_SCANNER`, and `S60scanner` starts `/usr/bin/scanner`.
  - **079's condition** ("`p25-httpd/src/lsm` and `sw_demod` leave once the scanner's LSM replaces
    them as the reference"): the scanner's LSM is checked against SDRTrunk itself, and unit A runs
    on it. The condition looks met; nobody has written so.
- **What still refers to it:**

  | Where | What | Breaks if it goes |
  |-------|------|-------------------|
  | `bench/fbench/regmaps.py:45`, `bench/tests_host/test_regmaps.py`, `test_cli.py:278-282` | `P25_SVD = p25-httpd/p25-pac/p25.svd` | yes: `fbench regmaps build` and three host tests |
  | `bench/agent/build.rs:73` | the same SVD | no: falls back to `maps/p25_regs.fallback.json` |
  | `tools/p25_lsm_hdl_replay.py:93`, `p25_baseline_analyze.py`, `p25_chain_compare.py`, `p25_decode_imbe_capture.py` | read p25-httpd files or run its tests | yes (all four are archive candidates, section 8) |
  | tezuka_fw `package/scanner/scanner.mk:11,15-16` | rsyncs `p25-httpd/p25-pac` "the register PAC it builds against" | no (the rule matches nothing; inferred) |
  | tezuka_fw `package/p25-httpd/`, `Config.in:28`, `build.sh:151-166`, `post-build-p25.sh:43` | the unselected package and guards | no |
  | `build_hdl.sh:401`, `.gitignore:20-22,141` | comments and ignore lines | no |
  | `maia-hdl/test/golden_vectors/*.json` | written by `p25-httpd/src/lsm/golden_dump.rs` | no: the files are tracked. Only regenerating them needs p25-httpd |
  | `scanner/doc/DESIGN.md:1003-1006`, `scanner/README.md:6,32`, `BUILD_FPGA.md:305,332-349`, `doc/P25_ADDRESS_MAP.md:18-19`, `doc/API_CONSUMERS.md`, the scanner's ambe `reference/README.md:5` | describe it as current | text only |
  | `scanner/src` comments (`api/ws.rs`, `api/legacy.rs`, `hardware/presets/mod.rs:141`, `trunking/calls/replay_tests.rs`, and others) | provenance and parity notes | no |

- **Recommended:** **delete** (git keeps it) once the bench reads the radio core's map
  (section 9), the four tools are archived, and section 1's push is done. Then:
  - tezuka_fw: drop the `p25-pac` rsync lines and `package/p25-httpd/`;
  - fix the texts above, the `.gitignore` lines and the memory notes (section 13);
  - DESIGN §15 phase 8 closes.

## 4. maia-hdl

### 4.1 `p25_hdl` modules

The radio core reaches 10 of the 35 files (1,373 lines). Nothing reaches the other 25 (7,669
lines). Every test still imports: `pytest --collect-only` finds 393 tests in 50 files with no
error.

| Module(s) | Last change | Reached | Referred to by | Recommendation |
|-----------|-------------|---------|----------------|----------------|
| `p25_top`, `config`, `configs`, `axil_bridge`, `lane_packetizer`, `lane_ring`, `__init__`, `p25_top_version` | to d343685 / 78356c6 (2026-10-03) | yes | the build, 079, `scanner/core-pac` | **keep** |
| `p25ddc.py` | 0898736 (2026-04-15) | yes, the lanes' DDCs | `test_p25ddc`, the 079 study (DDC lanes stay) | **keep, update** the header: it cites `LsmAgc`, `LsmDecimator2`, `p25-httpd/src/fpga.rs::configure_ddc()`, dates and phase history |
| `iq_packer.py` | 6d149f0 (2026-04-10) | yes, the capture ring; hwval's `legacy_ring` | `test_iq_packer`, cocotb `iq_packer` | **keep, update** the header: it describes "post-DDC IQ at 62.5 kSPS", "Phase 6C" and `P25_ADDRESS_MAP.md`; it now packs raw AD9361 samples at up to 64 MB/s |
| the 16 `lsm_*` (5,781 lines), `dibit_packer` | 86eef5d (2026-09-27) and earlier | no, since 3a (d343685) | 17 test files, 4 golden JSONs (547 KB), `tools/p25_lsm_hdl_replay.py`, `tools/README.md:35`, old docs | **archive** with their tests, `golden_vector_loader.py` and the JSONs. The software LSM replaced them; `p25_lsm_hdl_replay.py --hdl-root` can point at the archived copy |
| `c4fm_demod`, `symbol_timing` | 8c704b4, 94faae9 (2026-04-09) | no, since 2026-04-23 | their tests | **delete** with their tests |
| `channel_mux`, `traffic_pipeline` | fa246d3 (2026-05-02) | no, never built | no tests; `traffic_pipeline`'s header claims `p25_top` uses it | **delete** |
| `per_target_ddc` | fa246d3 | no | `test_per_target_ddc` | **archive** (3b's synthesizer is a different design) |
| `polyphase_channelizer`, `polyphase_proto_coeffs` | fa246d3 | no | `test_polyphase_channelizer`, 079 (named for reuse), `tools/polyphase_proto_design.py` | **keep** for 3b. The coefficients (M = 64, K = 6, critically sampled) do not fit 3b and will be regenerated |
| `signal_energy` | fa246d3 | no | `test_signal_energy` | **Andy's call:** a round-robin per-bin power IIR, close to 3b's activity integrator (inferred). Keep for 3b or archive |

### 4.2 Tests and fixtures

| Item | Last change | Recommendation |
|------|-------------|----------------|
| `test/golden_vectors/p25_core_0.2.0.svd` (42 KB) | — | **delete**: nothing reads it since 3a |
| `test/hwval_axi_wmodel.py`, `hwval_axil_bfm.py` | f2de4d3 | **keep** whatever happens to hwval: `test_p25_top`, `test_lane_ring` and `test_axil_bridge` use them |
| `sim_hdl.sh` / `.bat` (the Docker test runner) | 3b1d0bd (2026-04-07) | **update or delete.** Nothing calls it, the tests run from `.venv-hdl`, and under it three `test_p25_top` tests would fail: they read files outside the copy it makes (inferred) |
| `maia-hdl/maia-hdl/test_cocotb/iq_packer/` | 2026-04-09 | **delete**: an empty stray directory |

### 4.3 Projects, IP and committed build products

| Item | Last change | Recommendation |
|------|-------------|----------------|
| `projects/fishball7020_p25/` | a8aae49 (2026-10-03) | **keep, update** the stale comments: `system_bd.tcl:8` "DDC + LSM demod + dibit DMA"; `system_constr.xdc`'s `lsm_` names and the `manual_decim` false path; `system_project.tcl`'s "Phase 6E.6e" |
| `projects/fishball7020_p25/rerun_with_strategy.tcl` | cfd691f (2026-04-10) | **delete**: its strategy is the default now, and it writes an XSA with no timing check, around 3a's gate |
| `projects/fishball7020_p25/fishball_p25.sdk/system_top.xsa` | a8aae49 | **keep**: the record of what the image consumes (identical to tezuka_fw's copy) |
| `projects/fishball7020_iio/fishball.sdk/system_top.xsa` (0.78 MB) | 91616bd (2026-04-12, in a p25-httpd commit) | **delete**: it differs from the XSA tezuka_fw builds the Maia image from, and nothing uses it (inferred) |
| `projects/fishball7020_hwval/` | f2de4d3 (2026-09-26) | **keep, update** the README's deltas table: "seven P25 ring masters" (now three), "bad-timing XSA promoted" (now an error). **The hwval bitstream has never been baked**: no IP output, no build products, no XSA in tezuka_fw |
| `projects/pluto/` | 327a6b2 (2026-04-16, `MODE_1R1T`) | **keep**: both fork projects source its `system_bd.tcl` |
| the other upstream projects (`e200*`, `fishball_iio`, `libre_iio`, `pluto_iio`, `plutoplus*`, `Makefile`) | F5OEO, 2025 | **keep** (upstream; no fork script builds them) |
| `maia-hdl/ip/p25-core/default/p25.svd`, the `vivado_*.backup.*` pairs | 2026-04 to 2026-10, untracked | **delete** (stale 0.x SVD, Vivado leftovers) |

### 4.4 Upstream files the fork changed

- **`maia_hdl/dma.py`:** adds `DmaStreamRingWrite`, which the radio core uses. **keep**.
- **`maia_hdl/spectrometer.py`:** a generic truncation pattern (equal to upstream's at order 12)
  and a dated history comment. **Update:** take the history comment out. The 079 study puts any
  further spectrometer change (items 5 and 6) in a `p25_hdl` wrapper, leaving `maia_hdl`'s blocks
  alone.

## 5. doc/

### 5.1 The docs

| Doc | Last change | Referred to by | Recommendation and what is wrong |
|-----|-------------|----------------|----------------------------------|
| `P25_ADDRESS_MAP.md` | 6c9b553 (2026-09-30) | `CLAUDE.md:26`, `scanner/doc/BRIEF.md:238`, `iq_packer.py:14`, `test_iq_packer.py:9`, other old docs | **archive** beside the 0.3.0 build backup. It is core 0.3.0's map from top to bottom, yet calls itself the "Single Source of Truth". Point CLAUDE.md at 079 "Step 3a" and `scanner/core-pac/core.svd`, or write a short `doc/RADIO_CORE_MAP.md` from them |
| `HDL_LAYOUT_AND_ROADMAP.md` | 3a1a8c9 (2026-04-18) | old docs | **archive**: the LSM-era layout; 079 replaced its roadmap. §11-12 (polyphase design space, budget) are background for 3b |
| `DASHBOARD_PLOTS.md` | cabcb21 (2026-04-22) | `ROADMAP.md:356,370` | **archive**: gateware taps that 3a removed; `/ws/iq` is gone. §1 and §5 are worth lifting into the 20 Hz plots design when it starts |
| `DASHBOARD_CLEANUP.md` | fa246d3 (2026-05-02) | change 056 | **archive**: the retired dashboard |
| `CODE_REVIEW_2026_04_16.md`, `CODE_REVIEW_2026_09_28.md` | 3826652, 956bf0d | `ROADMAP.md:166,192`, `scanner/doc/BRIEF.md:55` | **archive**: the second review's plan became 076 |
| `LIVE_GLITCH_VALIDATION_PLAN.md` | f2de4d3 (2026-09-26) | `tools/README.md:54`, `tools/p25_bench_tx_replay.py:5` | **archive**: its goal was to keep the HDL chain in production |
| `P25_PS_PIPELINE.md`, `P25_PS_vs_SDRTRUNK.md` | 1f51717, 8272fcf (2026-04-19) | each other, old docs | **archive**: pinned to April p25-httpd trees |
| `P25_TUNING_REDESIGN.md` | ca3799e (2026-04-22) | change 046, a p25-httpd comment | **archive**: done as 046 |
| `VOCODER_PIPELINE.md` | fa246d3 | a p25-httpd comment | **archive**: p25-httpd's pipeline; the cost facts live in 079 and memory |
| `PROJECT_INVENTORY.md`, `PROJECT_TIMELINE.md` | 9df3706 (2026-06-10) | each other | **archive**: the April-June index and playbook, with dead references (`run_tests.bat`, old memory files) |
| `P25_API.md`, `API_CONSUMERS.md` | 141b1ad, 62561cc | `CLAUDE.md:26`, `bench/fbench/transport.py:230`, `ROADMAP.md:268`, DESIGN | **archive with p25-httpd.** If the "one API, all consumers equal" rule is wanted, one paragraph in DESIGN §11 carries it |
| `ADI_HDL_INTEGRATION.md` | 02f0230 (2026-04-15) | change 043 | **update**: it says 2R2T (lines 138, 250, 314, 381) where Fishball is 1R1T since 043; §7 lacks the P25 core; the `BUILD_FPGA.md` link is wrong |
| `HW_VALIDATION_SUITE.md` | c0a8fbb (2026-10-03) | CLAUDE.md, bench, hwval headers, tezuka_fw | **update** when the bench moves to the radio core: lines 128, 149, 521, 527 (p25-httpd, "p25f"); F15 (3a's bridge answers every access); F4 (dibit rings gone); F20 (re-check against the new carve-outs) |
| `PANEL_ADDON_BOARD.md` | 327a6b2 (2026-04-16) | nothing current | **keep** (the handheld idea is live); link it from ROADMAP's handheld rows; its software side (§9.2-9.4) is p25-httpd's |
| `hwval_register_map.md` | f2de4d3 | generated; a test keeps it current | **keep** |
| `ROADMAP.md` | abc6214 (2026-10-04) | CLAUDE.md, memory | **update**, section 5.2 |

### 5.2 ROADMAP.md

The "Proposed order" table:

| Row | Says | Now |
|-----|------|-----|
| 072b Encrypted calls | idea | open |
| 074b Packet data in history | next | parked (DESIGN D11) |
| 076 Restructure | design approved 2026-10-01 | **done**; left: p25-httpd out (section 3), the UI replacement |
| 079 General radio core | step 1 next | steps 1, 3a and 4 done; the next bakes are in 079's study |
| Remote libiio control, Agent control (MCP) | idea | open; their text says p25-httpd, `/api/endpoints`, `P25_API.md` (now the scanner, `/api/v1/routes`, `scanner/doc/API.md`) |
| Spectrum survey | idea | partly done: ATSC (080, 081), the live window's survey, ISM bands moved to data mode. Open: the 24-hour occupancy tool and captures |
| Data mode | study | current; it takes 082 or later when work starts |
| Real-time diagnostics | idea | open; its "Today" is p25-httpd's (`/api/spectrum_wide` polling, `/ws/iq` pre-diff tap, per-chain plots, the 066 CPU figures) |
| Handheld page | idea | open; point it at `PANEL_ADDON_BOARD.md` |

Sections to cut to one line each, because the work is done: 071, 072 (left open: per-speaker
time), "Code review and refactor", and "C4FM voice" (079 step 4 did it; vendor grants are still
open). "Boot and board configuration" is mostly p25-httpd's. Line 319 says "wideband IQ goes to
the SD card at up to 16 MSPS"; the scanner has no such route yet (079, open questions).

### 5.3 doc/diagnostics/

- **What:** dated session dumps (2026-04-17 to 2026-05-03) and fbench's run folders (2026-09-26 to
  10-01): 2,808 files, 948 MB.
- `.gitignore:89` ignores the folder, yet 248 files (24 MB) are tracked from before.
- **fbench writes there:** `bench/config/bench.toml:11` sets `diagnostics_dir = "doc/diagnostics"`,
  against CLAUDE.md (`runs/`) and memory `feedback_bench_cli`.
- Several change docs and the changelog cite untracked files in it (for example
  `2026-04-25/CHANNELIZER_REDESIGN.md`).
- **Recommended:**
  - **archive** the dated folders to `_archive/diagnostics_2026-04-17_to_2026-10-01/`;
  - `git rm --cached` the 248 tracked files in the same move (history keeps them);
  - point fbench's `diagnostics_dir` at `runs/bench`.

### 5.4 doc/changes and CHANGELOG_FORK.md

- **Next free change number: 082.** No file, no changelog entry and no commit message uses 082-089.
- **No change doc** for 060-063, 065, 067-074 (these have changelog entries), 076 (its record is
  `scanner/doc/DESIGN.md`) and 077 (never built). See CLAUDE.md line 84 above.
- **CHANGELOG_FORK.md** (2368044): current through 081 and the 2026-10-04 image. **Update:**
  - it has no entry for 075 itself (only 075a and 075b), nor for 040-044 and 046;
  - its "Pending / Future" section (line 4434) is long done;
  - line 4407 links `002_upstream_sync.md` without the file's date suffix.

## 6. scanner/doc/

| Doc | Last change | Recommendation and what is wrong |
|-----|-------------|----------------------------------|
| `DESIGN.md` | abc6214 (2026-10-04) | **update.** The header says "Phase 0 is next". §5 describes `RxInput::Dibits`, `InputKind` and the lane capability table, all gone from the code. §8 says schema v2 (now v4). §11 names `p25-json` and `/api/endpoints`. §13's fabric table is core 0.3.0's (91.6 % of slices, 172 DSPs); point it at 079. §15 says the scanner uses `p25-pac` (it uses `core-pac`); phase 8 is open and the table has no status column. The status log has no entry for the 079 cutover |
| `API.md` | c41c59e (2026-10-03) | **keep**: generated, and a test keeps it equal to the route table |
| `API_FIELDS.md` | caa07c8 (2026-10-03, before the cutover) | **update** by regenerating it on unit A: `tools/api_fields.py --host 192.168.120.50`, with a live site and a finished TV scan. It still shows `dibit_*`, `pll_q213`, `readback.control_lsm`, `core_version "0.3.0"`, and has no mode or ATSC routes |
| `API_INVENTORY.md` | 2869e8a (2026-10-02) | **update**: drop the profile and names routes (replaced by aliases, D15), line 114's maintenance-mode issue (fixed in 078) and the lane LSM readback (gone); add the mode and ATSC routes; re-rank the cleanup list |
| `BRIEF.md` | f5a501b (2026-10-01) | **archive**: 076's brief, fulfilled; its p25-httpd deploy steps and "don't change the gateware" are history. Update DESIGN line 4's pointer |
| `UI_BRIEF.md` | fc09a47 (2026-10-03) | **update**: mark aliases done; add a line on each mode's tabs (ATSC: Channels, Viewer; data: Scan, Devices, Captures) |
| `DATA_MODE.md` | abc6214 | **keep**. §3 and §11 follow the 079 study once Andy decides item 1 (DDC lanes stay) |
| `status.json` (untracked) | 2026-10-01 20:42 | **delete, or move to `runs/076/`; ask Andy.** Andy saved it by hand: two concatenated `/api/v1/status` answers from unit A, before and after its factory reset (the 076 session's transcript says so). Nothing generates it, and its `profile` and dibit fields are gone |

## 7. The repo root

| Item | Git | Last change | Referred to by | Recommendation |
|------|-----|-------------|----------------|----------------|
| `DEVLOG.md` | tracked, fork | 70206ec (2026-04-16) | `README.md:133`, old docs | **archive**: an April developer reference (the LSM-era chain, register map, sessions). Its git remotes section could move into CLAUDE.md's "Related". Fix the README link (an upstream file, fork-edited: Andy's call) |
| `DEVPLAN.md` | tracked, fork | 84c32d3 (2026-04-15) | `README.md:88`, old docs, a p25-httpd comment, `tools/p25_status_and_next_step.py` | **archive**: the original phased plan; ROADMAP replaced it |
| `BUILD_FPGA.md` | tracked, fork | f2de4d3 (2026-09-26) | `README.md:89`, old docs | **update** before the next bake. Wrong: a "P25 demod" in the PL; the dibit masters on HP1; timing failures "promoted" (an error since 079, with a utilization report); `p25-pac` regenerated (now `scanner/core-pac`); the `package/p25-httpd` image and `S60p25-httpd`; the device tree's `p25-dibit` and `p25-traffic` nodes. Lead with the pretty wrappers |
| `build_fpga.bat`, `build_hdl.bat`, `build_hdl.sh` | tracked, fork | d343685 (2026-10-03) | the pretty wrappers | **keep** (`build_hdl.sh:401`'s p25-pac comment goes with p25-httpd) |
| `build_fpga_p25_pretty.sh`, `build_fpga_hwval_pretty.sh`, `build_tezuka_p25_pretty.sh` | tracked, fork | to e020ec2 (2026-10-04) | CLAUDE.md, memory | **keep** |
| `sim_hdl.bat`, `sim_hdl.sh` | tracked, fork | 3b1d0bd (2026-04-07) | `BUILD_FPGA.md:67,307` | see section 4.2 |
| `clean.bat` | tracked, fork | 3b1d0bd | `DEVLOG.md` only | **delete** (nothing calls it; it knows nothing of the p25 or hwval artefacts), unless Andy uses it |
| `bake.log`, `tezuka_build.log` | ignored | each build | the wrappers, CLAUDE.md | **keep** (rewritten on every build) |
| `logs/` | ignored | 2026-10-01 | nothing | **delete**: one empty `p25_data_capture.jsonl` |
| `_validation/` | ignored | 2026-05-04 | `PROJECT_TIMELINE.md` | **archive**: 26 MB of p25-httpd-era poll logs (the older half is already in `_archive/2026-05-03_cleanup/validation_stale`) |
| `sourceme.first`, `readme-images/`, `README.md`, `CHANGELOG.md`, `CODE_OF_CONDUCT.md`, `CONTRIBUTING.md`, `DCO.txt` | upstream | — | — | **leave alone** |

## 8. tools/

65 scripts (64 Python, one PowerShell), two Java harnesses and a README. No tool opens a device,
UIO or sysfs directly. The stale ones depend on core 0.3.0 only through p25-httpd routes. The
April-June docs and change docs cite most of them; nothing current runs them.

**Keep (12):**

| Tool | Last change | Referred to by | Update |
|------|-------------|----------------|--------|
| `api_fields.py`, `api_fields_meanings.py` | ea10fba, 560b235 (2026-10-03/04) | `API_FIELDS.md`, memory | — |
| `atsc_check.py` | bb37023 (2026-10-04) | 080, 081, memory | — |
| `scanner_live_check.py` | f78ebc8 (2026-10-01) | `API_INVENTORY.md`, memory | It reads `control.input.dibit_resyncs` and `iq_dropped`, gone with the radio core (now `blocks`, `dropped`, `gaps`) |
| `build_progress.py`, `check_verilog_stale.ps1` | 3dad898, fbe62ca (April) | the build wrappers, `build_fpga.bat` | — |
| `sdrtrunk_dmr_reference.py` (+ `sdrtrunk_dmr_harness/`) | c67996e (2026-09-30) | CLAUDE.md, 075, the DMR tests' `DMR_CAPTURE_DIR` | Docstring: "as p25-httpd's /api/control_iq_dump writes them" is now `/api/v1/iq/control.wav`. `sdrtrunk_lsm_reference.py` imports it |
| `sdrtrunk_lsm_reference.py` (+ `sdrtrunk_lsm_harness/`), `p25_lsm_compare.py` | d1e7712 (2026-10-03) | 079, `lsm_tests.rs`, memory | — |
| `p25_ddc_filter_design.py` | 9004a2c (2026-09-27) | generates `scanner/src/hardware/presets/table.rs`, `p25ddc.py`, DATA_MODE | Its usage points `--emit-rs` at p25-httpd. Its emitted header describes the removed `LsmDecimator2 + LsmFir` chain. The 50 kSPS output is a constant, which data mode's wide lanes need as an option |
| `p25_corpus_index.py` | 0b9d564 (2026-09-27) | `rf.p25_corpus`, bench tests | — |
| `sdrtrunk_teardown_stats.py` | 8b81921 (2026-09-27) | 057, 058, bench analysis | `--p25-calls` and `--p25-log` read p25-httpd dumps: port them to `/api/v1/activity/calls` and `/api/v1/events`, or drop them |

**Delete (3, and a fixture):**

- `route_shapes.py`, with `scanner/tests/fixtures/routes/shapes.json`: a p25-httpd route contract
  that no test reads;
- `p25_ws_iq_rate.py`;
- `p25_sticky_lock_test.py`.

**Archive (50).** Move them in import groups: `p25_nid_fec.py` with `p25_decode_capture.py`,
`p25_lsm_demod.py` and `p25_nid_analyze.py`. Archive `p25_lsm_hdl_replay.py` with the two
`bench/tests_host/test_corpus.py` tests that load it.

| Group | Tools | Last change | Why |
|-------|-------|-------------|-----|
| Need p25-httpd or the 0.3.0 LSM gateware | `p25_lsm_hdl_replay`, `p25_baseline_analyze`, `p25_chain_compare` | 2026-06 to 09-27 | Read p25-httpd's crate or the archived `lsm_*` modules |
| p25-httpd routes the scanner does not serve | `live_baseline`, `compare_sim_vs_board`, `monitor_p25_decoder`, `p25_audit_capture`, `p25_bin_correlate`, `p25_bin_long_sweep`, `p25_bin_overlay`, `p25_call_gated_capture`, `p25_call_monitor`, `p25_capture_session`, `p25_chain_forensics_capture`, `p25_check`, `p25_constellation_capture`, `p25_constellation_capture_hdl`, `p25_decode_capture`, `p25_decode_imbe_capture`, `p25_forensics_pull`, `p25_grant_iq_capture`, `p25_log_export`, `p25_nid_analyze`, `p25_plots_local`, `p25_retune_monitor`, `p25_retune_probe`, `p25_settle_measure`, `p25_status_and_next_step`, `p25_symbol_diagnostics`, `p25_sync_sweep`, `p25_tdu_lc_forensics`, `p25_ws_eye_capture`, `poll_grants_persist`, `poll_log_persist`, `poll_recordings_persist`, `voice_capture` | 2026-04-09 to 10-01 | `/api/traffic`, `/api/log`, `/api/hdl_lsm`, `/ws/iq`, `/api/constellation`, the dibit dumps, `/api/traffic_bins` and others, all gone |
| Through the scanner's legacy routes, finished | `p25_imbe_test`, `p25_ws_audio_capture` | April-May | `bench/fbench/wsaudio.py` does the audio timing with every lane |
| Offline one-offs, finished | `unit_fixtures`, `p25_audio_stats`, `p25_iq_inspect`, `p25_dibit_diff`, `p25_lsm_demod`, `p25_nid_fec`, `replay_tdulc_validity`, `simulate_call_pipeline`, `sdrtrunk_timeline_analyze`, `p25_constellation_montage`, `p25_bench_tx_replay` | 2026-04 to 10-01 | Superseded: the scanner's LSM, its NID decoder and its replay tests; `rf.p25_corpus` with B as the transmitter |
| 3b's prototype filter | `polyphase_proto_design` | fa246d3 | Keep it with `polyphase_channelizer` if 3b reuses that (section 4.1); otherwise archive |

**`tools/README.md`** (c67996e): **update.** Rewrite it as a short catalogue of the kept tools.
Today it counts 54 scripts, defaults to `192.168.2.1`, and leaves out 12 tools.

## 9. bench/

- **The bench still reads core 0.3.0** (079's open item, not done):
  - `bench/agent/build.rs`'s bank table and SVD path;
  - `bench/share/p25_regs.json` and `bench/agent/maps/p25_regs.fallback.json`;
  - `fbench/regmaps.py` (`P25_SVD`, "p25f") and `fbench/units.py`;
  - nothing in `bench/` names `p25-lanes`, "rad1" or `core.svd`.
- **What that breaks on unit A:** the capture registers moved from 0xE0-0xE8 to 0xC0-0xC8. So
  `xport.p25_ring_prbs`, `xport.p25_ring_lap` and `iface.fpga_loopback` write past the last bank,
  and should fail rather than pass falsely (inferred, not run).
- **What still works:** unit detection (the UIO name and the identity offsets did not move), and
  every IIO and hwval test.
- **Unit B is still on 0.3.0,** so the port picks the map by `product_id`.

| Item | Last change | Recommendation |
|------|-------------|----------------|
| `agent/build.rs`, `agent/src/cmd/ring.rs`, `txlink.rs`, `rings.rs`, `regmap.rs` tests, both p25 map JSONs, `fbench/regmaps.py`, `units.py`, `tests_host/test_regmaps.py`, the `info_p25` fixtures | c0a8fbb (2026-10-03) and earlier | **update** to `scanner/core-pac/core.svd`, choosing the map by product id. Then `fbench setup agent` on both units: the agents there predate 078's maintenance-mode fix |
| `rf.p25_replay` | 408ed98 (2026-09-26) | **update**: port it to `/api/v1` (`receivers`, `PUT /api/v1/hold` with `lane`). It aborts on the scanner and sits in the `rf` suite, so `fbench run rf` hits it; take it out of the suite until then |
| `rf.p25_corpus` (CLAUDE.md's check) | 06aaf2c (2026-10-01) | **update.** Modes A and B work through the legacy routes; last run 2026-10-01 on core 0.3.0 (99.24-99.37 %), not yet on 1.0.0. Its per-item counters and `ldu` come back null. Mode C needs `/api/traffic` and `/api/monitor`: port it to the hold and aliases |
| `rf.cw_ppm` | 408ed98 | **update**: it cross-checks p25-httpd's `/mnt/jffs2/p25-ppm-cal.json`, still on A's flash. Read `GET /api/v1/radio/crystal` |
| `sys.boot_log` (`analysis/bootlog.py`) | f2de4d3 | **update**: its marker matches only `p25-httpd` |
| `bench/config/bench.toml` | 06aaf2c | **update**: A at `192.168.2.1` is the bench-link wiring, not its Ethernet address; `diagnostics_dir` (section 5.3) |
| `bench/README.md`, `bench/agent/README.md`, docstrings in `transport.py`, `services.py`, `p25_score.py`, `runner.py` | to d588630 (2026-10-03) | **update** the p25-httpd text |
| everything else in `bench/` | — | **keep** |

## 10. tezuka_fw

| Item | Recommendation |
|------|----------------|
| `package/scanner/scanner.mk:11,15-16` | **update**: drop the `p25-pac` rsync lines and fix the comment |
| `package/p25-httpd/`, `Config.in:28` | **delete** with p25-httpd (no defconfig selects it) |
| `build.sh:151-166`, `post-build-p25.sh:43` | **delete** with it (harmless guards) |
| `README.md:22,48,99,116`, `CLAUDE.md:26-27,64`, `build.bat:53`, `overlay_tezuka/etc/init.d/S91nfs-mount:19` | **update**: they name p25-httpd, `S60p25-httpd` or the old fishball-p25 repo as current |
| `S60scanner` and `S50p25-httpd-certificates` (`/mnt/jffs2/p25-httpd.crt`, `.key`) | **keep**: file names on the units; renaming them would orphan the units' certificates |
| untracked `2026-10-03-output_images.zip`, `build_stage3_verify.log` | **delete** (the images are in `output_images/` and the radio core backup is in `_archive`; inferred) |

## 11. MAIA_SDR/_archive and the folders beside it

`_archive` holds 1.7 GB. Only one batch has a README.

| Batch | Size | README | Recommendation |
|-------|------|--------|----------------|
| `2026-05-03_cleanup` | 925 MB | no | **update**: add a README (stale validation logs, voice captures, debug logs, bake logs, scratch, Vivado journals, from the 2026-05-03 cleanup) |
| `build_2026-10-03_p25-core-0.3.0` | 509 MB | yes | **keep** |
| `claude_memory_2026-10-01` | 1.1 MB | no | **update**: add a README (the two memory sets before the 2026-10-01 consolidation) |
| `unitA_sd_2026-09-30` | 298 MB | no | **update**: add a README (`sd_extras.tar`: unit A's bench files, one 120 s wideband capture, one forensics run; inferred) |

Beside it in `MAIA_SDR\`:

| Folder | Size | What | Recommendation |
|--------|------|------|----------------|
| `build_scripts/` | 124 KB | nine March build scripts, predecessors of the in-repo ones | **archive** |
| `work_docs/` | 216 KB | 14 planning docs, February to April | **archive** |
| `doc/` | 42 KB | a stray `diagnostics/2026-04-30/post_flash_silent_audio/` (a tool run from the wrong folder; inferred) | **archive** |

New batches for this cleanup would be `_archive/cleanup_2026-10-<day>/` with a README naming each
moved item, where it came from and the commit that removed it.

## 12. The old fishball-p25 folder

`C:\Users\Andy\Projects\fishball-p25`: 1.6 GB, last commit e309ab0 (2026-04-08).

- **Its work is safe:** no commit is unpushed. The repo was pushed on 2026-10-01 and
  `andylee77/fishball-p25` is archived on GitHub. Its status shows only a moved submodule pointer.
- **Nothing in maia-sdr refers to it,** apart from `DEVLOG.md`'s history. tezuka_fw's
  `CLAUDE.md:64` names the local path, and its `README.md` links the GitHub repo as the P25 project.
- **The old workspace's transcripts** are in
  `C:\Users\Andy\.claude\projects\c--Users-Andy-Projects-fishball-p25\` (224 MB). Its memory set is
  already in `_archive/claude_memory_2026-10-01/`.
- **Recommended: delete the folder** (Windows reported it in use on 2026-10-01; Andy deferred it).
  The transcripts folder too, if Andy does not want them.

## 13. Memory notes

| Note | Last updated | What no longer holds | Recommendation |
|------|--------------|----------------------|----------------|
| `MEMORY.md` | — | line 1: "old fishball-p25 folder cleanup deferred" | **update** with section 12 |
| `project_status.md` | 2026-10-01 | "Next: change 076"; the fishball-p25 push and GitHub archive listed as to do (both done 2026-10-01) | **update** |
| `feedback_dev_authorization.md` | 2026-09-28 | the note that the fishball-p25 push and archive were blocked and left to Andy | **update** (they are done) |
| `reference_gateware_pitfalls.md` | — | "taps must be peak-scaled ... use `rescale_to_peak()`": wrong. `p25_ddc_filter_design.py` designs unit-DC-gain taps ("No peak-rescaling") and `P25DDC` sets `macc_trunc` [14, 17, 17] for them; `rescale_to_peak` no longer exists. The timing bullet (0.3.0's spectrometer-enable path, 064's fixes) is replaced by 3a's common-edge fan-out (079's study) | **update** |
| `reference_radio_facts.md` | — | "8, 12 or 16 MSPS" (presets 2-16; ATSC uses 10 and 16); `hardware/ddc_rate.rs` (p25-httpd's; the scanner's is `hardware/presets`); the control LSM reset as recovery (the gateware LSM is gone); iio writes revert "when p25-httpd restarts" | **update** |
| `feedback_build_traps.md` | 2026-10-03 | "the p25-httpd package rsyncs the checkout"; `make p25-httpd-dirclean`, `output/build/p25-httpd-v0.1.0` | **update** to the scanner package (`make scanner-dirclean`; inferred from Buildroot's naming) |
| `project_076_restructure.md` | 2026-10-03 | "p25-httpd, `lsm/` and `sw_demod/` still in the repo (to remove; move `p25-pac` first)": the blocker is now the bench's map. The tools line names `live_baseline.py` and `route_shapes.py` | **update** when section 3 is done |
| `project_hw_validation_bench.md` | 2026-10-03 | "gateware built with `build_fpga_hwval_pretty.sh`": it has never been baked; the bench maps are still core 0.3.0's | **update** |
| `reference_sdrtrunk.md` | 2026-10-03 | the offline P25 decode is p25-httpd's ignored `software_decode` test; the scanner's are the LSM and C4FM WAV tests | **update** when p25-httpd goes |
| the rest | — | nothing found | **keep** |

## 14. Proposed batches

Each batch is one commit (or one per repo), with the CLAUDE.md checks passing after it.

1. **Push** maia-sdr and tezuka_fw with their tags (section 1). Andy's go.
2. **Texts that mislead today** (no moves):
   - CLAUDE.md (section 2);
   - `BUILD_FPGA.md`;
   - the memory notes (section 13);
   - the stale headers (`p25ddc.py`, `iq_packer.py`, the P25 block design and constraints, the
     spectrometer's history comment);
   - DESIGN's header, §5, §13 and §15;
   - ROADMAP's rows;
   - `API_INVENTORY.md`;
   - `UI_BRIEF.md`;
   - tezuka_fw's texts.
3. **Delete the dead-on-arrival files:**
   - `channel_mux`, `traffic_pipeline`, `c4fm_demod` and `symbol_timing`, with their tests;
   - `p25_core_0.2.0.svd`;
   - `rerun_with_strategy.tcl`;
   - the `fishball7020_iio` XSA;
   - the three tools and `shapes.json`;
   - `logs/`, the stray cocotb directory and `clean.bat`.
4. **The bench on the radio core** (section 9; 079's open item). This unblocks 5.
5. **p25-httpd out** (section 3), with tezuka_fw's package and rsync lines, and `P25_API.md` and
   `API_CONSUMERS.md` to the archive.
6. **The archive move:**
   - the LSM gateware, its tests and vectors;
   - the 50 tools;
   - the 16 docs (13 in section 5.1, `BRIEF.md`, `DEVLOG.md`, `DEVPLAN.md`);
   - `doc/diagnostics`' dated folders;
   - `_validation/`;
   - the `MAIA_SDR` neighbours;
   - with READMEs for the new batch and the three old ones.
7. **Regenerate `API_FIELDS.md`** on unit A (after 4-5; unit A is shared, so ListAgents first).
8. **Delete the old fishball-p25 folder** when Windows lets go of it.
