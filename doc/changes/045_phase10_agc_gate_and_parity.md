# 045 — Phase 10: AGC noise-floor gate + traffic-chain API parity

**Date:** 2026-04-16
**Branch:** `fishball-p25`
**BUILD_TAG:** `2026-04-16-p10prep-agc-gate-traffic-parity-grantmap`

## Summary

Root-cause fix for the traffic-chain "first call splits into 3+ files"
acquisition failure mode. Combines an HDL change to the LSM AGC loop
with Rust API work that fills in the control-vs-traffic endpoint
asymmetry and adds a live gain knob + persistent grant map.

Two tracks:

1. **HDL** — per-chain LSM AGC gains a noise-floor gate. Inputs
   below a tunable magnitude threshold skip the gain-update step
   entirely, preventing idle-channel noise spikes from dragging the
   gain register into a bad operating point.

2. **Rust** — traffic chain gets symmetric diagnostic endpoints
   matching the control side; a new live-gain endpoint; a
   persistent grant-frequency map; and per-chain `agc_enable` over
   the wire. Scanner-mode TG allow-list was already handled by the
   existing `/api/monitor` + `MonitorList` — no new endpoint needed
   there.

## On-site diagnosis that motivated the change

Session 2026-04-16, Clay County NAC 0x8A1, signal strong (RSSI
86 dB, front-end gain 60 dB):

- A TG 318 call recorded into **three** WAV files instead of one.
  File timings (rec 35 / 36 / 37): 1.80 s → 2.2 s silent gap →
  0.54 s → 2.9 s silent gap → 35.64 s. After the second gap the
  call decoded cleanly for 35 s.
- Dashboard showed `tdu_lc_count = 1129` vs real `tdu_count = 16`
  and `ldu1+ldu2 = 915`. TDU_LC at 70× the real TDU rate is the
  all-ones DUID pattern (`0b1111`) that BCH "corrects" noise
  toward — exact mechanism documented in
  [037](037_phase8_hdl_lsm_review.md).
- `/api/constellation` in idle showed traffic-chain
  `p50(magnitude) = 0.33`, 23 % of samples with `|IQ| < 0.2`, PLL
  residual −0.27 rad, timing drifting. Control chain on the same
  board, same second: `p50 = 0.84`, 0 % tiny, PLL ±0.005 rad,
  timing stable.
- Gain sweep (40 / 50 / 60 / 70 dB front-end, board idle) showed
  traffic-chain convergence only at 70 dB:
  | gain_db | control p50 / tiny / pll | traffic p50 / tiny / pll |
  | --- | --- | --- |
  | 40 | 0.93 / 0 % / +0.000 | 0.41 / 20 % / −0.533 |
  | 50 | 0.96 / 0 % / −0.008 | 0.34 / 29 % / −0.521 |
  | 60 | 0.68 / 0 % / +0.005 | 0.33 / 23 % / −0.271 |
  | 70 | 0.73 / 1 % / −0.226 | **0.68 / 0 % / −0.073** |

The traffic chain isn't starved for signal — it's stuck in a bad
operating point because the idle-channel dibit stream has
intermittent noise spikes, and SDRTrunk's AGC loop is fast-attack
/ slow-release. Every spike drives `req_gain = 1 / mag` tiny and
the asymmetric `min(gain, req_gain)` clamp snaps `gain` down
immediately. The slow 0.05-per-symbol lerp then cannot climb back
because the next spike snaps it down again. When a real call
finally arrives, the AGC has to unwind from that bad operating
point for hundreds of symbols — exactly the observed
acquisition-gap pattern.

## HDL change

### `maia-hdl/p25_hdl/lsm_agc.py`

- New module constant `MAG_UPDATE_THRESHOLD_DEFAULT = 1024`
  (raw Q1.15 → `1024 / 32768 = 1/32 ≈ −30 dBFS`). A healthy LSM
  symbol is near `sqrt(2)/2 · 32 768 ≈ 23 170`, so `1024` leaves
  ~27 dB of margin for the "real signal is present" classification.
- New `LsmAgc(mag_update_threshold=...)` kwarg (default
  `MAG_UPDATE_THRESHOLD_DEFAULT`). Set to `0` to restore the exact
  SDRTrunk-identical behaviour (gate fires only on strictly zero
  magnitude, matching the original `if magnitude > 0` guard).
- `DIV_INIT` state widens its guard: instead of
  `with m.If(mag_val == 0)` it now uses
  `with m.If(mag_val < self._mag_update_threshold)`. Still falls
  through to `APPLY` when gated — outputs still flow at the
  current gain, only the gain-update step is skipped.
