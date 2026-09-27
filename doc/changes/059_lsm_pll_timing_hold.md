# 059 — LSM PLL/timing hold on "no signal" (carrier-gap trap fix)

**Date:** 2026-09-27. **Branch:** fishball-p25. **Bake required:** yes. The gateware
change carries P25 core version 0.2.0. The register map is unchanged: the SVD differs
only in its version string. p25-httpd reads the version and adapts, so one binary runs
on either bitstream (BUILD_TAG `2026-09-27-signal-hold-preempt-059`).

## Why

The traffic LSM chain lost the second and later transmissions on a channel. After a
carrier gap its PLL sat at the clamp (`pll_dbg` ±8579, π/3) and every dibit came out
rotated one quadrant: 0 IMBE until a PS reset. Board (2026-09-26): pll 288 → 7051 →
8333 within ~0.5 s of gap noise; a manual reset un-trapped it. The PS watchdog
(`p25-httpd/src/app/traffic_pll_watchdog.rs`) papers over it: tone scene 0 % → 639/639.

Mechanism (reproduced in simulation, below):

- The decision-directed PLL keeps updating on the noise between transmissions. On
  noise the per-symbol error is uniform in ±π/4. Each step is clamped to ±0.03 rad,
  so the accumulator random-walks and reaches the ±π/3 clamp within a fraction of a
  second.
- With the clamp above π/4, the clamp ends are absorbing. With a small true offset b,
  a PLL at ±π/3 puts the constellation more than π/4 off. The slicer then picks the
  neighbouring quadrant and the error pushes further into the clamp. For clamp C the
  +C end traps for b in (C − π/2, C − π/4) and the −C end for b in (π/4 − C, π/2 − C).
  For C = π/3 both ends trap for |b| < 200 Hz, which is the normal operating point.
- The next transmission therefore starts from a wrong-quadrant stable state.

## Change (maia-hdl/p25_hdl)

- `lsm_agc.py`: new registered output `gated_out`. It is the same decision as the idle
  gate (`mag < mag_update_threshold`, gain update skipped) and is valid with
  `decision_strobe_out`. It is 0 in BYPASS and after reset, and never 1 with threshold 0.
- `lsm_signal_hold.py` (new): `LsmSignalHold`, hysteresis on `gated_out`:
  - hold after 4 consecutive gated symbols;
  - release after 8 consecutive non-gated symbols;
  - `reset_in` holds.
- `lsm_demod_loop.py`: instantiates it and feeds `hold` to the PLL update. The Gardner
  correction strobe into `LsmTimingInterp` is masked while held; the TED keeps running.
  New parameters `hold_enter_symbols` (4), `hold_exit_symbols` (8; 0 removes the logic
  and is bit-identical to the old loop) and `max_pll_abs_q13`. New port `hold_dbg`
  (simulation only, not in the register map).
- `lsm_pll_update.py`: `hold_in` on both forms. It is sampled with `symbol_strobe` and
  folded into the CORDIC form's existing zero-input skip. When held the accumulator is
  unchanged; `pll_strobe` still fires. `MAX_PLL_ABS` changes from π/3 to 0.65 rad
  (below). There is also a per-instance `max_pll_abs_q13` (default: the module
  constant).
- `lsm_pll_rotate.py`: comments only (the clamp range inside the LUT).
- `lsm_demod.py`: passes the parameters through, plus `hold_dbg`.

Both `p25_top` chains, control and traffic, get the hold (defaults). Registers are
unchanged: the gate threshold is the existing `*_lsm_agc_config.mag_update_threshold`
(256).

### Choices

- **Gate source.** The AGC gate already separates "signal" from "carrier-gap noise" on
  the bench: gap noise sits below 256 and the signal around 1000–1200. Reusing it means
  one threshold register tunes both behaviours.
- **What is held.** The PLL state is a frequency offset (phase per symbol) and the
  timing state is the symbol-clock phase. Both change slowly, so the value that was
  right at the end of the last transmission is right for the next one on the same
  channel. Freezing them across a gap or fade costs nothing.
  - The AGC gain is already frozen by the gate.
  - Gardner corrections on noise are a random walk too. They are small because the
    noise is scaled by the gain frozen from the signal, but useless, so they are masked
    as well. A new talker's timing phase is arbitrary in either case.
- **Hysteresis.**
  - Enter after 4 gated symbols (0.8 ms). At most 3 noise symbols reach the PLL, each
    moving it by at most 0.03 rad. Isolated sub-threshold symbols inside a weak
    transmission do not freeze tracking.
  - Leave after 8 consecutive symbols above the gate (1.7 ms). Noise that crosses the
    gate a few percent of the time essentially never produces that run.
  - The reset state is "held", so a freshly tuned channel waits for 8 good symbols
    before the loops adapt. This adds 1.7 ms to acquisition.

