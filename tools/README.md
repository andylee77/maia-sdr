# tools/ — script catalog

46 Python scripts for live monitoring, offline replay, log analysis,
HDL/PS reference decoding, and design work. Most target the on-target
Fishball P25 daemon at `192.168.2.1:8080` by default; offline tools
take file paths.

## SDRTrunk reference / log analysis

| Script | Purpose |
|---|---|
| [`sdrtrunk_timeline_analyze.py`](sdrtrunk_timeline_analyze.py) | Build a counts + cause-effect + flowchart `.md` for an SDRTrunk session. Reads CC `decoded_messages.log` + per-traffic-channel logs, narrows to the call window, tabulates TSBK opcode counts, maps each CC opcode to its traffic-side effect. Used to produce [doc/diagnostics/2026-04-30/sdrtrunk_baseline/timeline.md](../doc/diagnostics/2026-04-30/sdrtrunk_baseline/timeline.md). |
| [`replay_tdulc_validity.py`](replay_tdulc_validity.py) | Replay an SDRTrunk-style timeseries log and evaluate several TDULC validity policies offline (no flash cycle needed). |
| [`simulate_call_pipeline.py`](simulate_call_pipeline.py) | Offline call-pipeline simulator. Replay an SDRTrunk-style timeseries log through a Python port of the call-tracker / grant-follower lifecycle. |
| [`p25_log_export.py`](p25_log_export.py) | Export the Fishball event-log ring to local files for SDRTrunk-style offline analysis. |

Usage example (the SDRTrunk timeline):

```
python tools/sdrtrunk_timeline_analyze.py \
  --cc 'C:/Users/Andy/SDRTrunk/event_logs/<CC>_LCN-11_decoded_messages.log' \
  --traffic label1=857.9875=<path1> label2=858.4625=<path2> ... \
  --out doc/diagnostics/<date>/sdrtrunk_baseline/timeline.md
```

## Dibit / NID / symbol diagnostics

| Script | Purpose |
|---|---|
| [`p25_nid_analyze.py`](p25_nid_analyze.py) | NID batch-capture sweep tool — pulls the on-target capture ring, runs BCH offline at multiple `t_max` values to find the best operating point. |
| [`p25_nid_fec.py`](p25_nid_fec.py) | Pure-Python BCH(63,16,11) reference encoder + decoder. Bit-exact with `p25-httpd/src/protocol/p25/fec/bch.rs` and `maia-hdl/p25_hdl/lsm_nid_bch_fec.py`. |
| [`p25_sync_sweep.py`](p25_sync_sweep.py) | Phase 6F.7+ sync-threshold sweep tool. Hits `/api/sync_tune` across a range of thresholds and reports the histogram + true-sync vs false-sync split. |
| [`p25_symbol_diagnostics.py`](p25_symbol_diagnostics.py) | Per-dibit symbol stream stats — `dibit = (sign(I), sign(Q))` → P25 symbol value {+1, +3, −3, −1}. |
| [`p25_lsm_demod.py`](p25_lsm_demod.py) | Pure-Python reference port of SDRTrunk's P25 LSM demod chain. |
| [`p25_tdu_lc_forensics.py`](p25_tdu_lc_forensics.py) | TDU_LC forensic analyzer — distinguish real-but-corrupted TDU_LCs from BCH-over-corrected garbage. Useful when triaging the TDU_LC inflation problem. |

## IQ / decode / capture

| Script | Purpose |
|---|---|
| [`p25_capture_session.py`](p25_capture_session.py) | Full-session Fishball IQ capture for SDRTrunk cross-validation. Records baseband during a call window. |
| [`p25_audit_capture.py`](p25_audit_capture.py) | Pull all inputs needed for an audit-style diff against a SDRTrunk reference (mirrors `doc/diagnostics/2026-04-30/audit/audit.py` pattern). |
| [`p25_call_gated_capture.py`](p25_call_gated_capture.py) | Capture traffic-chain constellation + eye snapshots ONLY during active calls (skip idle gaps). |
| [`p25_decode_capture.py`](p25_decode_capture.py) | Replay an aligned capture from on-target through the same TSBK decode pipeline in pure Python with verbose intermediate state. Use when CRC fails on every block and you need to localise (deinterleave/trellis/CRC). |
| [`p25_decode_imbe_capture.py`](p25_decode_imbe_capture.py) | Decode captured IMBE frames via the Fishball vocoder and save as WAV. |
| [`p25_imbe_test.py`](p25_imbe_test.py) | Capture raw IMBE frames from Fishball and analyze them. |
| [`voice_capture.py`](voice_capture.py) | One-shot voice-grant catcher — waits for an unencrypted voice grant, captures pre/during/post traffic-side metrics + IMBE ring + WAV, writes timestamped dir. |
| [`p25_iq_inspect.py`](p25_iq_inspect.py) | Sanity-check a `.cs16` capture from `/api/wideband_iq_capture` (8 MSPS pre-DDC IQ). Reports DC offset, I/Q balance, per-second power, top-N FFT peaks. Run after capturing to verify the wideband DMA tap delivers clean baseband. |
| [`p25_grant_iq_capture.py`](p25_grant_iq_capture.py) | Watch `/api/grants`, snap a fixed-duration wideband IQ capture the instant the next non-encrypted grant fires. Logs the LCN/TG and prints a ready-to-paste `cargo test software_decode` recipe so the offline decoder targets the right frequency. |
| [`p25_bench_tx_replay.py`](p25_bench_tx_replay.py) | Bench rig TX: replay a wideband `.wav`/`.cs16` capture through a second PlutoSDR (cabled + attenuated, NEVER an antenna) so the Fishball sees identical, ground-truth-known site RF every run. Parses SDRTrunk-style filenames for freq/rate; `--cyclic` for gap-free device-side looping. See `doc/LIVE_GLITCH_VALIDATION_PLAN.md` "Bench rig". Requires pyadi-iio. |

