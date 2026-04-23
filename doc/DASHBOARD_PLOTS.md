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

Both the Anritsu P25 analyser and the OP25 **Datascope** view are
**post-matched-filter, post-PLL-derotation, post-Gardner-timing-
recovery, pre-slicer**. The signal has been:

1. Matched-filtered (RRC, alpha = 0.2, span 16) so the pulse shape
   is restored to its ideal form.
2. PLL-derotated so the π/4-DQPSK carrier rotation is removed and
   the four constellation points sit at fixed angles (±45° / ±135°
   for CQPSK, or on the I axis for C4FM).
3. Timing-recovered by the Gardner TED so every overlay lands at
   exactly the symbol phase (`t = k * T_sym + t0`), not a random
   sub-sample offset.

After those three, the eye has four clean crossings per symbol, the
constellation has four tight point clusters, and the deviation plot
is a staircase between ±1.8 kHz (C4FM) / ±3 normalised units (LSM).

### Reference images in this doc

- **Anritsu P25 analyser (attached to the tuning session).** Both
  test-pattern captures (e.g. `p25_lsm_1011`, NAC 293h) and live
  off-air captures from real P25 systems produce the same clean
  constellation + eye when the analyser has lock. The test-pattern
  capture is tighter on BER / Mod Fidelity because the source is
  noise-free, but the *shape* of the plots is what off-air should
  look like too — it's not a "test-gen only" pattern.
- **OP25 Datascope view.** Same idea as an Anritsu eye but rendered
  by the OP25 Python plotter over live off-air audio. The
  `op25_repeater/apps/rx.py --plot datascope` trace shows ~10
  symbol periods on the x-axis with decided levels pinned at
  roughly ±1 and ±3, and clear diagonal crossings at the inter-
  symbol boundaries. The screenshot the user attached to this doc
  is exactly that view off a live P25 system — the four horizontal
  rails and the diamond-shaped inter-symbol eye openings at x ≈ 0.5,
  1.5, 2.5, … are the canonical "this is locked" fingerprint.

Both are what a correctly presented eye plot off this hardware
*should* look like when the post-PLL tap is in place. The Anritsu
test-pattern BER number is the calibration ceiling; the OP25
Datascope picture is the everyday off-air target.

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

| Property | Anritsu / OP25 Datascope | Fishball today |
|---|---|---|
| Matched filter | yes (RRC α = 0.2) | yes (`LsmFir` RRC, same taps) |
| AGC | yes | yes (`LsmAgc`, per-symbol) |
| PLL derotation | **yes** | **no (for plots)** — slicer runs on it, but no tap |
| Timing recovery | **yes** | **no (for plots)** — same |
| Plot tap | post-PLL, post-timing, pre-slicer (Datascope) or post-slicer (Anritsu constellation) | pre-PLL, pre-timing (except `/api/constellation`) |

So the gap between a reference plot and our dashboard eye is exactly
the two "no" rows. Closing them is §7. Preset choice, sample rate,
amplitude scaling, and non-integer SPS are all red herrings — OP25
Datascope runs on a non-integer SPS too (its Gardner TED recovers
symbol time regardless), and its output still looks like the
attached screenshot.

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
- **OP25 Datascope** (`op25_repeater/apps/rx.py --plot datascope`)
  — the canonical off-air P25 eye plot. Ten symbol periods on the
  x-axis, soft symbols overlaid, four rails at ±1 / ±3, diamond
  inter-symbol openings at x ≈ k + 0.5. This is the target output
  shape for our eye widget once Path A or Path B in §7 is done.
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

## 9. Phase 10.7 — Plots & Observability (implementation plan)

This is the committed plan for fixing every item in §§1–8 in one
bake. It slots between Phase 10.5 (voice-chain stability) and
Phase 11 (polyphase channelizer) in
[HDL_LAYOUT_AND_ROADMAP.md](HDL_LAYOUT_AND_ROADMAP.md). The whole
point is **measuring what the PL actually does** rather than
reconstructing it on the PS. Every rendered plot has a PL-sourced
tap; the PS touches data only to format it.

### 9.1 Decisions already locked

