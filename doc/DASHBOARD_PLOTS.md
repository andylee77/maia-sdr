# Dashboard Plots — IQ, Spectrogram, Eye, Constellation, Deviation

**Status:** Living reference. Current state at 2026-04-22 + proposed
upgrades.
**Scope:** Every visualisation the P25 dashboard draws, plus where
in the signal chain it taps and why each one looks the way it does.
**Related:** [P25_TUNING_REDESIGN.md](P25_TUNING_REDESIGN.md),
[P25_PS_PIPELINE.md](P25_PS_PIPELINE.md),
[diagnostics/2026-04-18/eye/](diagnostics/2026-04-18/eye/).

## 1. Why the plots don't look like Anritsu or OP25

Two things need to be separated:

### What a reference-quality plot shows

An Anritsu MT8212 / Aeroflex 3920 / OP25 HDR-style display is
**post-matched-filter, post-PLL-derotation, post-Gardner-timing-
recovery, post-slicer**. The signal has been:

1. Matched-filtered (RRC, alpha = 0.2, span 16) so the pulse shape
   is restored to its ideal form.
2. PLL-derotated so the π/4-DQPSK carrier rotation is removed and
   the four constellation points sit at fixed angles (±45° / ±135°
   for CQPSK, or on the I axis for C4FM).
3. Timing-recovered by the Gardner TED so every overlay lands at
   exactly the symbol phase (`t = k * T_sym + t0`), not a random
   sub-sample offset.
4. Sliced — each symbol is a decided dibit ∈ {00, 01, 10, 11}
   mapped to {−3, −1, +1, +3}.

After all four, the eye has four clean crossings at the symbol rate,
the constellation has four tight point clusters, and the deviation
plot is a staircase between ±1.8 kHz (C4FM) / ±3 normalised units
(LSM).

The Anritsu screenshot attached to this doc shows this ideal state
driven by a **test-pattern generator** (`p25_lsm_1011`) — a perfectly
repetitive known signal. Mod Fidelity 1.97 %, BER 0.000 %, Symbol
Dev 1773 Hz. That is the calibration upper bound, not what you see
on a live off-air P25 channel.

### What *our* plots currently show

All dashboard plots read from rings tapped **before the PLL and
before timing recovery**. The only post-matched-filter tap we have
is `lsm_iq_dma` (post-RRC), and that is still pre-PLL and
pre-timing-recovery.

That means:

- The eye is washed out because consecutive symbol periods land at
  different sub-symbol phases when overlaid.
- The constellation from `/api/constellation` (post-DDC, software
  Gardner + PLL in `src/lsm/`) does look close to the Anritsu
  picture when the signal is good — that endpoint does run the PLL
  and timing recovery in software on the PS. `/ws/iq?source=post_lsm`
  does not.
- The spectrum looks right across the board — spectrum visualisation
  does not need PLL / timing.

Upgrade path: a post-PLL HDL tap ([project_clean_eye_plot_todo.md](../../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/project_clean_eye_plot_todo.md))
OR client-side Gardner + PLL in the eye-plot JS. Details in §7.

## 2. Tap-point map

```text
AD9361 RX          Fs varies by preset (2–16 MSPS)
   │
   ▼
[DDC]              Stage 1 /d1, Stage 2 /d2, Stage 3 /d3 → 62.5 kSPS
   │
   ├─ iq_dma ring ───────────────────────────────► post-DDC IQ
   │   traffic_iq_dma (same, traffic chain)          62.5 kSPS
   │                                                 (spectrum,
   │                                                  constellation,
   │                                                  eye option A)
   ▼
[LsmDecimator2 /2] 62.5 → 31.25 kSPS
   │
   ▼
[LsmFir LPF]       83 taps, SDRTrunk Remez baseband
   │
   ▼
[LsmFir RRC]       105 taps, alpha=0.2, span=16
   │
   ├─ lsm_iq_dma ──────────────────────────────────► post-RRC IQ
   │   traffic_lsm_iq_dma (same, traffic chain)       31.25 kSPS
   │                                                  (eye option B)
   ▼
[LsmAgc]           per-symbol magnitude normalisation
   │
   ▼
[LsmTimingInterp]  Gardner TED, parabolic interp
   │
   ▼
[LsmPllRotate]     loop filter, ±π/3 clamp
   │
   ▼
[LsmDiffDemodSlicer]
   │
   ▼
dibit stream       9 600 bits/s → NID / sync / TSBK / LDU
```