## Live monitoring (long-running pollers)

| Script | Purpose |
|---|---|
| [`monitor_p25_decoder.py`](monitor_p25_decoder.py) | General-purpose p25-httpd diagnostic data collector over time. |
| [`p25_call_monitor.py`](p25_call_monitor.py) | Fast poll loop for catching short P25 calls (sub-second granularity). |
| [`p25_status_and_next_step.py`](p25_status_and_next_step.py) | Comprehensive on-target status + roadmap snapshot — pulls `/api/system`, `/api/traffic`, `/api/chain` and produces a one-pager. |
| [`p25_check.py`](p25_check.py) | Lightweight on-target verification — connectivity, build_tag, basic counters. Use FIRST when picking up a stale session. |
| [`poll_grants_persist.py`](poll_grants_persist.py) | Append every newly-completed grant from `/api/grant_decode_stats` to a rolling local file. |
| [`poll_log_persist.py`](poll_log_persist.py) | Append every new entry from `/api/log` incrementally. |
| [`poll_recordings_persist.py`](poll_recordings_persist.py) | Download every new WAV from `/api/recordings` to local disk so the on-target ring doesn't roll. |

## WebSocket capture (live IQ / audio / eye)

| Script | Purpose |
|---|---|
| [`p25_ws_audio_capture.py`](p25_ws_audio_capture.py) | Capture `/ws/audio` and characterise arrival timing (drain spikes, gap distribution). Made during the audio-pacer arc. |
| [`p25_ws_eye_capture.py`](p25_ws_eye_capture.py) | Capture raw `/ws/iq?source=post_ddc\|post_lsm` and render offline eye plots. |
| [`p25_ws_iq_rate.py`](p25_ws_iq_rate.py) | Measure actual `/ws/iq` byte rate per source — diagnoses backpressure / starvation on the WebSocket path. |

## Constellation / IQ visualisation

| Script | Purpose |
|---|---|
| [`p25_constellation_capture.py`](p25_constellation_capture.py) | Poll `/api/constellation` at ~1 Hz, save JSON + PNG per snapshot, tag with timestamp + pll + timing. Writes `summary.jsonl` so post-analysis can pick interesting frames without re-parsing PNGs. |
| [`p25_constellation_capture_hdl.py`](p25_constellation_capture_hdl.py) | HDL-side variant — matches the firmware Q1.13 scaling for the rotate output. |
| [`p25_constellation_montage.py`](p25_constellation_montage.py) | Build a 6-panel montage of constellation snapshots ordered by cluster variance. |
| [`p25_plots_local.py`](p25_plots_local.py) | Local diagnostic plots from the HDL pre-diff post-PLL ring — streams `/ws/iq?source=pre_diff` and renders constellation / eye / phase plots. |

## Channelizer / FFT bin analysis

| Script | Purpose |
|---|---|
| [`p25_bin_correlate.py`](p25_bin_correlate.py) | Time-series cross-correlation between per-LCN wideband power and grant activity — used during the polyphase-channelizer M1 work. |
| [`p25_bin_long_sweep.py`](p25_bin_long_sweep.py) | Long-running bin↔LCN correlator. Polls `/api/traffic_bins` and accumulates correlation matrix. |
| [`p25_bin_overlay.py`](p25_bin_overlay.py) | Capture wideband spectrum + channelizer bin energies and plot them overlaid. |
| [`polyphase_proto_design.py`](polyphase_proto_design.py) | Prototype lowpass FIR design for the P25 polyphase channelizer (M2 work). |

## Filter / DDC design (offline)

