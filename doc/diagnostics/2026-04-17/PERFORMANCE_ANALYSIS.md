# Fishball P25 — Build Performance Analysis (2026-04-17)

> ## CORRECTION 2026-04-17 — sp_dbg interpretation was wrong
>
> The Gardner TED analysis in §1.4, the "X-pattern ↔ timing-loop-gain" diagnosis in §3.2–3.5, and the "halve TED_GAIN" recommendation in §6.2-4 / §6.5-12 are **invalidated**. See §A "Corrections" appended below this document for the full write-up.
>
> **tl;dr:** `sp_dbg` is Q4.12 oscillating by design through `sps = 6.5104 ≈ 26,667 Q12 units` per symbol ([lsm_timing_interp.py:96-110](../../../maia-hdl/p25_hdl/lsm_timing_interp.py)). The observed ~6,600-unit span is ~25% of one symbol period — normal operation, not "50% of full scale hunting." The "full scale ~13,000 at sps=13" assumption was numerically wrong (sps is 6.5104, not 13). `TED_GAIN = SPS/4.0 = 1.628` is also "verbatim from `demod_lsm_with_state`" and reference-tested to within 6 Q4.12 ULPs of the float Rust reference, which itself is SDRTrunk-faithful. Do not change it on this evidence.
>
> The cluster-variance 14× span in 90 s, the TSBK3-vs-TSBK2 9.5 pp CRC spread, the `TDU_LC = 1977` anomaly, and the `vocoder_errors = 0` + 40 %-silent audio are all still real observations — just not attributable to the Gardner TED loop-gain.
>
> What remains valid: everything factual (counters, timings, audio stats, constellation PNG captures). What needs re-interpretation: the *attribution* of the X-pattern to timing-loop-gain. See §A.3 for the candidate list.

## Document metadata

| Field | Value |
|-------|-------|
| Session | 2026-04-17 ~22:45 local |
| Board | Fishball Z7020 @ `192.168.2.1:8080` |
| Build tag | `2026-04-16-p10prep-tgmon-dom-reuse-encfilter` |
| Uptime at capture | 5,754 seconds (~1.6 hours) |
| Site | Clay County NAC `0x8A1`, WACN `BEE00`, control 860.9625 MHz |
| RF config | `rx_lo=858.1 MHz`, `rf_bw=8 MHz`, `sampling=8 MSPS`, `ddc_offset=+2.863 MHz`, `gain=60 dB manual`, `rssi=83.25 dB` |
| Clock | **UNLOCKED** — NTP-on-boot not yet implemented (known TODO) |

## Executive summary

**Control chain is working well. Voice chain is the weak spot.** The NID decoder hits 98.83% valid across 70k attempts, but TSBK CRC is 71.6% total (TSBK3 block weakest at 65.9%), and the downstream audio quality is poor (40% silence in a 2.16s voice snippet, RMS std/mean=0.82).

**Root cause of the "not solid" feel** — symbol timing, not PLL, is the dominant instability. Captured 108 constellation snapshots in 90 seconds; cluster variance spans 14× (0.0088 to 0.1266) with visible progression tight → radial streaks → **X-pattern diagonal lines** → arc smearing. During the X-pattern states `pll_final ≈ 0` but `timing_final` jumps to 0.5-0.7, meaning the Gardner TED is tracking the wrong symbol boundary while carrier lock is fine.

**Robotic audio hypothesis** — `vocoder_errors: 0` means mbelib accepts every frame as "valid IMBE bits," but the bit-level extraction is being fed a symbol stream whose timing wanders. Timing errors corrupt individual bits within IMBE frames in patterns too subtle for vocoder self-check. The control chain hides this behind BCH+CRC; the voice chain has no per-frame FEC.

**`tdu_lc_count: 1977` vs `tdu_count: 35`** is anomalous and worth investigation. Did not increment during 20-second observation — suggests bursts happen at some other trigger (possibly on-retune or on certain DUID sequences) and are cumulative from earlier in the uptime.

**ARM performance is not the bottleneck.** HTTP latencies are 2-22 ms (median ~20 ms for `decoder_compare` which aggregates the most stats), IQ DMA keeps up at 242 kbps sustained, zero dibit overflows across 1.6 hours, one single IQ overflow at boot.

See §6 for the concrete recommendation list.

## 1. Live performance numbers

### 1.1 Decoder comparison (3-column matrix)

| Metric | PS C4FM | PS LSM | PL HDL |
|--------|---------|--------|--------|
| Messages | 123 | 1,000 | — |
| NID attempts | 8,928 | 72,061 | 70,291 |
| NID decode failures | 8,798 | 930 | — |
| NID valid | 120 | 71,105 | 69,468 |
| **NID valid pct** | **1.3%** | **98.7%** | **98.83%** |
| Sync hits | 8,928 | 72,061 | — |
| Sync near | 1,670,959 | 177,314 | — |
| TSBK block attempts | 306 | 213,291 | — |
| TSBK CRC OK | 173 | 152,796 | — |
| TSBK CRC OK (plain) | 97 | 81,400 | — |
| TSBK CRC OK (XORed) | 76 | 71,396 | — |
| TSBK trellis fail | 0 | 0 | — |

