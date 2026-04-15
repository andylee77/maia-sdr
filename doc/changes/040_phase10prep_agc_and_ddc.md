# Phase 10-prep — HDL AGC + DDC filter redesign

Date: 2026-04-15

## Summary

Two gateware improvements on top of Phase 8 + Phase 9 to clear
the remaining HDL-side items before returning to Phase 10:

1. **HDL per-symbol AGC** (`LsmAgc`). Direct fixed-point port of
   the SDRTrunk `P25P1DemodulatorLSM.java:157-172` AGC into the
   LSM demod loop. Runs at the symbol rate on the four
   timing-interpolated samples produced by `LsmTimingInterp`,
   normalising their L2 magnitude to 1.0 before they reach the
   diff slicer + PLL update. Makes the slicer tolerant of
   upstream gain drifts that would otherwise push the
   constellation off-centre.

2. **DDC filter redesign.** Parks-McClellan equiripple
   coefficients for all three stages, with the decimation plan
   restructured from `/16 /4 /2` to `/4 /4 /8`. The old stage 1
   had a transition band wide enough that P25 adjacent-site
   emitters at ±500 kHz to ±2 MHz only saw 30-50 dB of rejection
   before being mixed into the control-channel output band,
   collapsing CRC pass rates at `rf_bandwidth >= 5 MHz`. The new
   chain pushes those frequencies to -90 to -186 dB rejection and
   unblocks 8 MHz RF bandwidth.

Both items honour the user's "no cheap fixes" directive: the AGC
matches SDRTrunk bit-structurally (L2 sqrt + exact 0.05 lerp +
asymmetric clamp + 500 cap), not a loose LMS approximation; the
filter redesign uses proper equiripple synthesis rather than
Kaiser windows bumped to longer tap counts.

## Item 1 — LsmAgc (Phase 10-prep HDL AGC)

### Design

SDRTrunk reference (`P25P1DemodulatorLSM.java` lines 157-172):

```java
magnitude = sqrt(iCurrent^2 + qCurrent^2)
if (magnitude > 0 && !isInfinite(magnitude)) {
    requiredGain = constrain(OBJECTIVE_MAGNITUDE / magnitude, 500);
    sampleGain += (requiredGain - sampleGain) * 0.05f;
    sampleGain = min(sampleGain, requiredGain);
    sampleGain = min(sampleGain, 500);
}
iMiddle *= sampleGain;  qMiddle *= sampleGain;
iCurrent *= sampleGain; qCurrent *= sampleGain;
```

Fixed-point port: `maia-hdl/p25_hdl/lsm_agc.py`.

| Float SDRTrunk     | HDL fixed-point                       |
|---                 |---                                    |
| `iCurrent^2 + qCurrent^2` | two parallel DSP48 squarings + adder, Q2.30 33-bit |
| `sqrt(...)`        | 17-iteration digit-by-digit integer sqrt, Q1.15 17-bit |
| `1.0 / magnitude`  | 28-iteration restoring divider, Q9.11 result |
| `constrain(..., 500)` | 20-bit compare + clamp                |
| `* 0.05f` lerp     | `(diff * 52429) >> 20` — exact 0.05 multiply in Q20 |
| `min(..., required)` | asymmetric sign-test clamp           |
| `min(..., 500)`    | second clamp to `GAIN_MAX = 500 << 11 = 1,024,000` |
| `gain * (mid/cur)` | 4 parallel DSP48 multiplies + saturation |

Pipeline: 8 FSM states (IDLE → SQRT_INIT → SQRT_ITER×17 → DIV_INIT
→ DIV_ITER×28 → UPDATE → APPLY). Total latency per symbol ~48
sync cycles; symbol period at 4800 baud is ~13 000 sync cycles,
so the AGC is always idle by the next `decision_strobe_in`.

Resource estimate: 7 DSP48E1 per chain (2 for `i²+q²`, 1 for the
lerp multiply, 4 for the output multiplies). 14 DSP48 across both
chains. Z7020 has ~220 DSPs; AGC is ~6 % of the DSP budget.