Today's rings / endpoints:

| Ring / endpoint | Tap | Rate | Bit depth | What it's for |
|---|---|---:|---|---|
| `iq_dma` / `traffic_iq_dma` | post-DDC | 62.5 kSPS | 16-bit i/q | Spectrum, eye (option A), dashboard IQ scatter |
| `lsm_iq_dma` / `traffic_lsm_iq_dma` | post-RRC | 31.25 kSPS | 16-bit i/q | Matched-filter eye (option B) |
| `lsm_dibit_dma` / `traffic_lsm_dibit_dma` | post-slicer | 4.8 kSym/s | 2 bits | Decoder input; dibit dumps |
| `/api/constellation` | software post-PLL | symbol-rate | f32 i/q | Constellation scatter (closest to Anritsu today) |
| `/api/spectrum` | software FFT over `iq_dma` | 62.5 kSPS | f32 dB | Narrowband spectrum |

No tap today exists for:

- **Post-PLL pre-slicer** (the one that gives a clean eye).
- **Pre-DDC wideband IQ** (the one that gives a spectrogram of the
  whole ±(fs/2) front-end band — useful for seeing adjacent channels
  and the analog filter skirt).

## 3. Plots today

### 3.1 Spectrum (narrowband)

- **Endpoint:** `/api/spectrum?chain=control|traffic&fft=<N>&averages=<M>`.
- **Source:** `iq_dma` / `traffic_iq_dma` at 62.5 kSPS, 4096-sample
  window by default, `fft` power-of-two size, `averages` non-
  overlapping segments averaged for noise-floor suppression.
- **Axes:** x-axis is center_hz ± 31.25 kHz (fftshifted); y-axis
  dB relative to 16-bit full scale.
- **What it shows well:** the P25 channel itself, adjacent-channel
  energy within ±31.25 kHz, DC spur (if any).
- **What it does NOT show:** anything outside the post-DDC band.
  The real "what does my RF environment look like" view is still a
  TODO ([project_wideband_fft_display_todo.md](../../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/project_wideband_fft_display_todo.md))
  — Maia SDR already has a wideband spectrometer we can instantiate
  alongside `p25_core`.

### 3.2 Constellation (`/api/constellation`)

- **Endpoint:** `/api/constellation?chain=control|traffic`.
- **Source:** reuses the retired Phase 6D software LSM pipeline —
  post-DDC IQ is fed through `src/lsm/demod.rs` which runs Gardner
  TED + PLL + slicer in software and emits `(I, Q)` at symbol time.
- **Axes:** ±1 to ±3 (or ±full-scale after AGC). Expected clusters
  at (±√2, ±√2) for LSM simulcast / CQPSK, or on the I axis for
  pure C4FM.
- **What it shows well:** carrier lock quality (rotation = PLL not
  locked), SNR (spread around each point), DC offset (whole cloud
  shifted off centre), symbol-timing jitter (radial smear per point).
- **Interpretation cheat sheet:** [reference_p25_constellation_interpretation.md](../../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/reference_p25_constellation_interpretation.md).
- **Caveat:** this is the only plot in the dashboard that's actually
  post-PLL + post-timing. It is the closest to the Anritsu
  constellation quadrant we have today.

### 3.3 Eye — post-DDC (`/ws/iq?source=post_ddc`)

- **Source:** `iq_dma` at 62.5 kSPS → browser overlays N symbol
  periods on a canvas.