- **Don't un-delete `maia_sdr`.** That IP bundles recorder +
  old Maia DDC + its own register bank + multiple DMA masters.
  Instead, instantiate the three sub-modules we actually want —
  [spectrometer.py](../maia-hdl/maia_hdl/spectrometer.py),
  [fft.py](../maia-hdl/maia_hdl/fft.py),
  [spectrum_integrator.py](../maia-hdl/maia_hdl/spectrum_integrator.py)
  — directly inside `P25Core`.
- **Path A from §7 (post-PLL HDL tap) wins.** Paths B/C are not
  implemented. We spend the bake once and get the clean signal at
  the source instead of re-deriving it per-repaint in JS.
- **One canonical plot per category, no user variants.** No more
  "fast spectrogram vs slow spectrogram", no more `source=post_ddc`
  vs `source=post_lsm` eye options.
- **Plots live in their own tab.** Radio tab keeps the widgets it
  already has; the new Plots tab has a single fullscreen canvas
  with a picker.

### 9.2 HDL — new DDR carve-outs

Extends the DDR table in
[P25_ADDRESS_MAP.md](P25_ADDRESS_MAP.md). Source-of-truth remains
that doc; this section is a summary.

| Name | Base | Sub-buffers × size | Total | Rate | Purpose |
|------|------|--------------------|-------|------|---------|
| `post_pll_iq_dma`         | `0x1F00_0000` | 8 × 32 KB | 256 KB | ~9.6 kSPS × 4 B ≈ 38 KB/s | Control-chain rotated IQ (mid + sym) after `LsmPllRotate`. Feeds the eye + deviation + constellation — one ring, three plots. |
| `traffic_post_pll_iq_dma` | `0x2000_0000` | 8 × 32 KB | 256 KB | 38 KB/s | Traffic-chain twin. |
| `wideband_spec_dma`       | `0x2100_0000` | 4 × 16 KB | 64 KB  | ~32 KB/s @ 5–10 Hz | 4096-bin wideband FFT from `Spectrometer`, pre-DDC tap off `rxiq_cdc`. |

Total new DDR: 576 KB (well inside the existing reserved region).

### 9.3 HDL — new register banks