PS C4FM is dormant on this LSM site as expected (1.3% NID valid). The HDL LSM chain feeds PS LSM; both see the same dibit stream and agree to within 0.04% on NID validity.

### 1.2 TSBK CRC by block position

| Block | Attempts | CRC OK | CRC OK % | Delta from best |
|-------|---------:|-------:|---------:|----------------:|
| TSBK1 (first) | 73,200 | 53,760 | 73.44% | −2.04pp |
| TSBK2 (middle) | 73,200 | 55,253 | **75.48%** | 0 |
| TSBK3 (last) | 73,199 | 48,260 | **65.93%** | −9.55pp |

`blocks_per_tsdu = 2.99999` — all three blocks are being attempted; the gap is a decode-quality gap, not a framing gap.

TSBK3 being the weakest is consistent with *accumulated* bit errors late in the TSDU. The deinterleaver spreads errors across the 196-bit TSBK body, but late-block positions end up sampling more of the wandering part of the timing track. The 10pp spread between TSBK2 and TSBK3 is the single largest decode-quality signal in this snapshot.

### 1.3 TSBK CRC by opcode (top 10 by volume)

| Opcode | Label | Seen | Fail | CRC OK % |
|--------|-------|-----:|-----:|---------:|
| 0x16 | SNDCP_DCH_ANN_EX | 35,120 | 6,995 | 80.1% |
| 0x33 | IDEN_UPDATE_TDMA | 25,488 | 4,041 | 84.1% |
| 0x3D | IDEN_UPDATE | 25,013 | 4,140 | 83.4% |
| 0x3B | NET_STATUS_BCAST | 18,220 | 2,345 | 87.1% |
| 0x05 | UU_ANS_REQ | 18,220 | 7,332 | **59.8%** |
| 0x3A | RFSS_STATUS_BCST | 17,771 | 1,897 | **89.3%** |
| 0x30 | TDMA_SYNC_BCST | 17,661 | 4,203 | 76.2% |
| 0x39 | SEC_CCH_BROADCST | 17,126 | 3,130 | 81.7% |
| 0x09 | TELE_INT_V_CH_GRANT_UPDT | 13,428 | 7,681 | **42.8%** |
| 0x0B | (unknown) | 8,892 | 5,764 | **35.2%** |

The 0x0B parser is literally not implemented (`parsed: false`) — its "CRC fail" count is effectively meaningless since the block can't be tested. The 0x05 and 0x09 opcodes are real decode-quality outliers. Both are *low-volume* opcodes (typically emit near the end of TSDU bursts), so their poor CRC may be the same "late block" effect as TSBK3 — worth cross-referencing with block position.

### 1.4 HDL runtime health (last 1s window, sampled 15 times)

Every 1s window has:

- 10-14 NID events, all valid
- `iq_buf_rolls: 7-8` (expected: ring wraps every ~130 ms at 242 kbps)
- `iq_kbps: 242` (matches 62.5 kSPS × 4 bytes/sample IQ rate)
- Zero overflows (dibit or IQ)
- `sync_dist_best: 0` on every window

**PLL register range** (Q2.13 signed, ±8192 full scale):

| Window | pll_min | pll_max | span |
|--------|--------:|--------:|-----:|
| 1 | −2214 | +1686 | 3,900 |
| 2 | −1689 | +1760 | 3,449 |
| 3 | −3029 | +1480 | 4,509 |
| 4 | −1336 | +1105 | 2,441 |
| 5 | −2440 | +1745 | 4,185 |
| 6 | −1254 | +1410 | 2,664 |
| 7 | −1773 | +1724 | 3,497 |
| 8 | −657 | +1655 | 2,312 |
| 9 | −1700 | +1579 | 3,279 |
| 10 | −1905 | +1498 | 3,403 |
| 11 | −1277 | +2139 | 3,416 |
| 12 | −1373 | +3127 | 4,500 |
| 13 | −2601 | +1565 | 4,166 |
| 14 | −2589 | +1128 | 3,717 |
| 15 | −1686 | +1731 | 3,417 |

Mean span is ~3,500 units, roughly ±π/3 which *is* the documented PLL clamp — but the PLL shouldn't be *hitting* that range at steady state. This is consistent with the code review's earlier note that the HDL PLL bound is ±π/3 (matches SDRTrunk); the issue is the PLL is repeatedly excursioning *toward* that clamp rather than sitting stably near zero.

**Sample-point register range** (Q4.12 signed, full scale ~13,000 at sps=13):

| Window | sp_min | sp_max | span |
|--------|-------:|-------:|-----:|
| All 15 windows | 1,032-1,378 | 7,395-8,068 | ~6,600 |

Sample-point routinely spans over half its full range within one second. This is the Gardner TED hunting. See §3 for how this ties to the X-pattern constellation.

### 1.5 IRQ rates