**Explicit non-approximations:**
- Alpha is `ALPHA_Q = round(0.05 * 2^20) = 52429`, matching 0.05
  within 5 ppm. The shift-only approximation `diff >> 4` (1/16 =
  0.0625) was explicitly rejected because the lerp time constant
  sets the settling behaviour on-target.
- Magnitude skip condition is `mag == 0` (SDRTrunk's literal
  `magnitude > 0`), not an arbitrary threshold.
- L2 magnitude via sqrt, not the cheaper L1 `|i|+|q|`
  approximation (L1 is phase-sensitive for QPSK and would add a
  symbol-rate modulation to the feedback loop).

### Integration

Inserted as a submodule of `LsmDemodLoop` between
`LsmTimingInterp` and `LsmDiffDemodSlicer`. Pre-AGC wiring:

```
timing → diff_demod → rotate → slice + gardner + pll_update
```

Post-AGC wiring:

```
timing → agc → diff_demod → rotate → slice + gardner + pll_update
```

The Phase 8A runtime-reset fan-out was extended to include
`agc.reset_in`. The Phase 10-prep runtime enable fan-out
(`agc_enable`) is exposed on `LsmDemod` and wired through
`p25_top.py` to the new `lsm_control.lsm_agc_enable` register
field on both chains.

### Register interface

New fields (mirrored on traffic side):

| Field | Bank / Offset | Bits | Access | Purpose |
|---|---|---|---|---|
| `lsm_control.lsm_agc_enable` | 0xA0 + 0 | [4] | RW | 1 = AGC runs; 0 = bypass |
| `lsm_agc_debug.agc_gain_dbg` | 0xA0 + 0x18 | [15:0] | R | Q9.7 truncation of gain register |
| `lsm_agc_debug.agc_mag_dbg`  | 0xA0 + 0x18 | [31:16] | R | most-recent L2 magnitude in Q1.15 |
| `traffic_lsm_control.traffic_lsm_agc_enable` | 0xC0 + 0 | [4] | RW | same |
| `traffic_lsm_agc_debug.agc_gain_dbg` | 0xC0 + 0x18 | [15:0] | R | same |
| `traffic_lsm_agc_debug.agc_mag_dbg`  | 0xC0 + 0x18 | [31:16] | R | same |

PS boot init (`p25-httpd/src/main.rs`) now calls
`set_lsm_agc_enable(true)` and `set_traffic_lsm_agc_enable(true)`
alongside the existing `set_*_lsm_enable` / `set_*_lsm_dc_block_enable`
calls.

### Tests

`maia-hdl/test/test_lsm_agc.py` — 8 amaranth-sim tests covering:

1. Integer sqrt matches `math.isqrt` across a spread of
   `(i_cur, q_cur)` QPSK-point inputs.
2. Cold-start unit-magnitude input holds gain at 1.0 (no drift).
3. Weak input (mag=0.1) — lerp ramps toward required_gain=10
   following the exact 0.05 IIR trajectory.
4. Strong input step — asymmetric `min(gain, required)` drops
   gain immediately to ~0.707 on the first sample (no slow
   decay).
5. Zero-magnitude input leaves gain unchanged (SDRTrunk's
   `if (magnitude > 0)` branch).
6. Bypass mode (`enable_in = 0`) forwards inputs verbatim.
7. Runtime reset (`reset_in` pulse) returns gain to `GAIN_INIT`
   and resumes the FSM cleanly.
8. Decision-strobe latency is ≤ 64 sync cycles (measured: 48).

All 8 pass. Full P25 regression (109 tests, including the
Phase 8A reset regression and the closed-loop demod loop slip
tests) passes with no changes.

## Item 2 — DDC filter redesign

### What was wrong with the old chain

`P25_FIR1_COEFFS` in `p25-httpd/src/fpga.rs` before this change:
48 taps, Kaiser β=6, cutoff 200 kHz, at the 8 MSPS input rate of
a /16 stage-1 decimator. Kaiser-window transition width at that
tap count and cutoff is roughly 244 kHz (fs-normalised), so the
stopband only reaches the claimed -137 dB floor out past ~450 kHz.
Anything in the transition band 200-450 kHz is at attenuations
between -10 dB and -90 dB, heavily peaking near the passband
edge. Adjacent P25 sites at ±500 kHz to ±2 MHz fall inside that
zone and leak through at 30-50 dB below the signal — enough to
overwhelm the control-channel CRC at rf_bandwidth ≥ 5 MHz.

