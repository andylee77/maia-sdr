# P25 Tuning Redesign — Implementation Plan

**Status:** Draft. Single-shot implementation, no phased rollout and no
backward-compatibility for the current `/api/reinit` Tune UX — this is still
dev and clean beats compat.
**Author:** Working plan, 2026-04-22.
**Scope:** `p25-httpd/` (PS/Rust + dashboard) and `tools/p25_ddc_filter_design.py`.
**Out of scope:** Vivado / HDL module rebuild. The existing DDC gateware already
exposes the knobs this plan needs (see §3).

## 1. Goals

1. Let the operator pick **sample rate, RF bandwidth, and center frequency**
   from a small set of presets at runtime — no rebuild, no boot-arg change.
2. Separate **center frequency** (RX LO, the analog tuner) from **radio
   frequency** (the P25 channel the DDC is parked on), the way a scanner radio
   does. The current UI conflates "tune" with "re-init the whole front end".
3. **Auto-center** by default: when the operator types a new radio frequency,
   the receiver picks an RX LO so that the NCO offset stays inside ±(BW/2) and
   well clear of the passband edges, then drops the DDC onto the channel.
   A **Lock** toggle pins the current RX LO and only moves the NCO, so the
   operator can sweep across a band without the analog front end resettling.
4. **6.25 kHz / 12.5 kHz step buttons** next to the radio-frequency field, so
   the operator can walk the P25 channel grid with `Up/Down` instead of
   retyping numbers. The existing `step="0.001"` (1 kHz) free-form box goes
   away.
5. **Pre-calculated sample-rate/bandwidth presets** in a dropdown. Presets are
   the values whose decimation cascades are known to produce the correct
   62.5 kSPS DDC output with deep enough anti-alias stopband; anything off the
   preset list is rejected.

## 2. Physical constraints

These are *fixed* and bound the design space. Any preset must respect them.

- **LsmFir / LsmDecimator2 expect 62.5 kSPS at the DDC output**, which
  downstream becomes 31.25 kSPS at the symbol-decision rate. The LPF_TAPS_31250
  and 105-tap RRC in `maia-hdl/p25_hdl/lsm_fir.py` are baked for that rate.
  This is the pivot point for the whole sample-rate table.
- **Total DDC decimation = sample_rate_Hz / 62 500.** Any preset where this
  is not an integer that factors cleanly across the 3 stages is out.
- **FIR tap budgets:** FIR4DSP 256 taps max, FIR2DSP 128 taps max
  (`tools/p25_ddc_filter_design.py` §"Tap budget"). Per-stage decimation must
  leave enough taps per branch for the stopband target.
- **Stage 3 must attenuate 15.625–31.25 kHz by ≥60 dB** because LsmDecimator2
  is a naïve /2 with no anti-alias filter (this is the root cause v2 fixed;
  see [doc/changes/041_p25ddc_fork.md](changes/041_p25ddc_fork.md)). Don't
  loosen this.
- **AD9361 valid ranges (ADI AD9361 datasheet):**
  - `sampling_frequency`: ~2.083 MHz to 61.44 MHz
  - `rf_bandwidth`: 200 kHz to 56 MHz
  - `RX_LO`: 70 MHz to 6 GHz
  In practice `rf_bandwidth` should track `sample_rate` to keep the analog
  filter from aliasing into the DDC's stage-1 passband.
- **NCO range:** ±(sample_rate/2). For an 8 MSPS preset that's ±4 MHz; for a
  2 MSPS preset it shrinks to ±1 MHz. Scanner mode must respect this when
  deciding whether to retune the LO or just shift the NCO.

## 2a. Aside — why the post-LSM eye looks so different from the post-DDC eye

This comes up every time the sample-rate / decimation conversation gets
reopened, so it's worth nailing down before §3. The data below is all from
[doc/diagnostics/2026-04-18/eye/](diagnostics/2026-04-18/eye/).

**Post-DDC tap** ([post_ddc PNG](diagnostics/2026-04-18/eye/eye_control_post_ddc_nsym2_iq.png))
— `source=post_ddc` on `/ws/iq`, fed from `iq_dma` / `traffic_iq_dma`:

| | value |
|---|---|
| sample rate | 62.5 kSPS |
| samples/symbol | **13.02** (at 4800 sym/s) |
| RMS I / Q | 177 / 176 |
| peak \|I+Q\| | **595** |
| chain position | DDC output, **before** LsmDecimator2, **before** LsmFir RRC, **before** LsmAgc, **before** LsmPllRotate, **before** timing recovery |

**Post-LSM tap** ([post_lsm PNG](diagnostics/2026-04-18/eye/eye_control_post_lsm_nsym2_iq.png))
— `source=post_lsm` on `/ws/iq`, fed from `lsm_iq_dma` / `traffic_lsm_iq_dma`:

| | value |
|---|---|
| sample rate | 31.25 kSPS |
| samples/symbol | **6.51** (at 4800 sym/s) |
| RMS I / Q | 239 / 240 |
| peak \|I+Q\| | **909** |
| chain position | **after** LsmDecimator2 /2, **after** LsmFir RRC, **before** LsmAgc, **before** LsmPllRotate, **before** timing recovery |

### What "half amplitude" really is

The post-LSM eye is **not** at half amplitude. Its RMS is ~35% *higher*
and its peak is ~53% *higher* than post-DDC (909 vs 595, 239 vs 177).
The visual impression of halved amplitude is a pixel-scaling artefact —
each PNG auto-ranges to its own peak so the y-axis is different, and the
post-LSM plot spreads over more gridlines because the RRC pulse has
ringing tails the raw DDC signal doesn't. The RRC matched filter has
passband gain > 1; that's why RMS goes up.

### What the "32 vs 64" question is really asking

"Is 31.25 kSPS vs 62.5 kSPS (rounded 32 vs 64) the reason the post-LSM
eye is smeared?" — **No.** The smearing survives at any sample rate.
Proof: a clean P25 eye needs every overlay to land at the same symbol
phase, and the LSM tap is captured **before the PLL derotator and before
timing recovery**. The π/4-DQPSK constellation is still rotating by
π/4 per symbol at that point, and the 6.51 samples/symbol relationship
to the 4800-baud symbol clock is not integer, so overlays land at
arbitrary sub-symbol phases. The eye never aligns, so it washes out.

Dropping to 32x decimation (the `4M` preset) would put 6.51 sps at the
post-LSM tap and 13.02 sps at post-DDC — identical to today. Doubling to
128x keeps those figures identical too. **Sample-rate preset choice does
not move the post-LSM eye.** What moves it is adding a post-PLL tap in
the HDL, flagged as TODO in
[project_clean_eye_plot_todo.md](../../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/project_clean_eye_plot_todo.md).

### What the presets *do* affect

The post-DDC eye gets slightly cleaner at higher decimation because the
stage-3 filter removes more out-of-band energy — that's the entire point
of the P25DDC v2 fork. Going from `8M` to `4M` slightly relaxes stage 3
(the transition band fits in fewer taps), but v2 already has enough
stage-3 headroom that this shouldn't be visible in the eye. Going wider
(`12M`, `16M`) gives the operator more NCO window for scanner mode, at
the cost of higher AD9361 noise floor; the eye shouldn't materially
change at the channel of interest because the DDC still concentrates on
a 12.5 kHz slot.

**Takeaway for this plan:** the preset choice is about RF coverage and
NCO window size, not about eye-plot quality. The eye-plot cleanup is a
separate follow-up (post-PLL HDL tap), tracked outside this doc.

## 3. Current state (what we have, what's missing)

### What's already there

- **HDL DDC is already runtime-programmable.** `maia_hdl/ddc.py` exposes
  `decimation1/2/3`, `operations_minus_one1/2/3`, `bypass2`, `bypass3` and a
  1024-word coefficient RAM as PS-writable registers. `fpga.rs::load_fir1/2/3`
  already reprograms decimation and `operations_minus_one` on every
  `configure_ddc()` call — but always with the same 3 constants
  (`P25_DEC1=4, P25_DEC2=4, P25_DEC3=8`) and the same 3 coefficient tables.
  **No Vivado rebuild is required to add new presets.**
