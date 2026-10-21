# 087 — Live audio: a windowed-sinc interpolator in the player

**Date:** 2026-10-20. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** in the checkout; unit A still runs `2026-10-06-rec1` without it.

## Why

Andy heard Clay dispatch (TG 300, consoles 1011-1014) as "tingy" on many calls, remembered it
clearer, and remembered SDRTrunk sounding better. Unit A was otherwise healthy: 14 days up, no
dropped input blocks or grants, and 0.59 % IMBE frame errors over 488 calls with voice.

## What was measured

Each spectrum is the average of a call's speech frames, in bands relative to its own 300-1000 Hz
level, so the comparison is of shape, not loudness.

- **Our vocoder and SDRTrunk's agree.** 231 of Andy's SDRTrunk calls pair a `.mbe` (the IMBE
  frames) with the `.mp3` SDRTrunk made from them. The same frames through our vocoder
  (`mbe_offline_replay`) and SDRTrunk's MP3 agree within ±0.5 dB in every band from 150 Hz to
  3.4 kHz, over 92 console and 138 mobile calls. SDRTrunk's P25 audio is JMBE and a fixed ×5 gain
  clipped at 0.95, with no filter or AGC. Its MP3 resamples to 16 kHz (libresample4j, −6 dB at
  3.6 kHz) and encodes at 16 kbps.
- **The consoles' own sound changes by day.** On one build (2026-10-06 to 10-20), consoles 1011
  and 1014 alternate between thin days and full days. The tilt (1.5-3.4 kHz less 0-300 Hz) is
  +14 to +21 dB on thin days and −12 to −17 dB on full ones; mobiles do not follow. The thin sound
  is in the frames the consoles send.
- **The live player added images.** `ring.js` resampled 8 kHz to the output rate by linear
  interpolation. Above 4 kHz it left images at −19.7 dB to the speech band (median of 30 calls,
  worst −8.3 dB), and it drooped 1.8 dB at 2 kHz and 4.3 dB at 3.4 kHz. A proper resampler
  leaves −66 dB. Recordings are not affected, since the browser resamples them itself.
- **Not the cause:** the post-vocoder AGC. Its gain is broadband, and its soft knee touched at
  most 0.07 % of samples.

## Andy's listening (2026-10-20)

Sounded good: a full day's console call; a thin day's call through a proper resampler; our
vocoder's render of an April call; and the thin and full calls through the new `ring.js`. Did not:
the thin day's call as recorded (8 kHz, resampled by the PC's player); the same call through the
old `ring.js`; and SDRTrunk's MP3 of the April call. Resampled properly, the thin days sound good
too: the player's resampling was the main problem, and the consoles' daily change only a lesser
one.

## The interpolator

`Ring` reads 32 samples around the read point through a Kaiser-windowed sinc (β 7) cut off at 0.47
of 8 kHz (3.76 kHz). The taps come from a table of 129 rows, one for each of 128 positions between
two samples plus the next sample, each row summing to one; the output interpolates between the two
nearest rows. Samples past the newest one count as zero, so a transmission still plays to its last
sample. The table is built inside the class (`Ring.interpolator`) because the worklet receives the
class as source text. The 16 samples of look-ahead (2 ms) sit inside the 150 ms target.

## Steps and checks

- Prototype in Python on 40 recordings: images −78 dB at both 48 and 44.1 kHz; flat to 3.2 kHz,
  −0.33 dB at 3.4 kHz, −2.2 dB at 3.6 kHz.
- `ring.js` run under VS Code's Node, with the class rebuilt from its source text as the worklet
  does, 160-sample writes every 20 ms and 128-sample mixes:
  - tones flat to 3 kHz and −0.33 dB at 3.4 kHz (the old file: −1.76 dB at 2 kHz);
  - images on 30 calls at −61.2 dB (median; worst −30.5 dB), where the old file gives −19.7 dB;
  - no underruns at 48 or 44.1 kHz;
  - about four times the old file's cost: 200-300× real time per lane on the PC.
- `node --check` on `ring.js`, `sources.js`, `player.js` and `gate.js`; their imports are unchanged.

## Status log

- 2026-10-20: in the checkout.