### Accumulator clamp: 0.65 rad (secondary guard)

`MAX_PLL_ABS` changes from π/3 to 0.65 rad (`MAX_PLL_ABS_Q13` 8579 → 5325). This is
below π/4, so near the operating point neither clamp end is absorbing. Noise-free
attractors for a true offset b (1 rad/symbol = 764 Hz):

| Clamp | Q2.13 | Unclamped range | Trap-free zone around 0 | An absorbing end exists for | Interior wrong-quadrant lock for |
|---|---|---|---|---|---|
| π/3 (old) | 8579 | ±800 Hz | none | \|b\| < 400 Hz (both ends for \|b\| < 200 Hz) | \|b\| > 400 Hz |
| 0.72 rad | 5898 | ±550 Hz | \|b\| < 50 Hz | 50–650 Hz (far end only) | \|b\| > 650 Hz |
| 0.65 rad (new) | 5325 | ±497 Hz | \|b\| < 103 Hz | 103–703 Hz (far end only) | \|b\| > 703 Hz |

Auto-PPM keeps the control-channel residual within tens of Hz, and traffic channels
share the reference, so 0.65 rad puts the operating point inside the trap-free zone. It
is the fallback where the hold cannot engage: noise above the gate (a noisier site, or a
lowered threshold). Tracking cost: above 497 Hz the accumulator sits at the clamp. The
rest of the offset is left as a constellation rotation (0.10 rad at 575 Hz), and it still
decodes to ±575 Hz, as π/3 does (sweep below). A cold start fails at ±600 Hz (π/4 per
symbol, the slicer boundary) with either clamp. 0.72 rad would trade half the trap-free
zone for 50 Hz more unclamped range. Auto-PPM
Stage B and the tracker read the control PLL within ±497 Hz instead of ±800 Hz; Stage A's
FFT step already brings the residual inside that.

## Evidence (simulation)

Tool: `tools/p25_lsm_hdl_replay.py` (bit-true front end + the Amaranth `LsmDemod`, see
058). New scenario options, none of which add a reset:

- `--gap S --append-wav B.wav` (next transmission after S s of noise);
- `--gap-at T:S` (a gap inside one input);
- `--noise-db`/`--noise-seed`;
- per-part summary rows: syncs by rotation, valid NIDs, first sync, PLL start/end/range,
  % of symbols gated and held.

Gateware variants:

- `--hdl-root` (HEAD export, the board's gateware);
- `--legacy` (hold removed, π/3; bit-identical to HEAD over 8638 symbols including clamp
  hits);
- `--hold-enter`/`--hold-exit`/`--pll-clamp-q13`.

Level: RMS 1230 (board level). Columns: HEAD = the board's gateware (π/3, no hold);
"hold" = this change with π/3; "hold + 0.65" = this change as committed.

| Scenario | HEAD | hold | hold + 0.65 |
|---|---|---|---|
| Next transmission after a gap, no reset: 19 runs (`_79` → 1 s/3 s noise −20 dB → `_80`, 4 seeds each; `_70` self-splice ×4; near-gate noise −12 dB ×4; CFO ±300 Hz; weak signal) | **3/19 lost** (PLL returns at ≈ −7500 → pinned 96 %, 0 upright syncs, 0 NIDs) | 0/19 lost | 0/19 lost |
| PLL movement during the gaps | full range, ±8579 | ≤ 0.11 rad (3 symbols before the hold engages), then frozen | same |
| Gardner corrections during a 3 s near-gate gap (noise mag ≈150, seed 1) | 13.7 % of symbols, −11.4 samples of drift | 0 | 0 |
| Gap noise above the gate (gate 32, noise mag ≈60; 4 seeds) | 4/4 lost (pinned +8579) | 4/4 lost | **0/4 lost** (PLL returns at the clamp, 5325, and pulls in; first sync 53 ms) |
| Cold start, 2 s: `_79`, `_70`, A's capture | 28 / 20 / 34 syncs, 10 / 11 / 10 NIDs | identical | identical |
| CFO sweep −575…+575 Hz, cold (11 points: 0, ±150, ±300, ±450, ±525, ±575; 17 syncs in the clip) | 17/17 syncs, 8 NIDs at every point | identical (±525/575 not run) | identical (at the clamp 38–77 % of the time at ±525/575 Hz, still 17/17) |
| CFO ±600 Hz (= π/4/symbol), cold | 0 upright (17 rotated) | same | same |
| Weak signal, RMS 300 / 400 / 550 (mag ≈285, 33 % / 5 % / 0 % of symbols gated) | 28 syncs, 10 NIDs each | identical (held 41 % / 6 % / 0 %) | identical |
| Control chain, continuous CC recording (084247 +60 s, 8 s, fading: 3 % gated) | 104 syncs, 34 NIDs | identical (held 3 %) | identical |

The first frame sync after a gap came within 20 ms of the carrier's return in 9/19 hold
runs and 6/19 HEAD runs. Otherwise the next sync, about 45 ms later, was the first: the
new talker's symbol timing is re-acquired either way.

Amaranth tests (`maia-hdl/test`):

- `test_lsm_signal_hold.py` (new, 12). Hysteresis against a Python model; reset holds;
  isolated gated symbols do not hold. Demod loop on the synthetic golden with 150 Hz CFO
  → sub-gate noise gap → golden again, no reset: the PLL is constant through the gap;
  sample_point advances only by the nominal samples-per-symbol (no Gardner corrections);
  the second segment decodes 100 %. The legacy loop walks > 1000 Q2.13 with Gardner
  corrections.
- `test_lsm_agc.py` (+3): `gated_out` against the threshold boundary, `gate_dbg`,
  threshold 0, BYPASS and reset.
- `test_lsm_pll_update.py` (+6): `hold_in` freezes both forms and is sampled with the
  strobe; hold combines with the (0, 0) skip; a seed reset works while held; the
  per-instance clamp; range checks.

LSM and p25 suites: 149 passed, 2 skipped, 2 failed. The two failures,
`test_lsm_agc::test_reset_in_restores_gain_to_init` (expects the pre-2026-04-29
`gain_dbg` reset value) and `test_lsm_nid_bch_fec::test_error_correction_at_t1_t6_t11`,
fail identically on HEAD.

## Change (p25-httpd)

- **Core version:** `p25_top._version` and `build_fpga.bat` `IP_CORE_VERSION` go from
  0.1.0 to 0.2.0. `p25-pac/p25.svd` carries the new version string; the registers are
  unchanged.
- **`hardware/core_version.rs` (new, host-tested):** `CoreVersion` decodes the
  `version` register, which `IpCore::take` reads once.
  - `has_lsm_signal_hold()` is true from 0.2.0.
  - `pll_clamp_q213()` is 8579 before 0.2.0 and 5325 from 0.2.0.
  - The startup log line reports the version and the hold.
- **`traffic_pll_watchdog.rs`:** the watchdog runs only on gateware older than 0.2.0.
  On 0.2.0 an onset reset would discard the held, correct PLL value. A PLL pinned at the
  0.65 rad clamp means a real offset beyond ±497 Hz, which a reset cannot help.
  `/api/traffic` `pll_watchdog.enabled` shows which case applies.
- **`grant_follower.rs`:** `resume_needs_reset(pll, ms_since_voice, clamp)` takes the
  running gateware's clamp instead of the fixed π/3 constant. The same-frequency resume
  still resets a chain that has been idle for more than 1 s, which is the proven 057 path.
- **`/api/traffic` `traffic_lsm_chain`:** new fields `core_version`, `signal_hold` and
  `pll_clamp_q13`.
- **Sticky lock released at the end of transmission (`grant_follower.rs`).** The Mode B
  corpus lost 3 transmissions this way. Another TG's grant arrived while the locked call
  waited out its 2 s `end_grace_ms` after the TDULC. SDRTrunk had already freed its
  channel by then: 1.13–1.65 s after the last voice, p5–p90 over 213 traffic recordings.
  - The lifecycle publishes the active call's pending end marker
    (`ImbeForwarder::active_end_marker`, from `mirror_active`).
  - Once the marker has been pending for `END_PREEMPT_AFTER_MS` (600 ms), the sticky
    gate lets another TG's grant take the chain. That wait leaves room for the
    resumed-voice check, and the chain is still freed before SDRTrunk's p5.
  - The log reason is `end_marker_preempt`.
- **Re-follow after a sticky reject.** A clear grant rejected by the sticky gate is
  remembered, the same way 057 remembers timeout closes. A grant update for the same TG
  and frequency within `REFOLLOW_STICKY_MS` (2 s) of the reject follows it once the chain
  is idle or its call has ended, so the rest of that transmission is recovered. The log
  reason is `refollow_after_sticky`.
  - **Why the window is 2 s.** Voice starts about 0.5 s after the grant, and a clear
    transmission lasts 1.8 s (median; p25 1.44 s, p75 3.06 s over the corpus's 241).
    Later updates mostly come from the system's hang after the transmission.
  - **What an unbounded window cost.** The first build used the 30 s timeout window.
    TG 318 was re-followed 4.9 s after its reject and 1.8 s after its transmission had
    ended, and TG 319's next real grant, 0.5 s later, was rejected: 99 frames lost.

**Targeted scenes** (`_134834_98`, `_094704_28`, `_144747_59`, `_100134_365`, 53 followable
clear transmissions, first build with the 30 s window):

| Build | IMBE | Missed |
|---|---|---|
| 0.2.0, sticky gate as in 057 | 8487/9090 (93.4 %) | 4: the 3 end-grace rejects and the out-of-band grant |
| + end-marker preempt + re-follow (30 s window) | 8910/9099 (97.9 %) | 1: the late re-follow described above |

**Full Mode B corpus, final build** (BUILD_TAG `2026-09-27-signal-hold-preempt-059`, 2 s
re-follow window, core 0.2.0):

| Build | Followable clear | IMBE of SDRTrunk's | Missed | Partial |
|---|---|---|---|---|
| 0.1.0 + PS watchdog (057c) | 219 | 31455/34029 (92.4 %) | 21 | 3 |
| 0.2.0 (hold only) | 219 | 33291/34029 (97.8 %) | 4 | 3 |
| 0.2.0 + end-marker preempt + re-follow (this change) | 219 | 33795/34038 (99.3 %) | 0 | 6 |

- 0 relay underruns.
- 11 of the 14 short rows lose 9–36 frames, one or a few LDUs. The largest losses are two
  transmissions at scene edges: 54/108 and 81/117, the scenes where the replayed CC runs
  ahead of the traffic recordings (058).
- The two-tone scene reports one single-frame amplitude dip at tone frame 9 in this run
  and in the watchdog run, and none in the hold-only run. It is the same frame each time,
  so it is not random loss. Not yet compared against SDRTrunk's MP3.
- The Rust reference loop (`lsm/demod.rs`) keeps π/3; the golden vectors never reach
  the clamp.

## Board verification (2026-09-27, unit A, B replaying the Mode B corpus)

**Bake:** timing met, WNS +0.255 ns, WHS +0.006 ns.

**Deploying without a Tezuka rebuild.**

- The bitstream is compressed, so its size varies per build. The new one is 13,120 bytes
  shorter than the partition in A's May BOOT.bin.
- `bootgen -read` shows that partition has no data checksum (`checksum_offset` 0) and no
  authentication. The new `.bit.bin` was padded after its DESYNC with NOOP words
  (0x20000000) to the old length and swapped in at byte 104,256.
- The header dumps are identical. The original is at `/mnt/sd/bench/backup/BOOT.bin.core010`.

**Result.** p25-httpd reports core 0.2.0, the signal hold and a 5325 clamp, and leaves
the watchdog off.

**Tone / PLL-trap scene `B_20260503_084247_1695`, watchdog off:** 639/639 IMBE (100 %). The
traffic `pll_dbg`, polled every 200 ms:

- tracked −1046…+114 with signal;
- stayed at exactly −1827 for 25 s of gap noise;
- never exceeded |1827|.

On the old gateware the same gaps walked it to ±8579.

**Full Mode B corpus** (42 scenes, 320 transmissions). Both runs were scored with the
corrected scorer (`fbench analyze`):

| Build | Followable clear | IMBE of SDRTrunk's | Missed | Scenes losing their first calls |
|---|---|---|---|---|
| 0.1.0 + PS watchdog (057c) | 219 | 31455/34029 (92.4 %) | 21 | 10 |
| 0.2.0, watchdog off | 219 | 33291/34029 (97.8 %) | 4 | 0 |

**The hold also fixed control-channel acquisition.**

- On 0.1.0, 17 transmissions were lost at scene starts. After the ~20 s silence between
  scenes, the control chain decoded no grant for 4 s to over 30 s. The PS watchdog only
  guards the traffic chain, and nothing ever reset the control chain.
- With the hold on both chains, every scene decodes its first grant.

**The 4 remaining misses:**

- 3 are sticky-lock rejects inside the 2 s end grace, fixed below.
- 1 is a grant to 856.4375 MHz, which the replay does not carry.

## Bake notes

- No register-map change: the SVD generated from this tree matches
  `p25-httpd/p25-pac/p25.svd` apart from the version string, so the PAC code is
  unchanged. `p25_top` converts to Verilog; `signal_hold` appears in both chains, and
  the clamp constant is `14'h14cd`.
- Logic per chain:
  - one FF (`gated_out`), sharing the gate comparator;
  - `LsmSignalHold`: a 4-bit counter, one FF and about 10 LUTs;
  - one AND on the Gardner strobe into `LsmTimingInterp`;
  - one OR into the PLL skip flag.

  All inputs are registered. There is no new DSP or BRAM, and nothing enters the known
  tight cones (AGC UPDATE, timing-interp `sample_point` → DSP) beyond one extra input
  on the adjust-enable LUT.
- The hold threshold is `*_lsm_agc_config.mag_update_threshold` (256). Threshold 0, or
  the AGC disabled, turns the hold off.
