# 046 — Tuning redesign: DDC presets + scanner-style API

**Date:** 2026-04-22
**Branch:** fishball-p25
**Plan:** [doc/P25_TUNING_REDESIGN.md](../P25_TUNING_REDESIGN.md)

## Summary

Replaces the old `/api/reinit` endpoint and the three-MHz-textbox
Board Info widget with a preset-driven tuning API and a scanner-style
dashboard widget. The HDL DDC is unchanged — its decimation and FIR
RAM were already runtime-programmable. What changed is entirely on
the PS + dashboard + filter-design-tool side.

## What changed

### Filter-design tool

[tools/p25_ddc_filter_design.py](../../tools/p25_ddc_filter_design.py)
now sweeps a fixed preset table and emits a single
`p25-httpd/src/hardware/ddc_presets.rs` module with per-preset
coefficient + decimation tables. Each preset is parameterised by
`(sample_rate_hz, d1, d2, d3)` where
`d1*d2*d3 = sample_rate_hz / 62_500`, so every preset produces
62.5 kSPS at the DDC output and the downstream LsmFir / LsmDecimator2
chain is untouched.

Presets shipped (all meet ≥-55 dB cascaded rejection at 25 kHz, the
LsmDecimator2 fold-back hotspot):

| Preset | Sample rate | Decim | NCO window | 25 kHz rejection |
|--------|------------:|-------|-----------:|-----------------:|
| `2M`   |  2.000 MHz | /2/4/4 | ±1.00 MHz | -69.7 dB |
| `3M`   |  3.000 MHz | /3/4/4 | ±1.50 MHz | -67.2 dB |
| `4M`   |  4.000 MHz | /4/4/4 | ±2.00 MHz | -69.7 dB |
| `5M`   |  5.000 MHz | /5/4/4 | ±2.50 MHz | -69.7 dB |
| `6M`   |  6.000 MHz | /6/4/4 | ±3.00 MHz | -79.8 dB |
| `7M`   |  7.000 MHz | /7/4/4 | ±3.50 MHz | -67.2 dB |
| `8M`   |  8.000 MHz | /4/4/8 | ±4.00 MHz | -70.8 dB |
| `9M`   |  9.000 MHz | /9/4/4 | ±4.50 MHz | -67.2 dB |
| `10M`  | 10.000 MHz | /10/4/4 | ±5.00 MHz | -69.7 dB |
| `12M`  | 12.000 MHz | /12/4/4 | ±6.00 MHz | -62.5 dB |
| `16M`  | 16.000 MHz | /16/4/4 | ±8.00 MHz | -62.5 dB |

The `8M` preset is bit-identical to the validated 2026-04-15 P25DDC v2
design (176/128/256 taps, stage 3 passband 7.25 kHz, stopband 31.25 kHz).
All other presets use `[sample_rate_MHz, 4, 4]` which puts the
non-power-of-2 factor in stage 1 where the anti-alias transition band
is the widest.

Per-stage tap budget caps derived from the FIR4DSP /2 folded and
FIR2DSP unfolded polyphase loaders in `fpga.rs::load_fir_*dsp()`:

```text
max_taps_fir4dsp(decim) = 2 * (128 // decim) * decim
max_taps_fir2dsp(decim) = (128 // decim) * decim
```

### Rust