| IRQ | Total | Rate (Hz) | Meaning |
|-----|------:|----------:|---------|
| `iq` | 43,902 | 7.63 | Control IQ DMA sub-buffer complete (32 KB each @ 250 kB/s = 7.8 Hz) |
| `traffic_iq` | 43,902 | 7.63 | Same rate — traffic DDC IQ path runs whenever LSM is disabled-but-armed |
| `dibit` | 1,829 | 0.32 | Control C4FM dibit DMA (dormant chain, low rate) |
| `lsm_dibit` | 1,686 | 0.29 | Control LSM dibit DMA (32 KB ring / 4800 sym × 0.5 B per dibit ≈ 9s between IRQs) |
| `traffic` | 127 | 0.022 | Traffic C4FM (idle mostly) |
| `traffic_lsm_dibit` | 114 | 0.020 | Traffic LSM (only during calls) |

The 242 kbps IQ rate and 7.63 Hz IRQ rate align exactly with the expected 62.5 kSPS × 4 bytes/sample ÷ 32 KB sub-buffer. IQ path is healthy.

### 1.6 Traffic / voice counters

```
HDU=36  LDU1=351  LDU2=326  TDU=35  TDU_LC=1977
extracted=6093  dropped=63  pcm=964800
grants_seen=1409  grants_rejected_encrypted=376  retunes=81
```

**Validations:**

- `LDU1 + LDU2 = 677`; `677 × 9 = 6,093` → **extracted count is exact.** IMBE bit extraction math is correct.
- `dropped / extracted = 1.03%` — low enough to not explain robotic audio on its own.
- `HDU ≈ TDU ≈ 35-36` — matches the ~35 call ends observed.
- `vocoder_pcm_produced = 964,800 samples = 120.6 s of audio` across 35 calls ≈ 3.5 s/call average.

**Anomalies:**

- **`TDU_LC = 1977` vs `TDU = 35`.** A TDU_LC is a specific end-of-call data unit; there should be ~1 per real end-of-call, not 57×. Across 20 seconds of live polling this counter did NOT increment, so it's either (a) cumulative from a pre-fix era when the burst bug was active, or (b) fires under some trigger that wasn't active during the observation window. Project memory notes `0a68148` was supposed to make TDU_LC handling idempotent; either the fix doesn't cover all paths or the counter itself is counting something other than dispatched events. **Worth investigating.**
- **`grants_rejected_encrypted / grants_seen = 27%`.** The site is heavily encrypted — 27% of observed grants are flagged encrypted via the `service_options.encrypted` bit. Consistent with `grant_map` showing multiple TGs (417, 1147, 1177) with `encrypted_count` == `count`.

## 2. Audio quality snapshot — recording `rec26_tg300.wav`

Pulled a 2.16-second call from TG 300 (unencrypted) via `/api/recordings/26.wav`.

| Metric | Value | Expected for clean speech |
|--------|------:|:--------------------------|
| Duration | 2.16 s | — |
| Sample rate | 8 kHz 16-bit mono | standard P25/mbelib output |
| Peak amplitude | 12,584 | < 30,000 (no clipping) |
| RMS amplitude | 1,031 | ~2,000-4,000 |
| **Silent frames (\|x\|<100)** | **40.3%** | 5-15% |
| **Per-100ms RMS std/mean** | **0.82** | 0.2-0.4 |
| Min per-100ms RMS | 2 | > 100 |
| DC offset | 1.2 | near 0 (fine) |
| Zero crossings/s | 2,556 | 500-2,500 for speech |

**Interpretation:** the audio is not just quiet — it has **long gaps of near-pure silence alternating with bursts of audio**. std/mean of 0.82 means the signal envelope is wildly non-stationary at the 100 ms scale. Healthy P25 voice has `std/mean` around 0.25-0.4 (some variance from speech prosody). 0.82 says the vocoder output is intermittently dropping out.

This is consistent with IMBE frames that are *bit-close* to valid (mbelib decodes them without flagging) but enough corrupted that the synthesizer outputs silence or near-silence for the corrupted frames. **The vocoder error counter is not catching this** because mbelib only flags frames where the Golay+Hamming protecting the IMBE parameters fails past correction — marginally-corrupted parameters still produce output, just bad output.

## 3. Constellation analysis — the X-pattern diagnosis

### 3.1 What was captured

90-second poll of `/api/constellation?chain={control,traffic}` at ~1.5 Hz. 108 snapshots total; every snapshot rendered to PNG with pll/timing/cluster-variance in the filename. All saved under [doc/diagnostics/2026-04-17/constellation/](constellation/).

Summary statistics across all 54 control-chain captures:

| Metric | Min | Max | Span ratio |
|--------|----:|----:|-----------:|
| cluster_var_mean | 0.0088 | 0.1266 | **14×** |
| pll_final (rad) | −0.575 | +0.720 | 1.295 |

14× variance in cluster tightness *on the same signal, same receiver, within 90 seconds* is the headline number.

### 3.2 Montage — tight vs loose states

[constellation/montage_tight_vs_loose.png](constellation/montage_tight_vs_loose.png) — 6-panel side-by-side:

**Top row (tight lock, `cv` ~0.017-0.026):**

- 4 clean clusters at the expected ±π/4 and ±3π/4 positions
- Slight radial fuzz only
- `timing_final` near 0

**Bottom row (loose lock, `cv` ~0.04-0.05):**

