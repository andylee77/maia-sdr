# Change 049 — PS perf endpoint + JMBE recurrence + scanner pivot

**Date:** 2026-04-29
**Branch:** fishball-p25
**BUILD_TAG:** `2026-04-29-jmbe-cos-recurrence`
**Bake required:** YES — `lsm_agc.py` reset block

## Summary

Mixed-scope session that landed three substantive changes plus a
strategic redirect for the project as a whole. In session order:

1. PS Cores observability endpoint + dashboard panel
2. `/ws/audio` close-detection fix (resolves "3 audio WS clients" anomaly)
3. HDL AGC seed-load diagnostic fix
4. Tezuka rootfs cleanup (mosquitto / api_controller / DATV watchers
   stripped from p25 builds)
5. SSH key + jffs2 host-key persistence
6. Scanner architecture pivot — drop multi-traffic-channel work
7. JMBE synthesis hot-path optimization (~10× speedup on inner loop)

The pivot in (6) reframes everything that follows. Detail below.

## Strategic pivot — single-chain scanner

Operator decision: drop multi-traffic-channel work entirely. Revised
goal is a highly optimized single-chain portable scanner-radio with
three operating modes:

- Follow one selected TG across freq hops
- Follow a group of TGs (whichever is currently active)
- Scan-all-unencrypted on a site

Architectural implications:

- `CHANNELIZER_REDESIGN.md` Stages 1 and 2 are **out of scope**.
  Multi-decoder polyphase channelizer, per-channel persistent
  decoders — not happening.
- Stage 1's "IQ ring lookahead" idea is reduced in scope: instead of
  feeding N decoders, it can optionally feed *one* software demod
  path to fill the cold-settle window for a single freq.
- Optimization order changes:
  1. Vocoder NEON / recurrence — frees one A9 core
  2. HDL settle reduction — sync correlator threshold, Costas BW,
     BCH t-threshold during hunting
  3. IQ buffer catch-up only if (1) + (2) don't get us there

See `memory/project_2026_04_29_scanner_pivot.md` for the durable
record of this decision.

## What landed

### `GET /api/ps_cores`

Per-core CPU% + per-thread CPU% over a configurable interval. Two
`/proc/stat` + `/proc/self/task/*/stat` reads spaced `interval_ms`
apart (default 250 ms). No state plumbing — each request is
self-contained.

```json
{
  "cpus": [{"id": 0, "busy_pct": 3.9, ...}, {"id": 1, "busy_pct": 100.0, ...}],
  "threads": [{"name": "p25-vocoder", "tid": 888, "cpu_pct": 101.4, ...}, ...],
  "loadavg_1": 0.43,
  "num_cpus": 2,
  "total_threads": 12
}
```

cpu_pct is "% of one core" so a thread pinning a core reads 100,
matches `top` semantics. busy_pct = 100 - idle - iowait.

Dashboard adds a **PS Cores** card below Board Info on the Radio tab,
polled every 2 s, with per-core busy bars (green / amber / red) plus a
top-12 thread table sorted by cpu_pct desc.

### `/ws/audio` close-detection fix

The handler was blocked on `rx.recv().await` for the audio broadcast
channel and never drove the socket recv side. When a browser tab
closed during chain idle (no audio chunks broadcasting), the receiver
stayed subscribed until the next chunk arrived and `send()` failed.
Stale tabs accumulated as `audio_tx.receiver_count()`; this is what
produced the operator-observed "3 audio WS clients" reading on the
dashboard with one real listener.

Now uses `tokio::select!` over both `rx.recv()` and split socket recv
side. Close frames + transport errors observed promptly during idle.

### HDL `gain_dbg` reset clobber fix

[maia-hdl/p25_hdl/lsm_agc.py:725-727](../../maia-hdl/p25_hdl/lsm_agc.py)

The reset block correctly loaded the `gain` register from `seed_in`
via `Mux(self.seed_in != 0, self.seed_in, GAIN_INIT)`, but in the same
cycle clobbered `gain_dbg.eq(0)` — and `gain_dbg` is the only
PS-visible readback. The PS reads it immediately after the reset
pulse, before any decision-strobe re-populates it from the live gain
register, so the readback structurally always read 0. Result:
`/api/log` retune events showed `agc_drift = -agc_seed_written` on
every single retune, looking like the seed wasn't loading.

The data path was fine the whole time. Fix mirrors the seeded value
into `gain_dbg` (Q9.7 truncation of seed_in, or GAIN_INIT >> 4 when
seed = 0). After bake + flash, `traffic_agc_post_reset` matches
`agc_seed_written` and `agc_drift` becomes a real load-correctness
diagnostic.

### JMBE synthesis recurrence

[p25-httpd/src/jmbe/mod.rs](../../p25-httpd/src/jmbe/mod.rs) — the
`get_voiced` synthesis loop was outer=n (160) / inner=li (~56) with
1-2 `f32::cos` calls per inner iteration. ~16 k libm cosine calls per
20 ms frame on Cortex-A9 (no native trig) ≈ 1.5 M cycles ≈ 2-3 ms of
pure cosine per frame. Vocoder pegged one A9 core during a single P25
call.

Restructured to outer=li / inner=n with the standard trig recurrence:

```
cos(θ + Δ) = cos(θ)·cos(Δ) − sin(θ)·sin(Δ)
sin(θ + Δ) = sin(θ)·cos(Δ) + cos(θ)·sin(Δ)
```

