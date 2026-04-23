# 2026-04-23 Session Closeout — Signal Analysis + State

**Build:** `2026-04-23-dt-carveout-ppm-guard-manual-override`
**Board uptime at capture:** 366 s
**Site:** Clay County NAC 0x8A1 / WACN BEE00 / SysID 0x8A0 (RFSS 1, site 1)

## Top-line: the HDL AGC loop is now closed

| Metric | Start of session | End of session | Δ |
|---|---|---|---|
| **`agc_product`** (AGC loop error) | **0.447** | **0.9999** | loop essentially perfect |
| **cluster_radius** (post-AGC mag) | 0.49 | **0.818** | +67% |
| **EVM** | 0.52 | **0.417** | −20% |
| **ambiguous_frac** | 0.24 | **0.103** | **−57%** |
| **control rail energy** | ~40% | **66.9%** | +26 pp |
| **Wideband FFT bins** | 4096 (1.95 kHz) | **16384 (488 Hz)** | 4× resolution |
| **Wideband noise floor display** | −42 dB (miscal) | **−100.3 dBm** (SDRTrunk-matched) | 58 dB cal fix |
| **Control SNR** | ~28 dB | **31.3 dB** | +3 dB |
| **PPM handling** | hardcoded −0.54 in init script | auto-cal + persist + sanity guard | runtime, per-board |

`agc_product ≈ 1.000` means the HDL AGC gain × measured input magnitude is exactly hitting the target magnitude. That's the signature of a properly-closed control loop. The previous 0.447 reading was the AGC NOT updating at all — the gate threshold (1024 Q1.15) was above actual signal level (~0.015 Q1.15), so the loop was stuck at whatever gain it had at boot.

## What landed — 14 commits, 2 repos, 1 HDL bake

### maia-sdr (fishball-p25 branch)

| Commit | Summary |
|---|---|
| `3dad898` | `tools/build_progress`: strip ANSI before matching |
| `377ef95` | PPM sanity guard + `PUT /api/ppm?lo_shift_hz=X` manual override |
| `821c235` | `build_progress.py`: Tezuka mode + wrapper script |
| `473c3d0` | Fix Tezuka build (`BitReader.bit()`) + FPGA pretty wrapper |
| `bd4fe85` | **FPGA bake artifact** (XSA) — WNS +0.303 ns / WHS +0.012 ns |
| `06368c9` | **HDL bake**: AGC gate threshold runtime-tunable + FFT 12→14 (16k bins) |
| `13a3dcf` | Wideband dB calibration 96 → 148 + expose HDL AGC debug taps |
| `adab58f` | auto-PPM: 8-frame stage-A avg + 15 min fine-tune task + NTP guard |
| `124e6fa` | Dashboard PPM-cal status row + Recalibrate button |
| `780b82c` | auto-PPM persist (`/mnt/jffs2/p25-ppm-cal.json`) + tuner card fixes + remove hardcoded PPM |
| `74c65df` | auto-PPM calibration: wideband peak-find stage A + PLL residual stage B |

### tezuka_fw (fishball-dev branch)

| Commit | Summary |
|---|---|
| `3ae2760` | DT carveout 128K→256K + buffer-size 32K→128K for FFT14 |
| `d13f05e` | `S60p25-httpd`: drop hardcoded `--lo-ppm -0.54` → `--lo-ppm 0` |

## Current system state (captured at uptime 366 s)

### Front-end
- `rx_lo=858099998`, `radio_freq=860962500` (control)
- `sample_rate=8 MSPS`, `rf_bw=8 MHz`
- `rx_gain=60 dB` manual, `rx_rssi=95.75 dB`
- `ddc_control_offset=2862908 Hz`

### PPM (the auto-cal is working)
- `lo_shift_hz=406`, `lo_ppm=−0.4731`
- `boot_lo_ppm=0.0` (from init script `--lo-ppm 0`)
- `calibrated_this_session=true` (auto-PPM ran after lock)
- Boot sanity guard rejected the stale +1.37 ppm from a previous broken write; auto-PPM then wrote the current good value.

### HDL LSM AGC — **the main signal-chain fix**
```
threshold:  256 (Q1.15 raw) = 0.0078 = −42.1 dBFS
agc_gain:   33.40×
agc_mag:    0.0299 (Q1.15 raw = 981)
agc_product: 0.9999   [target 1.0]  ← loop closed
pll_dbg:    −855 Q2.13 (−80 Hz residual)
```

### Wideband FFT (16k bins, properly calibrated)
- 16384 bins, 488.3 Hz/bin, span 8 MHz centered 858.1 MHz
- Noise floor: **−100.3 dBm** (matches expected SDRTrunk reading)
- Peak SNR: **31.3 dB**
- Control channel at 860.9628 MHz, −70.5 dBm, exactly where expected
- Strong real emission at 857.988 MHz (nearby traffic channel, not an artifact)

### Voice-chain activity (uptime 366 s)
- `grants_seen=388` (encrypted rejected: 116)
- `HDU/LDU/TDU = 12/150/377`
- `traffic_sync hits=509 near=10484` (still 20:1 ratio — see Open Issues)
- `IMBE ext/drop = 1341/36` (2.7% drops — reasonable)
- **silent_frames = 218 / 1341 = 16.3%** (see Open Issues)
- `vocoder_errors = 0`

## Open issues carried into the next session

### 1. Recording audio wraps across calls (bug reported 2026-04-23)