- New 16-bit `gate_dbg` counter that increments on each gated
  symbol. Not yet exposed via CSR (would require a register-bank
  expansion + PAC regen); kept as an internal signal reachable
  from HDL simulation.
- Reset path extended to clear `gate_dbg` alongside `gain`,
  `gain_dbg`, and `mag_dbg`.

### `maia-hdl/p25_hdl/lsm_demod_loop.py`

- `LsmDemodLoop.__init__` gains an
  `agc_mag_update_threshold=MAG_UPDATE_THRESHOLD_DEFAULT` kwarg,
  forwarded to its `LsmAgc()` submodule. Lets p25_top instantiate
  control and traffic chains with different thresholds in a later
  change if needed (not used in this bake — both chains share the
  default).
- New `agc_gate_dbg` debug tap wired up from the submodule.

### `maia-hdl/p25_hdl/lsm_demod.py`

- Same `agc_gate_dbg` tap threaded up to the outer `LsmDemod`
  wrapper so future bakes can land the CSR exposure.

### Tests

`maia-hdl/test/test_lsm_agc.py` gains three new cases:

1. `test_mag_update_threshold_gates_gain_update` — warms the AGC
   on `mag = 0.5` until settled, then drives 60 sub-threshold
   symbols (magnitude < `MAG_UPDATE_THRESHOLD_DEFAULT`). Asserts
   `gain_dbg` is bit-for-bit identical before and after, and that
   `gate_dbg` incremented by exactly 60.
2. `test_mag_update_threshold_zero_matches_sdrtrunk` —
   constructs `LsmAgc(mag_update_threshold=0)`, drives 30 weak
   (but non-zero) symbols, asserts `gate_dbg` stays 0 and the
   gain climbs as SDRTrunk would.
3. `test_mag_update_threshold_rejects_invalid` — ensures
   out-of-range thresholds fail at construction (−1 and
   `1 << 17`), with `(1 << 17) − 1` as the legal upper bound.

All 11 AGC tests + the existing
`test_lsm_demod_loop.py` (3 cases) and
`test_lsm_demod.py` + `test_p25ddc.py` (9 cases combined) pass.

## Rust changes

### New endpoints for control/traffic parity

| Endpoint | Twin of | Purpose |
| --- | --- | --- |
| `GET /api/traffic_lsm_dibit_dump` | `/api/control_lsm_dibit_dump` | Sync histogram + dibit histogram + raw-DUID distribution on the traffic framer |
| `GET /api/traffic_iq_capture` | `/api/control_iq_capture` | Rolling recent-dibits buffer for offline analysis |
| `GET /api/traffic_iq_capture_aligned` | `/api/control_iq_capture_aligned` | Arms the next sync hit on the traffic framer, returns full pipeline trace |
| `GET /api/traffic_lsm_control?dc_block=0\|1&agc=0\|1` | `/api/control_lsm_control` | Read + toggle traffic-chain DC blocker + AGC enable |

The traffic `lsm_control` endpoint exposes the **new `agc` toggle**
in addition to `dc_block`, mirroring the new HDL capability on
both chains. `enable` and `dibit_dma_enable` remain read-only from
the API because they're driven by `retune_traffic_chain`.

### `GET/PUT /api/rx_gain?db=N`

Live AD9361 `hardwaregain` knob. Range `[-3, 76]` dB in 1 dB
steps. Previously the only way to change gain was
`/api/reinit?hardwaregain=...`, which rewrites every front-end
field. This is a targeted knob for A/B gain experiments without
disturbing LO / BW / DDC.

Response echoes live gain + mode + RSSI + the previous gain (so
curl-based sweep loops see a delta). Behind `#[cfg(target_os =
"linux")]` like the existing AD9361 endpoints.

### `GET /api/grant_map`

Persistent `HashMap<(tg: u16, frequency_hz: u64), GrantMapEntry>`
populated on **every** grant observed on the control channel,
regardless of follow decision. Rows report `count`,
`encrypted_count`, `first_seen_unix_ms`, `last_seen_unix_ms`.

The response also includes a `frequencies` roll-up — the same
data grouped by frequency only, sorted by activity — so a future
"auto-center the LO on the most active slots" endpoint has the
minimum-max-distance input it needs. The scanner-mode dashboard
will use the per-(tg, freq) rows as its TG picker.

