# Phase 10.7 plot data — HDL post-PLL ring vs /api/constellation

## Why this exists

Before reflashing with the Phase 10.7 dashboard changes we pulled live
data from both the new HDL post-PLL WebSocket and the legacy
`/api/constellation` software path, so we could characterise whether
the "breathing" (good snapshot → scatter → good) the user was seeing
on the dashboard is a real signal artefact or a measurement artefact
of the software-PLL-per-snapshot pipeline.

## Capture setup

Two 30 s windows against Clay County NAC 8A1 control (860.9625 MHz =
LO 858.1 MHz + DDC offset 2.862965 MHz), back-to-back:

| Source | Tool | Sample rate | Block window | # blocks |
|---|---|---|---|---|
| `/api/constellation?chain=control` | [p25_constellation_capture.py](../../../tools/p25_constellation_capture.py) | 1 Hz poll × 156 symbols | 1 s | 26 |
| `/ws/iq?source=post_pll&chain=control` | [p25_constellation_capture_hdl.py](../../../tools/p25_constellation_capture_hdl.py) | 9.6 kSPS (2 sps × 4800 Hz) | 1 s = 4800 symbols | 30–34 |

## Breathing headline

| Metric | HDL ring (new) | /api/constellation (old) | Verdict |
|---|---|---|---|
| `radius_mean` min – max | 0.943 – 0.965 (**1.02×**) | 0.605 – 0.907 (**1.50×**) | HDL radius is **~25× more stable** |
| `radius_mean` stdev | 0.005 | 0.072 | 14× tighter on HDL |
| `cluster_var_mean` max/min | 1.83× | **8.15×** | HDL ~4–5× more stable |
| `angle_std` max/min | 1.02× | 1.39× | HDL ~14× more stable |

The `/api/constellation` breathing is almost entirely a **measurement
artefact**: the endpoint re-runs the software Gardner + PLL over each
fresh 33 ms / 156-symbol snapshot, and the software PLL doesn't settle
in that window. The HDL chain is continuously locked, so the ring's
cluster centres stay pinned to within ±0.5 % of `mean = 0.956`.

**After the Phase 10.7 dashboard flash, the constellation on the Plots
tab reads the HDL ring and the breathing you were seeing should
largely disappear.**

## Shape difference (important gotcha)

The HDL post-PLL tap (`rotate_sym.i_out / q_out`) is **after** the
differential demod, so each sample is `z[n] · conj(z[n-1])`. For a
clean LSM signal this produces an **X-pattern** in I/Q scatter — the
four dibit values land at angles ±π/4 and ±3π/4, and the signal traces
continuously between them as consecutive symbols rotate.

`/api/constellation` taps **before** the diff-demod, so it shows the
traditional **4-dot constellation** at (±1, ±1) — one cluster per
dibit.

Both are correct; they are different views of the same signal:

| View | Where to find it | What it shows | What good looks like |
|---|---|---|---|
| 4-dot constellation | `/api/constellation` | pre-diff-demod post-PLL | 4 tight clusters at (±1, ±1) |
| P25 eye (deviation) | `/ws/iq?source=post_pll` → `atan2(Q, I)·4/π` | post-diff phase angle | samples snap to ±1 / ±3 rails |
| X-pattern | `/ws/iq?source=post_pll` I/Q scatter direct | post-diff-demod complex | X through origin (mostly ±π/4, ±3π/4) |

The Plots-tab dashboard today:

- **Constellation picker** — reads the HDL ring, renders I/Q scatter (X-pattern). Good for watching dibit-transition structure; NOT the traditional 4-dot view.
- **Eye picker** — reads the HDL ring, renders `atan2(Q, I)·4/π` with ±3 / ±1 rails. This is the correct P25 LSM eye.

If we want a traditional 4-dot constellation on the Plots tab, a future
HDL bake can add a pre-diff-demod post-PLL tap. For now,
`/api/constellation` remains the authoritative 4-dot view.

## What "clean" looks like, by endpoint

### /api/constellation (pre-diff post-PLL, software SDRTrunk-matched)

| Metric | Clean | Scatter |
|---|---|---|
| `cluster_var_mean` | < 0.02 | > 0.05 |
| `radius_mean` | 0.85 – 0.95 | < 0.75 |
| `angle_std` | < 1.3 | > 1.5 |
| `pll_final` | `abs(pll) < 0.05` | transient excursions |

### /ws/iq?source=post_pll (post-diff post-PLL, HDL)

Constellation-shape expectations: X-pattern with bright concentrations
at (±√2/2, ±√2/2) — i.e. near the 45° diagonals — and dim trails
through origin.

Eye expectations (atan2·4/π view): clean 3-eye opening between
rails at ±3 and ±1. Locked signal snaps to the rails at
integer symbol periods; transitions are the eye "crosses" halfway
between.

## Raw captures

- `constellation_breathing/summary.jsonl` — 26 /api/constellation snapshots
- `hdl_ring_capture/summary.jsonl` — 30–34 HDL-ring 1-s blocks
- `hdl_ring_capture/const_*.png` — per-block I/Q scatter (HDL, X-pattern)
- `hdl_ring_capture/eye_*.png` — per-block atan2·4/π eye (HDL, ±3/±1 rails)
- `constellation_breathing/*.png` — per-snapshot /api/constellation scatter

## References worth keeping

- [tools/p25_constellation_capture.py](../../../tools/p25_constellation_capture.py) — legacy /api/constellation capture (still useful for the pre-diff 4-dot view)
- [tools/p25_constellation_capture_hdl.py](../../../tools/p25_constellation_capture_hdl.py) — new HDL-ring capture (constellation + eye, this phase)
- [p25-httpd/src/lsm/demod.rs](../../../p25-httpd/src/lsm/demod.rs) — SDRTrunk-faithful Rust LSM demod driving /api/constellation
- Memory `reference_sdrtrunk_paths` — where to find Java sources for cross-check
- Memory `reference_p25_constellation_interpretation` — angular/radial/cluster failure-mode cheat sheet
