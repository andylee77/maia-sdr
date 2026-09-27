# 054 — Low-latency dibit delivery + air-time traffic gating (finding F4)

**Date:** 2026-09-26. **Branch:** fishball-p25. **BUILD_TAG:**
`2026-09-26-dibit-lowlatency-airtime` (the same tag also carries the autoppm sign fix,
doc/changes/055). **Bake required:** NO — PS only (p25-httpd). No HDL change.

## Why

Finding F4 of `doc/HW_VALIDATION_SUITE.md` (evidence: doc/changes/053, "Ring transport"
item 4):

- `DmaStreamRingWrite` advances `last_buffer` and raises its IRQ only when a whole 4 KiB
  sub-buffer (16384 dibits = 3.413 s) completes, and the PS reader delivered whole
  sub-buffers on IRQ. Dibits reached the PS 0–3.41 s after they were demodulated, in
  3.41 s blocks — on the control channel too, so grants were decoded up to 3.41 s late
  and the traffic retune happened late (fits the 3.2–3.5 s cold First-IMBE maxima in
  doc/changes/049).
- The traffic reader sampled `locked = current_talkgroup != 0` once per 3.41 s batch,
  while TG changes, framer resets and CallClose acted in real time: cross-call bleed
  (the previous call's dibits decoded under the new call on same-TG re-grants and
  same-frequency reuse) and tail truncation (a batch arriving after CallClose was
  dropped). IMBE batches were stamped with TG / call_id / captured_at at processing time.

The ring hardware is bit-exact (053, `xport.p25_ring_prbs`), so the fix is entirely in
how the PS reads and attributes the data.

## What changed

### 1. Position-based dibit reader (both rings)

New portable module `p25-httpd/src/hardware/dibit_ring.rs` (host-tested) + new reader
loops in `app/dibit_readers.rs`.

HDL facts used (`maia_hdl/dma.py` `DmaStreamRingWrite`, `p25_hdl/dibit_packer.py`,
`p25_top.py` banks 0xA0 / 0xC0):

- 32 dibits per 64-bit word, 16-beat bursts (128 B = 512 dibits ≈ 107 ms), ring 8 × 4 KiB
  = 32 KiB (27.3 s per lap), base aligned to the ring size.
- `*_dibit_next` (0xB0 / 0xD0, plain R) is `awaddr`, the address of the NEXT burst to be
  issued. AW runs at most two bursts ahead of W, so the burst being filled starts at
  `next − 256 B`, and every burst below that has had all 16 W beats sent (packet-mode
  HP1 interconnect FIFO) and lands in DDR within microseconds.
- `last_buffer` is `*_drop_count` (0xAC / 0xCC) bits [18:16]. The reader never touches
  the read-to-clear words 0xA4 / 0xC4 / 0x0C.

Position math:

```text
off     = next_address mod 32768            (base is ring-aligned)
delta   = (off − abs_prev mod 32768) mod 32768
abs_new = abs_prev + delta                  (absolute byte position, u64, never wraps)
safe    = abs_next(previous poll) − 256     (≥ one poll old, never < 2 ms old)
deliver [pos, safe), pos = safe             (whole 64-bit words; dibit index = 4 × byte)
```

