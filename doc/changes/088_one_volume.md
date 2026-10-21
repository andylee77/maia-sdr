# 088 — One volume: recordings at the header's volume, every volume to full scale

**Date:** 2026-10-20. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** unit A runs the test build `2026-10-20-audio2` in RAM. A reboot returns it to the SD
image `2026-10-06-rec1`.

## Why

Andy asked for the volume control on each recording to go, and for the header's volume to be the
overall volume. He did not need 200 % ("100 % is plenty loud"). He had turned the header up and
then set the PC's volume very low, because playback clipped. Above 100 %, the header's gain lifts
the mix past full scale, and the output clips it.

087's interpolator also clipped a little. It rebuilds the waveform between samples, and that can
rise past the samples themselves, since the server's AGC lets peaks reach 0.998. The final
`clip()` cut those overshoots: with the leveller off, 12 of 30 calls reached full scale, where the
linear interpolator gave none.

## Changes

- **The header's volume** runs 0-100 % and covers live audio and recordings. The Traffic 1 and
  2 volumes on Now run 0-100 % too. A volume saved above 100 % plays at 100 %.
- **Recordings** on Now's calls and Activity's alerts play in `audio/recording.js`. Each player
  has play and pause, its position and its time, and no volume of its own. `audio.volume`
  follows the header, including while the slider is dragged.
- **`ring.js`:** the leveller's soft knee (above 0.8) now comes after the interpolator, and it
  applies with the leveller off too.

## Measured

`ring.js` was run under VS Code's Node on 30 recent calls, at 48 kHz and volume 100 %. The table
shows how many calls had any sample at full scale:

| `ring.js` | Leveller off | Leveller on |
|---|---|---|
| Linear (`2026-10-06-rec1`) | 0 / 30 | 14 / 30 |
| 087 | 12 / 30 | 25 / 30 |
| 088 | 0 / 30 | 20 / 30, the leveller's knee saturating rather than a hard clip |

- **Tones:** still flat to 3 kHz, −0.33 dB at 3.4 kHz.
- **Images above 4 kHz:** −50.8 dB, against −61.2 dB in 087 and −19.7 dB in rec1. The rise is
  the knee's distortion on the 0.08 % of samples above 0.8.

## Left as it is: the leveller

The server's AGC already brings each call to −22 dBFS. The leveller aims at −20 dBFS and rides its
gain up slowly through quiet passages, so a loud syllable after a pause reaches its knee before
the fast attack catches it: 20 of 30 calls drive peaks to the top of the knee. Setting its target
to the server's (2500 / 32768) only brought that down to 14. With Level off, nothing reaches full
scale.

## Checks

- **`node --check`** on every changed module. Every `import { ... }` in the UI names an export.
  `audio/recording.js` is in the asset list.
- **Headless Chrome** against the PC daemon (`runs/ui_check`, build `2026-10-20-audio2`), with
  no exceptions, console errors or failed loads on Now and Activity:
  - Volume range: the header and both Traffic sliders end at 100. A saved volume of 2 shows
    100 % and plays at gain 1.
  - Playback: a recording plays, its position follows and can be dragged, and Pause and Play
    work.
  - Starting points: playing from 5 s starts at 5.08 s. At the end the button shows Play, and
    Play then starts from 0.
  - Volume: the header at 40 % sets the recording's volume to 0.4 while the slider moves, and
    saves it on release. No `audio[controls]` is left.
- **Host tests:** 515 pass.

## Status log

- 2026-10-20: committed (4582aa6). The test build `2026-10-20-audio2` went onto unit A (binary
  only):
  - live on Clay, with control decode at 100 %;
  - the UI files served byte for byte;
  - in headless Chrome against A, a Now call's recording and an Activity dispatch ("call #151740,
    4.1 s") play in the new player, with no `audio[controls]`, and the header at 0 silences the
    recording.
