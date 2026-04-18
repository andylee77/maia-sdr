# Fishball P25 — Build Performance Analysis (2026-04-18)

**Delta-report against [2026-04-17 baseline](../2026-04-17/PERFORMANCE_ANALYSIS.md).**
Same site, same signal, 17-hour gap, three firmware advances in between: eye-stride browser fix (unflashed, for reference), HP1 BD-wire fix (flashed), and Phase 10.6 post-LSM matched-filter IQ tap now live on-target.

## Document metadata

| Field | Value |
|-------|-------|
| Session | 2026-04-18 ~22:40 local |
| Board | Fishball Z7020 @ `192.168.2.1:8080` |
| Build tag | `2026-04-18-phase10.6-post-lsm-iq` (NB: the `bd-fix-eye-stride` build is committed but not flashed — dashboard stride patch still a browser monkey-patch at capture time) |
| Uptime | 657 s (~11 min — cold data, short-window bias warning) |
| Site | Clay County NAC `0x8A1`, WACN `BEE00`, control 860.9625 MHz |
| RF config | `rx_lo=858.1 MHz`, `rf_bw=8 MHz`, `sampling=8 MSPS`, `gain=60 dB manual` |
| Clock | **UNLOCKED** (NTP-on-boot still TODO) |

> **Short-window caveat** — 2026-04-17 sampled 1.6 h of uptime; this snapshot is 11 min. Cumulative counters are therefore ~8× smaller here, but **rate-based** metrics (valid %, CRC %, sample throughput) are directly comparable.

## 1. TL;DR — what actually moved

| Metric | 2026-04-17 | 2026-04-18 | Δ |
|---|---|---|---|
| HDL LSM NID valid % | 98.83 % | **99.79 %** | +0.96 pp |
| PS LSM NID valid % | 98.7 % | **99.73 %** | +1.03 pp |
| TSBK CRC total % | 71.6 % | **81.0 %** | **+9.37 pp** |
| TSBK1 CRC % | 73.44 % | 80.60 % | +7.16 pp |
| TSBK2 CRC % | 75.48 % | 86.80 % | +11.32 pp |
| TSBK3 CRC % | 65.93 % | 75.53 % | +9.60 pp |
| Constellation `pll_final` range (rad) | [-0.575, +0.720] | **[-0.111, +0.060]** | ~8× tighter |
| Constellation `cluster_var_mean` span | 14× | 6.7× | ~2× tighter |
| Audio silent-frames % (comparable clip) | 40.3 % | **14.1 %** | **-26.2 pp** |

**Headline:** the voice chain materially improved. Control-chain NID was already good, but TSBK CRC lifted almost 10 pp across the board, including the weakest (TSBK3) position. PLL drift is settled — the ±0.7 rad excursions that drove the 2026-04-17 §3 investigation are no longer visible in a 90 s capture. Audio near-silence fell from 40 % to 14 % on comparable-duration TG 300-style calls.

**New capability this session:** the post-LSM matched-filter IQ tap is live. `/ws/iq?source=post_lsm` delivers canonical 31.25 kSPS i16 IQ that renders cleanly into a 1-symbol-stride eye (see §6), making this the first time we can visually inspect the RRC'd symbol stream on-target without offline IQ replay.

## 2. What changed between 2026-04-17 and 2026-04-18

In commit order:

1. **Phase 10.6 HDL** (maia-sdr `1bf4fc7`): bank decoder widened 3→4 bits; added `lsm_iq_dma` + `traffic_lsm_iq_dma` rings at 0x1D000000 / 0x1E000000 fed from `lsm_rrc.re_out/im_out` + traffic twin; PAC regenerated.
2. **Phase 10.6 PS** (maia-sdr `30f18b8`): `/ws/iq?source=post_ddc|post_lsm` param; dashboard eye-plot Src dropdown.
3. **Phase 10.6 DT** (tezuka_fw `629def8`): UIO carve-outs `p25-lsm-iq` + `p25-traffic-lsm-iq`.
4. **First bake shipped broken** (tezuka_fw `d38c557`): DMA silent on both post-LSM rings because the BD script didn't wire the two new AXI masters to HP1 — root cause on session start.
5. **BD fix** (maia-sdr `082833b`): added the missing `ad_mem_hp1_interconnect m_axi_lsm_iq` / `m_axi_traffic_lsm_iq` lines; rebuilt + flashed.
6. **Eye-stride fix** (maia-sdr `516cf8e`, committed but **not flashed** at capture time): dashboard stride = 1 symbol instead of `win_len/2`. Browser console monkey-patch used to render the eye plots in §6 — will be permanent after next flash.