- The lap is resolved by continuity (poll ≈ 40 ms ≪ 27.3 s lap). Guard, each case
  logged to the EventLog and followed by a re-anchor ("resync") plus a framer reset:
  address advance larger than `1200 B/s × elapsed × 1.25 + 512 B` or going backwards
  (`jump`), ≥ 90 % of the lap budget since the previous reading (`stall`; the new lap is
  chosen from elapsed time × rate), ring base changed (`base_changed`),
  `last_buffer` disagreeing with `next_address` about the lap phase (`phase_mismatch`,
  once per episode: `(last_buffer + 1) mod 8` must be the sub-buffer the next AW targets
  or the one before it), and, at delivery, undelivered bytes older than
  `abs_next + 256 − 32768` (`overrun`: already or about to be overwritten; only
  reachable after two consecutive long reader stalls, because delivery uses the
  previous reading's safe end — those bytes are skipped, never handed out).
- Start-up: the first reading anchors `abs = off + 32768` and `pos = abs − 256`
  (delivery starts at the first burst completed after the reader started). A fresh DMA
  engine (`last_buffer = 7`, `next = base + 256`) passes the phase check.
- Before copying, every covering 4 KiB sub-buffer is invalidated through the maia-kmod
  ioctl (every poll; a sub-buffer still being written is safe to invalidate).
- Poll interval 40 ms by default (`--dibit-poll-ms`, `POST /api/dibit_delivery?poll_ms=`).
  The DMA IRQ is kept as an extra wake hint; a wake within 2 ms of the previous reading
  only defers.
- The control reader uses the same path, so grants reach the follower ≈ 0.1–0.2 s after
  they were on the air instead of up to 3.41 s.

Expected delivery age (burst fill 0–107 ms + 1–2 poll intervals): the host simulation
of the DMA model (70 s, 40 ms ± 5 ms polls, occasional IRQ wakes) measures mean
113.7 ms, max 194.6 ms, no gap or overlap across two ring laps, every delivered byte
landed before the reading it was taken from. Pre-054: 0–3413 ms.

### 2. Production clock and air time

`DibitClock` (same module) maps absolute dibit indices to PS monotonic time. Each reading
bounds the dibits produced so far: with `A = abs_next / 128`,
`D(t) ∈ [512·(A − 2), 512·(A − 1) + 32]` (+32: DibitPacker word in flight), widened by
4 dibits. While the chain runs, `D(t) = D(t_ref) + 4800·(t − t_ref)`; the clock
intersects every reading with the projected interval (drift allowance ±200 ppm), reseeds
on an empty intersection, and keeps pause/resume periods so dibits produced before a
`traffic_lsm_enable` pause map to the right time.

Error bound for the production time of dibit `i`:
`|t̂ − t| ≤ uncertainty_dibits / 2 / 4800 s + drift + HDL pipeline delay`. The interval
is ≤ 552 dibits (≈ 57 ms half-width) right after start-up or a reseed and converges as
burst boundaries fall between polls; in the simulation it reaches 43 dibits (≈ ±4.5 ms)
within 10 s. The constant HDL delay between antenna and slicer (DDC + LSM FIRs, ≈ 5–10 ms)
is not modelled. `/api/dibit_delivery` reports the live `uncertainty_ms`.

### 3. Air-time epochs on the traffic ring

New portable module `p25-httpd/src/app/dibit_airtime.rs` (host-tested).

Every action that changes what the traffic chain's dibits mean is recorded as a cut on
the dibit axis at the **production index at action time** (next dibit the HDL will
produce), with a snapshot of the live context (TG, source, call_id, encrypted, freq):

| Action | Recorded by | Cut |
|---|---|---|
| retune (NCO + optional LSM reset + enable), bare NCO write, LSM reset, `traffic_lsm_enable` 0→1 | `IpCore` hooks (`retune_traffic_chain`, `set_traffic_ddc_frequency`, `pulse_traffic_lsm_reset`, `set_traffic_lsm_enable`) — every caller covered | discontinuity: framer reset + settle discard |
| `traffic_lsm_enable` 1→0 | same | pause (clock stops) |
| grant accepted with retune | follower (`GrantHold`) | gate closed until the retune lands |
| TG set / released, same-freq resume, channel reuse, encrypted teardown, CallClose, grant refresh changing source / encryption | follower (`TgChange`, `CallClose`, `CtxUpdate`) | full context |
| new call_id | lifecycle (`CallOpen`, via `ImbeForwarder::set_live_call_id`) | call_id only |
| TG force-idle | `/api/encrypted_tgs` | full context |

Placement:

- Hardware cuts carry the next-address register read under the IpCore lock right after
  the action, lifted onto the absolute axis with the reader's latest position: this is
  the "next-address register plus the reader's position" derivation. The reading is
  intersected with the clock; the cut goes to the LOW edge of the interval and
  `(hi − lo) + settle_dibits` (default 48 dibits = 10 ms, the DDC + LSM FIR flush) are
  discarded after it and counted (`dibits_discarded_presettle`).
- Software cuts go to the interval midpoint, without discard.
- No cut is placed below `claimed_end`, the end of what the reader already took (the
  reader claims cuts under the same lock order: snapshot, copy and claim happen while it
  holds the IpCore lock, so any hardware cut recorded later is necessarily at or after
  the claimed data). `cuts_clamped` counts raises; they only occur when the clock is off
  by more than the reader's one-burst lag.

Processing (`plan_chunk`): each delivered chunk is split at its cuts; each piece is fed
(or gated when its context has TG 0, or discarded before a discontinuity's settle point)
under its own context. The framer is reset at every discontinuity and at every gate flip
(a partial frame of one call is never completed with another call's dibits). Pure
context cuts do not reset the framer, so frames are attributed by the index at which
they COMPLETE: a new PTT's HDU that began just before the grant was processed is kept
under the new call. A `CallClose` closes the gate at its cut: the closing call's
in-flight dibits (produced before the close, delivered after it) are decoded under the
closing call instead of being dropped.

While feeding a piece, `ImbeForwarder` reads TG / source / call_id / encrypted from the
piece's context (`begin_segment` / `end_segment`, only under the traffic decoder's write
lock, so the sw_demod feeder still sees the live values). IMBE batches (`ImbeBatch`, new
struct replacing the tuple) carry that call_id / TG, the encrypted flag, and
`captured_at_ms` = estimated air time of the dibit word that completed the LDU. In-band
HDU / LDU2 encryption latches the piece's context (sticky within that call) and mirrors
into the live flag only while the live call is still that call.

Downstream:

- vocoder: the encryption skip uses the batch flag, not the live `call_encrypted` read at
  vocode time.
- recorder: chunks flagged `airtime` route by `call_id` (active or draining slot); the
  capture-time window stays the fallback for legacy / poll chunks and call_id 0.
- lifecycle: an `airtime` chunk of another call no longer refreshes the active call's
  keep-alive or first-audio time.

### 4. Gating fixes found on the way

- **Stale CallClose released the chain.** The follower released the traffic chain
  (TrafficChain idle, TG 0, source / freq cleared) on EVERY `CallClose`, including the
  synthetic open/close pair the lifecycle emits for each not-followed grant (sticky-lock,
  encrypted or monitor-list rejects on other channels) and the predecessor's close of a
  preempting grant, which arrives after the follower already moved to the new grant. On a
  busy site, any rejected grant zeroed the TG of the call being followed, so its dibits
  were gated off until another primary grant re-acquired it. The follower now releases
  only for the live call (`current_call_id`), and the lifecycle publishes a successor's
  call_id before it closes the predecessor. The "call quality" snapshot for the next
  retune is taken only for real closes.
- In the accepted-grant path the `CcGrantArrival` boundary is now sent right after
  `handle_grant_event` (µs later than before) so the grant hold is recorded before the
  lifecycle can publish the new call_id.

### 5. Instrumentation

- `GET /api/dibit_delivery`: per ring — modes, dibit age at delivery (mean / p50 / p90 /
  p99 / max + histogram, weighted per dibit), production-clock state and uncertainty,
  backlog, counters (polls, IRQ wakes, bytes / dibits delivered, resyncs and skipped
  bytes, phase mismatches, copy errors, cuts recorded / applied / clamped, epoch splits
  inside a chunk, framer resets, dibits fed / gated / discarded pre-settle), last resync;
  traffic also the last 64 applied cuts (kind, index, estimate width, discard, context,
  estimated air time, record-to-apply delay).
- EventLog (`system` category): mode switches, every resync, `/api/dibit_delivery`
  changes, and a delivery summary per ring every 5 minutes (only when data flowed).
- Ages are measured in every mode (the legacy path keeps the tracker and clock running on
  its IRQ wakes), so legacy vs. airtime is comparable on the same metric.

### 6. Fallback switch

`--dibit-delivery airtime|poll|legacy` (default `airtime`) and
`POST /api/dibit_delivery?mode=...[&ring=control|traffic]`:

- `legacy`: pre-054 whole sub-buffers on IRQ, live per-batch gating, real-time framer
  resets in the follower (the IRQ wait now also times out after 500 ms so a mode switch
  or a lost wake-up is noticed; a timeout wake with no new sub-buffer delivers nothing).
- `poll`: low-latency delivery with the pre-054 live gating (isolates latency from
  attribution in an A/B).
- `airtime`: low-latency delivery + epochs.

Hand-over is seamless legacy → poll (continues at the first undelivered sub-buffer); poll
→ legacy skips the rest of the current sub-buffer (no duplicates) and resets the framer.
The follower / API skip their real-time framer resets only while the traffic ring
actually runs airtime (`ImbeForwarder::epochs_active`).

## Tests

`cargo test` (host): 148 passed, 0 failed, 4 ignored (pre-054: 103 passed; 42 new here,
3 are the autoppm tests of 055). New:

- `hardware/dibit_ring_tests.rs` (23): geometry, phase check against the DMA model over
  two laps, copy plan (sub-buffer crossing, ring wrap), start-up anchor and the
  previous-reading rule, wraparound continuity, resync on backwards / implausible jump /
  stall (correct lap re-anchor) / base change / phase mismatch (once per episode) /
  overrun after two long stalls (lapped bytes never handed out), early
  IRQ wake, legacy tracking + hand-over rewind, lift of action-time readings, full 70 s
  simulation (gapless, landed-only, age bounds, clock brackets the truth, convergence),
  time-of-index error bound, pause/resume mapping, reseed, reading interval vs. model.
- `app/dibit_airtime_tests.rs` (19): chunk planning (CallClose keeps the tail, gate open
  / close resets, retune discard across chunks, late cuts, same-TG re-grant, call_id-only
  CallOpen, encryption stickiness, exact dibit accounting), recorder gating by mode,
  clamping and ordering, hardware discontinuity brackets the true production index,
  pause / resume, legacy mode keeps the clock, and an end-to-end attribution run (writer
  model + tracker + recorder + planner over 17 s with same-freq resume, CallClose, retune,
  same-TG re-grant, pause / resume, and a retune where the new call_id is published
  before the retune lands): every fed dibit more than 25 ms from an action carries the
  context of its air time (worst mismatch 2.9 ms from an action), the closing call's tail
  is decoded, and no old-frequency dibit reaches the new call.
- `test_sim` model (`hardware/dibit_ring_sim.rs`) of DibitPacker + DmaStreamRingWrite
  register behaviour, shared by both.

Linux type-check: `cargo check --target armv7-unknown-linux-gnueabihf` stops in the
`aws-lc-sys` build script (no `arm-linux-gnueabihf-gcc` on the Windows host) before any
Rust is checked, so the Linux-only code was type-checked with
`cargo-zigbuild check --target armv7-unknown-linux-gnueabihf.2.31` (zig from
`.venv-hdl`): clean.

## Not verified without hardware

- Actual delivery age and grant latency on the board (the numbers above are from the
  host model of the DMA engine).
- That `*_dibit_next` / `last_buffer` behave as modelled under HP1 load (the phase check
  and resync counters will show it; any non-zero `phase_mismatches` or `resyncs` in
  steady state needs a look).
- Clock convergence with the real Gardner loop (symbol slips show up as `reseeds`).
- The settle window (48 dibits) against the real FIR flush after a retune.
- CPU cost of 25 polls/s per ring (expected negligible: 3 register reads, one ioctl and a
  ≤ 128 B copy per poll with data).
- The behaviour change of the stale-CallClose guard on a busy site (expected: followed
  calls no longer drop out when other grants are rejected).

## Bench result (2026-09-26)

**Setup.** Unit B replays 28 s of site capture 1777801424 (13–41 s) into unit A, looping
gap-free through 20 dB. Setup and trims are in
[055](055_autoppm_sign_fix.md#bench-replay-setup-used). B's TX is −55 dB, and A runs its
stored 470 Hz correction.

**Ground truth.** SDRTrunk decoded the same air live on 2026-05-03 (`.mbe` files and
`event_logs` decoded messages): 4 transmissions on two grants, 297 IMBE frames,
33 LDUs (17 + 16) and 4 HDUs per loop.

Each row is 6 loops (168 s):

| Build / mode | IMBE per loop | Of SDRTrunk | LDU | HDU | Traffic age p50 / p99 / max |
|---|---|---|---|---|---|
| deployed `2026-05-03-forensics-sd-redirect` | 171 | 57.6 % | 19.0 | 1.5 | whole 3.4 s sub-buffers |
| this build, `legacy` | 193 | 65.1 % | 21.5 | 1.7 | 1.94 s / 23.7 s / 25.7 s |
| this build, `poll` | 297 | 99.9 % | 33.0 | 4.0 | 116 ms / 195 ms / 1.16 s |
| this build, `airtime` | 297 | 99.9 % | 33.0 | 4.0 | 115 ms / 189 ms / 189 ms |

What the table shows:

- Low-latency delivery alone recovers every frame SDRTrunk got. This clip has no
  competing grants, so air-time attribution adds nothing here, and costs nothing either.
- In `airtime` mode: control ring age p99 187 ms, 0 resyncs, 0 phase mismatches,
  0 clock reseeds, clock uncertainty ≈ 5 ms, 83 cuts applied, 0 clamped, 1241 dibits
  discarded pre-settle.
- The same-binary `legacy` row beats the deployed build (65.1 % against 57.6 %). That is
  consistent with the CallClose gating fix, which is active in every mode.
- The control channel is at 97.7–97.8 % TSBK CRC-ok (≈ 38.8/s) in all rows, matching the
  offline SDRTrunk-faithful decode of the clip (1106 TSBKs in 28 s).

## Bench measurement (suggested)

1. `POST /api/dibit_delivery?mode=legacy&reset=1`, run the P25 replay, read
   `GET /api/dibit_delivery` (age p50 / p99 / max per ring) and the recordings / grant
   stats; repeat with `mode=poll` and `mode=airtime`.
2. Expect control and traffic age p99 ≈ 0.2 s (legacy: ≈ 3.4 s), `resyncs = 0`,
   `phase_mismatches = 0`, `cuts_clamped ≈ 0`, and in airtime mode non-zero
   `epoch_splits` with no cross-call audio on same-TG re-grants / same-freq reuse.

## Files

- New: `p25-httpd/src/hardware/dibit_ring.rs`, `dibit_ring_tests.rs`,
  `dibit_ring_sim.rs`; `p25-httpd/src/app/dibit_airtime.rs`, `dibit_airtime_tests.rs`.
- Changed: `app/dibit_readers.rs` (new reader loops), `hardware/fpga.rs` (ring snapshot /
  copy / legacy cursor, epoch hooks), `hardware/rxbuffer.rs` (`buffer_size`),
  `app/imbe_forwarder.rs` (`ImbeBatch`, segment context, epoch marking),
  `app/vocoder_task.rs`, `audio/mod.rs` (`AudioChunk::airtime`), `audio/recorder.rs`,
  `app/grant_follower.rs` (epoch marks, CallClose guard, boundary order, grant hold,
  lifecycle call_id publication), `httpd/api/chain.rs` + `system.rs` + `mod.rs`
  (`/api/dibit_delivery`), `httpd/api/traffic.rs`, `httpd/api/talkgroups.rs`, `main.rs`
  (CLI flags, wiring, BUILD_TAG), `doc/P25_API.md`.