- **loose A:** clusters present but with radial streaks reaching toward the origin
- **loose B: the X-pattern** — diagonal lines visibly connecting opposite cluster pairs
- **loose C:** clusters start to merge into elongated arcs along the ±45° and ±135° diagonals

### 3.3 Why it's timing, not PLL

Key observation from the montage metadata:

| Panel | cv | pll_final | timing_final |
|-------|---:|----------:|-------------:|
| tight A | 0.017 | +0.019 | +0.02 |
| tight B | 0.020 | +0.031 | +0.87 |
| tight C | 0.026 | −0.030 | +0.02 |
| loose A | 0.041 | +0.019 | +0.70 |
| loose B | 0.050 | +0.049 | +0.59 |
| loose C | 0.050 | −0.003 | +0.48 |

`pll_final` is ≈0 in all six panels — carrier lock is fine. `timing_final` is the variable that tracks cluster tightness.

**Mechanism:** when the Gardner TED is tracking correctly, each sample lands *at* the symbol decision instant and the IQ value sits in its cluster. When the TED tracks wrong (mid-transition sample instead of peak-symbol sample), the sample lands *between* two symbols on the trajectory from one constellation point to the next — which for LSM's ±π/4 constellation is a diagonal line. **That is literally the X the user saw.** It's not noise, it's inter-symbol interference introduced by the timing loop sampling at the wrong phase.

### 3.4 Why the control chain still decodes

Even at `cv ≈ 0.05` (loose state), the BCH(63,16,23) ML decoder on the NID corrects multi-bit errors at t up to 11. The 48-bit sync correlator has 3-bit tolerance. NID decode keeps working up to substantial timing slip. **TSBK CRC (71.6%) is the first metric that directly reflects the timing wobble** because the 196-bit TSBK body is 1/2-rate Viterbi-protected but does NOT have a second-layer FEC beyond the 16-bit CRC — enough bit errors past Viterbi's correction capability and the CRC fails.

### 3.5 Why the voice chain suffers

LDU1 and LDU2 carry 9 raw IMBE frames each at 144 bits/frame. IMBE has its own internal Golay+Hamming (protecting the ~88 parameters inside each frame), but the mbelib threshold for rejecting a frame as "uncorrectable" is intentionally generous — marginal frames produce garbled audio rather than silence. So the same timing wobble that TSBK CRC exposes at the control chain *directly corrupts IMBE parameters* on the voice chain, with no error counter to surface it. This is the robotic audio.

## 4. API + test-script catalog

### 4.1 HTTP/WS endpoint count by category

The daemon exposes **43 endpoints** across 12 functional domains. Full catalog is in [doc/P25_API.md](../../P25_API.md) (20 of 43 documented). The surface breaks down roughly:

| Category | Endpoints | In P25_API.md? | Tested by a script? |
|----------|:---------:|:--------------:|:-------------------:|
| System / identity | 2 | Yes | Yes (`p25_check.py`) |
| Grants / bands / monitor | 4 | Partial | Partial |
| Decoder stats (control) | 6 | Yes | Yes (4 scripts) |
| Decoder stats (traffic) | 3 | No | No |
| Dibit / IQ dumps | 5 | Partial | Partial |
| LSM control knobs | 2 | Partial | Partial |
| IMBE / vocoder / audio | 4 | No | Partial |
| Recording management | 2 | No | No |
| WebSocket streams | 2 | Partial | No |
| Runtime tuning knobs | 6 | Partial | No |
| Front-end / retune | 3 | No | No |
| Self-describe / aliases | 4 | Partial | Partial |

**Notable undocumented endpoints (in `P25_API.md` sense):**

- Traffic-chain symmetric additions (Phase 10-prep): `/api/traffic_lsm_dibit_dump`, `/api/traffic_iq_capture`, `/api/traffic_iq_capture_aligned`, `/api/traffic_lsm_control`
- Phase 10-prep operator knobs: `/api/rx_gain`, `/api/grant_map`, `/api/bch_t`, `/api/encrypted_tgs`, `/api/monitor`, `/api/modulation`, `/api/reinit`
- Phase 7-8 additions: `/api/imbe_dump`, `/api/audio_test`, `/api/audio`, `/api/log`, `/api/nid_capture`, `/api/recordings`, `/api/recordings/{id}`
- Debug-tab sources: `/api/spectrum`, `/api/constellation`, `/api/endpoints`
- WebSocket: `/ws/audio`, `/ws/events`

### 4.2 Test script inventory