| Script | Purpose |
|---|---|
| [`p25_ddc_filter_design.py`](p25_ddc_filter_design.py) | P25DDC filter design, multi-preset sweep across AD9361 ADC rates. Designs the 3-stage DDC for control + traffic chains, every preset producing 50 kSPS DDC output (62.5 kSPS before the 2026-05-03 retune). |

## Retune / settle / lock diagnostics

| Script | Purpose |
|---|---|
| [`p25_retune_monitor.py`](p25_retune_monitor.py) | Continuous traffic-chain monitor at 5 Hz. Detects retune events (TG/freq change), reports per-retune lock stats: tune latency, first-LDU latency, clean-LDU/TDU/phantom-TDU_LC counts, verdict (LOCKED / MARGINAL / NEVER). CSV per retune. |
| [`p25_retune_probe.py`](p25_retune_probe.py) | Trigger N retunes to a known voice frequency and sample constellation/dibit/traffic state at fixed offsets post-retune. Diagnoses the grant-to-lock gap + never-lock rate. |
| [`p25_settle_measure.py`](p25_settle_measure.py) | Channelizer-redesign Stage 0: how fast does the chain produce real audio after a retune (with PLL+AGC seeding)? Anchors on `/api/log` retune/nco_skip events, times to next TRF_HDU/TRF_LDU1. |
| [`p25_sticky_lock_test.py`](p25_sticky_lock_test.py) | Phase 7A.1 sticky-lock verification — checks that `retunes` stays flat while locked on the same TG (pre-fix thrashed >15/sec). |

## Chain forensics (HDL vs SW)

Track 2 of the 2026-05-03 three-track plan. The HDL traffic chain
decodes ~57 % of LDUs on the same wideband IQ where the SW reference
hits 98.96 % bit-exact agreement with SDRTrunk. These two scripts
co-capture HDL chain state alongside the wideband input, then run the
SW reference offline against the same input and slide-align the two
dibit streams to pinpoint where the chains diverge.

| Script | Purpose |
|---|---|
| [`p25_forensics_pull.py`](p25_forensics_pull.py) | **(preferred)** Companion to the on-device forensics ring (build `2026-05-03-on-device-forensics+`). Arms `/api/forensics_arm`, polls `/api/forensics_status`, scp's each new run dir + matching wideband.cs16 down. No host-side polling = no dibit loss. Supports `--follow-encrypted` to capture encrypted calls for diff testing (audio garbled but dibits intact). |
| [`p25_chain_forensics_capture.py`](p25_chain_forensics_capture.py) | Pre-on-device-forensics fallback: host-side polling of `/api/traffic_dibit_capture`. Lossy under HTTP latency spikes. Use only if running an older build without on-device forensics. |
| [`p25_chain_compare.py`](p25_chain_compare.py) | Pair tool. Given a forensics run dir, runs `cargo test --release software_decode` against the captured wideband IQ, then slide-aligns the SW dibit stream against the HDL dibit stream and emits a per-window agreement report + CSV. Locates the symbol position where HDL diverges from SW. Use `--multistage` (Kaiser cascade) or pair with `SOFTDEC_HALFBAND=1` (SDRTrunk-faithful halfband cascade port — bit-exact w/ SDRTrunk on validated 2026-05-03 wideband). Optional `--ref-bits` to also diff against an SDRTrunk reference. |

## Other diagnostics

| Script | Purpose |
|---|---|
| [`p25_audio_stats.py`](p25_audio_stats.py) | Compute the same audio-quality metrics used in the 2026-04-17 perf analysis. |
| [`compare_sim_vs_board.py`](compare_sim_vs_board.py) | Side-by-side diff: simulator-predicted recordings vs board's actual recordings. |
| [`build_progress.py`](build_progress.py) | Live-tail Vivado build logs and emit clean phase-by-phase progress (synth / opt / place / route / bitgen). |

---

## Bench suite

Hardware validation (two cabled Fishballs, on-board agent, JSON CLI): see
[bench/README.md](../bench/README.md) and `python bench/fbench.py list`.

---

## Common host config

- Default target: `192.168.2.1:8080` (Fishball Z7020). Most scripts
  accept it as a positional arg or via env.
- Scripts that hit the API expect a build with `BUILD_TAG` matching
  the relevant feature; check `/api/system` first via `p25_check.py`.
- Linux-only flags (cfg(linux)) won't show up on Windows host
  cargo-checks — see memory `feedback_check_cfg_linux_callsites.md`.

## Adding a new script

Convention check before committing:

1. Add a one-line module docstring on line 1 (used by this README's
   auto-extract).
2. If the script takes a host arg, default it to `192.168.2.1:8080`.
3. If long-running, write to a timestamped subdir of `cwd`, not to
   the repo.
4. Update this README's category table.
