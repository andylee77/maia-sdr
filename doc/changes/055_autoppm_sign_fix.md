# 055 — Auto-PPM residual sign fix

**Date:** 2026-09-26. **Branch:** fishball-p25. **Bake required:** no (p25-httpd only).

## Why

`pll_dbg` reads the NCO frequency minus the carrier frequency. Stage B of
`run_calibration` and the continuous tracker both assumed the opposite and moved the
shift by *plus* the residual. The correct fixed point was therefore repelling: every
correction doubled the error.

This is the 2026-04-24 "positive feedback" in
`doc/diagnostics/2026-04-24/AUDIO_DROPS_ANALYSIS.md` §Issue 1. There the shift walked
from −0.535 to +1.11 ppm in 10 min, and each tick's residual was larger than the last
(−67, −101, −153, −230, −343, −393 Hz). It was put down to `pll_dbg` bias. The
"estimate" rewrite kept the same sign (`shift + residual`), and the ±50 Hz recal anchor
has been holding it back since. It also explains why unit A runs on a
`manual_override` calibration.

## Evidence (bench, 2026-09-26)

Setup: unit B replays a site clip from RAM (below) into unit A through 20 dB. A runs
p25-httpd with `lo_shift_hz` 470. Residuals are 30–40 s medians of `pll_dbg` polled at
5 Hz and converted with `pll_dbg × 4800 / (2π · 8192)`.

| Change | Residual (median) |
|---|---|
| B TX LO 859.212510 MHz, shift 470 | −101 Hz |
| B TX LO +105 Hz | −205 Hz |
| B TX LO −211 Hz (859.212400 MHz), shift 470 | +0.5 Hz |
| shift 370 | −87 Hz |
| shift 470 | +8 Hz |
| shift 570 | +106 Hz |

Raising the carrier lowers the residual, and raising the NCO raises it, both with a slope
of about 1 Hz/Hz. So the residual is NCO − carrier, and the shift must move by −residual.
The means agree in sign but are noisier, with outliers at the replay loop splice. The
tracker's 10 % trimmed mean handles those.

## Change

- `app/autoppm.rs`: new `pll_q213_to_hz()` and `true_shift_estimate_hz(shift, pll) =
  shift − residual`. Both Stage B (`final_lo_shift`) and the tracker sampler
  (`estimate_hz`) use it. Unit tests pin the bench measurements and show that
  assigning the estimate always shrinks the error.
- `app/mod.rs`: `autoppm` also compiles under `cfg(test)`, so those tests run on the
  host.
- `httpd/api/tuning.rs`: the `/api/ppm/nudge` doc states the sign.

## Verified live (2026-09-26, build `2026-09-26-dibit-lowlatency-airtime`)

`POST /api/ppm_calibrate` on the bench replay:

- Stage A put the FFT peak at +385 Hz.
- Stage B's residual was −79.6 Hz.
- The fixed code applied 385 − (−79.6) = **465 Hz**, within 5 Hz of the hand-found 470.
- The residual afterwards was −1.1 ± 1.8 Hz.
- The old sign would have applied 305 Hz, 165 Hz the wrong way.

The live result was not written over the stored `manual_override` file, which still holds
470.

The clip's CC offset (+472 Hz) matches A's own error, because the captures carry A's
uncorrected reference (SDRTrunk's −0.47 ppm was not applied to the IQ). So A's 470 Hz is
right in absolute terms, and unit B sits at about +0.11 ppm (A + 0.662).

## Follow-ups

- Consider widening or removing the ±50 Hz anchor, which only existed to contain the
  runaway.
- The 2026-04-30 audit saw the traffic chain sitting at `pll_dbg` ≈ −1240 (−116 Hz)
  across 9 calls. That is consistent with a calibration pulled the wrong way; re-check
  it on air with this fix.

## Bench replay setup used

- Clip: `C:/Users/Andy/SDRTrunk/my_captures/1777801424_859212970_4000000_baseband.wav`
  (recorded by unit A as a PlutoSDR through SDRTrunk, correction −0.47 ppm), 13–41 s,
  cut to int16 at 0.9 × 2^14 peak. It contains the CC 860.9625, a continuous carrier on
  858.4625, and TG 300 calls on 857.9875 (clip 2.5–8.5 s) and 858.4375 (21–26.5 s).
- Unit B: p25-httpd stopped. The clip is in `/tmp` (RAM) and streamed by
  `while :; do cat clip; done | iio_writedev -u local: -b 1048576 cf-ad9361-dds-core-lpc
  voltage0 voltage1`: gap-free with no CMA limit, and the loop splice every 28 s is the
  only discontinuity. TX 4 MSPS, RF bandwidth 5 MHz, TX LO 859.212400 MHz, gain −55 dB.
- The TX LO trim cancels three errors: the clip's own residual error, B's reference
  (0.662 ppm above A, `rf.cw_ppm`), and A's calibration. With it, A's stored +470 Hz
  sees the replay the same way it sees the air.
- Level: at −70 dB the traffic chain AGC never engaged. Its input magnitude stayed below
  the 256 (Q1.15) update threshold, the gain stayed at 1.0, and `pll_dbg` froze. At
  −55 dB A's ADC sits at −22 dBFS RMS / −10 dBFS peak, CC SNR is 29 dB (limited by the
  clip), and TSBK CRC-ok is 97.8 %.