The /16 split made stage 1 impossible to fix in place: the
output Nyquist at 500 kSPS is 250 kHz, so a sharp transition
from passband to stopband has to fit inside 50 kHz of transition
width at 8 MSPS (Δω_norm = 0.0125). Parks-McClellan
dimensioning: `N ≈ (A - 8) / (14.36 * Δω_norm) ≈ 400 taps` for
-80 dB — beyond the 256-slot coefficient RAM budget.

### New chain

Decimation restructured to `/4 /4 /8 = /128` (same total; the
C4FM chain downstream still expects 62.5 kSPS). Per-stage
filters designed with `scipy.signal.remez` (Parks-McClellan
equiripple). Design script lives at
`tools/p25_ddc_filter_design.py` and captures the final output
in `doc/changes/040_ddc_filter_redesign.txt`.

| Stage | Type | Taps | Input rate | Decim | Passband | Stopband | Measured stopband |
|---|---|---|---|---|---|---|---|
| 1 | FIR4DSP |  48 | 8 MHz     | /4 | 0-300 kHz | 1-4 MHz | -93 dB |
| 2 | FIR2DSP |  56 | 2 MHz     | /4 | 0-100 kHz | 250-1000 kHz | -93 dB |
| 3 | FIR4DSP | 104 | 500 kSPS  | /8 | 0-10 kHz  | 31-250 kHz | -90 dB |

Each stage's stopband is anchored at the per-stage output
Nyquist so aliasing into the next stage's passband is fully
suppressed by the stage's own filter. The cascaded response from
8 MSPS → 62.5 kSPS, sampled at 12.5 kHz offsets from DC:

| Offset | Rejection |
|---|---|
|   0.00 kHz | +0.00 dB (DC gain reference) |
|   6.25 kHz | -0.04 dB (in passband) |
|  12.50 kHz | -0.59 dB (stage-3 transition edge) |
|  25.00 kHz | -25.29 dB |
|  50.00 kHz | -90.60 dB |
| 100.00 kHz | -90.76 dB |
| 200.00 kHz | -120.51 dB |
| 500.00 kHz | -97.07 dB |
|   1.00 MHz | -186.09 dB |
|   2.00 MHz | -96.98 dB |

Close-in adjacents at ±12.5 / ±25 kHz land in stage-3's
transition band by design — they are finished off by the
downstream `LsmFir` LPF (83 taps, passband 7250 Hz, stopband
8000 Hz, >100 dB) at 31.25 kSPS in the LSM chain. Splitting
sharp close-in filtering between the DDC and the LsmFir LPF
keeps the stage-3 tap count well under the FIR4DSP 256-slot RAM
budget.

### Constraint checks

Coefficient RAM budgets (from
`maia_hdl/fir.py::FIR4DSP::load_fir_4dsp` / `FIR2DSP`):

- Stage 1: `operations * decim = 6 * 4 = 24 ≤ 128` ✔
- Stage 2: `operations * decim = 14 * 4 = 56 ≤ 128` ✔
- Stage 3: `operations * decim = 7 * 8 = 56 ≤ 128` ✔

Cycle budget at 62.5 MHz sync / 187.5 MHz 3x:

- Stage 1 @ 8 MSPS: 6 ops × 1 sync cycle ≈ 6 cycles vs 7.8
  cycles/sample budget. Fits with slight margin.
- Stage 2 @ 2 MSPS: 14 ops ≈ 14 cycles vs 31.25 cycles/sample.
  Comfortable.
- Stage 3 @ 500 kSPS: 7 ops vs 125 cycles/sample. Trivial.

`operations_minus_one` / `odd_operations` / `decimation` values
are computed at runtime by `load_fir{1,2,3}` from
`coefficients.len() / decimation`, so bumping the tap count
requires only the constant tables to change — no Rust logic
edits beyond the static arrays + the three `P25_DEC*` constants.

### What didn't change