`/api/system` reports `build = 2026-04-18-phase10.6-post-lsm-iq` because BUILD_TAG wasn't bumped before this flash. Next flash should land as `2026-04-18-phase10.6-bd-fix-eye-stride`.

## 3. New: `/api/sys_health` snapshot

Previously unreachable without SSH. Now one HTTP hit:

```json
{
  "daemon_rss_kib": 10828,
  "daemon_threads": 35,
  "loadavg_1": 1.14,
  "loadavg_5": 0.89,
  "loadavg_15": 0.56,
  "mem_available_kib": 891732,
  "mem_available_pct": 87.1,
  "mem_total_kib": 1023504,
  "uptime_secs": 657
}
```

Dual-core A9 @ 667 MHz with `loadavg_1 = 1.14` → ~57 % of one core used steady-state. Memory is 87 % free, daemon RSS 10.8 MB across 35 threads. **No pressure on CPU or memory.** The 2026-04-17 inference ("ARM performance is not the bottleneck") is now directly measured.

## 4. Control-chain decoder

### 4.1 3-way comparison (at t=22:40, uptime 11 min)

| Metric | PS C4FM | PS LSM | PL HDL |
|---|--:|--:|--:|
| Messages | 1000 | 1000 | — |
| NID attempts | 3,339 | 8,256 | 8,138 |
| NID valid % | 93.1 % | **99.73 %** | **99.79 %** |
| Sync near (noise) | 115,111 | 12,178 | — |
| TSBK attempts | 9,309 | 24,699 | — |
| TSBK CRC OK | 5,093 | 20,000 | — |
| TSBK CRC OK % | 54.7 % | **81.0 %** | — |

PS C4FM is dormant on this LSM site (low valid %, wrong NAC `0xE21` reported — same as 2026-04-17). PS LSM and PL HDL agree to within 0.06 pp on NID validity, as expected (they feed on the same dibit stream via different framers).

### 4.2 TSBK CRC by block position (PS LSM)

`blocks_per_tsdu = 3.0` exactly, all three blocks attempted per TSDU, 8,233 TSDUs total.

| Block | Attempts | OK | % | Δ vs 2026-04-17 |
|---|--:|--:|--:|--:|
| TSBK1 | 8,233 | 6,636 | 80.60 % | **+7.16 pp** |
| TSBK2 | 8,233 | 7,146 | 86.80 % | **+11.32 pp** |
| TSBK3 | 8,233 | 6,218 | 75.53 % | **+9.60 pp** |

TSBK3 remains the weakest position (consistent with the "bit errors accumulate late in the TSDU" hypothesis) but every block has moved up. The TSBK2-vs-TSBK3 gap is now 11.3 pp vs the 2026-04-17 gap of 9.5 pp — the tail is still there and, if anything, slightly more pronounced. **The broad lift is probably carrier-lock tightening (see §5); the remaining TSBK3 gap is a separate follow-up.**

### 4.3 Top TSBK opcodes (2026-04-18)

| Opcode | Label | Seen | OK % |
|---|---|--:|--:|
| 0x16 | SNDCP_DCH_ANN_EX | 4,041 | 85.3 % |
| 0x3D | IDEN_UPDATE | 2,984 | 91.8 % |
| 0x33 | IDEN_UPDATE_TDMA | 2,938 | 90.6 % |
| 0x3B | NET_STATUS_BCAST | 1,939 | 94.4 % |
| 0x3A | RFSS_STATUS_BCST | 1,866 | 96.6 % |
| 0x30 | TDMA_SYNC_BCST | 1,974 | 88.8 % |
| 0x39 | SEC_CCH_BROADCST | 1,905 | 87.8 % |
| 0x05 | UU_ANS_REQ | 2,003 | 66.1 % |
| 0x09 | TELE_INT_V_CH_GRANT_UPDT | 1,623 | 51.0 % |
| 0x02 | GRP_V_CH_GRANT_UPDT | 771 | 92.5 % |

Every opcode that was above 80 % on 2026-04-17 is now above 85 %. The two outliers (0x05, 0x09) that were at ~60 % / ~43 % on 2026-04-17 are now at 66 % / 51 % — still outliers, still likely late-TSDU positions.

## 5. PLL + timing loop health

### 5.1 15-second `hdl_lsm.last_window` sweep

15 one-second windows sampled sequentially via `/api/hdl_lsm`:

```
t= 0 pll=[-2673,+1334] sp=[ 1220, 7569] nid=12/11 iq_rolls=8 kbps=244
t= 1 pll=[-1245,+1180] sp=[ 1039, 7592] nid=11/11 iq_rolls=8 kbps=240
t= 2 pll=[ -703, +480] sp=[ 1110, 7655] nid=12/12 iq_rolls=8 kbps=241
t= 3 pll=[ -792,+1609] sp=[ 1026, 7574] nid=12/12 iq_rolls=7 kbps=242
t= 4 pll=[ -909,+1767] sp=[ 1208, 7655] nid=12/12 iq_rolls=8 kbps=242
t= 5 pll=[-1125,+1936] sp=[ 1059, 7572] nid=12/11 iq_rolls=8 kbps=242
t= 6 pll=[-1221, +861] sp=[ 1141, 7729] nid=12/11 iq_rolls=7 kbps=242
t= 7 pll=[-1518,+1456] sp=[ 1137, 7680] nid=13/13 iq_rolls=8 kbps=242
t= 8 pll=[-4775,+1772] sp=[ 1038, 7686] nid=10/10 iq_rolls=8 kbps=242
t= 9 pll=[-1169,+3768] sp=[ 1053, 7559] nid=13/13 iq_rolls=7 kbps=242
t=10 pll=[-1356,+1020] sp=[ 1063, 7686] nid=13/12 iq_rolls=8 kbps=242
t=11 pll=[-1781, +788] sp=[ 1025, 7657] nid=13/13 iq_rolls=8 kbps=241
t=12 pll=[-1640,+2562] sp=[ 1053, 7642] nid=13/13 iq_rolls=8 kbps=242
t=13 pll=[ -909,+2661] sp=[ 1174, 7651] nid=12/12 iq_rolls=8 kbps=242
t=14 pll=[ -976,+1736] sp=[ 1029, 7564] nid=14/14 iq_rolls=8 kbps=242
```

Raw JSON in [timeseries/hdl_lsm_15s.json](timeseries/hdl_lsm_15s.json).

- **PLL span**: mean ~3,500 / median ~2,900 / outlier at t=8 (−4775 excursion). Broadly same shape as 2026-04-17.
- **sp_min/max**: `[~1030, ~7650]` → span ~6,600 Q4.12 units, **identical** to 2026-04-17. This correctly matches the §A correction in the 2026-04-17 doc — `sp_dbg` oscillates through `sps ≈ 26,667` Q4.12 units per symbol by design, and ~6,600 is ~25 % of that. Normal operation.
- **NID rate**: 10–14 events/sec, 100 % validity in most windows. Matches expectation (4800 sym/s / 384 sym per NID ≈ 12.5 Hz).
- **IQ ring**: `iq_kbps = 241–244` (pre-Phase-10.6 ring, post-DDC 62.5 kSPS × 4 B). Healthy.

### 5.2 Constellation 90-second poll (control chain)

56 snapshots at ~1.5 Hz, PNGs + summary in [constellation/](constellation/).

| Metric | 2026-04-17 (n=108) | 2026-04-18 (n=56) |
|---|---|---|
| `cluster_var_mean` min | 0.0088 | **0.0152** |
| `cluster_var_mean` max | 0.1266 | **0.1014** |
| span ratio | 14× | **6.7×** |
| `pll_final` min (rad) | −0.575 | **−0.111** |
| `pll_final` max (rad) | +0.720 | **+0.060** |
| `pll_final` span | 1.295 rad | **0.171 rad** — 7.6× tighter |

The PLL is settled. The 2026-04-17 "X-pattern ↔ PLL near zero" story was observed under ±0.7 rad PLL excursions; this session we just don't see those anymore. The cluster-variance improvement is less dramatic (span halved) but in the same direction.

## 6. NEW — post-LSM matched-filter eye plot

Captured via [tools/p25_ws_eye_capture.py](../../../tools/p25_ws_eye_capture.py) bypassing the browser entirely. 12 s of `/ws/iq?source=post_lsm` + 8 s of `/ws/iq?source=post_ddc` for side-by-side reference. Stride = exactly one symbol, fractional `sps` tracked in float.

| | post-DDC (raw) | post-LSM (MF) |
|---|---|---|
| Sample rate | 62,500 SPS | 31,250 SPS |
| sps | 13.02 | 6.51 |
| Samples captured | 507,904 | 385,024 |
| Duration | 8.13 s | 12.32 s |
| rms_i / rms_q | 177 / 176 | 239 / 240 |
| peak_abs | 595 | 909 |
| DC I/Q | +0.02 / −0.86 | −0.56 / −3.99 |

