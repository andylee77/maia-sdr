# Project Timeline & Session Playbook

**For future-Claude (or anyone) picking up the Fishball P25 project.**
Stop re-deriving what's been tried; read this first.

This is a meta-doc: pointers + arc summary, not source-of-truth detail.
For per-change records see `doc/changes/NNN_*.md` and
`CHANGELOG_FORK.md`. For the working repo navigation see
[PROJECT_INVENTORY.md](PROJECT_INVENTORY.md).

---

## 1. Read order on session start

1. **`MEMORY.md`** in `~/.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/` — top entry is always "READ FIRST" for current arc.
2. **The current "READ FIRST" memory** it points to (e.g. `project_2026_05_03_session_pickup_three_tracks.md` as of this writing).
3. **The latest `doc/diagnostics/<date>/SESSION_*.md`** referenced from that memory.
4. This doc (`PROJECT_TIMELINE.md`) for context, gotchas, and tool/workflow.
5. [PROJECT_INVENTORY.md](PROJECT_INVENTORY.md) when you need to find a specific file.

If `MEMORY.md` says "the bug is X, the plan is Y, the next thing is
Z" — trust that. Don't re-derive from `git log` unless it conflicts
with what you observe live on the board.

---

## 2. Project arc (chronological)

Concise narrative of what's been built and what's been refuted. Each
arc has a date, a one-line outcome, and a pointer for depth. For every
shipped change, `CHANGELOG_FORK.md` has the canonical entry.

### 2.1. Maia SDR foundation (pre-fork)

Upstream `F5OEO/maia-sdr`. Provides AD9361 capture, IIO DMA, DDC,
register infrastructure, FFT spectrometer, IQ recorder. Reused by P25
verbatim where possible (`maia_hdl.ddc.DDC`, `dma.DmaStreamRingWrite`,
`register.RegisterCDC`).

### 2.2. P25 scaffolding & gateware (Phase 0–6E, doc/changes 001–019)

- **Build scripts + repo layout** (001–002)
- **FPGA gateware** (003–010): C4FM demod, symbol timing, dibit packer; first bake
- **LSM HDL port** (011–019): NID/BCH FEC, CORDIC PLL, demod top, dc_blocker, watchdog. Bake 019 is the first integrated LSM bitstream.

### 2.3. P25 control-channel throughput (Phase 6F, doc/changes 020–029)

TSBK parser, multi-block reassembly, IDEN_UP / RFSS_STATUS fix. By 6F
end the receiver pulls 20+ TSBKs/sec from the Clay County control
channel — confirmed RF is never the bottleneck.

### 2.4. Phase 6 closeout → Phase 7 traffic chain (changes 030–036)

LDU/IMBE extraction (035), JMBE vocoder integration (036). First audio
out the speaker. Phase 6D IQ-LSM later retired (039) in favor of the
new traffic chain.

### 2.5. Phase 8–10 — DDC fork, AGC gate (changes 037–045)

- **P25DDC fork v1+v2** (040–042): Maia DDC fork with peak-tap-at-131071 coefficient convention (NOT unit DC gain). 8 MHz `rf_bandwidth` works on v2 (better than 4 MHz on v2).
- **Bake 2 + AGC gate** (044–045): traffic IQ chain symmetry, dashboard batch endpoint, AGC noise-floor gate.

### 2.6. Tuning redesign + lifecycle refactor (Apr 22–25, changes 046–048)

- **Tuning redesign** (046): unified retune flow.
- **Traffic PLL/AGC seeding** (047): pre-existing seed inputs added to `LsmAgc` / `LsmPllUpdate`. (These were silently dropped during the 2026-05-03 dual-DDC pivot — restored in 050.)
- **Lifecycle refactor** (048): per-grant `call_track`, capture-time routing.

### 2.7. Channelizer redesign needed (Apr 25)

Single-chain limitation hit. `doc/diagnostics/2026-04-25/CHANNELIZER_REDESIGN.md`
documents the architectural pivot to a software channelizer over
`wideband_iq_dma`. **Stages 1–2 of the channelizer plan later dropped**
when operator pivoted to scanner-radio model (single-chain optimised).

### 2.8. PS perf, audio pacer, scanner pivot (Apr 26–29, change 049)

- **Vocoder NEON / get_unvoiced O(N²) DFT fix** — 98.2 % of audio budget. Tier A optimisation shipped.
- **Audio pacer**: drain-slot routing, WS-in-Worker + MessagePort. Eliminates main-thread starvation queueing WS frames.
- **Log verbose-gate**: Voice/Duid categories drop at push time.
- **Scanner pivot** (memory `project_2026_04_29_scanner_pivot.md`): operator dropped multi-traffic-channel goal. New target: 1 CC + 1 traffic, fast track/follow/scan.

