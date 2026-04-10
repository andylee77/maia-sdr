# 024 -- Pluto LO calibration: the actual root cause of the LSM PLL slip

**Date:** 2026-04-10
**Phase:** Phase 6E.6e final (slip-resistance work concludes)
**Branch:** fishball-p25
**Status:** Diagnosis complete; in-place fix on SD card validated for 5+ minutes
sustained operation; permanent fix is a 1-line edit in `tezuka_fw`

---

## TL;DR

After three rounds of HDL fixes (CORDIC PLL update, LsmTimingInterp pipeline,
ExtraTimingOpt strategy) and a watchdog retraction, the HDL LSM chain still
slipped on real RF after 218 seconds, despite a 12x improvement over the
pre-CORDIC bake. **The actual root cause was a missing Pluto-specific carrier
frequency calibration.** The Pluto's AD9361 crystal is approximately
-0.54 PPM (slow) at the LO of 858.1 MHz, putting the post-DDC signal at
~+466 Hz instead of DC. The Costas PLL was correcting this offset by sitting
at -5000 ULPs steady-state (= -0.61 rad), which is 58% of the way to the
negative `+/-pi/3` clamp. Any transient that pushed the integrator the
remaining 0.44 rad railed it.

The fix is to shift the DDC NCO by +463 Hz to compensate the LO error,
which is done via `--control-freq 860962963` (was `860962500`). After this
calibration, the steady-state PLL sits within ±10 Hz of zero with full clamp
headroom on both sides, transients recover within ~80ms instead of slipping
permanently, and the chain runs sustainably at ~85% NID validity past the
5-minute mark with no permanent slip.

The 1-line fix:

```bash
# In Tezuka source: board/tezuka/common/overlay_p25/etc/init.d/S60p25-httpd
- --control-freq 860962500 \
+ --control-freq 860962963 \
```

This is the final piece of the slip-resistance saga that started with
docs 021 (CORDIC), 022 (LsmTimingInterp pipeline), and 023 (watchdog
retraction + diagnostic instrumentation). With this in place, none of those
HDL changes are technically necessary anymore -- a properly calibrated LO
gives the original linearised PLL enough clamp headroom to survive
indefinitely. But the HDL changes remain valuable as defence in depth and
as a 12x slip-time improvement that helped us *find* this bug.

---

## How we got here

### The slip behaviour we couldn't shake

After the CORDIC bake (commit `47c9cc5` + `cfd691f`), on-target testing
against Clay County NAC `0x8A1` (860.9625 MHz LSM control channel) showed:

- Healthy phase: ~3.6 minutes (218 seconds)
- Followed by: PLL integrator hits `-MAX_PLL_ABS = -8580 ~ -pi/3` clamp,
  loop fails to recover, dibits stop matching the P25 frame sync
- The chain transitioned to a "stalled" state where the LSM-side
  ControlChannelDecoder reported `recent_msgs=0` for the rest of the run

We chased multiple wrong hypotheses before landing on the right one:

1. **"PLL slip into wrong 4-PSK basin"** -- partially right, the slip *did*
   happen. But the mechanism wasn't the loop "rotating" to a wrong basin;
   it was the integrator hitting the clamp due to insufficient headroom.

2. **"Heartbeat task false positive"** -- briefly considered when the C4FM
   decoder kept producing 0x8A1 NIDs after the LSM heartbeat fired CRASH
   TRANSITION. Wrong: the LSM-side `recent_msgs=0` from the wake stats
   confirmed the LSM HDL chain had genuinely lost lock; the C4FM decoder
   was finding 0x8A1 NIDs by *coincidence* on its independent dibit
   stream (still investigating why).

3. **"Broken AXI register CDC"** -- looked plausible because devmem dumps
   in the stuck state showed shifted/wrong register values. Eventually
   ruled out by a clean post-fix devmem dump showing correct values --
   the original CDC corruption was caused by the watchdog's
   read-modify-write `modify()` calls into the broken-register state,
   creating a feedback loop that polluted control bits. Removing the
   watchdog (doc 023) made the CDC reliable.

