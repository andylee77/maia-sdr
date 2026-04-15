# P25DDC fork — v2 filter design (filter-tool-only step)

Date: 2026-04-15
Branch: `bisect-safety`
Scope: `tools/p25_ddc_filter_design.py` only (no HDL, no Rust, no bake yet).

## Summary

First concrete step of the P25DDC fork. Regenerates the three DDC stage
filters to a SDRTrunk-faithful, unit-DC-gain design that spends far
more of the available FIR coefficient-RAM budget, with the goal of
fixing the adjacent-channel fold-back bug that Phase 10-prep left
behind.

**This commit does not change on-target behaviour.** It only
regenerates the Python filter-design script and its output data
dump. The Rust coefficient tables in
[p25-httpd/src/fpga.rs](../../p25-httpd/src/fpga.rs) still carry the
Phase 10-prep coefficients; the new coefficient tables are in
[041_p25ddc_fork.txt](041_p25ddc_fork.txt) and will be copied in when
the full fork lands.

## Why

Two bugs in the Phase 10-prep DDC design surfaced during on-target
testing on 2026-04-15:

### Bug 1 — LsmDecimator2 fold-back band is unprotected

[maia-hdl/p25_hdl/lsm_decimator.py:10-15](../../maia-hdl/p25_hdl/lsm_decimator.py#L10-L15)
documents that `LsmDecimator2` is a **naive /2 decimator** with no
anti-alias filter of its own. It emits every other input sample and
relies on the upstream DDC stage 3 to have already killed everything
outside ±8 kHz before the /2. That assumption was true of the original
64-tap Kaiser stage 3, but Phase 10-prep's 97-tap Parks-McClellan
stage 3 (pb=10 kHz, sb=31.25 kHz, -90 dB) has a much wider transition
band, and the cascade response in
[040_ddc_filter_redesign.txt](040_ddc_filter_redesign.txt) shows only
**-25 dB rejection at 25 kHz offset**.

A signal at 25 kHz in the DDC output aliases to **6.25 kHz after
LsmDecimator2's /2**, landing directly on the P25 channel center. The
downstream [LsmFir LPF](../../maia-hdl/p25_hdl/lsm_fir.py#L273) passes
0..7250 Hz (it's an exact replica of SDRTrunk's baseband Remez LPF),
so the aliased interference is *not* further attenuated at the slicer.
A 20 dB-stronger adjacent emitter at 25 kHz offset leaves the P25
signal with 5 dB of margin — well below the decode threshold.

This is the structural cause of the control-CRC gap and the
traffic-side lock-on-retune failures tracked in
`project_p25_control_throughput_regression.md` and
`project_p25ddc_fork_next_project.md`.

### Bug 2 — Hidden coupling between coefficient design and gain/scale

Phase 10-prep's [rescale_to_peak()](../../tools/p25_ddc_filter_design.py)
forced every stage's peak quantised coefficient to land at the Q1.17
maximum (131071). That bakes a non-unit per-stage DC gain into the
filter shape, preserving the ~17x / ~5x / ~8x cascaded gain the
original Maia Kaiser filters had. It also silently couples
coefficient design and `macc_trunc`, which cost one full bake cycle
on 2026-04-15 when unit-DC-gain filters starved the demod by ~700x
(see `feedback_maia_ddc_peak_scale.md`). The rescale trick needs to go.

## What changed in `tools/p25_ddc_filter_design.py`

1. **Unit DC gain throughout.** `rescale_to_peak()` is deleted. Each
   stage's Q1.17 quantised taps sum to ~131072 = 1 << 17, and the
   scale factor is an explicit per-stage `P25_OUTPUT_SHIFT{1,2,3} = 17`
   constant computed from the measured DC gain. Coefficient design
   and gain/scale are now orthogonal.
2. **Tighter ripple target.** 0.01 dB passband ripple across all
   three stages (matches SDRTrunk's baseband LPF specification).
   Phase 10-prep was 0.1 dB.
3. **Stage 1 passband narrowed** from 300 kHz to 200 kHz. Still far
   wider than any P25 NCO-tune requirement, but gives stage 1 more
   headroom for a deeper stopband.
4. **Stage 2 passband narrowed** from 100 kHz to 60 kHz. Same reasoning.
5. **Stage 3 is the big one.** Passband narrowed from 10 kHz to
   7250 Hz (an exact match with SDRTrunk's Remez baseband LPF
   passband edge, and with the downstream LsmFir LPF). Stopband
   target pushed to -220 dB so remez grows the tap count until it
   hits the FIR4DSP 256-tap budget, which it does at 248 taps.
   The goal isn't actually -220 dB rejection — it's forcing remez
   to spend the full coefficient budget on a very steep equiripple
   transition, which is what drives the mid-transition rejection
   at the 15.625-31.25 kHz fold-back band.
6. **New diagnostic output.** Per-stage DC gain, Q1.17 peak
   utilization, `output_shift` derivation, and a reworked cascaded
   rejection table that explicitly labels the LsmDecimator2
   fold-back band and maps each fold-back frequency to where it
   lands on the P25 channel after the downstream /2 decimation.

## Numbers — before vs after

Full tap counts, stopband depth, and cascade response:

| Stage | Phase 10-prep | v2 | notes |
|---|---|---|---|
| Stage 1 taps | 48 | 176 | scipy remez numerically limited above 176 at this band config; 256 budget not fully used, but -109 dB is >35 dB below ADC noise floor |
| Stage 1 stopband | -93 dB | -109 dB | |
| **Stage 2 taps** | **56** | **128** | **full FIR2DSP budget** |
| Stage 2 stopband | -93 dB | -193 dB | |
| **Stage 3 taps** | **104** | **256** | **full FIR4DSP budget** |
| Stage 3 stopband | -90 dB | -129 dB | at the stopband edge; the `stopband_db=220` target deliberately trades stopband depth for steeper mid-transition slope (the thing we actually care about) |
| Cascaded DC gain | ~1x (via rescale hack) | **1.0001** (unit, explicit) | |
| Per-stage output_shift | baked into coefficient rescale | **17, 17, 17** (explicit constants) | |

### Critical fold-back band — this is the whole point

| Offset in DDC output | Folds to (after LsmDecimator2 /2) | Phase 10-prep | v2 | LsmFir handles it? |
|---|---|---|---|---|
| 6.25 kHz | (passband) | 0 dB | 0 dB | n/a — desired signal |
| 15.625 kHz | 15.625 kHz | n/a | -6 dB | yes, in LsmFir stopband |
| 18.75 kHz | 12.5 kHz | n/a | -18 dB | yes, in LsmFir stopband |
| 20 kHz | 11.25 kHz | n/a | -25 dB | yes, in LsmFir stopband |
| **25 kHz** | **6.25 kHz** | **-25 dB** | **-71 dB** | **NO — aliases onto channel center** |
| 28 kHz | 3.25 kHz | n/a | -120 dB | no — aliases into passband |
| 31.25 kHz | (fold-back edge) | -91 dB | -129 dB | |

The single remaining worst case is 25 kHz → 6.25 kHz, because it
aliases to a frequency that LsmFir's downstream tight baseband LPF
passes through. The v2 design hits it with 71 dB of DDC-side
rejection, versus Phase 10-prep's 25 dB. A 20 dB-stronger adjacent
P25 emitter at +25 kHz now lands at **-51 dB** on the slicer, rather
than the previous **-5 dB** — a **46 dB improvement** at the single
most critical offset. That is the whole point of this fork.

## Raw design-tool output

Saved as [041_p25ddc_fork.txt](041_p25ddc_fork.txt). That file
contains:

- the per-stage equiripple design summaries,
- the per-stage DC gain + `output_shift` computation,
- the full cascaded rejection table,
- the ready-to-paste `P25_FIR{1,2,3}_COEFFS` Rust tables for
  `p25-httpd/src/fpga.rs`,
- the `P25_OUTPUT_SHIFT{1,2,3}` Rust constants.

The raw `.txt` is the authoritative reference; this `.md` is the
human-readable narrative.

## What this commit does NOT do

1. No HDL changes. `p25_hdl/p25_top.py` still uses `maia_hdl.ddc.DDC`;
   no new `p25ddc.py` module yet.
2. No Rust changes. `p25-httpd/src/fpga.rs` still carries the Phase
   10-prep coefficient tables.
3. No bake. The firmware on the board still runs Phase 10-prep.

Those are the next three steps in the fork plan.

## Next steps (explicit, in order)

1. **New HDL module `maia-hdl/p25_hdl/p25ddc.py`.** Port-compatible
   wrapper around `Mixer` + `FIR4DSP` + `FIR2DSP` + `FIR4DSP` with
   an explicit `output_shift` parameter per stage, rather than the
   Maia DDC's fixed `macc_trunc`. Drop-in replacement at
   [p25_top.py:163](../../maia-hdl/p25_hdl/p25_top.py#L163) and
   [p25_top.py:408](../../maia-hdl/p25_hdl/p25_top.py#L408).
2. **New `ddc_output_shift` register fields** in the control and
   traffic register banks.
3. **Rust side** — copy the new `P25_FIR{1,2,3}_COEFFS` and
   `P25_OUTPUT_SHIFT{1,2,3}` constants from
   [041_p25ddc_fork.txt](041_p25ddc_fork.txt) into
   `p25-httpd/src/fpga.rs`, extend `configure_ddc()` to load the
   output-shift registers.
4. **Amaranth-sim unit tests** for `P25DDC` — per-stage response
   checks against a scipy reference, a cascade test on a
   multi-tone vector that includes the 25 kHz fold-back adjacent,
   and a `p25_top` smoke test with the new module substituted.
5. **Bake + on-target verify** — the real acceptance test is a
   live `rf_bandwidth` sweep back up to 8 MHz. If TSBK CRC holds
   and the 25 kHz fold-back audio contamination stops, the fork
   succeeded.

## References

- [040_ddc_filter_redesign.txt](040_ddc_filter_redesign.txt) —
  Phase 10-prep design data dump (for comparison)
- [maia-hdl/p25_hdl/lsm_decimator.py](../../maia-hdl/p25_hdl/lsm_decimator.py) —
  the naive /2 decimator that this fork is accommodating
- [maia-hdl/p25_hdl/lsm_fir.py:273-297](../../maia-hdl/p25_hdl/lsm_fir.py#L273-L297) —
  the downstream baseband LPF (SDRTrunk-exact)
- [maia-hdl/p25_hdl/lsm_fir.py:300-331](../../maia-hdl/p25_hdl/lsm_fir.py#L300-L331) —
  the downstream RRC matched filter (SDRTrunk-exact)