### 2.9. Audit + same-freq-skip refuted (Apr 30)

`framer_arm` per-DUID counters localised HDL-vs-PS framer divergence.
Manual PPM `lo_shift=470` nailed PLL median 0 (was −1100). **Same-freq
nco_skip "keep chain" attempt FIELD-REFUTED** — PLL drifts to ~i16 max
during gap, slicer/timing/sync follow into degenerate state. Reverted
build `2026-04-30-revert-keep-chain`.

### 2.10. M2A → SW demod → dual-DDC pivot (May 2)

- **M2A old-chain delete** baked clean (WNS +0.193, WHS +0.017). PS-side cross-build broke at hundreds of call sites.
- **PS-side software P25 demod** built on new HDL `wideband_iq` DMA tap. **98.96 % bit-exact match with SDRTrunk on captured RF.** This is the reference oracle.
- **Dual-DDC pivot** (`fa246d3`): one DDC for control, one for traffic, retire old `wideband_iq_dma` chain that the M2A delete froze.
- **MultistageDdc code complete** (4/4 unit tests pass), not yet validated offline or live.

### 2.11. Seeding bake → seeds live (May 3, changes 050–051)

- **Bake `2026-05-03-seeding-bake`** flashed, infrastructure works (CDC fence, latch on reset) but **does not improve First-IMBE.**
- **PS iterated through 3 bandaids, all dead-end:** PLL-only seed; coast-no-reset; quality-coast-loose-gate.
- **`grants_history.jsonl` in `_validation/` is the captured corpus** for the next session's track-1 work (UPD-derived `air_duration_ms`).
- Currently committed but not flashed: `38442a7` (seeds-live + LoS + recording_saved + traffic spectrum re-wire).

### 2.12. Current state (2026-05-03)

- Branch: `fishball-p25`, 126 commits ahead of `origin/fishball-p25`.
- Last commit: `36e80ef` (PROJECT_INVENTORY.md).
- Pending bake-flash cycle: BUILD_TAG `2026-05-03-seeds-live`.
- Active 3-track plan in [`project_2026_05_03_session_pickup_three_tracks.md`](../../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/project_2026_05_03_session_pickup_three_tracks.md):
  1. Lifecycle UPD-air-duration fix (corpus: `_validation/grants_history.jsonl`).
  2. HDL dibit forensics co-capture + offline compare vs SW (98.96 % bit-exact SW vs 57 % HDL).
  3. WAV-into-HDL via AD9361 BIST loopback.

---

## 3. The bake-flash-validate workflow

**Who does what:**

| Actor | Responsibilities |
|---|---|
| **Andy (operator)** | Runs bakes (`build_fpga.bat --p25`); runs Tezuka firmware build; flashes Fishball; has hands on hardware; observes live audio quality |
| **Claude** | Edits HDL + PS code; runs host unit tests; drafts change docs; drives PS-side debugging from live logs and captures; never bakes; never flashes |

**Standard cycle:**

1. **Edit** Amaranth in `maia-hdl/p25_hdl/` or PS Rust in `p25-httpd/src/`.
2. **Unit-test HDL**: `pytest maia-hdl/test/` (long sweeps gated behind `MAIA_HDL_SLOW_TESTS=1`).
3. **Type-check PS**: `cd p25-httpd && cargo check --target armv7-unknown-linux-gnueabihf` ⚠️ **never trust host-x86 cargo check alone — see §6 gotchas.**
4. **Bump BUILD_TAG** in `p25-httpd/src/main.rs`.
5. **Write change record**: `doc/changes/NNN_<topic>.md` and append entry to `CHANGELOG_FORK.md`.
6. **Commit** (Andy's instruction; don't auto-commit).
7. **Andy bakes** with `build_fpga.bat --p25`. Wrapper handles Verilog regen, SVD, `svd2rust` PAC, Vivado synth/impl/bitgen, XSA export. **Don't run `build_hdl.bat` or `svd2rust` standalone** unless PS-side cross-build needs PAC before the bake closes.
8. **Andy runs Tezuka build** (Buildroot in Docker; mounts this repo at `/mnt/maia-sdr`).
9. **Andy flashes** Fishball.
10. **Validate**: `tools/p25_check.py` first to confirm `BUILD_TAG` on `/api/system` matches what we shipped. Then dashboard at `192.168.2.1:8080`. A/B against SDRTrunk if claiming a quality improvement.

**Key files in the cycle:**