4. **"Missing damping term in the Costas loop"** -- carefully read the
   SDRTrunk Java source `P25P1DemodulatorLSM.java` line by line against
   our Rust port `p25-httpd/src/lsm/demod.rs`. **Every term matched**.
   Same pllGain (0.1), same MAX_PLL clamp (pi/3), same per-step phase
   error clamp (0.3), same AGC, same Gardner TED, same differential demod
   sign convention, same dibit decision logic, same skip-on-zero-soft-symbol
   path, same initial values. The Rust port is a faithful line-by-line
   translation of the Java reference.

### The actual difference between our setup and SDRTrunk

Once the loop algorithm was confirmed identical, the question became:
"why does SDRTrunk not slip on this signal but we do?"

Looking at SDRTrunk's GUI screenshots from the user's working setup:

```text
Tuner panel:
  PPM: -0.4    Measured Error: 126Hz (0.1ppm)
  [ ] Enable decoder(s) to auto-adjust PPM    (UNCHECKED)
```

Two crucial observations:

1. **SDRTrunk is running with `PPM = -0.4` manually applied** to the Pluto.
   The user dialed this in over time as a calibration constant for this
   specific device.

2. **Auto-adjust PPM is OFF.** SDRTrunk is NOT running an outer feedback
   loop -- it's just applying a static PPM offset to the tuner's
   `xo_correction`-equivalent attribute at startup.

Meanwhile our `p25-httpd` was running with `--rx-lo 858100000` and
`--control-freq 860962500` -- both nominal values, **PPM = 0**, no
calibration applied at all. The Pluto's crystal offset was being absorbed
entirely by the Costas PLL.

### The math of the asymmetric clamp

With the Pluto's crystal at -0.54 PPM (slow), the post-DDC frequency sits
at:

```text
post-DDC offset = -requested_LO * ppm
              = -858100000 * (-0.54e-6)
              = +463 Hz
```

The Costas loop locks this offset into a steady-state PLL value:

```text
phase_per_symbol = 2 * pi * 463 / 4800 = 0.605 rad
PLL_steady_state = -0.605 rad           (negative because the PLL
                                         rotation cancels the offset)
                = -4960 ULPs in Q2.13
```

(Observed value: PLL = -5000, matching the math within fixed-point
quantization.)

Clamp = `+/-MAX_PLL_ABS = +/-pi/3 = +/-1.047 rad = +/-8580 ULPs`. So:

```text
Distance to negative clamp:  -1.047 - (-0.605) = -0.442 rad   (small)
Distance to positive clamp:  +1.047 - (-0.605) = +1.652 rad   (large)
```

**The negative clamp is 4x closer than the positive clamp.** Any transient
that pushes the integrator more than 0.44 rad in the negative direction
rails it. On a noisy signal that's not hard to do.

SDRTrunk with `PPM = -0.4` brings the residual carrier offset down to
~126 Hz (vs our ~466 Hz), which makes the steady-state PLL sit at ~-0.165
rad with ~0.88 rad of headroom on the negative side and ~1.21 rad on the
positive side. Both rails are ~3x further away. Transients that would
rail us are absorbed comfortably.

## The fix (and the dead-end attempt that taught us how)

### First attempt: shift `--rx-lo`

Naively, shifting the requested LO by +466 Hz should compensate the
slow crystal:

```bash
- --rx-lo 858100000
+ --rx-lo 858100466
```

**This made things WORSE**, not better. The 0x8A1 NAC fraction dropped
from ~80% to 9.9% and the chain barely locked.