| Script | Active? | Depth | Notable |
|--------|:-------:|:-----:|---------|
| [tools/p25_check.py](../../../tools/p25_check.py) | Yes | Acceptance checklist — covers Phase 6-7D criteria | **Crashes on NameError at line 452** (`lsm_running`, `hdl_pct` undefined) — see CODE_REVIEW 1.2 |
| [tools/p25_status_and_next_step.py](../../../tools/p25_status_and_next_step.py) | Yes | Roadmap evaluation | Multi-endpoint; best "what to fix next" signal |
| [tools/p25_call_monitor.py](../../../tools/p25_call_monitor.py) | Yes | Per-call state transitions | 250 ms poll |
| [tools/monitor_p25_decoder.py](../../../tools/monitor_p25_decoder.py) | Yes | Symbol timing loop diagnostics | Long-run JSONL capture |
| [tools/voice_capture.py](../../../tools/voice_capture.py) | Yes | E2E voice call capture | Requires `iio_readdev` for IQ; hits 7 endpoints |
| [tools/p25_constellation_capture.py](../../../tools/p25_constellation_capture.py) | **New** (this session) | Constellation snapshots + PNG + summary JSONL | — |
| [tools/p25_constellation_montage.py](../../../tools/p25_constellation_montage.py) | **New** (this session) | 6-panel tight-vs-loose montage | — |
| `p25_nid_fec.py`, `p25_lsm_demod.py`, `p25_sticky_lock_test.py`, `p25_decode_imbe_capture.py`, `p25_imbe_test.py`, `p25_ddc_filter_design.py`, `p25_nid_analyze.py`, `p25_sync_sweep.py` | stubs or offline tools | — | Not in API test path |

### 4.3 Coverage gaps

Endpoints exposed by the daemon that have **no script exercising them**:

1. `/api/control_iq_capture` + `/api/control_iq_capture_aligned` — raw IQ ring for offline BCH/TSBK cross-validation
2. Traffic-chain dumps (`/api/traffic_lsm_dibit_dump`, `/api/traffic_iq_capture*`, `/api/traffic_lsm_control`)
3. `/api/grant_map` — per-TG frequency history
4. `/api/rx_gain` — gain knob
5. `/api/nid_capture` — per-DUID NID ring (endpoint returned 0 entries in this session — may be broken or need different params)
6. `/api/bch_t` — BCH threshold tuner
7. `/api/encrypted_tgs` — TG encryption list
8. `/api/monitor` — TG monitor list
9. `/api/modulation` — modulation auto-select
10. `/api/spectrum` — FFT bins (Debug tab only)
11. `/api/reinit` — front-end re-init (sensitive — could retune)
12. `/api/recordings` — recording management (dashboard only)
13. `/api/set_time` — browser clock push (dashboard only)
14. `/api/endpoints` — API self-describe (dashboard only)
15. `/ws/audio` — WebSocket audio stream (dashboard only)

### 4.4 Stale endpoints in scripts

Spot-checked the tools directory; **no stale references to Phase 9-retired endpoints** (`/api/lsm` is gone, scripts all use `/api/hdl_lsm` + `/api/control_lsm_control`). The Phase 10-prep rename from `/api/lsm_control` to `/api/control_lsm_control` has propagated to all active scripts.

## 5. ARM performance (indirect inference)

SSH access was attempted — board's host key had changed (cleared and re-trusted), but the board requires key-based auth that we don't have. So metrics here are all inferred from HTTP behavior.

### 5.1 HTTP response latency

10-sample `decoder_compare` request series:

| Median | p90 | Min | Max |
|-------:|----:|----:|----:|
| 20.7 ms | 22.8 ms | 2.1 ms | 22.8 ms |

The 2-22 ms bimodal distribution suggests a hot path (cache hit) and a cold path (full stats aggregation). `decoder_compare` is one of the heavier endpoints — it pulls ~50 fields across three decoders. **20 ms for an aggregating endpoint on a dual-core 667 MHz A9 is fine** — nowhere near saturation.

### 5.2 DMA keeping up with HDL

- IQ DMA: `iq_kbps: 242` sustained, `iq_buf_rolls: 8` per second (one full 32 KB sub-buffer wrap per 125 ms). Matches the expected 62.5 kSPS × 4 B = 250 kB/s.
- `iq_overflow_ticks: 1` total across 1.6 hours of uptime — one single overflow at boot, zero since.
- `dibit_overflow_ticks: 0` total — zero dibit-path overflows across the session.

**The PS is consistently draining the FPGA rings faster than they fill.** No CPU-starvation story.

### 5.3 What we can't measure without SSH

- ARM load average — no endpoint.
- Process CPU %, RSS — no endpoint.
- Context switch rate, irq-per-cpu distribution — no endpoint.
- AD9361 internal temperature, PLL lock state, DCXO trim — `iio_attr -u ip:192.168.2.1` from a Linux/WSL host can reach these directly.
- Free memory / disk — no endpoint.

**Recommendation:** adding a `/api/sys_health` endpoint that reports `/proc/loadavg`, RSS of the daemon process, and free memory would make this introspectable without needing SSH. Low effort; high signal for future investigations.

## 6. Recommendations

In priority order for "still not solid" fixes:

### 6.1 Immediate — understand before changing

1. **Cross-check the TDU_LC counter semantics.** Read [p25-httpd/src/p25/traffic_manager.rs](../../../p25-httpd/src/p25/traffic_manager.rs) around the TDU_LC increment path. If the 1977/35 ratio is the accumulated pre-fix burst history, add a comment noting this and reset the counter on the next bake. If it's still firing extra events, the idempotent fix is incomplete.
2. **Measure the control chain timing-track variance versus TSBK-block position.** Add a per-TSBK-position histogram of `sp_dbg` range at the time of each CRC success/failure. This directly tests the hypothesis that TSBK3's 65.9% vs TSBK2's 75.5% is a timing-track tail effect.
3. **Build a histogram of `timing_final` across all 108 captures** — correlate against `cluster_var_mean` to confirm the X-pattern ↔ timing relationship holds statistically.