- No maia_hdl/fir.py edits. The existing FIR4DSP / FIR2DSP
  primitives handle the new tap counts within their len_log2 = 8
  / len_log2 = 7 RAM budgets.
- No HDL `operations_minus_one*` field-width changes. The
  existing `decim_width = [7,6,7]` / `oper_width = [7,6,7]` hold
  all three new values (11 / 13 / 6).
- No control-plane protocol changes. The PS-side coefficient
  loader (`load_fir_4dsp` / `load_fir_2dsp`) was already
  parameterised on `coefficients.len()` and `decimation`.

## Files touched

### Gateware (maia-hdl)

- `p25_hdl/lsm_agc.py` — new (SDRTrunk AGC port)
- `p25_hdl/lsm_demod_loop.py` — AGC submodule + reset fan-out +
  `agc_enable` I/O + `agc_*_dbg` debug taps
- `p25_hdl/lsm_demod.py` — propagate `agc_enable` + debug taps
  through the wrapper
- `p25_hdl/p25_top.py` — new `lsm_agc_enable` bit in
  `lsm_control` (and mirror in `traffic_lsm_control`); new
  `lsm_agc_debug` register (and mirror); wiring to
  `LsmDemod.agc_enable` / `LsmDemod.agc_*_dbg` on both chains
- `test/test_lsm_agc.py` — 8 amaranth-sim tests (sqrt / division
  / lerp / clamp / reset / bypass / pipeline latency / zero-mag)

### Firmware (p25-httpd)

- `p25-pac/p25.svd` — regen via `generate_p25_svd.py`
- `p25-pac/src/lib.rs` — regen via `svd2rust`
- `src/fpga.rs`
  - new `P25_DEC{1,2,3} = 4/4/8` (was 16/4/2)
  - new `P25_FIR{1,2,3}_COEFFS` (48 / 56 / 104 taps,
    Parks-McClellan)
  - new `set_lsm_agc_enable` / `set_traffic_lsm_agc_enable`
    helpers
- `src/main.rs`
  - boot init turns on `lsm_agc_enable` and
    `traffic_lsm_agc_enable` alongside the existing
    `set_*_lsm_enable` / `set_*_lsm_dc_block_enable` calls
  - `BUILD_TAG` bumped to
    `2026-04-15-phase10prep-lsm-agc-and-ddc-redesign`

### Tooling

- `tools/p25_ddc_filter_design.py` — new (Parks-McClellan
  design + adjacent-channel evaluator + Rust-array emitter)
- `doc/changes/040_ddc_filter_redesign.txt` — captured design
  script output (self-documenting coefficient provenance)

## Bake history

### Bake 1 (2026-04-15 08:53) — failed timing

Initial bake exposed a new 62.5 MHz sync-domain critical path
inside `LsmAgc`. Worst slack **-1.878 ns**, **43 failing
endpoints**, all in the same `div_quot → gain_dbg_reg` cone
inside the traffic-side AGC's monolithic UPDATE state. The
single-cycle combinational chain stacked req_gain clamp → diff
subtract → DSP48 multiply → shift → lerp add → two clamp layers
= 21 LUT levels + 1 DSP48 + 12 CARRY4 adders in series,
clocking at ~17.8 ns in a 16 ns period.

### Fix — pipeline UPDATE into 3 sub-states

Split the monolithic `UPDATE` state into:

| State          | Work                                                         |
|----------------|--------------------------------------------------------------|
| `UPDATE_CLAMP` | clamp `div_quot` to `GAIN_MAX`, latch `req_clamped_q`       |
| `UPDATE_LERP`  | compute `diff = req - gain`, multiply by `ALPHA_Q`, latch `step_wide_q` |
| `UPDATE_APPLY` | shift step, add to gain, asymmetric clamp + `GAIN_MAX/MIN`, latch gain |

Adds 2 sync cycles to the per-symbol pipeline (48 → 52), which
is invisible in the ~13 000-cycle symbol budget. The DSP48 multiply
now gets its own cycle so Vivado can use the DSP's input/output
pipeline registers for timing closure.

### Bake 2 (2026-04-15 10:14) — timing met