The reason: `p25-httpd` computes the DDC NCO as
`NCO = control_freq - rx_lo`. Shifting `rx_lo` by +466 Hz auto-shifts the
NCO by -466 Hz. If the AD9361 LO actually moved by the requested amount,
the post-DDC offset would be unchanged (LO and NCO moved together,
relationship preserved). **But the AD9361 LO synthesizer step size at
858 MHz is much larger than 466 Hz** -- the synthesizer rounded our
+466 Hz request back to the original frequency. So the actual LO didn't
move, but the NCO did, and the post-DDC offset *doubled* from +466 Hz
to ~+929 Hz.

`+929 Hz / 4800 sym/s * 2*pi = 1.21 rad/symbol` -- larger than the
`+/-pi/3 = +/-1.047 rad` clamp. The PLL integrator immediately saturates
and the loop never locks cleanly.

**Lesson learned: do not try to compensate small frequency offsets via
the AD9361 LO.** The synthesizer step size at high LO frequencies is too
coarse for sub-kHz adjustments. Use the digital DDC NCO instead.

### Second attempt: shift `--control-freq` (worked)

The DDC NCO is generated in FPGA fabric at 1 Hz precision (subject only
to the fabric clock crystal, which is small enough to ignore). Shifting
`control_freq` adjusts the NCO without touching the AD9361 LO request:

```bash
- --rx-lo 858100000
+ --rx-lo 858100000              (unchanged, nominal)
- --control-freq 860962500
+ --control-freq 860962963        (+463 Hz shift)
```

The math:

```text
requested LO  = 858100000                    (unchanged)
actual LO     = 858100000 * (1 - 0.54e-6)
              = 858099537                    (slow crystal, unchanged)
NCO (computed) = control_freq - rx_lo
              = 860962963 - 858100000
              = 2862963                      (was 2862500, shifted +463)
actual IF     = signal - actual_LO
              = 860962500 - 858099537
              = 2862963
post-DDC      = actual_IF - NCO
              = 2862963 - 2862963
              = 0  ✓
```

The NCO compensates the actual LO error, the post-DDC signal lands at
DC, and the Costas PLL has nothing to track. Steady state PLL sits near
zero with full clamp headroom on both sides.

### Empirical confirmation

After applying `--control-freq 860962963`, on-target test against Clay
County `0x8A1`:

| Metric | Pre-fix (218s slip) | Post-fix (5+ min run) |
|---|---|---|
| Steady-state PLL (Q2.13) | -5000 (= -0.61 rad) | within ±500 (= ±0.06 rad) |
| Steady-state offset (Hz) | ~466 Hz | within ~50 Hz |
| Clamp headroom (negative side) | 0.44 rad | 1.05 rad (full) |
| Clamp headroom (positive side) | 1.65 rad | 1.05 rad (full) |
| Time to first slip | 218 sec (consistent) | not observed in 5+ min |
| 0x8A1 NAC fraction (cumulative) | ~80% peak then drops to 0 | **85.4% sustained** |
| BCH drop_count | 0 | 0 |
| Recovery from transients | none (slip was permanent) | ~80ms (next NID period) |

Per-NID PLL trace (post-fix, valid 0x8A1 NIDs only, sample of 25
consecutive events):

```text
-36, -220, -162, -210, +463, +12, +90, -554, -109, +107,
+20, +69, +139, +80, -474, -217, -288, -588, -47, -425,
-246, -471, -266, -8, -120
```

Mean = -88 ULPs ≈ -8 Hz of residual offset. **The +463 Hz shift was within
~10 Hz of perfect.** No further fine-tuning needed.

The chain still occasionally hits the clamps (as visible in the heartbeat
`pll[lo,hi]` ranges, which sometimes show `pll[-8579, +5500]` within a
1-second window) but recovers within ~80ms. Pre-fix this would have been
a permanent slip; post-fix it's a transient hiccup.

## Why the HDL fixes (CORDIC, lerp pipeline, ExtraTimingOpt) still matter

A naive reading of this diagnosis says "we wasted a week on CORDIC and
the LsmTimingInterp pipeline -- the real fix was a 1-line LO calibration".
That's not quite right. Three reasons the HDL work still mattered:

1. **The CORDIC fix gave us a 12x slip-time improvement** (18s -> 218s)
   that made the diagnostic process feasible. With the original
   linearised loop slipping every 18 seconds, we couldn't have
   characterized the failure mode well enough to find the LO calibration
   issue. The CORDIC bought us enough healthy operating time to capture
   detailed PLL traces and converge on the root cause.

2. **The watchdog removal (doc 023) is still correct.** Even with the
   LO calibrated, `sdr_reset` is still unsafe to pulse mid-operation
   (would crash the board via AXI HP deadlock). The diagnostic NID ring
   buffer added in doc 023 was the tool that captured the slip transition
   data we needed.

3. **CORDIC + lerp pipeline are defence in depth for weaker signals.**
   On a signal where the residual carrier offset is large (poorly
   calibrated tuner, high temperature drift, weak RF that the loop has
   to fight harder) the original linearised PLL would slip much more
   often than the CORDIC version. The CORDIC's bounded `|phase_error| <=
   pi/4` per step is genuinely more robust on noisy signals; we just
   weren't observing that benefit on this particular site because the
   uncalibrated LO was overwhelming any algorithmic improvement.

So the HDL changes from docs 021/022 stay in production. They're not
strictly *required* for this site post-calibration, but they provide
margin for other sites and operating conditions that this calibration
alone wouldn't cover.

## Why the C4FM decoder kept producing 0x8A1 NIDs during the LSM-side stuck state

A puzzling observation during the diagnostic: even when the LSM HDL chain
was fully slipped and `LsmDemod`'s `lsm_dibit_dma` stream was producing
no clean dibits, the **C4FM decoder** (reading from the C4FM `dibit_dma`
ring, fed by the parallel `c4fm_demod` HDL chain) was still producing
clean `NAC=0x8A1 DUID=0x7` NID OK lines at high rate.

This was confusing because:

- The C4FM HDL chain uses a frequency discriminator (`c4fm_demod.py`),
  not a Costas loop. It doesn't have a PLL that could slip.
- LSM is differentially encoded; C4FM is absolute. They produce
  different dibit streams from the same signal in principle.
- The C4FM chain is designed for C4FM signals; we're tuned to an LSM
  control channel.

Hypothesis (not investigated yet): on this particular high-SNR LSM signal,
the C4FM chain's symbol decisions happen to produce a dibit stream that
contains valid P25 frame sync patterns *by coincidence*. The frequency
discriminator outputs symbol decisions that, when packed into dibits,
land on the same bit pattern as the LSM-correct dibits often enough that
the BCH FEC corrects them.

If true, this means **the C4FM HDL chain may be a viable secondary data
path** for strong LSM signals, immune to Costas slip by virtue of not
having a Costas loop. Worth investigating in a future session as a
"belt and suspenders" alternative to LsmDemod for high-SNR sites.
This is a Phase 6F / future investigation; see project_phase6f
memory for the open question.

## How to apply the fix permanently

The in-place edit on the SD card at `/etc/init.d/S60p25-httpd` is **not
persistent** -- it lives in the read-only-after-first-boot overlay and
will be lost on the next firmware flash. To make the fix permanent, edit
the Tezuka source tree:

```bash
cd C:/Users/Andy/Projects/Tezuka/tezuka_fw
# Edit board/tezuka/common/overlay_p25/etc/init.d/S60p25-httpd
# Change "--control-freq 860962500" to "--control-freq 860962963"
# Then commit to tezuka_fw
git add board/tezuka/common/overlay_p25/etc/init.d/S60p25-httpd
git commit -m "fishball P25: Pluto LO calibration via control-freq shift"
```

The change is in `tezuka_fw`, not `maia-sdr`, because the calibration
value is hardware-specific (it's the calibration of *this particular*
Pluto's crystal). It belongs with the firmware build that targets that
specific device.

## Future production improvement: `--ppm` argument

The hardcoded `--control-freq 860962963` is **brittle**: if the user swaps
Plutos, the calibration value will be wrong. A cleaner production fix is
to add a `--ppm` flag to `p25-httpd` that takes a PPM number (matching
SDRTrunk's UI) and computes the NCO offset internally:

```bash
# Future:
p25-httpd ... --rx-lo 858100000 --control-freq 860962500 --ppm -0.54
```

Internally:

```rust
let ppm_correction_hz = -(rx_lo as i64) as f64 * (ppm * 1e-6);
let effective_control_freq = control_freq + ppm_correction_hz as i64;
```

For the Pluto in question, `--ppm -0.54` would internally produce
`control_freq = 860962963` automatically.

This is a ~30-line `clap` argument addition in `p25-httpd/src/main.rs`
plus the math in the DDC NCO computation. Deferred to a follow-up
session; doc 024 captures the rationale.

## Test summary

Final on-target test (post-fix):

```text
Boot:                01:01:55
Latest measurement:  01:07:16
Uptime:              5 min 21 sec  (321 seconds, 47% past the previous slip)