- `BUILD_FPGA.md` — full bake guide
- `reference_full_bake_process.md` (memory) — 11-step checklist Amaranth → flashed
- `feedback_user_drives_bake.md` (memory) — bake split rules
- `feedback_bump_build_tag.md` (memory) — BUILD_TAG must change per ship

---

## 4. Tool decision tree

`tools/README.md` is the catalog. This is the "if I want to ___, run ___" guide. All scripts default to `192.168.2.1:8080`.

### "I'm picking up a session — is the board alive?"

```bash
python tools/p25_check.py
```

Returns: connectivity, `BUILD_TAG`, basic counters. **First thing every session.**

### "Show me what's happening live on the board"

- Dashboard: open `http://192.168.2.1:8080/` in a browser (PCM-paced audio, recent calls, plots, system info).
- One-pager from CLI: `python tools/p25_status_and_next_step.py`.

### "Catch a single call"

| Want | Tool |
|---|---|
| Voice grant + IMBE + WAV in one shot | `tools/voice_capture.py` |
| Wideband IQ during the next non-encrypted grant | `tools/p25_grant_iq_capture.py` |
| Constellation + eye snapshots ONLY during active calls | `tools/p25_call_gated_capture.py` |
| Full-session capture for SDRTrunk A/B | `tools/p25_capture_session.py` |

### "Long-running rolling capture"

| Want | Tool |
|---|---|
| Every newly-completed grant appended to a file | `tools/poll_grants_persist.py` |
| Every new event-log entry | `tools/poll_log_persist.py` |
| Every new WAV recording | `tools/poll_recordings_persist.py` |
| Live decoder vital signs | `tools/monitor_p25_decoder.py` |

⚠️ **These polls write to `_validation/` by default.** They can outlive a Claude session. `tasklist | grep python.exe` to find them; the process count alone isn't enough — check what they're writing.

### "Compare against SDRTrunk (the gold reference)"