### 6.2 Short-term HDL / tuning knobs

4. **Investigate Gardner TED loop gain.** The sample-point register spans ~6,600 units (Q4.12, ~50% of full scale) within every 1s window — too wide for steady-state lock. Reducing the loop gain (or clamping `max_timing_adj` per [maia-hdl/p25_hdl/lsm_gardner_ted.py](../../../maia-hdl/p25_hdl/lsm_gardner_ted.py)) should tighten the lock at the cost of slower pull-in. Start with a bench sim at half the current gain and see if cluster variance stabilises.
5. **Per-block TSBK CRC telemetry.** The 10pp spread between TSBK2 and TSBK3 is a diagnostic goldmine. Expose each block's CRC pass rate conditional on (TSBK2 passed AND TSBK3 failed) vs (both passed) — tells us how strongly TSBK3 failure correlates with in-TSDU timing drift.
6. **Expose a traffic-chain constellation endpoint** that's non-zero during calls. Currently `/api/constellation?chain=traffic` returns data even when idle (presumably stale); a forced-live version that captures during LDU1 symbols would let us see whether the X-pattern is worse during voice than on the control channel. If it is, that's direct evidence the robotic audio is timing-driven.

### 6.3 Vocoder / voice

7. **Add a per-frame IMBE quality gate.** The Rust vocoder currently trusts mbelib's self-check. Add a second filter: measure the L4-norm of synthesised PCM per 144-bit frame; if a frame produces near-silence (< RMS threshold) or near-pure-tone, flag it as a "suspected corrupted IMBE" in an optional counter. Would let us quantify the robotic-ness that `vocoder_errors` is missing.
8. **Consider a conservative frame-repeat when consecutive IMBE frames look bad.** SDRTrunk's JMBE has a frame-repeat path for low-confidence frames. Port the approach — a repeated-previous-frame is less disruptive to the listener than a half-corrupted new frame.

### 6.4 Test / infra

9. **Fix [tools/p25_check.py](../../../tools/p25_check.py) line 452-453** — `lsm_running` and `hdl_pct` undefined. This tool is the first-line diagnostic per project memory; fixing it unblocks everything downstream.
10. **Add a `/api/sys_health` endpoint** — load avg, RSS, thread count, free mem. Makes ARM analysis tractable without SSH.
11. **Document the remaining 23 undocumented endpoints in [doc/P25_API.md](../../P25_API.md).** The dashboard already uses them all; the doc just drifted.

### 6.5 Longer-term architecture

12. **The "still not solid" feel traces almost entirely to the Gardner TED loop.** Options, roughly in order of effort:
   - Tune gains (hours)
   - Add a second-order PI/II loop filter with slower integral-gain response (days)
   - Per-site adaptive loop gain (weeks)
   - Soft-sync correlator upgrade (from the HDL roadmap Phase 15 — weeks) would improve initial timing pull-in, which feeds into steady-state stability
13. **Consider capturing constellation snapshots continuously** (ring of last 100 at low rate) on the board itself so we can grep the most interesting moments after-the-fact rather than polling and possibly missing them.

## 7. Files saved from this session

| Path | Content |
|------|---------|
| [doc/diagnostics/2026-04-17/api_snapshot_t0.txt](api_snapshot_t0.txt) | Raw JSON dump of 13 endpoints at t=0 |
| [doc/diagnostics/2026-04-17/http_latency.txt](http_latency.txt) | 10-sample HTTP timing |
| [doc/diagnostics/2026-04-17/imbe_dump.txt](imbe_dump.txt) | Raw IMBE frame ring |
| [doc/diagnostics/2026-04-17/timeseries/hdl_lsm_15s.txt](timeseries/hdl_lsm_15s.txt) | 15-sec PLL + sp timeseries |
| [doc/diagnostics/2026-04-17/timeseries/traffic_imbe_20s.txt](timeseries/traffic_imbe_20s.txt) | 20-sec traffic counter timeseries |
| [doc/diagnostics/2026-04-17/recordings/rec26_tg300.wav](recordings/rec26_tg300.wav) | Downloaded call recording for audio analysis |
| [doc/diagnostics/2026-04-17/constellation/](constellation/) | 108 PNGs + summary.jsonl |
| [doc/diagnostics/2026-04-17/constellation/montage_tight_vs_loose.png](constellation/montage_tight_vs_loose.png) | 6-panel tight/loose comparison |
| [tools/p25_constellation_capture.py](../../../tools/p25_constellation_capture.py) | Constellation poll + save tool (new) |
| [tools/p25_constellation_montage.py](../../../tools/p25_constellation_montage.py) | Montage generator (new) |

## 8. Verification status