- **SPS:** 62 500 / 4 800 = **13.02** samples/symbol — non-integer,
  so every overlay lands at a slightly different sub-symbol offset.
- **Why it's wavy-not-open:** the DDC output is the raw demodulated
  baseband with no pulse shaping. The "eyes" of C4FM/LSM only form
  after the RRC matched filter. Pre-RRC the trace looks like a
  frequency-modulated sine wave (which it essentially is).
- **Reference:** [diagnostics/2026-04-18/eye/eye_control_post_ddc_nsym2_iq.png](diagnostics/2026-04-18/eye/eye_control_post_ddc_nsym2_iq.png).

### 3.4 Eye — post-LSM / post-RRC (`/ws/iq?source=post_lsm`)

- **Source:** `lsm_iq_dma` at 31.25 kSPS (after LsmDecimator2 /2 +
  LPF + RRC).
- **SPS:** 31 250 / 4 800 = **6.51** — still non-integer. This is
  why even at the matched-filter tap the eye looks smeared.
- **Why the amplitude looks different from post-DDC:** the RRC has
  passband gain > 1 and stretches the pulse tails. RMS is ~35 %
  *higher* post-LSM than post-DDC (239 vs 177 in the 2026-04-18
  capture), despite the plot looking "washed out". The washed-out
  look is entirely from the pre-PLL carrier rotation distributing
  samples across every sub-symbol phase.
- **Reference:** [diagnostics/2026-04-18/eye/eye_control_post_lsm_nsym2_iq.png](diagnostics/2026-04-18/eye/eye_control_post_lsm_nsym2_iq.png) + `summary_control_post_lsm.json`.

### 3.5 Why preset choice (2M vs 4M vs 16M) does not change the eye

Every preset produces 62.5 kSPS at the DDC output and 31.25 kSPS at
the LSM tap by construction (see `tools/p25_ddc_filter_design.py`).
So sample rates at the two IQ taps are identical across all presets
— changing preset only changes the AD9361 input rate and the NCO
window width, not anything the eye sees.

## 4. Why ours ≠ Anritsu / OP25 / SDRTrunk (summary)

| Property | Anritsu / OP25 | Fishball today |
|---|---|---|
| Matched filter | yes (RRC α = 0.2) | yes (`LsmFir` RRC, same taps) |
| AGC | yes | yes (`LsmAgc`, per-symbol) |
| PLL derotation | **yes** | **no (for plots)** — slicer runs on it, but no tap |
| Timing recovery | **yes** | **no (for plots)** — same |
| Slicer | yes | yes (`LsmDiffDemodSlicer`) |
| Plot tap | post-slicer | pre-PLL, pre-timing (except `/api/constellation`) |

So the gap between the Anritsu screenshot and our dashboard eye is
exactly the two "no" rows. Closing them is §7.

The Anritsu is additionally driven by a **test-pattern generator**
(`p25_lsm_1011` at NAC 293h) — a perfectly repeating pseudorandom
dibit stream with no noise. A real off-air signal will always be a
bit noisier than that reference picture even when the whole post-PLL
chain is in place.

## 5. What the Anritsu fields mean (as a reference target)

From the screenshot:

- **Mod Fidelity 1.97 %** — RMS deviation of measured symbols from
  the ideal {−3, −1, +1, +3} grid, normalised. Anritsu's threshold
  for "in spec" is typically < 5 %; this is an excellent signal.
- **BER 0.000 %** — symbol → bit demod has no errors over the
  measurement window.
- **Symbol Dev 1773 Hz** — peak deviation of the ±3 symbols (P25
  C4FM spec is 1800 Hz ±5 %).
- **NAC 293h** — network access code, 12-bit identifier for the
  site. Matches what our NID decoder extracts.
- **Freq Error 2.05 Hz** — residual carrier offset after PLL lock.
- **Sym Rate Err 7.66 mHz** — 4800 symbols/s ± 7.66 mHz is tighter
  than any crystal; this is a calibrated test generator.