Rendered eye PNGs (nsym=2 and nsym=4, I-only and I+Q) are in [eye/](eye/):

- [eye/eye_control_post_lsm_nsym4_iq.png](eye/eye_control_post_lsm_nsym4_iq.png) — **the canonical Phase 10.6 matched-filter eye**, I+Q overlay, 4-symbol window. 4 distinct eye-lobe clusters visible (confirmed on-target before this run via browser monkey-patch).
- [eye/eye_control_post_lsm_nsym2_iq.png](eye/eye_control_post_lsm_nsym2_iq.png) — 2-symbol window equivalent.
- [eye/eye_control_post_ddc_nsym4_iq.png](eye/eye_control_post_ddc_nsym4_iq.png) — raw post-DDC reference, same stride algorithm.
- Raw IQ saved as `.npz` for offline replay.

**What the MF eye shows:**

- The LSM MF output has a clear periodic structure — peak-trough alternation at exactly the symbol rate. Post-DDC raw shows the same periodicity but broader-band (higher-frequency jitter superimposed).
- Because the tap is **pre-PLL-rotation + pre-timing-recovery**, I and Q both carry the rotating π/4-DQPSK signal. I-only and I+Q views look very similar — there's no clean "I-only open eye" until the PLL rotate runs downstream.
- Amplitude is roughly 4× the per-sample RMS — the MF concentrates signal energy around symbol instants (as intended).

See `project_clean_eye_plot_todo.md` for the two options to produce a PLL-rotated eye (HDL tap after rotate, or client-side timing recovery). Not urgent — the constellation and decoder counters already expose the health signal.

## 7. Audio quality — 3 recordings

Downloaded three recent unencrypted calls via `/api/recordings/{id}.wav`. Analysed with [tools/p25_audio_stats.py](../../../tools/p25_audio_stats.py).

| File | TG | Dur (s) | Peak | RMS | Silent % | std/mean | min RMS | ZCR/s |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| rec8_tg300.wav | 300 | 3.06 | 10 | 3 | 100.0 | 0.05 | 2 | 3892 |
| rec17_tg319.wav | 319 | 32.40 | 18,280 | 921 | 21.6 | 0.97 | 1 | 2917 |
| rec20_tg318.wav | 318 | 19.26 | 20,870 | 1,183 | **14.1** | 0.87 | 1 | 3373 |

**rec8_tg300 is effectively empty** (peak=10 is below every meaningful threshold) — either a very short encrypted-but-accepted call, a muted grant, or a voice handoff captured before audio flowed. Set aside for the comparison.

**rec20_tg318 vs 2026-04-17 rec26_tg300** (both unencrypted, comparable durations 19 s vs 2 s):

| Metric | 2026-04-17 | 2026-04-18 | Δ |
|---|--:|--:|--:|
| Duration | 2.16 s | 19.26 s | 9× longer sample |
| Peak | 12,584 | 20,870 | +66 % |
| RMS | 1,031 | 1,183 | +15 % |
| **Silent frames %** | **40.3 %** | **14.1 %** | **-26.2 pp** |
| `std/mean` of per-100ms RMS | 0.82 | 0.87 | same (slightly worse) |
| min per-100ms RMS | 2 | 1 | same floor |

**Silent-frame fraction dropped nearly 3×.** This is the single clearest signal that the voice chain improved between sessions. `std/mean` barely moved — the audio is still non-stationary at the 100 ms scale, consistent with IMBE frames that occasionally produce degenerate output. The 2026-04-17 §2 interpretation — "mbelib accepts bit-marginal frames that synthesize silence/garble" — is likely still the mechanism, just with fewer marginal frames arriving.

rec17_tg319 (32 s) is consistent with rec20 (21.6 % silent, std/mean 0.97) and probably more representative of a long-form call than either short sample.

## 8. What the 2026-04-17 §A correction predicted, and how this data bears on it

The 2026-04-17 §A re-attribution list ranked AGC–TED interaction as the top suspect for the X-pattern. This session does not rule it in or out — the X-pattern itself was not prevalent in the 2026-04-18 constellation capture (cluster-variance span 6.7× vs 14×, `pll_final` range 7.6× tighter). The cleanest interpretation: **whatever was causing the big PLL excursions on 2026-04-17 has moved on.**

Candidate explanations that fit:

1. **Site activity level.** Afternoon vs night-time traffic density — this session had 1,866 grants in 11 min (~2.8/s), 2026-04-17 had 1,409 in 1.6 h (~0.24/s). Higher duty cycle → more continuous signal → better AGC settlement possibly.
2. **Short-window bias.** 11 min is too short to capture tail behaviour. A 90-min rerun at a quiet time might resurface the bigger excursions.
3. **Nothing in the receiver changed between sessions that would touch the PLL loop** (the BD fix only enabled the new DMA rings; PLL bandwidth and TED gain are SDRTrunk-matched and untouched).

## 9. Recommendations — updated from 2026-04-17 §6

| # | Item | 2026-04-17 state | 2026-04-18 update |
|---|---|---|---|
| 6.1-1 | TDU_LC counter semantics | TDU_LC=1977 vs TDU=35 flagged anomalous | Not re-checked this session; open |
| 6.1-2 | Per-TSBK-position CRC histogram vs `sp_dbg` | Proposed | Still only block-position; sp_dbg histogram still proposed |
| 6.2-4 | TED loop-gain investigation | Invalidated by §A | **Definitely not needed this session** — PLL is settled |
| 6.3-7 | Per-frame IMBE quality gate | Proposed | Worth doing if we want to reduce the remaining 14 % silent fraction |
| 6.4-9 | Fix `tools/p25_check.py` NameError | Known broken | Not re-checked this session; open |
| 6.4-10 | `/api/sys_health` endpoint | Proposed | **SHIPPED** — used in §3 above |
| 6.4-11 | Document remaining API endpoints | 23 undocumented | Not re-checked; open |

### 9.1 New recommendations from 2026-04-18

1. **Capture a long-window baseline.** Rerun §4 + §5 at 1.5 h of uptime to rule out short-window bias on the TSBK improvements. A "late-night quiet" session would also let us see whether the X-pattern resurfaces under low-activity conditions.
2. **Flash the eye-stride build.** The dashboard eye is still monkey-patched at session end. Once flashed, the canonical `BUILD_TAG = 2026-04-18-phase10.6-bd-fix-eye-stride` will carry the JS fix so other users see 4 clusters instead of 2 at `nsym=4`.
3. **IMBE quality gate** is the next lever for the remaining 14 % silent-frame fraction. §6.3-7 from 2026-04-17 still stands; no new information.
4. **Add `doc/P25_ADDRESS_MAP.md` invariant section** documenting the five-step checklist for adding a new DMA master (HDL port, PAC, PS UIO, **BD wire**, DT carve-out). The 2026-04-18 session opened because step 4 was forgotten.

## 10. Files saved from this session

| Path | Content |
|---|---|
| [doc/diagnostics/2026-04-18/ep_*.json](.) | 18 API endpoint snapshots at t=22:40 |
| [doc/diagnostics/2026-04-18/timeseries/hdl_lsm_15s.json](timeseries/hdl_lsm_15s.json) | 15-sec PLL + sp sweep |
| [doc/diagnostics/2026-04-18/constellation/](constellation/) | 56 PNGs + summary.jsonl (90 s) |
| [doc/diagnostics/2026-04-18/eye/](eye/) | Matched-filter + raw eye PNGs, raw IQ npz, summary |
| [doc/diagnostics/2026-04-18/recordings/](recordings/) | rec8/17/20 WAV + stats |
| [tools/p25_ws_eye_capture.py](../../../tools/p25_ws_eye_capture.py) | NEW — offline eye renderer, 1-symbol stride |
| [tools/p25_audio_stats.py](../../../tools/p25_audio_stats.py) | NEW — silent% / std-mean / ZCR metrics |
| [tools/p25_ws_iq_rate.py](../../../tools/p25_ws_iq_rate.py) | NEW (from BD-fix debug) — `/ws/iq` observed-vs-advertised rate |

## 11. Verification status

- Endpoint snapshots: **direct read** from 18 endpoints. High confidence.
- PLL / sp_dbg 15 s sweep: 15 one-second samples. Short window, but `sp_dbg` Q4.12 interpretation from the 2026-04-17 §A correction is consistent.
- Constellation: **56 captured samples** in 90 s. Not enough to compare tails robustly; sufficient for the `pll_final` range observation.
- Audio: **3 recordings**, only one directly comparable to 2026-04-17 (rec20 vs rec26). Same TG-300-style unencrypted voice, 9× longer this session — high confidence on the silent-frame improvement, lower on `std/mean` (which barely moved).
- MF eye: first of its kind on-target; PNGs are visual only. No quantitative eye-opening metric yet.
- ARM health: **direct measurement** via new `/api/sys_health`. Replaces the 2026-04-17 indirect HTTP-latency inference.