Operator report: "last frame of prior conversation ends up at start of next recording."

**Likely cause:** recorder's call-end detection fires on a boundary, but the vocoder's output buffer still has the previous call's final PCM samples in-flight. On the next `HDU`, the recorder starts a new WAV while that residual PCM is still being flushed — so the tail of call N appears at the head of call N+1.

**Where to look:** `p25-httpd/src/audio/recorder.rs` — the VOCODER_TAIL_WINDOW + FINALIZE_GRACE logic. See memories `project_recordings_span_two_sources.md` and `project_recorder_call_fragmentation.md` for prior analysis of adjacent bugs.

**Fix angle:** force the vocoder pipeline to drain completely into the old WAV before opening the new one. Either: hold new-recording start for 1-2 JMBE frame periods after last-IMBE, OR track the vocoder's in-flight frame count atomically and don't open next recording until it reaches zero.

### 2. Dashboard Plots-tab picker bounces between views (bug reported 2026-04-23)

Operator report: switching from Wideband to Constellation kept showing wideband → constellation → wideband → constellation.

**Root cause:** [dashboard.html:4042-4054](../../../p25-httpd/src/httpd/dashboard.html) — `plotSpectrumWide()` uses `setInterval(tick, 200)` for polling. When the picker changes, `plotStop()` clears the interval, but an `await fetchJson('/api/spectrum_wide')` already in-flight completes AFTER `plotStop` returned, and its callback runs `drawPlotSpectrum(...)` on top of the new plot. The new plot's renderer paints next, causing the flip-flop.

**Fix angle:** add a picker-generation counter. Before every `drawPlotSpectrum` / `drawPlotConstellation`, compare the captured generation to the current `PLOT.picker` + counter; if stale, drop the draw. Two-line change.

### 3. Wideband plot rendering is slow (reported 2026-04-23)

Operator report: "the wideband plot is very slow."

**Root cause:** we're polling `/api/spectrum_wide` every 200 ms = 5 Hz. Payload size went **4× up** at FFT14:

| FFT order | Bins | Typical JSON size | @ 5 Hz poll |
|---|---|---|---|
| 12 (old) | 4096 | ~60 KB | 300 KB/s |
| **14 (now)** | **16384** | **~320 KB** | **1.6 MB/s** |

Zynq's 650 Mbps Ethernet handles the bandwidth, but JSON encoding + parsing at 5 Hz burns CPU and makes the browser sluggish.

**Fix angle:** (a) drop wideband poll to 2 Hz (500 ms interval) — simple, already plenty for visual, cuts 60 %; OR (b) serve binary payload over `/ws/spectrum_wide` (Int16 mag_db) instead of JSON floats, ~75% size reduction; (c) both.

### 4. Silent vocoder frames remain at ~16 %

This is *not* related to AGC or PPM — unchanged after this session's fixes. Root cause is further upstream: JMBE's soft-decision decode marks frames silent when the IMBE parameters look implausible. Investigating requires instrumenting the per-frame IMBE parameter envelope — separate thread, not a quick fix.

### 5. Traffic sync near-miss ratio 20:1

`sync_hits=509 / sync_near_misses=10484`. Marginal improvement from start of session (was 45:1 on the old gain config; now 20:1 post-AGC-fix). Still high. Candidate causes:
- Traffic chain PLL not re-locking quickly on grant retunes
- Sync-threshold too aggressive
- Real RF conditions on the traffic frequency

Not blocking audio decode (we're producing 1341 IMBE frames with zero vocoder errors), but worth investigating if we want more call-start reliability.

### 6. `agc_product` tuning opportunity

Currently at 0.9999 at **threshold=256**. Could try `PUT /api/agc_threshold?chain=both&value=150` (between noise ~144 and signal ~981) to see if the loop tracks more aggressively or produces tighter cluster. Safe to experiment live — runtime-tunable now.

## Tools added this session

- `tools/build_progress.py` — live-tail Vivado / Tezuka logs, emit one line per milestone, auto-mode detection
- `build_fpga_p25_pretty.sh` — pretty wrapper for FPGA bake
- `build_tezuka_p25_pretty.sh` — pretty wrapper for Tezuka build
- `tools/p25_plots_local.py` — offline constellation/eye/diff rendering from HDL ring capture
- `tools/p25_constellation_capture_hdl.py` — HDL ring capture for offline analysis
- `tools/p25_symbol_diagnostics.py` — per-symbol decision quality analysis

## API additions this session

| Endpoint | Purpose |
|---|---|
| `GET /api/agc_threshold` | read both chain thresholds |
| `PUT /api/agc_threshold?chain=control\|traffic\|both&value=<u16>` | live-tune gate |
| `GET /api/ppm` | current lo_shift_hz + ppm + cal timestamp |
| `POST /api/ppm_calibrate` | one-shot stage A + B auto-PPM |
| `PUT /api/ppm?lo_shift_hz=<i64>` | manual override (−1000..+1000 Hz) |

## Entry point for the next session

Read this doc + memory `project_2026_04_23_session_close` (to be created). The state going in is:

- AGC loop is closed; signal quality metrics look solid
- Audio still has the cross-call-wrap bug (item 1) — this is the biggest operator-visible issue
- Dashboard has two plot-tab UX bugs (items 2+3) — easy fixes
- Silent-frame rate + sync near-miss are investigation-level items, not quick fixes