Slots 10–15 are currently free
([P25_ADDRESS_MAP.md §AXI-Lite register banks](P25_ADDRESS_MAP.md#axi-lite-register-banks)).
Phase 10.7 claims 10, 11, 12:

| Bank | Byte base | Name | Registers | Mirrors |
|------|-----------|------|-----------|---------|
| 10 | `0x7C46_0140` | `post_pll_iq` | status (overflow + last_buffer), control (enable), next_address | bank 8 (`lsm_iq`) |
| 11 | `0x7C46_0160` | `traffic_post_pll_iq` | status, control, next_address | bank 9 (`traffic_lsm_iq`) |
| 12 | `0x7C46_0180` | `spectrometer` | `spec_num_integrations` (RW), `spec_peak_detect` (RW), `spec_abort` (Wpulse), `spec_last_buffer` (R), `spec_next_address` (R) | new layout |

No bank-decoder change needed — Phase 10.6 already widened to
4 bits (16 banks).

### 9.4 HDL — new IRQ bits

Extends the `control.interrupts` table:

| Bit | Name | Source |
|-----|------|--------|
| 5 | `post_pll_iq_dma` | `post_pll_iq_dma.interrupt` |
| 6 | `traffic_post_pll_iq_dma` | `traffic_post_pll_iq_dma.interrupt` |
| 7 | `wideband_spec_dma` | `spectrometer.interrupt_out` |

### 9.5 HDL — signal changes

**[lsm_demod_loop.py](../maia-hdl/p25_hdl/lsm_demod_loop.py):** add
outputs that expose the already-computed rotated IQ as first-class
ports instead of leaving them as debug taps:

```python
# New outputs (sync domain)
self.i_rot_out        = Signal(signed(16))   # Q1.15 rotated IQ
self.q_rot_out        = Signal(signed(16))   # (narrowed from
                                             #  rotate_sym's 18-bit Q3.15)
self.rot_strobe_out   = Signal()             # 1 cycle per rotated
                                             # sample (mid + sym
                                             # interleaved → 9.6 kSPS
                                             # with 4800 Hz symbol
                                             # rate)
```

Wire to `rotate_sym.re_out / im_out / strobe_out` plus an interleave
with `rotate_mid.re_out / im_out` so the eye plot sees two samples
per symbol (enough to see the eye diamond; not as dense as 4×
but matches OP25 Datascope's typical render).

**[lsm_demod.py](../maia-hdl/p25_hdl/lsm_demod.py):** pass-through
the new signals from `LsmDemodLoop` to the top level.

**[p25_top.py](../maia-hdl/p25_hdl/p25_top.py):**

1. Two new `IQPacker` + `DmaStreamRingWrite` pairs for the post-PLL
   rings — copy the Phase 10.6 `lsm_iq_dma` template verbatim.
2. Instantiate `Spectrometer(dma_base_address=0x2100_0000,
   dma_buffers_log2=2, dma_name='wideband_spec')` with its 16-bit
   IQ inputs wired off `rxiq_cdc.re_out / im_out` (pre-DDC, full
   AD9361 sampling rate). Requires adding `clk2x` / `clk3x` domains
   to `P25Core` if not already present (P25DDC already uses
   `clk3x`).
3. Three new `Registers` blocks + `RegisterMap` entries for banks
   10/11/12.
4. Three new `PulseSynchronizer` instances for the new interrupt
   sources (sync → s_axi_lite).

### 9.6 HDL — system_bd.tcl + package_ip.tcl

- [system_bd.tcl](../maia-hdl/projects/fishball7020_p25/system_bd.tcl):
  three new `ad_mem_hp1_interconnect` calls (still plenty of HP1
  headroom per §B of the scoping survey — ~755 KB/s used of
  1.7 GB/s). Alternatively, put the wideband spectrometer on HP2
  to keep the narrowband P25 rings isolated from the wideband
  bandwidth spike (HP2 is currently unused; §HP-port wiring table
  in the address map). **Decision: HP2 for the spectrometer, HP1
  for the two post-PLL IQ rings.**
- [package_ip.tcl](../maia-hdl/ip/p25-core/package_ip.tcl): add
  `ipx::associate_bus_interfaces` for the three new AXI masters.

### 9.7 HDL — regeneration + bake

Per the [future-self checklist](P25_ADDRESS_MAP.md#future-self-checklist-when-adding-a-new-register-bank-or-dma):

1. Update `P25Config` for new DDR carve-outs + `validate()`.
2. Regenerate `p25.svd` via `P25Core.svd()` (calls from the
   `p25_top.py` main entry point).
3. `build_hdl.bat --verilog-only --p25` then `build_fpga.bat --p25`.
4. `svd2rust` on p25-pac/ to regenerate bindings.

### 9.8 Tezuka device tree

Add three `reserved-memory` nodes + three UIO devices:

- `p25-post-pll-iq` (0x1F00_0000, 256 KB)
- `p25-traffic-post-pll-iq` (0x2000_0000, 256 KB)
- `p25-wideband-spec` (0x2100_0000, 64 KB)

Follows the pattern of `p25-lsm-iq` in
`tezuka_fw/board/tezuka/fishball7020/dts/fishball-p25.dtsi`.

### 9.9 p25-httpd — PS side

**[hardware/fpga.rs](../p25-httpd/src/hardware/fpga.rs):** three
new `RxBuffer` fields plus three `read_*_buffers()` methods, all
copying the `lsm_iq_dma` reader pattern (poll `*_next_address`,
compare against cached last, then `cache_invalidate()` plus
`buffer_as_slice()`).

**[httpd/api/debug.rs](../p25-httpd/src/httpd/api/debug.rs):**

```text
GET /api/spectrum_wide?averages=M
    → { center_hz, span_hz, bins: 4096, power_db: [f32; 4096],
        peak_detect: bool, integration_ms: u32 }
```

Reads from `wideband_spec_dma`. `power_db` is a direct exponential
unpack of the spectrometer's 47-bit mantissa + 8-bit exponent
(per [spectrometer.py:128–135](../maia-hdl/maia_hdl/spectrometer.py#L128-L135)).
**No PS FFT.** `span_hz` = AD9361 sample rate (preset-dependent,
2–16 MHz).

```text
GET /api/deviation?chain=control|traffic&window_syms=4800
    → { soft: [f32; N], hard: [i8; N],
        metrics: { symbol_dev_hz, mod_fidelity, freq_error_hz,
                   sym_rate_err_hz, ber, nac } }
```

Reads from `post_pll_iq_dma`. The IQ is **already PL-rotated** so
the PS work reduces to: project onto I-axis to get soft symbol,
slice to `{-3,-1,+1,+3}` for hard, compute error + Anritsu-style
metrics over a 1-second window. Matches the spec sketched in §6
but cleaner because there is no PS PLL/TED running per-request.

**[httpd/api/ws.rs](../p25-httpd/src/httpd/api/ws.rs):** add
`"post_pll"` case to the `source=` dispatcher, reading from
`read_post_pll_iq_buffers()` at 9.6 kSPS.

### 9.10 Dashboard — new Plots tab

**One new tab.** [dashboard.html](../p25-httpd/src/httpd/dashboard.html)
structure:

```html
<div class="tab-pane" id="tab-plots">
  <select id="plot_picker">
    <option value="spectrum_wide">Wideband spectrum</option>
    <option value="spectrum">Narrowband spectrum</option>
    <option value="constellation">Constellation</option>
    <option value="eye">Eye (post-PLL)</option>
    <option value="deviation">Deviation + metrics</option>
  </select>
  <canvas id="plot_canvas"></canvas>
  <div id="plot_metrics"></div>   <!-- shown for deviation -->
</div>
```

One canvas, five renderers, picker selects which is active. Each
renderer knows its own poll cadence. No fullscreen toggle beyond
"the tab is the plot" — the canvas fills the tab.

### 9.11 Dashboard — what gets retired

- `/ws/iq?source=post_ddc` and `?source=post_lsm` in the Debug tab
  → **deleted**. They were pre-PLL smeared-eye options. The eye
  plot in the new Plots tab is post-PLL only.
- The two competing spectrogram JS paths in
  `drawSpectrum*` → **collapsed to one**, driven by
  `/api/spectrum_wide`.
- The narrowband `/api/spectrum` stays, but as a *single* picker
  option, not multiple fast/slow variants.

### 9.12 BUILD_TAG

`2026-04-22-phase-10-7-plots` on merge of the HDL + PS + frontend
commits. Updated per-commit on the PS/frontend side to track
progress within the phase.

### 9.13 Order of work

1. This section (you are reading it).
2. P25_ADDRESS_MAP.md update — DDR + banks + IRQs.
3. HDL signal plumbing (LsmDemodLoop + LsmDemod).
4. HDL p25_top.py wiring + register banks + SVD regen.
5. HDL system_bd.tcl + package_ip.tcl.
6. p25-pac regen.
7. PS fpga.rs readers.
8. PS endpoints + WS source.
9. Dashboard Plots tab + retire legacy.
10. BUILD_TAG + commit stack ready for bake.

### 9.14 Risk register

- **HP2 first use.** `ad_mem_hp1_interconnect` has always been the
  P25 pattern; adding HP2 is novel. Fall-back: put the wideband
  spectrometer on HP1 like the other rings (plenty of headroom).
- **Spectrometer clock domain.** `clk2x`/`clk3x` already exist in
  `P25Core` for `P25DDC`. The `Spectrometer` uses `common_edge_2x`
  / `common_edge_3x` — need to share the `ClkNxCommonEdge`
  generator (not duplicate it).
- **SVD regen not CI'd.** Manual step. Phase 10.6 proved it works;
  just don't forget it.
- **`p25.svd` checked in stale.** The generated file must be
  committed alongside the p25_top.py change, otherwise p25-pac
  builds the old register layout and everything silently
  references wrong addresses.
- **Eye-plot density.** Two samples per symbol is enough for the
  diamond to open, but OP25 Datascope uses more. If it looks thin
  on target we can rotate all four Lagrange-interpolated samples
  (adds one `LsmPllRotate` instance → +1 BRAM18) as a follow-up.

## 10. TL;DR

- **Anritsu / OP25 Datascope = post-PLL + post-timing-recovery.**
  Both work on real off-air P25 signals, not just test generators;
  the Anritsu test-pattern capture is just the noise-free calibration
  ceiling of the same view.
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