`TrafficManager::tally_grant(talkgroup, frequency_hz, encrypted)`
does the populating. Called at
[main.rs:2040](../../p25-httpd/src/main.rs#L2040), right after
the raw-grant log entry, **before** the monitor-list / encryption
gates. That ordering matters: the map accumulates everything the
control channel announces, even TGs that are filtered out by the
scanner-mode allow-list or permanently blocked because they're
encrypted.

### Scanner mode — no new endpoint needed

Investigation found `MonitorList` in
[src/monitor.rs](../../p25-httpd/src/monitor.rs) and `/api/monitor`
already implement exactly the "lock to a list of TGs" behaviour
the user asked for — priority-ordered allow-list, empty = accept
all, grant-pipeline gate at
[main.rs:2040-2062](../../p25-httpd/src/main.rs#L2040-L2062).
Draft `/api/monitor_tgs` + `TrafficManager::monitor_tg_filter`
were removed before commit to avoid duplicating the existing
mechanism. Dashboard work will surface `/api/monitor` alongside
the new `/api/grant_map` picker.

### BUILD_TAG

Bumped to `2026-04-16-p10prep-agc-gate-traffic-parity-grantmap`
so `/api/system` on-target reports the combined shipment.

## Expected on-target behaviour (post-flash)

Before the bake flashed:

- Traffic-chain `p50(magnitude) = 0.33` in idle, 23 % tiny
  samples, PLL hunting, `best_sync_distance = 38`.
- First call after an Idle gap takes 2-5 seconds of acquisition
  before LDUs land cleanly. Splits across recording files while
  the AGC unwinds.
- `tdu_lc` count inflated by noise-corrupted NIDs that BCH
  "corrects" to all-ones (`0xF` = `TDU_LC`).

After flash (prediction):

- Idle-traffic `agc_gate_dbg` should climb steadily (gate firing
  on noise), `gain_dbg` should stay pinned at its last
  real-signal-driven value.
- Idle `p50(magnitude)` should shift upward as the steady-state
  gain matches the last real-signal operating point instead of
  the noise-spike floor.
- First-call acquisition should land within the 180 ms LDU
  budget, not 2-5 s. Recording files should stop fragmenting on
  the first call after a gap.
- `sync_near_misses / sync_hits` ratio on the traffic framer
  should drop (fewer noise-sync pickups). `tdu_lc` count should
  drop proportionally.

## Things this does NOT do

- `agc_gate_dbg` is still an internal signal. Future bake should
  add it to the `lsm_agc_debug` register (bank offset `0b110`)
  and the `traffic_lsm_agc_debug` equivalent (`0b110` in the
  traffic bank) so the dashboard can display it live. That adds
  SVD + PAC regen scope we didn't want in this bake cycle.
- Per-chain threshold override is wired through the kwargs but
  both chains use the default `1024` in this bake. If Clay-vs-
  Duval measurements show the threshold needs to differ, a
  one-line change in `p25_top.py` will set it independently.
- LO auto-center using `/api/grant_map` is a separate change.
  The data is now available; the decision algorithm and
  optional auto-apply flag are not.
- Dashboard changes (scanner-mode picker UI, traffic-chain debug
  panels, gain slider) are a separate frontend PR after on-target
  validation of this bake.

## How to verify on-target

1. Flash the new bitstream + `p25-httpd` binary via
   `build.bat --p25` (Tezuka build).
2. `GET /api/system` — confirm
   `build_tag = "2026-04-16-p10prep-agc-gate-traffic-parity-grantmap"`.
3. While idle (no calls): `curl /api/constellation?chain=traffic`.
   Expect `p50(|IQ|)` > 0.6 and `pll_final` close to 0.
4. `curl /api/grant_map` after a few minutes — should show rows
   for every distinct TG/freq grant observed.
5. `curl "/api/rx_gain?db=55"` — AD9361 gain immediately drops
   to 55 dB, traffic-chain AGC holds its previous gain (because
   the gate fires on the smaller samples until it adapts).
6. Open the Radio tab, wait for a call — recording should be a
   single WAV, not 3 fragments.

## References

- [037 Phase 8 HDL LSM review](037_phase8_hdl_lsm_review.md) —
  original traffic-chain root-cause analysis that surfaced the
  "BCH decoder corrected half the LDUs into TDU_LC" mechanism.
- [040 Phase 10-prep AGC + DDC](040_phase10prep_agc_and_ddc.md) —
  original AGC port from SDRTrunk `P25P1DemodulatorLSM.java:157-172`.
- [044 Bake #2 traffic IQ chain symmetry](044_bake2_traffic_iq_chain_symmetry_and_dashboard_batch.md) —
  previous bake that landed the traffic IQ DMA ring we're now
  diagnosing idle behaviour on.