SDRTrunk recordings live at `C:\Users\Andy\SDRTrunk\` — **NOT** the source repo (`reference_sdrtrunk_offline_test_recipe.md`).

| Want | Tool |
|---|---|
| Counts + cause-effect + flowchart for a SDRTrunk session | `tools/sdrtrunk_timeline_analyze.py` |
| Replay an SDRTrunk-style timeseries log offline | `tools/replay_tdulc_validity.py` |
| Offline call-pipeline simulator | `tools/simulate_call_pipeline.py` |
| Export Fishball event-log ring as SDRTrunk-style log | `tools/p25_log_export.py` |

### "Diagnose retune / settle / lock"

| Want | Tool |
|---|---|
| Continuous traffic-chain monitor at 5 Hz, lock stats per retune | `tools/p25_retune_monitor.py` |
| Trigger N retunes, sample state at fixed offsets | `tools/p25_retune_probe.py` |
| Time from retune to first TRF_HDU/TRF_LDU1 | `tools/p25_settle_measure.py` |
| Verify same-TG sticky-lock | `tools/p25_sticky_lock_test.py` |

### "Look at the symbol stream / NID / BCH"

| Want | Tool |
|---|---|
| Per-dibit symbol stats | `tools/p25_symbol_diagnostics.py` |
| BCH(63,16,11) reference (bit-exact with HDL + Rust) | `tools/p25_nid_fec.py` |
| Sweep `t_max` to find best NID operating point | `tools/p25_nid_analyze.py` |
| Sync threshold sweep | `tools/p25_sync_sweep.py` |
| TDU_LC vs over-corrected garbage | `tools/p25_tdu_lc_forensics.py` |
| SDRTrunk LSM demod chain reference (Python) | `tools/p25_lsm_demod.py` |

### "Look at the constellation / eye"

| Want | Tool |
|---|---|
| Poll `/api/constellation` at 1 Hz, save PNG + JSON | `tools/p25_constellation_capture.py` |
| HDL-side variant (Q1.13 scaling) | `tools/p25_constellation_capture_hdl.py` |
| 6-panel montage by cluster variance | `tools/p25_constellation_montage.py` |
| Eye plots from `/ws/iq?source=...` | `tools/p25_ws_eye_capture.py` |
| Local diag plots from HDL pre-diff post-PLL ring | `tools/p25_plots_local.py` |

### "WebSocket diagnostics"

| Want | Tool |
|---|---|
| Audio arrival timing characterisation | `tools/p25_ws_audio_capture.py` |
| `/ws/iq` byte rate per source | `tools/p25_ws_iq_rate.py` |

### "Filter / DDC design (offline, no board needed)"

```bash
python tools/p25_ddc_filter_design.py    # P25 3-stage DDC, multi-preset
python tools/polyphase_proto_design.py    # polyphase channelizer FIR
```

### "Channelizer / FFT bin analysis"

`p25_bin_correlate.py`, `p25_bin_long_sweep.py`, `p25_bin_overlay.py`.

### Per-run folder convention

When a tool generates artefacts, put them in a per-run folder under
`doc/diagnostics/<date>/<topic>/` with a `FINDINGS.md` self-contained
record colocated with the data. **Don't put helpers in `/tmp` or at
the repo root.** Memory: `feedback_diagnostic_run_dir_pattern.md`.

`doc/diagnostics/` is **gitignored**; treat it as evidence storage.
Promote anything load-bearing to `doc/changes/` or a memory file.

---

## 5. Refuted hypotheses (don't retry without new evidence)

| Hypothesis | Date refuted | Why |
|---|---|---|
| Same-freq `nco_skip` "keep chain" without LSM reset | 2026-04-30 | PLL drifts to ~i16 max during gap; slicer/timing/sync follow into degenerate state; framer can't find sync for 12s. Reverted. Fix would need a timing-recovery seed register OR full pipeline rewrite. |
| HDL-loop coast (no reset on retune) — PS bandaid | 2026-05-03 | Bug is HDL-loop-wide, not freq-specific. Bake didn't help. |
| PLL-only seed (without AGC + Gardner) | 2026-05-03 | Loop-wide bug. Other two seed inputs are also wired. |
| Quality-coast-loose-gate | 2026-05-03 | Gate condition still didn't change underlying decode rate. |
| Vocoder cos recurrence is the bottleneck | 2026-04-29 | NOT the bottleneck. `get_unvoiced` O(N²) DFT was 98.2% of budget. Fixed in `2026-04-29-rustfft-unvoiced`. |
| LDU1 LC `parse_ldu1_source` produces correct RIDs | 2026-04-25 | KNOWN BUG — produces garbage where SDRTrunk gets a valid value. Affects `actual_speaker` only. CC-source-authoritative shields the audio path. Deferred. |
| Stage 1 DDC filter is weak | 2026-04-15 | Mis-diagnosis. Real fix was at stage 3 (LsmDecimator2 /2 fold-back band). Resolved by P25DDC v2. |
| 8 MHz `rf_bandwidth` doesn't work | 2026-04-15 | Worked on P25DDC v2 (better than 4 MHz on v2). Pre-v2 history. |
| "Code that worked yesterday suddenly broke" | 2026-04-17 | Was actually no-software-AGC + fixed-integer slicer + antenna swap revealing a marginal margin. |
| Traffic-PPM application is correct | 2026-04-30 | Bug confirmed by operator (Claude initially dismissed). Operator-flagged signal-chain hypotheses get end-to-end code review now. |
| F-PLL acquisition needs 2-3 min wait | (older) | SUPERSEDED. 6G.1 cold-boot probe shows ~75-80% NID rate from t=0. Don't propagate the old advice. |

---

## 6. Common gotchas

### Build / cross-compile

- **`cargo check` on Windows is BLIND to `cfg(target_os="linux")` code.** Whole modules of `hardware/fpga.rs`, mod routing in `grant_follower`, much of `linux_main` are cfg-skipped on host. Always finish with `cargo check --target armv7-unknown-linux-gnueabihf` before committing PS code. (Memory: `feedback_cfg_linux_host_check_blindspot.md`, `feedback_check_cfg_linux_callsites.md`.)
- **`///` doc-comments on fn parameters**: Windows accepts as warning, Tezuka cross-compile rejects as hard error. Use `//` instead. (`feedback_no_doc_comments_on_fn_params.md`.)
- **Tezuka Rust toolchain is older stable**: avoid recently-stabilized features (e.g. `int_roundings`). Host cargo check won't catch this. (`feedback_tezuka_rust_toolchain.md`.)
- **`svd2rust` `BitReader` gotcha**: shrinking an SVD field to 1 bit breaks `.bits()` call sites. Cross-compile catches it but host x86_64 doesn't if cfg-gated. (`feedback_svd2rust_bitreader_gotcha.md`.)
- **`BUILD_TAG` must be bumped per ship**; on-target `/api/system` relies on it. (`feedback_bump_build_tag.md`.)

### Buildroot / firmware

- **Failed Buildroot leaves p25-httpd "installed"**; next build silently uses old binary. `make p25-httpd-dirclean` to force fresh recompile. (`feedback_buildroot_pkg_cache.md`.)
- **Buildroot staleness**: `find -newer` 1-second mtime resolution can miss new edits after a fast iteration. `touch -m` source files from host before re-running. (`feedback_buildroot_staleness_touch.md`.)
- **Vivado regen**: `build_fpga.bat` does NOT auto-regen `p25_core.v`; check mtime vs `p25_hdl/*.py` before debugging hardware. (`feedback_p25_verilog_regen.md`.)