- **`/api/reinit` already accepts `sample_rate` and `rf_bandwidth`** query
  params ([p25-httpd/src/httpd/api/tuning.rs:34-206](../p25-httpd/src/httpd/api/tuning.rs#L34-L206)).
  What's missing is that the PS-side DDC config ignores the new rate — it
  reloads the same 8 MSPS-optimized coefficients regardless.
- **Offset math already works.** `set_ddc_frequency` computes
  `nco_word = freq_to_nco(control_freq - rx_lo, sample_rate)` and range-checks
  ±(sr/2). Scanner mode reuses this directly.
- **`AppState.current_rx_lo`** already tracks the live LO so grant-follower
  traffic retunes stay correct. Scanner mode will reuse this.

### What's missing

- **Preset table.** Nothing today knows that 8 MSPS and 4 MSPS are both valid
  and 6.4 MSPS is not. Presets need to live in one place, queried by the PS
  at config time and by the dashboard at page load.
- **Coefficient sets for rates other than 8 MSPS.** The v2 design
  (`tools/p25_ddc_filter_design.py`) emits one set for 8 MSPS. It needs to
  emit one set per preset.
- **Scanner-style UX.** Today's Board Info panel is three number boxes and a
  Tune button. No step buttons, no center/lock split, no channel grid.
- **`/api/presets` endpoint.** The dashboard needs a way to get the preset
  table without hard-coding it in JS.
- **Center-auto vs center-lock mode** (scanner-vs-VFO semantics) isn't
  modeled anywhere — currently the LO moves every time you type a new
  `control_freq` unless you happen to also type the same `rx_lo`.

## 4. Proposed design

### 4.1 Preset table

Ship a single const table on the PS side, exposed via `GET /api/presets`,
consumed by the dashboard on load. Pivot is `sample_rate / total_dec = 62_500`
for every preset (see §2). Candidate initial contents:

| Name | Sample rate | Total dec | Stages (d1×d2×d3) | RF BW | NCO window | Notes |
|------|------------:|----------:|-------------------|------:|-----------:|-------|
| `2M`  | 2.000 MHz |  32 | /4 × /4 × /2 |  2 MHz | ±1.0 MHz | lowest FPGA load; tight window |
| `3M`  | 3.000 MHz |  48 | /4 × /3 × /4 |  3 MHz | ±1.5 MHz | 3-factor in stage 2 |
| `4M`  | 4.000 MHz |  64 | /4 × /4 × /4 |  4 MHz | ±2.0 MHz | clean 4³ factoring |
| `5M`  | 5.000 MHz |  80 | /5 × /4 × /4 |  5 MHz | ±2.5 MHz | 5-factor in stage 1 |
| `6M`  | 6.000 MHz |  96 | /4 × /6 × /4 |  6 MHz | ±3.0 MHz | 6-factor in stage 2 |
| `7M`  | 7.000 MHz | 112 | /7 × /4 × /4 |  7 MHz | ±3.5 MHz | prime 7 in stage 1 (tap-budget sensitive) |
| `8M`  | 8.000 MHz | 128 | /4 × /4 × /8 |  8 MHz | ±4.0 MHz | **current default** |
| `9M`  | 9.000 MHz | 144 | /9 × /4 × /4 |  9 MHz | ±4.5 MHz | 3² in stage 1 (tap-budget sensitive) |
| `10M` | 10.000 MHz | 160 | /5 × /4 × /8 | 10 MHz | ±5.0 MHz | |
| `12M` | 12.000 MHz | 192 | /6 × /4 × /8 | 12 MHz | ±6.0 MHz | |
| `16M` | 16.000 MHz | 256 | /8 × /4 × /8 | 18 MHz | ±8.0 MHz | max practical; higher AD9361 noise |

Factorization rules that governed the table:

- `decim_width=[7,6,7]` → stage 1 and stage 3 take any value up to 127,
  stage 2 up to 63.
- FIR4DSP cap `operations × decimation ≤ 128`, FIR2DSP cap
  `operations × decimation ≤ 128` (see
  [p25-httpd/src/hardware/fpga.rs:310-377](../p25-httpd/src/hardware/fpga.rs#L310-L377)).
  That puts a hard ceiling of 256 taps per FIR4DSP stage and 128 taps per
  FIR2DSP stage regardless of the per-stage decimation.
- Stage 3 is always the *hardest* to filter because its output at 62.5 kSPS
  must attenuate 15.625 kHz energy by ≥60 dB for LsmDecimator2. Dropping
  decim3 below /4 makes the relative transition band wider and easier, but
  shifts the burden onto stage 2, which only has 128 taps. The `3M` and
  `5M` presets put a non-power-of-2 factor in stage 1 or 2 and keep the
  stage-3 budget at /4, which is the easiest stage-3 case.
- `7M` and `9M` are feasibility-sensitive — prime / odd-prime decimations
  don't fold as cleanly into the polyphase coefficient layout. The filter-
  design tool (§4.2) will confirm they meet the stopband target before
  they ship; any preset that fails is dropped from the table at build time.
- AD9361 sample-rate tuning is quantized by the PLL/ADC multiplier chain,
  so actual rates may differ from nominal by tens of ppm. The DDC NCO
  already computes offsets against the *actual* sample rate reported by
  the AD9361 sysfs readback, so this is harmless.

Each preset entry is:

```json
{
  "name": "8M_wide",
  "sample_rate_hz": 8000000,
  "rf_bandwidth_hz": 8000000,
  "decim1": 4, "decim2": 4, "decim3": 8,
  "fir1_taps": "P25_FIR1_8M",
  "fir2_taps": "P25_FIR2_8M",
  "fir3_taps": "P25_FIR3_8M",
  "nco_half_window_hz": 4000000
}
```

The tap-table names are keys into a Rust `static` map of coefficient slices,
one slice per preset per stage. All presets produce 62.5 kSPS at the DDC
output.

### 4.2 Filter-design tool changes

`tools/p25_ddc_filter_design.py` grows a `--preset NAME` CLI flag that
selects the `(sample_rate, decim1/2/3)` tuple and re-runs the same Parks-
McClellan design. Output is one `p25_ddc_coeffs_<preset>.rs` per preset.
`build_fpga.bat` does **not** need to change — the coefficient tables are
baked into p25-httpd at `cargo build` time, not into the bitstream.

For each preset, the tool keeps the stage-3 stopband requirement
(≥60 dB down at 15.625 kHz from the passband edge) and adjusts tap counts to
fit the tap budget. If a preset can't meet the stopband target within the
budget, the tool refuses to emit it and the preset is dropped from the
table — better than shipping an aliased preset.

### 4.3 PS-side changes

- `configure_ddc()` takes a preset handle, not raw `P25_DEC*` and
  `P25_FIR*_COEFFS` constants. The three `load_firN` calls look up the
  preset's coefficient slice and its per-stage decimation.
- `AppState` gains `current_preset_name` (an `AtomicU8` index into the table),
  `center_locked` (`AtomicBool`), alongside the existing `current_rx_lo`.

Two endpoints, both new, both replacing `/api/reinit`:

- **`POST /api/preset`** — apply a sample-rate / BW preset. This is the
  slow path that resettles the AD9361 and reloads DDC coefficients.

  ```
  POST /api/preset
    {
      "preset": "8M",                 // required; one of /api/presets names
      "center_freq_hz": 858_500_000,  // optional; defaults to current LO
      "gain_mode": "manual" | "slow_attack" | "fast_attack" | "hybrid",
      "gain_db": 60.0                 // required iff gain_mode=="manual"
    }
  ```

- **`POST /api/tune`** — radio-frequency scanner tune. Fast path; in Auto
  mode it only moves the AD9361 LO when the requested frequency leaves the
  current window:

  ```
  POST /api/tune
    {
      "radio_freq_hz": 858_012_500,   // required
      "center_mode": "auto" | "lock"  // optional; default follows center_locked
    }
  ```

  - **auto:** if the requested `radio_freq` is within ±(BW/2 − 100 kHz) of
    the current RX LO, only the DDC NCO moves (fast, no AD9361 resettle).
    Otherwise the PS picks a new RX LO at the nearest multiple of 100 kHz
    that keeps the NCO offset inside ±(BW/2 − 100 kHz), retunes the AD9361,
    and reprograms the DDC NCO. The 100 kHz guard band keeps the NCO off
    the stage-1 passband edge. Sets `center_locked=false`.
  - **lock:** only the NCO moves. If the requested `radio_freq` is outside
    the window, return 409 with the window edges in the body so the UI can
    surface "frequency outside locked band". Sets `center_locked=true`.

**`/api/reinit` is removed.** The existing dashboard handler (`retuneControl`)
and the three-input Board Info row are deleted in the same commit that adds
the new widget. Boot-time config still goes through CLI args in
[p25-httpd/src/main.rs:233-294](../p25-httpd/src/main.rs#L233-L294); those
args resolve to a preset name at startup (the boot default is `8M`).

### 4.4 Dashboard UX

Replace the three free-form MHz boxes
([p25-httpd/src/httpd/dashboard.html:625-645](../p25-httpd/src/httpd/dashboard.html#L625-L645))
with a scanner-style widget:

```
┌──────────────────────── Radio Tuning ────────────────────────┐
│  Sample Rate  [8 MHz  ▼]     RF BW  [8 MHz  ▼]   [Apply]     │
│                                                              │
│  Center (RX LO)  858.500 MHz   [ Auto / Lock ]               │
│                                                              │
│  Radio Freq      [ 858.012500 ] MHz                          │
│                  [▲ +6.25k]  [▲ +12.5k]  [▼ −12.5k]  [▼ −6.25k]
│                  [ Tune ]                                    │
│                                                              │
│  NCO offset   −487.5 kHz      (window ±3.9 MHz)              │
└──────────────────────────────────────────────────────────────┘
```

Design rules:

- **Sample Rate / RF BW** are `<select>` populated from `/api/presets`. They
  are locked together: picking a sample-rate preset auto-selects the matching
  RF BW; users can override RF BW only within presets that support it.
- **Apply** button calls `/api/reinit` with the preset's values. Disabled
  until the selection differs from the live values.
- **Center / Auto-Lock** toggle: in Auto mode the displayed center follows
  the actual RX LO (read-only); in Lock mode the operator can type an RX LO
  directly and it stays put across Tune clicks.
- **Radio Freq** is the primary operator control. Step buttons commit
  immediately (`POST /api/tune`); typing in the field requires Tune / Enter.
  Arrow keys map to ±12.5 kHz; Shift+Arrow to ±6.25 kHz.
- **NCO offset** is a live read-only indicator of `radio_freq − rx_lo`,
  colored green inside ±(BW/2 − 100 kHz), yellow in the guard band, red
  outside.

### 4.5 Scanner considerations (deferred, but design for it)

The user's longer-term ask is scanner-style operation across a frequency
list. This plan doesn't implement the list itself, but the API shape above
is the substrate:

- `POST /api/tune` is the primitive the scanner walks.
- A future `POST /api/scan` can take `{ freqs: [...], dwell_ms, stop_on_grant }`
  and call `/api/tune` internally. Scanner mode will need to know which tune
  calls can stay inside the current LO window (fast, NCO-only) versus which
  will force an AD9361 resettle (slow, ~1–2 ms). A scan list can be ordered
  to minimize resettles.

## 5. Rollout

Single commit, no phasing. Order of operations inside the commit:

1. Extend `tools/p25_ddc_filter_design.py` to sweep all preset rates,
   verify each preset's stage-3 stopband, and emit one
   `p25_ddc_coeffs_<preset>.rs` module per feasible preset. Drop any
   preset that fails the stopband check from the table.
2. Add the preset static table + coefficient includes in `fpga.rs`.
   Refactor `configure_ddc()` to take a preset handle.
3. Add `/api/presets`, `POST /api/preset`, `POST /api/tune`. Remove
   `/api/reinit` and its handler.
4. Replace the Board Info tuning / modulation / gain rows with the scanner
   widget in `dashboard.html`. Drop `retuneControl()` and its helpers;
   add `applyPreset()`, `tuneRadio()`, `stepRadio(±Hz)`.
5. Update `doc/P25_API.md` and add a `doc/changes/NNN_tuning_redesign.md`.
6. Bump `BUILD_TAG` per
   [feedback_bump_build_tag.md](../../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/feedback_bump_build_tag.md).

**Post-flash validation** — on-target smoke test covering: (a) boot lands
on `8M`; (b) switching to `4M` and back at runtime succeeds without
gateware reset; (c) Auto-mode step buttons move only the NCO within the
window; (d) a radio-freq entry outside the window re-centers in Auto and
409s in Lock; (e) at least the `4M`/`8M`/`16M` presets decode the Clay
County NAC 8A1 control channel cleanly.

## 6. Risks and open questions

- **Preset coverage gaps.** 2 MSPS gives a narrow ±1 MHz NCO window, which
  cramps Auto mode; we may want to add 3 MSPS or 5 MSPS variants later.
  Defer until operators actually hit the window limit.
- **`rf_bandwidth ≠ sample_rate`.** The AD9361 lets these differ; in
  practice P25 performance is best when BW ≈ sample_rate. Initial presets
  pin BW = sample_rate; expose an "advanced" override only if a use case
  appears.
- **AGC / gain reset on sample-rate change.** Current `/api/reinit` re-applies
  gain settings; verify behavior when gain mode is `slow_attack` and rate
  changes mid-AGC convergence.
- **Scanner dwell on locked-encrypted channels.** Out of scope for this
  plan but worth flagging: `service_options.encrypted` from
  `GroupVoiceChannelGrant` is already available
  ([reference_p25_encryption_flag_from_control_channel](../../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/reference_p25_encryption_flag_from_control_channel.md)),
  so a future scanner can skip encrypted grants without per-call parsing.
- **Does the existing LsmDecimator2 /2 stay valid across all presets?** Yes:
  every preset produces 62.5 kSPS at the DDC output by construction, so the
  downstream /2 → 31.25 kSPS and the LsmFir / LsmDecimator2 stay untouched.
  This is why the preset table is pinned to 62.5 kSPS as its pivot.

## 7. Files expected to change

| File | Change |
|------|--------|
| `tools/p25_ddc_filter_design.py` | Add `--preset`; emit per-preset tables |
| `p25-httpd/src/hardware/fpga.rs` | Coefficient tables become per-preset; `configure_ddc()` takes a preset handle |
| `p25-httpd/src/state.rs` (or equivalent AppState module) | Add `current_preset_name`, `center_locked` |
| `p25-httpd/src/httpd/api/tuning.rs` | Preset validation in `/api/reinit`; new `/api/tune` handler |
| `p25-httpd/src/httpd/api/mod.rs` (or router) | Register `/api/presets`, `/api/tune` |
| `p25-httpd/src/httpd/dashboard.html` | Scanner-style tuning widget; replace the 3-input row |
| `p25-httpd/src/main.rs` | `BUILD_TAG` bumps per commit |
| `doc/P25_API.md` | Document `/api/presets` and `/api/tune` |
| `doc/changes/NNN_tuning_redesign_*.md` | Per-phase change docs |

## 8. References

- [doc/changes/041_p25ddc_fork.md](changes/041_p25ddc_fork.md) — P25DDC v2
  filter design and the stopband constraints that govern preset viability.
- [doc/P25_PS_PIPELINE.md](P25_PS_PIPELINE.md) — PS-side DDC + LSM pipeline
  overview.
- [doc/P25_API.md](P25_API.md) — current HTTP API surface that this plan
  extends.
- [tools/p25_ddc_filter_design.py](../tools/p25_ddc_filter_design.py) —
  coefficient-design tool to extend.
- [maia-hdl/p25_hdl/p25ddc.py](../maia-hdl/p25_hdl/p25ddc.py) — the DDC
  wrapper; no change needed, already parameterizable.
