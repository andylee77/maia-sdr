# 078 — Full-range frequency sweep (`rf.freq_sweep`); maintenance mode stops the scanner

**Date:** 2026-10-03. **Branch:** fishball-p25. **Bake required:** NO. Host (fbench) and the
on-board agent only; redeploy `fbench-agent` to both units (`fbench setup agent --unit A`,
`--unit B`). The scanner and the gateware are unchanged.

## Why

Unit B's AD9363 runs as an AD9361 (70 MHz-6 GHz through the Tezuka driver patch), outside
its specified 325 MHz-3.8 GHz and 20 MHz bandwidth. Whether that is valid needs the two
units' receive and transmit chains compared over the whole range, in both directions and
with more than one TX generator. The existing `rf.*` tests each run at one frequency.

## What

### `rf.freq_sweep` (`bench/fbench/tests/sweep_tests.py`)

One cabled direction per run, both units in maintenance mode.

- **Plan:** 84 RX LO points from 70 MHz to 5998.5 MHz. They are 8 per octave up to about
  1.1 GHz and at most 100 MHz apart above. The AD9363's 325 MHz and 3.8 GHz edges are
  included: 18 points lie below 325 MHz and 23 above 3.8 GHz. `freqs_mhz` replaces the plan.
- **Frequency layout:** the TX LO sits 1 MHz above the RX LO and the tone 0.5 MHz above the
  TX LO. Every line lands on its own baseband frequency:

  | Line | Baseband | Methods |
  |---|---|---|
  | tone | +1.5 MHz | all |
  | TX LO leakage | +1.0 MHz | dds, cyclic (pattern's tone is its LO) |
  | TX image | +0.5 MHz | dds, cyclic |
  | RX image | -1.5 MHz | all |
  | RX DC | 0 | all |

  The analysis finds the tone among its candidate places, so an inverted RX spectrum, or a
  generator that puts the tone below its LO, is recognised and recorded rather than misread.
- **Methods:** every TX method the unit supports sweeps the same plan in turn: `dds`
  (hwval and factory images), `pattern` (agent) and `cyclic` (libiio buffer). The receive
  results repeat under different generators, and the transmit results separate by method.
- **Per point:**
  - both synthesizers' lock bits (AD9361 SPI 0x247 RX, 0x287 TX, bit 1), read through the
    agent;
  - the LO read-backs;
  - one TX quadrature calibration (`calib_mode = tx_quad`, dds and cyclic), so leakage and
    image show what the chip calibrates at that frequency;
  - RSSI and one 64 Ki-sample capture at a fixed manual RX gain (40 dB).
- **Analysis:**
  - tone level and SNR;
  - the TX vs RX reference offset in ppm. A point off its neighbours' running median by
    more than 0.05 ppm and 100 Hz is an LO that did not land;
  - RX image, TX LO leakage and TX image in dBc, with lines within 10 dB of the noise
    marked as upper bounds;
  - the strongest spur.

  Worst cases are reported inside and outside each unit's specified range.
- **Fails on:** a point that does not tune, an unlocked synthesizer, an LO read-back off by
  more than 1 kHz, a missing tone (SNR below 10 dB) or an LO jump. Everything else is
  reported.
- **`compare=<run dirs>`:** overlays other runs and computes the level difference of every
  pair:
  - same TX unit: the RX chains' difference;
  - same RX unit: the TX chains' difference;
  - reversed: the direction asymmetry;
  - same direction: repeatability.

  Medians are given inside and outside the AD9363 range. It warns when the runs differ in
  pads, gain, attenuation, offsets, rate or capture length.
- **Plots:** `freq_sweep_level.png`, `freq_sweep_lines.png`, `compare_level.png`,
  `compare_diff.png`, `compare_lines.png`.
- **Self loop:** a board's own TX cabled to its own RX (`--tx A --rx A`) is a valid
  direction.

### `fbench analyze -p`

`fbench analyze <run_dir> -p key=value` re-runs the analysis with a changed parameter and
stores it with the run. Its first use is adding `compare` after the last cable position.

### Agent: maintenance mode on the scanner image

`maint enter` looked only for p25-httpd. On the scanner image it found none, stopped nothing
and reported success, so the scanner kept retuning the radio during every M test. The
agent now stops and restarts whichever radio daemon the image installs: the scanner
(`S60scanner`) or p25-httpd (`S60p25-httpd`). The state file records which one it stopped.
The reply keys are `daemon` and `daemon_pids` (formerly `p25_httpd_pids`).

### Register maps know traffic chain 2

Change 064 added banks 0x120-0x17F (`traffic2_sdr`, `traffic2_lsm` and its seeds). The
bench was never updated:

- the agent's build script panicked on the current SVD ("traffic2_ddc_coeff_addr at 0x120
  is in an unknown bank"), so the agent could not be built;
- `fbench regmaps build` refused the SVD.

Both now know the three banks, `traffic2_lsm_status` (0x144) is read-to-clear, and only
0x1E0-0x1FF stays vacant. `bench/share/p25_regs.json` and the agent's fallback map are
regenerated (70 registers).

### Corpus tests

Commit 06aaf2c made the `/ws/audio` recorder's chunks `(time, lane, pcm)`, but the test
fake kept sending `(time, pcm)`. Eleven corpus tests and both catalog corpus cases have
failed since then. The fake now sends lane 0.

## Running it

Move one padded cable through four positions. Set the `[[rf.links]]` entry in `bench.toml`
to the cabled path before each run:

| Cable | Run |
|---|---|
| B.TX1 → 40 dB → A.RX1 (as wired for the corpus) | `--tx B --rx A` |
| B.TX1 → 40 dB → B.RX1 | `--tx B --rx B` |
| A.TX1 → 40 dB → A.RX1 | `--tx A --rx A` |
| A.TX1 → 40 dB → B.RX1 | `--tx A --rx B -p compare=<the three run dirs>` |

The four runs give `RX B - RX A` twice (through TX A and through TX B) and `TX B - TX A`
twice. The two estimates should agree. At an estimated 3 s per point (not yet timed), a
run with two methods takes about 8-9 minutes.

## Checks

- Bench host tests: 283 passed. On the previous commit 14 failed: the corpus fake, and the
  register map test against the current SVD.
- Agent: `cargo test` 58 + 11 passed; the armv7 musl build succeeds
  (`bench/scripts/build_agent.sh`).
- Not run on hardware yet: it needs the bench link wired and the agent redeployed.