The linear-phase branches (Algs #131 / #132 / #133) now use the
recurrence: 2 transcendentals per harmonic-term (initial `sin_cos`
for base + step) plus 4-mul-1-add-1-sub per sample ≈ 7-8 cycles each.
Quadratic-phase branch (Alg #136, ~14 % of harmonics — only when
li < 8 AND |Δω| < 10 % ω₀) retained direct cos for now; quadratic
recurrence is messier and the linear path is the bulk.

`synthesis_window(n)` and `synthesis_window(n − SPF)` table lookups
were also hoisted out of the harmonic loop into precomputed
160-element arrays since they don't depend on li.

**Verification:** Added `test_synthesis_signature_stable` that
captures aggregate signal stats (RMS / peak / sample[80] / mean) for
a 5-frame TG 301 burst pulled live from `/api/imbe_dump`. Reference
values were captured against the pre-optimization decoder; the new
implementation passes within 1 % relative / 2e-5 absolute. All 65
existing JMBE tests also pass.

**Expected on-target:** vocoder thread drops from ~100 % to ~25-30 %
of one core during a call. Pending validation with PS Cores during a
live call after flash.

### Tezuka rootfs cleanup

`tezuka_fw/board/tezuka/common/post-build-p25.sh` (new) — strips
`S50mosquitto` and `S95bgcript` (which launches `api_controller.sh`
+ DATV watchers) from p25 builds. None of it is consumed by
p25-httpd, which talks to the FPGA via IIO directly. mosquitto +
api_controller were burning ~50 % of one core for the first ~5 min
post-boot dumping the entire AD9361 sysfs to MQTT topics, then
settling to <1 %. Clean baseline now.

### SSH key + jffs2 host-key persistence

`tezuka_fw/board/tezuka/common/overlay_p25/root/.ssh/authorized_keys`
(new) — operator's `id_ed25519.pub` baked into rootfs.
`post-build-p25.sh` enforces `0700` / `0600` perms.

`overlay_base/etc/init.d/S55hostkeys` had a long-standing path bug:
S21misc reads from `/mnt/jffs2/etc/dropbear/<filename>` but
S55hostkeys was writing to `/mnt/jffs2/<filename>` (no subdirectory).
The backup never met the restore, so dropbear regenerated host keys
every boot. Fixed; also added a `dropbearkey -t ed25519` invocation
in S55hostkeys for the first-boot case where the dropbear daemon
hasn't lazily generated the key yet.

### Tooling — `tools/p25_settle_measure.py`

Stage 0 channelizer-redesign measurement (now repurposed for the
single-chain scanner work). Anchors on `/api/log` retune / nco_skip
events with seed diagnostic, times to next `voice` TRF_HDU /
TRF_LDU1 / TRF_LDU2 event. Heartbeat mode polls
`/api/traffic_lsm_dibit_dump` for `sync.hits` + `nid_attempts`
deltas. Setup flags `--lock-freq` and `--agc-off` with auto-restore
on exit.

## Findings

### 3.4 s cold-settle floor (n=33)

```
median first-frame: 3276 ms   p90: 3444 ms   max: 3485 ms
bimodal: same-chain follow-on = 50 ms cluster
         cold engagement      = 3.3 s cluster
```

Frame-sync acquisition (2-3 LDU periods × 1.35 s) is the dominant
component. PLL + AGC seeding only addresses ~500 ms of the front
porch. Confirms `CHANNELIZER_REDESIGN.md`'s structural argument:
single-tuner-retune cannot get below ~50 ms with pure software/HDL
seeding because sync state is unseedable.

For the new scanner goal, this is the bar to lower. Knobs to
investigate:

- Sync correlator confidence threshold (loosen during hunting,
  tighten in steady state)
- Costas loop bandwidth (wider for fast pull-in)
- BCH NID t-threshold during hunting
- Pre-rotate dibit ring in software based on PLL seed

### "3 audio WS clients" was stale-receiver accumulation

Closed browser tabs went undetected during chain idle because the
WS handler only awoke on broadcast recv. Fixed.

### Vocoder=100% / tokio=high was normal-mode JMBE cost

Live PS Cores during a call: cpu0=3.9% (tokio + framer + everything
else), cpu1=100% (p25-vocoder thread). Core 0 had massive headroom —
no PS scheduling starvation. The high CPU was JMBE on Cortex-A9
without NEON or trig recurrence. Recurrence shipped above addresses
it.

### mosquitto was startup-only, not chronic

Initial observation of mosquitto at 49.7% was the first ~5 min of
boot while api_controller dumped AD9361 sysfs to MQTT. Settled to
<1% chronic. Removed for cleanliness, not for the audio jitter (which
was a wrong hypothesis — see vocoder note above).

## Pickup points for next session

1. Flash this build and re-measure on a fresh boot:
   - Vocoder CPU% via `/api/ps_cores` during a call → expect ~25-30 %
   - Settle stats via `tools/p25_settle_measure.py --duration 600`
   - AGC drift diagnostic via `/api/log` retune events → expect 0
2. With vocoder NEON/recurrence done, decide whether to:
   - Tackle HDL settle reduction next (3.4 s → sub-second)
   - Or pursue IQ-buffer catch-up for cold-settle audio recovery
3. Quadratic-phase branch optimization (Alg #136) deferred — only
   pursue if recurrence isn't enough headroom for scanner workload
4. Pre-allocate JMBE per-frame work buffers (~14 heap allocs per
   frame currently). Smaller win than recurrence; defer until
   budget pressure says otherwise.