### Diagnostics & validation

- **SDRTrunk is the gold reference**. HDL recordings are NOT ground truth. Always pull a SDRTrunk file when judging quality. (`feedback_sdrtrunk_is_the_reference.md`.)
- **SDRTrunk recordings location**: `C:\Users\Andy\SDRTrunk\` — NOT the source repo. (`reference_sdrtrunk_offline_test_recipe.md`.)
- **`doc/diagnostics/` is gitignored** — files there live on disk only. Don't treat them as "checked-in evidence."
- **`MEMORY.md` truncates above 24 KB.** Index entries must stay <150 chars. Promote detail into the linked memory bodies.
- **`lo_shift_hz` sign convention**: positive `lo_shift_hz` compensates for NEGATIVE crystal ppm. Wrong sign breaks control PLL; recover via `devmem 0x7c4600A0 32 | (1<<2)` (lsm_reset Wpulse). (`feedback_lo_shift_sign_convention.md`.)
- **Show every CC grant**: Recent Calls must be 1:1 with initial GRP_VCH_GRANT TSBKs. Don't filter 0-IMBE/0-HDU entries — those ARE the diagnostic. (`feedback_show_every_grant.md`.)

### Ops / process

- **Don't auto-commit**. Wait for explicit user instruction.
- **Don't add `Co-Authored-By` lines** to commit messages (user CLAUDE.md rule).
- **Trust operator signal-chain hypotheses**. When Andy flags PLL/AGC/slicer behavior as suspect, verify by reading code end-to-end, not by trusting a prior session's "it does X" assertion. Burned on 2026-04-30 traffic-PPM bug. (`feedback_operator_signal_chain_hypotheses.md`.)
- **No native `<audio>` element** in dashboard — use Web Audio API. (`feedback_no_native_audio_element.md`.)
- **Dashboard `innerHTML` kills audio**. Don't replace audio-host nodes. (`feedback_dashboard_innerhtml_kills_audio.md`.)
- **Cargo fix on Windows damages files**. Avoid `cargo fix`. (`feedback_cargo_fix_windows_damage.md`.)

---

## 7. Hardware quick reference

| Item | Value |
|---|---|
| Target board | Fishball Z7020 (Zynq-7020 + AD9361) |
| On-target HTTP | `192.168.2.1:8080` |
| Live AD9361 query (no SSH) | `iio_attr -u ip:192.168.2.1` |
| Test sites | Clay County NAC 8A1 @ 860.9625 MHz; Duval NAC 3BA @ 855.4875 MHz; both WACN BEE00 (`reference_p25_sites.md`) |
| AD9361 baseband | 8 MSPS (BBPLL: 1024/4 = 256 MHz ADC / 32 HB+FIR) |
| DDC output | 62.5 kSPS (control), 50 kSPS (post-2026-05-03 dual-DDC traffic) |
| Recordings on target | `/tmp/p25_recordings/` |
| Site overlay on target | `/mnt/jffs2/p25-sites/` |

---

## 8. Quick links

| Topic | File |
|---|---|
| **Repo navigation** | [PROJECT_INVENTORY.md](PROJECT_INVENTORY.md) |
| **Per-change records** | `doc/changes/NNN_*.md` |
| **Running ledger** | `CHANGELOG_FORK.md` (3000+ lines, scan don't read) |
| **Tool catalog** | [tools/README.md](../tools/README.md) |
| **API reference** | [P25_API.md](P25_API.md) |
| **Bake guide** | [BUILD_FPGA.md](../BUILD_FPGA.md) |
| **Memory index** | `~/.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/MEMORY.md` |
| **Roadmap** | [DEVPLAN.md](../DEVPLAN.md) |
| **Repo structure** | [DEVLOG.md](../DEVLOG.md) (despite the name, NOT a chronological log) |

---

## 9. When this doc is wrong

This doc captures the state at **2026-05-03**. Things go stale:

- A "READ FIRST" memory file gets superseded — update §1 pointer.
- A new arc lands — add a §2.x entry.
- A hypothesis gets re-confirmed — update §5.
- A new gotcha is paid for in build cycles — add to §6.
- A new tool lands in `tools/` — `tools/README.md` gets the entry; this doc only updates §4 if the decision tree changes.

**Don't let this doc become an immutable archive.** If it disagrees
with current code or current memory, current wins. Then fix the doc.