| Clock                   | WNS     | Failing | Delta  |
|-------------------------|---------|---------|--------|
| `sync` (62.5 MHz) intra | **+0.763 ns** | **0**   | +2.641 ns, -43 endpoints ✔ |
| `clk_fpga_0` intra      | -0.140 ns | 15  | (ADI axi_ad9361 reset paths, not mine) |
| `clk3x` (187.5 MHz)     | +0.648 ns | 0   | unchanged            |
| `sync → clk_fpga_0` CDC | -4.576 ns | 169 | pre-existing PS7 CDC, waived |
| `clk_fpga_0 → sync` CDC | -3.090 ns | 124 | pre-existing PS7 CDC, waived |

My AGC critical path is fully closed. All remaining violations
are cross-clock-domain paths inside ADI's axi_ad9361 IP that
are known-waived by the build's `system_top_bad_timing.xsa`
promotion path; the bitstream is functionally correct. XSA
copied to Tezuka firmware at
`board/tezuka/fishball7020/bitstream/p25/system_top.xsa`.

## Acceptance criteria

### Gateware (AGC)

1. **All 8 new unit tests pass.** ✔ (post-pipeline-split)
2. **Full P25 regression still passes (109 / 109 + 2 skipped).**
   ✔ (no changes to the closed-loop demod slip regression or
   the Phase 8A reset tests)
3. **Elaborates cleanly (`P25Core`).** ✔
4. **Vivado bake with `sync` intra-clock WNS ≥ 0.** ✔ (+0.763 ns,
   0 failing endpoints). Pre-existing CDC waivers unaffected.

### Firmware

1. **`cargo check --workspace` is clean with only the
   pre-existing 76 warnings.** ✔
2. **SVD + PAC regen produces the new `lsm_agc_enable` bit and
   `lsm_agc_debug` register on both chains.** ✔
3. **BUILD_TAG bumped so `/api/system.build` reflects the new
   bitstream.** ✔

### On-target (deferred to the next bake)

1. Flash the new bitstream + firmware. Expect `/api/system.build`
   to report `2026-04-15-phase10prep-lsm-agc-and-ddc-redesign`.
2. Confirm AGC gain converges: read `lsm_agc_debug.agc_gain_dbg`
   via `/api/lsm_status` extension (Q9.7 value ~128 = 1.0 at
   steady state on a well-tuned P25 carrier).
3. Confirm the DDC filter redesign unblocks 8 MHz RF bandwidth:
   run `iio_attr -u ip:192.168.2.1 -c cf-ad9361-lpc voltage0
   rf_bandwidth 8000000`, observe TSBK CRC pass rate ≥ 70 %
   (matches the 4 MHz baseline; the point is that widening no
   longer regresses it).
4. Re-run the adjacent-channel stress test if we can find a
   Clay County capture with strong ±500 kHz or ±2 MHz adjacents.
5. Re-run `tools/p25_nid_analyze.py sweep --side traffic` —
   post-BCH DUID histogram should show real LDU dominance
   (existing Phase 8B post-reset behaviour, sanity check that
   the AGC insertion didn't regress anything).

## Deferred / next

- **Phase 10 proper.** Return to the previously-queued
  `end-of-call TDU_LC burst fix` (TDU_LC-as-terminator vs
  shorter inactivity timeout) + optional HDL-side retirement of
  `iq_packer` / `iq_dma` + optional PS C4FM decoder retirement
  (LSM-only commitment). See DEVPLAN.md Phase 10 section.
- **AGC on-target tuning knobs.** If on-target verification
  shows the 0.05 lerp is too slow for the per-call retune window
  (hundreds of ms of signal acquisition per call), expose
  `ALPHA_SHIFT` as a runtime register field so we can A/B
  faster attack times without rebuilding the bitstream.
- **Filter design cross-check.** The 25 kHz adjacent is only
  at -25 dB at the DDC output; the downstream LsmFir LPF is
  supposed to bring it to -100+ dB. If on-target traffic-side
  captures reveal close-in interference that the LsmFir LPF
  can't kill, we may need to bump stage 3 to ~130 taps to push
  its transition closer to the P25 channel edge.