These five metrics are the canonical "how good is my P25 signal"
numbers and are exactly what a deviation-plot panel should surface
(§6).

## 6. Proposed: deviation plot

### Goal

Mirror the Anritsu "Symbol Dev" / "Mod Fid" / "Sym Rate Err" /
"Freq Err" fields in the dashboard so the operator can read live
signal-quality metrics at a glance, without reading the raw
constellation.

### Measurement

Use the same software PLL + timing chain that already feeds
`/api/constellation` (`src/lsm/demod.rs`). At each symbol time,
record:

1. **Soft symbol** = the post-PLL I-axis value (or L2 magnitude for
   LSM) before the slicer.
2. **Hard symbol** = the slicer's decided {−3, −1, +1, +3}.
3. **Error** = soft − hard.

Over a 1-second window (4 800 symbols) this gives:

- **Symbol Dev:** median |hard|, scaled to Hz if calibrated against
  the known C4FM ±1800 Hz ideal.
- **Mod Fidelity:** RMS(error) / RMS(hard) × 100 %.
- **Freq Error:** mean of the PLL loop-filter output (slow integrator
  value = carrier offset).
- **Sym Rate Error:** slope of Gardner TED residual over the window.

### Display

- A **staircase line plot** over time (x = symbol index, y = hard
  symbol value). Adds a reference signal-generator-style view of
  "what dibits are being decided right now."
- **Four scalar read-outs** next to the line plot, matching the
  Anritsu field layout (Received Power | Freq Err | Mod Fid | Sym
  Dev | BER | NAC | Sym Rate Err).

### API sketch

```
GET /api/deviation?chain=control|traffic&window_syms=4800
  → { ok,
      chain, window_syms, sample_rate_hz: 4800,
      soft: [f32; N],          // N = window_syms, post-PLL I
      hard: [i8;  N],           // {-3,-1,+1,+3}
      metrics: {
        symbol_dev_hz:   1773.0,
        mod_fidelity:    0.0197,     // fraction (×100 for %)
        freq_error_hz:   2.05,
        sym_rate_err_hz: 0.00766,
        ber:             0.000,
        nac:             "293h",
      }
    }
```

Reuses everything that already exists for `/api/constellation`, so
the implementation is mostly surfacing the metrics alongside the
scatter — the PLL loop-filter value, Gardner TED residual, and
slicer decisions are already computed, they're just not exposed.

## 7. Upgrading eye quality — three paths

### Path A: post-PLL HDL tap (cleanest, biggest lift)

Add a fourth IQ DMA ring tapped after `LsmPllRotate` inside the HDL
LSM chain. Tap signals: `lsm_pll_rotate.re_out / im_out`. This gives
samples that are already derotated; the browser only needs to find
the symbol-clock phase.

- **Pros:** clean, calibrated eye. Same resolution of improvement as
  going from `iq_dma` to `lsm_iq_dma` did in Phase 10.6.
- **Cons:** bitstream rebuild (new DMA ring + register bank + UIO
  device + Tezuka DT carve-out). ~1 day HDL + ~0.5 day PS + bake.
- **Notes:** even after this, timing-recovery lands at 6.51 SPS
  non-integer, so overlays will still be phase-scrambled. The
  browser would still need to do Gardner-style interpolation — but
  from derotated samples, which is trivial.

### Path B: client-side Gardner + PLL in JS (no HDL change)

Run a software Gardner TED + first-order PLL on the `lsm_iq_dma`
samples in the browser. The existing `tools/p25_ws_eye_capture.py`
sketch proves the math works in Python; porting to JS is ~150 lines.

- **Pros:** no bitstream change. Ships on the PS-only build.
- **Cons:** doubles browser-side JS work during live playback. On a
  modern browser, 31.25 kSPS × 2 channels × a 9-tap TED is trivial
  (< 1 % CPU). On a tablet it's still OK.