- [p25-httpd/src/hardware/ddc_presets.rs](../../p25-httpd/src/hardware/ddc_presets.rs) — auto-generated module. `DdcPreset` struct, `PRESETS` slice, `DEFAULT_PRESET` (8M), `find_preset(name)`. Compiled on every platform (no `cfg(linux)`); only the FIR loading uses it under `cfg(linux)`.
- [p25-httpd/src/hardware/fpga.rs](../../p25-httpd/src/hardware/fpga.rs) — `configure_ddc()` and `configure_traffic_ddc()` now take `&DdcPreset` instead of `sample_rate_hz: f64`. The inline `P25_DEC*` / `P25_FIR*_COEFFS` constants are gone; they live in `ddc_presets.rs` now.
- [p25-httpd/src/httpd/api/tuning.rs](../../p25-httpd/src/httpd/api/tuning.rs) — `get_reinit` deleted. Three new handlers: `get_presets`, `post_preset`, `post_tune`.
- [p25-httpd/src/httpd/mod.rs](../../p25-httpd/src/httpd/mod.rs) — `AppState` no longer carries `boot_rx_lo` / `boot_sample_rate` / `boot_rf_bandwidth` / `boot_hardwaregain`. Carries `current_sample_rate_hz` (atomic), `current_preset_idx` (atomic), `center_locked` (atomic) alongside the existing `current_rx_lo`.
- [p25-httpd/src/main.rs](../../p25-httpd/src/main.rs) — `--sample_rate` and `--rf_bandwidth` CLI flags removed; replaced by `--preset <name>` (default `"8M"`). Boot path resolves the preset, writes `sample_rate_hz` + `rf_bandwidth_hz` from the preset, and passes the preset to `configure_ddc()` / `configure_traffic_ddc()`.
- [p25-httpd/src/app/follower.rs](../../p25-httpd/src/app/follower.rs) — grant follower takes `Arc<AtomicU32> current_sample_rate_hz` instead of a fixed `sample_rate: f64`. Reads the live value on every retune so `POST /api/preset` propagates to the traffic chain without a respawn.

### API

```text
GET  /api/presets
  → { presets: [{name, sample_rate_hz, rf_bandwidth_hz,
                 decim: [d1,d2,d3], nco_half_window_hz,
                 rejection_25k_db, total_decim}...],
      current, default, center_locked }

POST /api/preset
  body: { preset: "8M",
          center_freq_hz?, gain_mode?, gain_db? }
  → { ok, applied[], errors[], preset, sample_rate_hz,
      rf_bandwidth_hz, rx_lo_hz, nco_offset_hz, readback{} }

POST /api/tune
  body: { radio_freq_hz, center_mode?: "auto" | "lock" }
  → { ok, radio_freq_hz, rx_lo_hz, nco_offset_hz,
      lo_moved, center_locked, preset, window_half_hz }
  returns 409 if Lock + outside window
```

Scanner semantics in `/api/tune`:

- **auto:** if the requested radio frequency is within the guard-banded window (±(BW/2 − 100 kHz) of the live RX LO), only the DDC NCO moves. Otherwise, the LO is recentered to the nearest 100 kHz step and then the NCO is set.
- **lock:** the LO never moves. 409 if the requested frequency is outside the guard-banded window; the body includes `window_half_hz` + `rx_lo_hz` so the UI can show the edges.

### Dashboard

[p25-httpd/src/httpd/dashboard.html](../../p25-httpd/src/httpd/dashboard.html) — the "Control (MHz) / Center (MHz) / BW (MHz) / Tune" row is replaced by:

- A preset dropdown populated from `/api/presets` with an **Apply preset** button.
- A **Radio Freq** field with four step buttons (±6.25 kHz, ±12.5 kHz) and a Tune button. `Enter` in the textbox commits; `Arrow Up / Arrow Down` step ±12.5 kHz (±6.25 kHz with Shift).
- **Center: Auto / Lock** radio buttons, plus live `RX LO` + `NCO offset` readouts fed from the existing `/api/stats` poll (no extra polling cost).

## Why

User is overhauling the P25 radio to give the operator scanner-radio semantics rather than the reboot-style `/api/reinit` UX, and to let the board adapt to different sites without rebuilding firmware for a new sample rate. The HDL already had everything needed; only the PS and dashboard needed to grow the concept of a preset + a fast-path tune. Single-commit replacement — no back-compat for `/api/reinit` since this is still dev.

## BUILD_TAG

`2026-04-22-tuning-redesign` (bumped in `p25-httpd/src/main.rs`).