- Control-chain NID + TSBK numbers: **direct read** from `/api/decoder_compare`, `/api/hdl_lsm`, `/api/tsbk_opcodes`. High confidence.
- PLL / sp_dbg ranges: **15 samples** over 15 s. Representative of current behavior; would need longer capture for robust tails.
- Constellation X-pattern: **108 captured samples** over 90 s; the tight↔loose oscillation is visually and statistically clear. The *attribution* to Gardner TED is strong but not proven — it could also be partly driven by signal-side phase noise. The next step in §6.2 item 3 would nail it.
- Audio analysis: **one recording** (2.16 s, TG 300). Statistics are strong for this one clip but audio quality can vary across calls; worth repeating with 3-5 recordings across different TGs and durations.
- ARM load inference: **indirect only.** SSH-less; `/api/sys_health` recommended.
- TDU_LC anomaly: **confirmed non-incrementing** across 20 s, but root cause not diagnosed. Investigation required.

---

## §A. Corrections (appended 2026-04-17 after peer review)

### §A.1 What was wrong

The original §1.4 interpretation of the `sp_dbg` register range and the §3.3/§6.2-4 attribution of the constellation X-pattern to Gardner TED loop-gain were based on a numerical error and a reference-constant oversight.

**Error 1 — sp_dbg full-scale assumption.** §1.4 stated "Q4.12 signed, full scale ~13,000 at sps=13." The HDL actually uses `sps = 31250 / 4800 = 6.5104` (see [lsm_timing_interp.py:130-141](../../../maia-hdl/p25_hdl/lsm_timing_interp.py#L130-L141)), and `sample_point` oscillates by exactly one `sps` per symbol decision by algorithmic design:

```python
sample_point -= 1.0                      # every input sample
if sample_point < 1.0:
    ...emit decision...
    sample_point += sps                  # +6.5104 ~= +26,667 Q12 units
```

So the natural operating range of `sample_point` per symbol period is **0 → sps**, which in Q4.12 is **0 → ~26,667 units**, not "0 → ~13,000." The observed 1 s span of ~6,600 units is therefore **~25% of one symbol period**, not "50% of full scale." This is normal operation of the timing recovery, not loop instability.

**Error 2 — changing a reference-matched constant on local evidence alone.** `TED_GAIN = SPS / 4.0 = 1.628` is explicitly "verbatim from `demod_lsm_with_state`" ([lsm_gardner_ted.py:61-64](../../../maia-hdl/p25_hdl/lsm_gardner_ted.py#L61-L64)), and [test_lsm_gardner_ted.py:143-229](../../../maia-hdl/test/test_lsm_gardner_ted.py#L143-L229) asserts the HDL result is within 6 Q4.12 ULPs of the float Rust reference on randomised inputs. The Rust reference is itself SDRTrunk-faithful. SDRTrunk decodes P25 cleanly on its own production hardware at this gain, so "halve TED_GAIN" is a deliberate divergence from the reference without evidence that the reference is the problem.

See auto-memory `feedback_sdrtrunk_is_the_reference` for the general rule.

### §A.2 Observations that remain valid

Everything in the original document that is a **counter, timing measurement, or raw visual observation** is still correct. The *attribution* of the X-pattern to the Gardner TED is what's invalidated.

Specifically still valid:

- §1.1 Decoder comparison table — NID 98.83%, TSBK CRC 71.6% etc.
- §1.2 TSBK block CRC: TSBK1 73.44% / TSBK2 75.48% / TSBK3 65.93% — the 9.5 pp TSBK3 spread is real.
- §1.3 Per-opcode CRC rates.
- §1.5 IRQ rates and DMA keep-up.
- §1.6 Traffic/voice counters, including the `TDU_LC = 1977 vs TDU = 35` anomaly.
- §2 audio recording statistics (40 % silent frames, RMS std/mean = 0.82).
- §3.1 the 14× cluster-variance span in 90 s.
- §3.2 the tight-vs-loose montage visual characterisation (tight clusters vs radial streaks vs X-pattern vs arc smearing — all real photographic observations).
- §5 ARM performance inferences.

### §A.3 Re-attribution candidates for the X-pattern / cluster variance

The X-pattern in the loose-state panels ([constellation/montage_tight_vs_loose.png](constellation/montage_tight_vs_loose.png)) — diagonal lines connecting opposite cluster pairs — is real. The original §3.3 attribution ("Gardner TED sampling mid-transition") is plausible as a mechanism but not supported by the `sp_dbg` range evidence once that range is correctly interpreted. Candidate mechanisms worth investigating, ordered by my current plausibility estimate:

1. **AGC–TED interaction (Phase 10 addition).** The Phase 10 `LsmAgc` with `mag_update_threshold=1024` (see `feedback_agc_noise_floor_gate`) is new since the LSM chain was last stable. Rapid AGC gain transitions rescale the differential-demod inputs to the Gardner TED mid-cycle; if the AGC settles in ~O(10) symbols, its envelope changes can transiently produce X-pattern samples on the output of `LsmPllRotate` even with timing perfectly locked. **Test:** capture constellation while holding AGC disabled (`lsm_agc_enable=0`) on a known-good signal and compare cluster variance distribution to AGC-enabled captures on the same signal.
2. **DC-blocker transients.** [LsmDcBlocker](../../../maia-hdl/p25_hdl/lsm_dc_blocker.py) is a leaky integrator with a finite time constant. Any DC step (e.g. mode change, AGC gain slam) drives the blocker output through a transient that superimposes on the symbol constellation. **Test:** same as above — disable DC blocker (`lsm_dc_block_enable=0`) and compare.
3. **Signal-side SNR variation at the captured instants.** The Fishball target is 860.9625 MHz Clay County, antenna on a static mount; multipath + thermal could produce 14× cluster-variance excursions independent of anything in the receiver. **Test:** cross-validate by capturing an IQ dump (`/api/control_iq_capture`) at a "loose" moment and replaying it through SDRTrunk on a PC; if SDRTrunk sees the same X-pattern, the HDL is exonerated and the cause is upstream.
4. **Sample-point edge case near `< 1.0` threshold.** If the `sample_point < 1.0` check is triggered in back-to-back input samples due to a large Gardner correction, the subsequent symbol decisions land on input samples that are offset differently from normal, and the constellation samples may end up between-cluster. Verified behaviour under dense correction traffic is not currently tested. **Test:** instrument sample_point trajectory through a known "loose" window; look for back-to-back decisions.
5. **PLL+TED coupling transient.** Even though both loops are individually reference-correct, their interaction under a step in signal envelope (from AGC or from signal itself) is a coupled system with no closed-form analysis. **Test:** same IQ-capture cross-validation as #3; SDRTrunk has the same algorithmic coupling, so if SDRTrunk is clean on the capture, the coupling is not the problem.

Candidates 1 and 2 are Fishball-specific (AGC and DC blocker are both Phase-10-prep HDL additions that SDRTrunk does not have in the same form). Candidate 3 is the cheapest to rule in/out. Candidate 4 is a long-shot but worth checking if 1-3 don't explain the variance.

### §A.4 Re-interpretation of the TSBK3 9.5 pp CRC spread

Original §1.2 attributed TSBK3's lower pass rate to "timing-track tail effect" — accumulated bit errors late in the TSDU. That mechanism is still plausible, but the evidence-chain now has a hole in it (the "timing track wanders" premise was based on the misread sp_dbg range). The TSBK3 spread still warrants investigation; plausible alternatives:

1. **Block-position-dependent deinterleaving or trellis state.** The 196-bit TSBK body is 1/2-rate Viterbi-protected + deinterleaved per-block. If the deinterleaver state reset sequence differs across block positions (e.g. TSBK1 starts fresh, TSBK2/3 inherit state), any bit error in TSBK1 propagates downstream. **Test:** per-block Viterbi error-count telemetry rather than per-block CRC.
2. **MAC-header / block-length decoding errors.** A mis-decoded TSBK2 length field could bleed into TSBK3 framing, producing block-3-specific failures. **Test:** trace TSBK3 CRC failures conditional on TSBK2 success vs failure.
3. **Real timing drift during a TSDU.** Still plausible even with the `sp_dbg` data re-interpreted — the drift would need to be characterised differently (e.g. by looking at whether CRC failures cluster temporally, not just per-block).

### §A.5 Re-interpretation of the `vocoder_errors = 0` + robotic-audio contradiction

Original §2 and §6.3-7 diagnosis (mbelib passes bit-marginal frames that synthesize silence/garble) is largely unchanged — that mechanism is true independent of the cause of the bit corruption. The recommended per-frame IMBE quality gate (§6.3-7) is still the right observability addition. What changes: the *cause* of the bit corruption is not necessarily "wandering timing loop"; it could be any of the candidates in §A.3 above.

### §A.6 What the redo should look like

- **Before** making any HDL change, run the cross-validation in §A.3-3: capture an IQ dump from a "loose" moment and replay through SDRTrunk. This single test cleanly separates "Fishball-specific bug" from "signal-side noise."
- **If SDRTrunk also sees the X-pattern** on the same capture: the cause is upstream (RF/AGC/DC-blocker/environmental), and the fix is on one of those surfaces — not inside the demod loop.
- **If SDRTrunk decodes cleanly** on the same capture: the cause is a Fishball-local HDL issue (most likely AGC-TED interaction or DC-blocker transient per §A.3). Then and only then is a targeted HDL change warranted — and even then, the first candidates to tweak are the Fishball-added modules (AGC, DC blocker), not the SDRTrunk-matched constants (TED gain, PLL gain).
- **Observability additions from §6** remain worth doing (per-TSBK-block telemetry, per-frame IMBE quality gate, live traffic constellation). They would have caught this on-target rather than requiring a 90 s constellation harvest.

### §A.7 Roadmap impact

[HDL_LAYOUT_AND_ROADMAP.md](../../HDL_LAYOUT_AND_ROADMAP.md) Phase 10.5 sub-item 1 ("Gardner TED loop-gain retune") is re-scoped to "cluster-variance root-cause investigation with SDRTrunk cross-validation." All other Phase 10.5 sub-items (per-TSBK-block telemetry, TDU_LC audit, IMBE quality gate, live traffic constellation) are unaffected.

### §A.8 Lessons logged

Two auto-memory entries added 2026-04-17 to prevent this pattern from repeating:

- `feedback_sdrtrunk_is_the_reference` — general rule for reference-matched constants.
- (this correction appendix) — specific incident record.
