# Project Inventory — Maia SDR + Fishball P25

Navigation index for the repo. Maintained as a living doc — when you
add a new top-level reference doc, change-record, or scripts directory,
add a one-liner here.

Snapshot date: **2026-05-03**, commit `38442a7` (`fishball-p25`).

---

## 1. Top-level entry points (repo root)

| File | Role |
|---|---|
| [README.md](../README.md) | Upstream maia-sdr README (don't modify) |
| [CHANGELOG.md](../CHANGELOG.md) | Upstream changelog (don't modify) |
| [CHANGELOG_FORK.md](../CHANGELOG_FORK.md) | **Our fork changelog — Maia + P25, primary** |
| [CONTRIBUTING.md](../CONTRIBUTING.md) | Upstream |
| [CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md) | Upstream |
| [BUILD_FPGA.md](../BUILD_FPGA.md) | P25 FPGA bake guide |
| [DEVPLAN.md](../DEVPLAN.md) | P25 dev roadmap |
| [DEVLOG.md](../DEVLOG.md) | Merged dev log (Maia + P25) |
| [CLAUDE.md](../CLAUDE.md) | Project instructions for Claude |

Subproject READMEs/CHANGELOGs: `maia-hdl/`, `maia-httpd/`,
`maia-httpd/maia-{json,pac}/`, `maia-wasm/`, `tools/`,
`p25-httpd/sites/`.

---

## 2. doc/ — architecture & reference docs

Single-source-of-truth design docs. If a topic has a doc here, link to
it from change records and session logs rather than re-explaining.

| File | Topic |
|---|---|
| [ADI_HDL_INTEGRATION.md](ADI_HDL_INTEGRATION.md) | adi-hdl submodule integration |
| [HDL_LAYOUT_AND_ROADMAP.md](HDL_LAYOUT_AND_ROADMAP.md) | HDL module layout |
| [P25_API.md](P25_API.md) | P25 HTTP API reference |
| [API_CONSUMERS.md](API_CONSUMERS.md) | API consumer list |
| [P25_ADDRESS_MAP.md](P25_ADDRESS_MAP.md) | FPGA register map |
| [P25_PS_PIPELINE.md](P25_PS_PIPELINE.md) | PS-side pipeline |
| [P25_PS_vs_SDRTRUNK.md](P25_PS_vs_SDRTRUNK.md) | PS-side vs SDRTrunk comparison |
| [P25_TUNING_REDESIGN.md](P25_TUNING_REDESIGN.md) | Tuning architecture |
| [VOCODER_PIPELINE.md](VOCODER_PIPELINE.md) | JMBE vocoder pipeline |
| [DASHBOARD_PLOTS.md](DASHBOARD_PLOTS.md) | Dashboard plots reference |
| [DASHBOARD_CLEANUP.md](DASHBOARD_CLEANUP.md) | UI debt inventory |
| [PANEL_ADDON_BOARD.md](PANEL_ADDON_BOARD.md) | Future hardware panel add-on |
| [CODE_REVIEW_2026_04_16.md](CODE_REVIEW_2026_04_16.md) | One-time review snapshot |

---

## 3. doc/changes/ — change ledger (001–051)

Numbered change records, one per significant feature or arc. Convention:
each significant change ships with a record here. Don't modify upstream
`CHANGELOG.md`; promote summaries to `CHANGELOG_FORK.md` instead.

| Range | Arc |
|---|---|
| 001–002 | Build scripts + upstream sync |
| 003–006 | P25 scaffolding, FPGA gateware, control decoder, web dashboard |
| 007–010 | Traffic channel, hardware bringup, observability |
| 011–019 | Phase 6E LSM HDL port (NID/BCH, CORDIC PLL, demod top, bake) |
| 020–028 | Phase 6F TSBK / IQ DMA / multi-block / pluto LO calibration |
| 029–032 | Phase 6F throughput → Phase 6 closeout |
| 033–036 | Phase 7 traffic scaffold + LDU/IMBE + vocoder + audio |
| 037–039 | Phase 8 HDL LSM review + runtime reset + Phase 6D retire |
| 040–045 | Phase 10 prep, P25DDC fork (v1+v2), 1R1T, dashboard batch, AGC gate |
| 046–049 | Tuning redesign, traffic PLL/AGC seeding, lifecycle refactor, scanner pivot |
| **050–051** | **2026-05-03 seeding bake + seeds live (current)** |

---

## 4. doc/diagnostics/ — session logs + findings

⚠️ **`doc/diagnostics/` is gitignored** (`.gitignore` line 87) — these
files live only on disk. They are evidence + scratch, not source of
truth. Rotate / archive periodically.

13 dated subdirs (2026-04-17 through 2026-05-03), ~146 MB total.

**Canonical session logs (READ FIRST per arc):**

| Date | Headline | File |
|---|---|---|
| 2026-04-24 | grant=call lifecycle close | `2026-04-24/SESSION_CLOSEOUT.md` |
| 2026-04-25 | CC-grant-centric refactor | `2026-04-25/SESSION_2026_04_25_CC_GRANT_CENTRIC.md` |
| 2026-04-26 | Lifecycle refactor + AGC/PLL seeding | `2026-04-26/SESSION_LOG_PART2.md` |
| 2026-04-29 | PCM pipeline + audio pacer + grant=call | `2026-04-29/SESSION_LOG_PCM_PIPELINE.md` |
| 2026-04-30 | Audit FINDINGS + same-freq-skip refuted | `2026-04-30/SESSION_LOG.md` |
| 2026-05-02 | M2A delete → SW demod → dual-DDC pivot | `2026-05-02/SESSION_LOG_DUAL_DDC.md` |
| 2026-05-03 | 3-track plan after seeding-bake dead-end | `2026-05-03/SESSION_PICKUP.md` |

**Key plans / closeouts that survive their dated folder:**

- `2026-04-25/CHANNELIZER_REDESIGN.md` — single-chain limitation analysis
- `2026-04-30/HDL_CHANNELIZER_PLAN.md` — module budget for channelizer (M1 done, M2 deferred)
- `2026-04-30/sdrtrunk_baseline/{timeline,findings}.md` — SDRTrunk baseline numbers our receiver should match

---

## 5. Sources of truth (cheatsheet)

| Question | Where to look |
|---|---|
| What's the current FPGA build tag? | `p25-httpd/src/main.rs` `BUILD_TAG` constant |
| What changed in build N? | `doc/changes/NNN_*.md` + `CHANGELOG_FORK.md` |
| How does feature X work? | `doc/<TOPIC>.md` (see §2); fall through to source |
| What did we try and refute? | Memory — `project_*_session_close.md` and `project_*_refuted.md` (in `~/.claude/projects/.../memory/`) |
| Which API endpoints exist? | `doc/P25_API.md` — kept in sync with `p25-httpd/src/httpd/api/` |
| What's an SDRTrunk-style log? | `reference_sdrtrunk_file_naming.md` (memory) + tools below |
| What's the field-deployed config? | `p25-httpd/sites/{clay,duval}.json` + `reference_p25_sites.md` (memory) |

---

## 6. Tests

### HDL Amaranth tests — `maia-hdl/test/` (38 files)

Maia foundation: `test_{cmult,cpwr,fft,fir,floating_point,mixer,mult2x,packer,pulse,register,spectrum_integrator}.py`.

P25 chain: `test_{c4fm_demod,symbol_timing,lsm_decimator,lsm_fir,lsm_diff_demod_slicer,lsm_gardner_ted,lsm_pll_rotate,lsm_pll_update,lsm_nid_bch_fec,lsm_sync_nid_extract,lsm_nid_pipeline,lsm_cordic_atan2,lsm_demod_loop,lsm_dc_blocker,lsm_demod,lsm_agc,lsm_timing_interp,p25ddc,signal_energy,polyphase_channelizer,per_target_ddc,iq_packer,dibit_packer}.py`.

Helpers: `amaranth_sim.py`, `common_edge.py`, `golden_vector_loader.py`.

Run via `maia-hdl/run_tests.bat` (or `pytest maia-hdl/test/`); long
sweeps gated behind `MAIA_HDL_SLOW_TESTS=1`.

### Rust inline tests — `p25-httpd/src/protocol/p25/` (11 modules)

`sdrtrunk_bits_test.rs`, `test_fixtures.rs`, `tsbk_tests.rs`,
`types_tests.rs`, `voice_frame_tests.rs`, `traffic_chain_tests.rs`,
`control_channel/tests.rs`, `fec/{bch_tests,tests,rs_p25_tests}.rs`,
`app/seed_snapshot.rs` tests.

Run via `cargo test` from `p25-httpd/`. For type-check only against
the target, use `cargo check --target armv7-unknown-linux-gnueabihf`
(see memory `feedback_cfg_linux_host_check_blindspot.md`).

### Tools / diagnostic scripts — `tools/` (47 Python + 1 PowerShell)

[tools/README.md](../tools/README.md) is the canonical catalog with
all scripts grouped by purpose (SDRTrunk reference, dibit/NID/symbol,
IQ/decode/capture, live monitoring, WebSocket capture, constellation,
channelizer/FFT, DDC design, retune/settle, other). Read it before
writing a new script — most diagnostic patterns already exist.

---

## 7. Memory directory

External to repo: `~/.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/`

129 files + `MEMORY.md` index. Buckets:

- `feedback_*` (32) — durable conventions and rules
- `project_*` (~70) — pickups, bugs, deferred TODOs, hypothesis records (refuted/confirmed)
- `reference_*` (20+) — ground-truth refs (P25 protocol, SDRTrunk semantics, target IPs, file naming)
- `user_*` (2) — operator profile

Always check `MEMORY.md` first; it lists the "READ FIRST" current-state
pickup at the top.

---

## 8. SDRTrunk-specific surfaces

SDRTrunk is the **gold-reference decoder** — every off-air capture
should be cross-validated against it. See memory
`feedback_sdrtrunk_is_the_reference.md`.

| Surface | What |
|---|---|
| Memory refs | `reference_sdrtrunk_paths.md`, `..._dual_path_audio.md`, `..._file_naming.md`, `..._call_model_and_opcode_coverage.md`, `..._baseline_2026_04_30.md`, `..._offline_test_recipe.md` |
| Repo doc | [P25_PS_vs_SDRTRUNK.md](P25_PS_vs_SDRTRUNK.md) |
| Site seeds (from `default.xml`) | [p25-httpd/sites/clay.json](../p25-httpd/sites/clay.json), [duval.json](../p25-httpd/sites/duval.json), [README.md](../p25-httpd/sites/README.md) |
| Code | [p25-httpd/src/protocol/p25/sdrtrunk_bits_test.rs](../p25-httpd/src/protocol/p25/sdrtrunk_bits_test.rs) (bit-exact compatibility test) |
| Tools that consume SDRTrunk artifacts | [`tools/sdrtrunk_timeline_analyze.py`](../tools/sdrtrunk_timeline_analyze.py), [`replay_tdulc_validity.py`](../tools/replay_tdulc_validity.py), [`simulate_call_pipeline.py`](../tools/simulate_call_pipeline.py), [`p25_log_export.py`](../tools/p25_log_export.py) |
| Recordings (external) | `C:\Users\Andy\SDRTrunk\` — NOT the source repo |

In `doc/diagnostics/` (gitignored): seven `log_all_sdrtrunk_style.log`
captures, `sdrtrunk_replay*.wav` artifacts, `sdrtrunk_logs/sdrtrunk_app.log`
copies. These are local-only.

---

## 9. Local archive

Cleanup-archived artifacts live OUTSIDE the repo at:

```
C:\Users\Andy\Projects\MAIA_SDR\_archive\2026-05-03_cleanup\
```

Subdirs: `root_scratch/` (37 throwaway probe JSON + WAV + cs16),
`bake_logs/` (8 `bake*.log` / `build_fpga_*.log` / `tezuka_*.log`),
`redundant_debug_logs_2026-04-19/` (5 redundant snapshots),
`vivado_backup_jou/` (6 vivado backup `.jou`/`.log` pairs),
`gain_sweep/`, `runs/`, `wb_frames_check/`, `ws_audio_capture/`,
`voice_capture_dated/` (2 dated April 15 captures).

Total ~55 MB. The `_validation/` directory (530 MB) was held back —
active polling scripts hold its log files open. Stop the polls and
move it manually. Once nothing in the archive is wanted, `rm -r` the
whole `_archive/2026-05-03_cleanup/` tree.

---

## 10. What's intentionally NOT here

- Generated build artifacts: `maia-hdl/projects/*/fishball*.{cache,gen,hw,ip_user_files,runs,srcs,xpr}/`, `maia-hdl/maia-sdr.svd`, `*.cs16`, Rust `target/`, Python `__pycache__/`
- Vivado active-session logs (`vivado.log`, `vivado.jou`, `timing_*.log`)
- Live diagnostic dumps in `doc/diagnostics/` (gitignored — see §4)
- Editor / harness state: `.claude/`, `.clinerules`, `.pytest_cache/`

These regenerate on demand; never commit them.