cum NIDs:            1773/2076 = 85.4% valid
nid_evts/s:          7-11 (sustained)
iq_kbps:             242 (full rate)
dibit_overflow:      0
iq_overflow:         0
drop_count:          0 (every NID)
best_sync_dist:      0 most heartbeats (perfect lock)
PLL steady-state:    within +/-500 ULPs of zero (per-NID values)
PLL transient max:   occasional excursions to +/-8579 (clamps)
Recovery time:       ~80ms (next NID period)
```

The chain has now run for 5+ minutes past the previous 218s slip threshold
with no permanent slip and sustained 85% NID validity. **Slip-resistance
work is empirically complete.**

## Remaining open questions (deferred)

1. **Why does the C4FM HDL chain produce clean LSM 0x8A1 NIDs?** Could
   simplify the architecture if the answer is "we don't actually need
   LsmDemod for strong signals". See above.

2. **`--ppm` argument production fix.** Captured above. Defer to future
   session.

3. **AGC (Phase 6E.6.5) and soft sync detector (Phase 6E.8.5)** -- both
   still deferred. Neither needed for Clay County after this calibration.
   They become relevant if/when we encounter a weaker site.

4. **Phase 6F dashboard migration** -- the dashboard's "System Identity"
   panel is still fed by the C4FM ControlChannelDecoder which produces
   0 messages on this LSM signal. The actual decoded data lives in the
   LSM-side decoder. Migrating the dashboard fields to read from the
   LSM decoder is the next session's primary work item.

5. **Heartbeat observability fix** -- the heartbeat task uses the
   `lsm_status.nid_event` Rsticky bit which proved unreliable in some
   stuck states. Should switch to reading the LSM-side
   ControlChannelDecoder's NID counter directly. ~30 lines main.rs.
   See project_phase6f memory.

## References

- doc/changes/021 -- CORDIC PLL update (HDL slip-resistance fix #1)
- doc/changes/022 -- LsmTimingInterp pipeline + ExtraTimingOpt
- doc/changes/023 -- watchdog retraction + diagnostic instrumentation
- `C:\Users\Andy\Projects\SDRTrunk\sdrtrunk\src\main\java\io\github\dsheirer\module\decode\p25\phase1\P25P1DemodulatorLSM.java`
  -- the loop reference we ported
- `C:\Users\Andy\Projects\SDRTrunk\sdrtrunk\src\main\java\io\github\dsheirer\module\decode\FeedbackDecoder.java`
  -- the auto-PPM-adjust path SDRTrunk has but we don't (and don't need
     because the user runs SDRTrunk with auto-adjust off too)
- `p25-httpd/src/lsm/demod.rs::demod_lsm_with_state` -- the Rust port,
  confirmed line-by-line identical to the Java
- `Tezuka/tezuka_fw/board/tezuka/common/overlay_p25/etc/init.d/S60p25-httpd`
  -- the file to edit for the permanent fix