- **Notes:** this is a pure presentation-layer fix. The decoder
  still uses the HDL path.

### Path C: show `/api/constellation` full-time as the "good plot"

The constellation endpoint *already* runs PLL + timing recovery
server-side and emits post-slicer points. Re-render it as a stand-
alone panel (not buried in the Debug tab) with larger canvas and
persistence. This is the lowest-effort option and gets a plot that
looks like the Anritsu constellation quadrant today.

Recommended order: **C first** (lowest effort, biggest UX gain today),
then **B** (eye plot upgrade without bake), then **A** if we ever
want per-symbol eye diagnostics at full 31.25 kSPS resolution.

## 8. What to reference when updating plots

Source code:

- Dashboard JS: [p25-httpd/src/httpd/dashboard.html](../p25-httpd/src/httpd/dashboard.html) — `drawEye*`, `drawSpectrum*`, `drawConstellation*`.
- Server endpoints:
  - [api/ws.rs](../p25-httpd/src/httpd/api/ws.rs) — `/ws/iq` tap selection.
  - [api/debug.rs](../p25-httpd/src/httpd/api/debug.rs) — `/api/spectrum`, `/api/constellation`.
- HDL tap points: [maia-hdl/p25_hdl/p25_top.py](../maia-hdl/p25_hdl/p25_top.py) (search `lsm_iq_dma`, `iq_dma`).
- Software PLL + Gardner: [p25-httpd/src/lsm/](../p25-httpd/src/lsm/) (used by `/api/constellation`).

Upstream references for what "right" looks like:

- **Anritsu MT8212 P25 analyser** — constellation / eye / deviation
  fields; screenshot attached to this doc as the gold standard for
  what a post-PLL P25 plot is supposed to look like.
- **SDRTrunk** ([reference_sdrtrunk_paths.md](../../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/reference_sdrtrunk_paths.md)):
  `io.github.dsheirer.dsp.psk` — the reference Gardner TED + PLL we
  already mirror in `src/lsm/`.
- **OP25** (`op25_repeater/apps/rx.py` eye-plot path) — same general
  approach; useful as a sanity check on plot layout conventions.
- **P25 TIA-102.BAAA-A** — Symbol Dev / Mod Fidelity / Freq Err
  tolerances that the deviation-plot metrics should calibrate
  against.

Existing diagnostic captures (for regression testing any plot
change):

- [diagnostics/2026-04-18/eye/](diagnostics/2026-04-18/eye/) —
  post-DDC + post-LSM captures + per-source amplitude summaries.
  Compare any new eye-plot rendering against these.
- [diagnostics/2026-04-19/PERFORMANCE_ANALYSIS_V2.md](diagnostics/2026-04-19/PERFORMANCE_ANALYSIS_V2.md) —
  timing reference for where the PS software PLL spends its cycles.

## 9. TL;DR

- **Anritsu / OP25 plot = post-PLL + post-timing.**
- **Fishball today** — only `/api/constellation` is post-PLL. The
  WebSocket IQ eye plots are pre-PLL and pre-timing, so they look
  smeared and pre-RRC plots look wavy.
- **Amplitude** — post-LSM is *higher* amplitude than post-DDC
  (RRC gain), not half. Any "half amplitude" impression is from
  canvas auto-range.
- **Preset choice does not affect the eye** — all presets produce
  62.5 kSPS at the DDC tap and 31.25 kSPS at the LSM tap.
- **Three paths to a clean eye:** surface `/api/constellation` as a
  first-class panel (easy), add a JS Gardner + PLL on top of
  `lsm_iq_dma` (medium), or add a post-PLL HDL tap (big but
  cleanest).
- **Deviation-plot proposal:** surface the already-computed PLL +
  TED metrics as the Anritsu-style "Symbol Dev / Mod Fid / Freq
  Err / Sym Rate Err / BER / NAC" field block.
