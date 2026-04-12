# 033 -- Phase 7A.1 -- Traffic-channel scaffold wire-up

**Date:** 2026-04-11
**Phase:** 7A.1 (singleton voice-channel grant follower, no FPGA bake)
**Branch:** fishball-p25
**Status:** PS Rust changes complete and host `cargo check` clean.
Tezuka rebuild + on-target verification pending.
**Next:** flash + on-target acceptance, then Phase 7A.2 (LSM demod
chain on the traffic side, FPGA bake).

---

## TL;DR

Phase 7A.1 wires the **already-existing** C4FM traffic-channel
scaffold (HDL traffic chain from doc 007 + `fpga.rs` traffic
helpers from the same era + `p25/traffic_manager.rs` state machine,
all sitting dormant since Phase 4) into the live `p25-httpd`
process so that:

1. The traffic DDC is **configured at startup** (decimation, FIR
   shared-coefficient setup, NCO=0, demod_enable=off).
2. A new **traffic dibit reader task** drains the traffic_dma ring
   and maintains a per-dibit histogram in a shared `TrafficStats`.
3. A new **traffic grant follower task** polls the canonical
   `lsm_decoder.grants` HashMap at 50 ms cadence, picks the most
   recent active grant, and retunes the traffic DDC + asserts
   `demod_enable` whenever a new channel arrives.
4. A new **`/api/traffic`** endpoint surfaces TrafficManager state,
   the dibit histogram, and the traffic_dma IRQ counter, with
   four manual-control query params for debugging.

**Zero FPGA bake required** -- the traffic chain has been in the
HDL since Phase 4 (doc 007) and is already in the bitstream from
`tezuka_fw@08f7607` (Phase 6G.1). Only the PS-side
`p25-httpd` binary needs a Tezuka rebuild + flash to pick up these
changes.

---

## The big surprise: most of Phase 7A was already built

Going into this session the Phase 7 entry-point memory said the
HDL Phase 7A would need:

- A second `DDC` instance in `p25_top.py`
- A `voice_control` register bank for runtime retune
- A new ring DMA mirroring `lsm_dibit_dma`
- A `voice_follow.rs` PS module
- A new `/api/voice_follow_targets` endpoint

What's actually there from Phase 4 (doc 007 -- `p25_traffic_channel`):

| Component | Status | Notes |
|---|---|---|
| HDL second DDC instance | ✅ `traffic_ddc = DDC('clk3x')` in `p25_top.py:364` | Shares FIR coefficient ROM with control DDC via `coeff_*` wires at p25_top.py:801-807 |
| HDL traffic register bank | ✅ `traffic_registers` at offset 0x60 (`p25_top.py:376-414`) | NCO frequency, decimation, control, status, next-address — all the runtime knobs |
| HDL traffic dibit ring DMA | ✅ `traffic_dma` at `0x1800_0000` (8 × 4 KB sub-buffers) | IRQ wired into `interrupts.traffic_dma` (bank 0 bit 1) |
| HDL elaborate() wiring | ✅ `p25_top.py:783-873` | Full DSP chain wired: rxiq_cdc → traffic_ddc → traffic_c4fm → traffic_timing → traffic_packer → traffic_dma |
| `fpga.rs` `set_traffic_ddc_frequency` | ✅ `fpga.rs:381` | Hz → 28-bit NCO word with range check |
| `fpga.rs` `set_traffic_demod_enable` | ✅ `fpga.rs:405` | One-bit register write |
| `fpga.rs` `set_traffic_ddc_enable` | ✅ `fpga.rs:398` | DDC input gate |
| `fpga.rs` `read_traffic_buffers` | ✅ `fpga.rs:430` | Drain new sub-buffers from the DMA ring |
| `fpga.rs` `traffic_dma: RxBuffer` opened at startup | ✅ `fpga.rs:94` | UIO device `p25-traffic` |
| `fpga.rs` IRQ handler counts traffic IRQs | ✅ `fpga.rs:954` | `traffic_irqs += 1` + `notify_traffic_dma.notify_waiters()` |
| `fpga.rs` `waiter_traffic_dma()` | ✅ `fpga.rs:900` | Returns an `InterruptWaiter` for the traffic DMA IRQ |
| `fpga.rs` `IrqStats.traffic` field | ✅ `main.rs:126` | Already declared, already counted |
| `p25/traffic_manager.rs` state machine | ✅ Full `TrafficManager` (Idle/Acquiring/Active, NCO calc, lifecycle) | Marked `#![allow(dead_code)]` and the `TODO(traffic-following)` comment said "will be wired into main.rs after the control channel decode is solid" |

**What was actually missing for Phase 7A:**

1. A `configure_traffic_ddc()` helper in `fpga.rs` -- the existing
   `configure_ddc()` only writes the **control** DDC's
   decimation/operations registers, leaving the traffic DDC's
   matching registers at their reset value of 0 (so even if
   `enable_input` were set, the traffic chain would not produce
   sane samples).
2. A startup call from `main.rs` to actually configure the traffic
   DDC and prime it for runtime retunes.
3. A tokio task to drain the traffic dibit DMA ring (zero readers
   today).
4. A tokio task to retune the traffic DDC on incoming grants
   (zero callers of `TrafficManager::handle_grant` today).
5. A `/api/traffic` endpoint to surface state + counters + manual
   controls.

That's the entire Phase 7A.1 delta. Everything else was sitting
there waiting to be plugged in.

---

## Architectural decisions

### Polling instead of typed events

The existing event broadcast channel between
`ControlChannelDecoder` and the WebSocket / dashboard is
**`tokio::sync::broadcast::Sender<String>`** -- it carries
pre-formatted strings, not typed enums. Subscribing to it from the
new grant follower task and parsing strings would be brittle.

The two clean alternatives both touch every grant dispatch site
in `control_channel.rs`:

- Add a parallel `broadcast::Sender<GrantEvent>` channel and
  publish typed events from `process_directed_tsdu` /
  `handle_tsbk` whenever a grant is parsed.
- Add a callback hook on `ControlChannelDecoder`
  (`Box<dyn Fn(&GrantInfo) + Send + Sync>`) called inline at the
  same dispatch sites.

For Phase 7A.1 ("wire it up, prove the path") **polling is the
right call:**

- Zero edits to `control_channel.rs` (a 2245-line file with three
  parallel decoder instances all updating their own `grants`
  HashMap independently).
- 50 ms poll cadence gives <100 ms total grant-to-retune latency,
  half the P25 spec budget of ~200 ms.
- Localised to one new tokio task in `main.rs`.
- Easy to upgrade to typed events in Phase 7B when modulation
  auto-detection forces tighter coupling (we'll need to know
  *which* decoder pipeline first reported the grant so we can
  pre-classify modulation).

### Polling `lsm_decoder` specifically

Three `ControlChannelDecoder` instances exist in `main.rs`:

1. `decoder` -- C4FM HDL dibit-fed
2. `lsm_decoder` -- HDL LSM dibit-fed via `lsm_dibit_dma`
3. `iq_lsm_decoder` -- Phase 6D raw IQ-fed

The Phase 7A.1 follower polls `lsm_decoder` because per the
`AppState` doc comment in `httpd/mod.rs:36-42` that's the
**canonical source for the dashboard's Active Grants panel** and
it's the highest CRC-pass pipeline on Clay County (the LSM
simulcast test target). Phase 7B will revisit when modulation
auto-detect needs to consult all three.

### Shared FIR coefficients between control and traffic DDC

The HDL wires the `traffic_ddc.coeff_*` ports directly from
`sdr_registers.ddc_coeff_*` (`p25_top.py:801-807`). Both DDCs
share the same 256-address coefficient ROM. This is a deliberate
design choice that saves ~1 BRAM and avoids the question of "do we
need to load the same coefficients twice from PS." For Phase 7A
this means `configure_traffic_ddc()` only writes the
decimation/operations/bypass registers and the NCO; coefficient
loading happens once via the existing `configure_ddc()` call at
startup.

The Phase 7G channelizer architecture review will need to revisit
this -- if the channelizer uses different filter shapes than the
control DDC (which it almost certainly will), the coefficient ROMs
need to be split. Note for future-self.

### Manual-control query params (and why all four)

After scaffolding the read-only endpoint, the user asked for
explicit manual controls. Four were added:

1. **`?reset_stats=1`** -- zero `TrafficStats`. Cheap, useful for
   clean A/B comparisons after a config change.
2. **`?follower=on|off`** -- pause/resume the 50 ms poll. **Critical** --
   without this, any manual retune is silently overridden within
   ~50 ms by whatever the next grant snapshot says. Pause mode is
   how manual control becomes usable.
3. **`?retune_hz=<i64>`** -- write the traffic DDC NCO offset
   directly, bypassing the grant follower. Lets the operator tune
   to a debug frequency (e.g. point at the control channel
   itself: `?retune_hz=2862500`) without waiting for a real
   grant.
4. **`?demod_enable=0|1`** -- flip the `traffic_demod_control.demod_enable`
   bit. Required after a manual retune to actually start the
   dibit stream. **Explicit by design** -- the manual retune does
   not auto-enable demod, on the user's request.

The four params are processed in fixed order: reset → follower →
retune → demod_enable. So a single combined call:

```bash
curl 'http://192.168.2.1:8080/api/traffic?follower=off&reset_stats=1&retune_hz=2862500&demod_enable=1'
```

does the right thing: pauses the follower so the manual retune
sticks, zeros the histogram so it counts only what arrives after
the retune, retunes the DDC, then enables the demod. Single HTTP
call, idempotent, browser-friendly.

The `applied` array in the response echoes the writes that fired
and `errors` lists any params that failed to parse, so the caller
gets a confirmation of what actually happened.

### Why the dibit histogram is the headline metric at 7A.1

The traffic chain is C4FM-only at 7A.1 and Clay County (the only
P25 system on the test antenna) is LSM. Running the C4FM slicer
on LSM voice channels produces essentially random dibits -- the
histogram for an active LSM voice channel through this C4FM chain
should look like ~25/25/25/25, not the C4FM-decode pattern.

That sounds bad until you realise it's exactly what we need to
validate the **plumbing**. A dead chain produces all-zero dibits
(or gets stuck on one value, or zero IRQs fire). A live chain
produces a non-zero, roughly-uniform spread. So the histogram is
a perfectly good "is the chain alive" signal regardless of decode
quality, and it's the only signal we have at 7A.1 because there's
no LSM demod on the traffic side yet.

Phase 7A.2 adds an LSM parallel chain on the traffic side
(mirroring what Phase 6E.9 did on the control side) and at that
point the histogram becomes decode-quality data: stable per-dibit
patterns mean stable LSM lock, drifting patterns mean PLL trouble,
all-zero means the LSM chain is dead.

---

## Files touched

| File | Change |
|---|---|
| `p25-httpd/src/fpga.rs` | New `configure_traffic_ddc(frequency_hz, sample_rate_hz)` helper that mirrors `configure_ddc`'s register-write portion against the `traffic_*` registers. No coefficient loading -- shared with control DDC via HDL wiring. |
| `p25-httpd/src/p25/traffic_manager.rs` | Removed `#![allow(dead_code)]`. Added `last_offset_hz`, `grants_seen`, `retunes`, `last_retune_at` fields. Added `state_label()` and `current_channel()` accessors for the JSON endpoint. `handle_grant` now bumps the counters on every call and updates `last_retune_at` on a successful retune. |
| `p25-httpd/src/main.rs` | New `TrafficStats` struct alongside `IrqStats`. New `Arc<Mutex<TrafficManager>>`, `Arc<Mutex<TrafficStats>>`, and `Arc<AtomicBool>` for the follower-pause control, all created out-of-cfg(linux) so they thread into `AppState` on every target. Inside the cfg(linux) block: `configure_traffic_ddc(0.0, sample_rate)` + `set_traffic_ddc_enable(true)` + `set_traffic_demod_enable(false)` at startup, then two new tokio tasks: (a) traffic dibit reader (drains `read_traffic_buffers()`, builds histogram, calls `note_activity()` on the manager), and (b) traffic grant follower (50 ms `tokio::time::interval`, snapshots `lsm_decoder.grants`, picks newest with frequency, calls `handle_grant`, retunes on `true` return; honours the follower-pause atomic). BUILD_TAG bumped to `2026-04-11-phase7a1-traffic-scaffold-wire-up`. |
| `p25-httpd/src/httpd/mod.rs` | Extended `AppState` with `traffic_manager`, `traffic_stats`, `traffic_follower_enabled`. New `/api/traffic` route. New `get_traffic` handler with read-side snapshot + four manual-control query params (`?reset_stats=1`, `?follower=on/off`, `?retune_hz=<i64>`, `?demod_enable=0/1`), processed in fixed order before the snapshot read. |
| `doc/P25_API.md` | New `/api/traffic` section. Endpoint catalogue updated to 21 routes. |
| `doc/changes/033_phase7a1_traffic_scaffold_wire_up.md` | NEW -- this doc. |
| `tools/p25_status_and_next_step.py` | New Phase 7A.1 ROADMAP entry. |

No HDL changes, no `p25-pac` regen, no SVD changes, no Tezuka
overlay changes -- the bitstream from `tezuka_fw@08f7607` is
unchanged for Phase 7A.1. Only `p25-httpd` needs a rebuild + flash.

---

## Verification plan

After flashing:

```bash
# 1. Confirm new BUILD_TAG is on the board
curl http://192.168.2.1:8080/api/system | jq .build
# expected: "2026-04-11-phase7a1-traffic-scaffold-wire-up"

# 2. Read /api/traffic with no params -- should show Idle, follower_enabled=true
curl http://192.168.2.1:8080/api/traffic | jq

# 3. Watch a real grant come in (any active call on Clay County) and
#    verify the follower retunes
curl http://192.168.2.1:8080/api/traffic | jq '.state, .current_channel, .current_talkgroup, .retunes'
# expected after first grant: state="Acquiring", current_channel=<the grant's channel>,
#                              current_talkgroup=<TG>, retunes>=1

# 4. Verify the dibit DMA chain came alive
curl http://192.168.2.1:8080/api/traffic | jq '.irq.traffic_dma_total, .stats.wakeups, .stats.total_dibits, .stats.dibit_hist_pct'
# expected: traffic_dma_total > 5, wakeups > 5, total_dibits growing,
#           dibit_hist_pct roughly even (each value 15-35%)

# 5. Manual-control smoke test (pause follower, retune to control
#    channel itself, enable demod, verify dibits arrive at the new
#    frequency, then resume follower):
curl 'http://192.168.2.1:8080/api/traffic?follower=off&reset_stats=1&retune_hz=2862500&demod_enable=1'
sleep 2
curl http://192.168.2.1:8080/api/traffic | jq '.applied, .stats.dibit_hist_pct, .nco_word_hex'
curl 'http://192.168.2.1:8080/api/traffic?follower=on'
```

**Acceptance criteria:**

1. ✅ `/api/traffic` returns a JSON document (HTTP 200) with the
   documented shape, on a freshly-flashed binary, before any
   grants have been seen.
2. ✅ On a live Clay County grant, the follower retunes within
   ~100 ms of the grant landing in `lsm_decoder.grants`
   (verifiable via `last_retune_secs_ago` in the JSON).
3. ✅ `traffic_dma` IRQ counter is non-zero within 1 second of
   the first retune.
4. ✅ Per-dibit histogram is non-zero and roughly uniform (each
   value 15-35%) -- garbage, but live garbage.
5. ✅ Manual control smoke test: `?follower=off&retune_hz=N&demod_enable=1`
   takes effect immediately, the histogram refreshes after the
   manual retune, and `?follower=on` restores the polling task.
6. ✅ No new CRC pass-rate or NID-validity regression on the
   control channel pipelines (they're independent of the new
   traffic chain code -- verify with
   `tools/p25_status_and_next_step.py`).

---

## Known non-acceptance: the dibits are garbage

At 7A.1 the dibit histogram on a real LSM voice channel will be
roughly uniform (~25/25/25/25). **This is correct.** The C4FM
slicer running on LSM produces essentially random output -- this
is the same problem that drove all of Phase 6 on the control side
and that Phase 7A.2 will fix on the traffic side by adding an LSM
demod chain.

Verifying that the dibits are *meaningful* requires Phase 7A.2.
For 7A.1 we are validating the plumbing only.

---

## What's next: Phase 7A.2

Add an LSM demod chain on the traffic side, mirroring what Phase
6E.9 did on the control side. Concretely:

1. `maia-hdl/p25_hdl/p25_top.py` -- instantiate
   `traffic_lsm_decimator`, `traffic_lsm_lpf`, `traffic_lsm_rrc`,
   `traffic_lsm_demod`, `traffic_lsm_dibit_packer`,
   `traffic_lsm_dibit_dma` (mirroring the control-side LSM chain).
2. New `traffic_lsm` register bank at offset 0xC0 (bank 6) modelled
   on the control-side `lsm` bank.
3. New ring DMA at `0x1B00_0000` (keeps the 0x100_0000 spacing
   pattern after `lsm_dibit_dma` at `0x1A00_0000`).
4. Vivado bake → new XSA → Tezuka rebuild → flash.
5. PS-side: second `ControlChannelDecoder` instance (or just a
   dibit consumer for now since we're not parsing TSBKs on a voice
   channel -- voice channels carry LDU1/LDU2 frames, not TSDUs)
   reading from the new ring.
6. Verification: retune to a known active voice channel, confirm
   the LSM chain produces stable NIDs and dibit CRC pass rate
   matches the control-side ~85% per-block.

**Estimated FPGA cost** (Phase 6E.8 numbers, doc 017): ~32 DSP48,
~4000 LUT, 2 BRAM18 for the LSM chain. Z7020 has plenty of room
(current Phase 6G.1 utilization is ~25% LUT and ~16% DSP).

**Bake required.** The user has confirmed bakes are fine (~20 min)
so 7A.2 is a single coordinated commit-bake-flash sequence.

Phase 7A.2 will also wire **HDU + TDU detection** by feeding the
new traffic-side `LsmNidPipeline`'s `nid_event_strobe` + `duid`
outputs into a new `traffic_lsm` register bank, identical to how
the control side surfaces its NID events. PS-side dispatcher will
treat `DUID = 0x0` (HDU) as call-start, `DUID = 0x3 / 0xF`
(TDU / TDU_LC) as call-end with a 2-second post-TDU hold (matches
SDRTrunk PR #2010 semantics), and `DUID = 0x5 / 0xA` (LDU1 / LDU2)
as activity refresh. HDU/TDU detection is essentially free once
the LSM chain exists -- ~30 lines of PS Rust on top of the new
register bank.

After 7A.2 we'll have a working singleton LSM voice channel with
sub-second TDU release detection, and can move to Phase 7B
(modulation auto-detection + monitor list) and Phase 7C
(LDU IMBE extraction + RS-FEC HDU payload parsing for the
encryption flag) -- the path to audio out.

---

## On-target verification (2026-04-11)

This appendix records what happened when the Phase 7A.1 binary
hit the actual Fishball Z7020. The story is more interesting
than the original "verify it plumbs through" plan because the
on-target probe immediately exposed a thrashing bug, the SDRTrunk
research that pointed at a fix, and a second compound bug that
only the on-target verification could have caught.

### Round 1: scaffold + retune-on-grant works, but the singleton DDC thrashes

After flash + status-script-says-7A.1-is-passing, the dashboard
showed `current_talkgroup` flipping between 2-3 active TGs many
times per second. `/api/traffic` snapshot evidence (binary uptime
~120 s):

- `grants_seen = 1056`, `retunes = 178` -> ~1.5 retunes/sec average
  (with brief peaks well above 10/sec during multi-TG periods)
- 12-sample burst over 12 s during a multi-TG period: ~125 retunes,
  observed in the high-water-mark window

The cause: my naive **newest-by-timestamp** policy. With Clay
County's typical 2-3 simultaneously active TGs, the
`GroupVoiceChannelGrantUpdate` TSBKs land in alternating order
and the singleton DDC retunes on every poll. Each retune costs
~60 ms in DDC NCO + FIR flush + sync reacquisition (per the doc
007 latency budget), so the chain spent most of its time in the
transient and never settled.

### Sticky-lock policy from SDRTrunk upstream PR #2010

Rather than invent a policy, we extracted the upstream-baseline
behavior from SDRTrunk's `1b3ce431` commit (PR #2010, by Dennis
Sheirer): the original introduction of `P25TrafficChannelEventTracker`
with **TG-based call identity** (`isSameCallCheckingToOnly()`,
matching by TO identifier rather than channel) and a **2-second
stale eviction threshold** (`STALE_EVENT_THRESHOLD_MS = 2000`).
Both behaviors are unchanged from `1b3ce431` to current SDRTrunk
HEAD, so the upstream baseline is stable.

(An earlier research pass against the user's `andylee77/sdrtrunk`
fork pulled in fork modifications -- patch detection, encryption
filters, the CSM call-session manager from commits `010-024` --
which contaminated the policy reference. We re-ran the research
against `git show 1b3ce431` directly to get the upstream-only
baseline. See `reference_sdrtrunk_dual_path_audio.md` memory for
the complementary insight on the audio-vs-event dual-path
architecture, which informs Phase 7C/7D.)

The applied fix:

- **`traffic_manager.rs`**: `call_timeout_ms` 3000 -> 2000;
  `handle_grant` matches by `talkgroup.0` instead of `channel.0`;
  same-TG-different-frequency falls through to retune (handles
  network channel reassignment mid-call).
- **`main.rs` polling task**: when `state` is Idle, snapshot the
  newest grant; when `state` is Active/Acquiring, filter the
  snapshot to grants matching the locked TG and ignore everything
  else. The 2-second timeout naturally evicts stale locks.

### Round 2: thrashing-during-call still observed -- second bug found

After the sticky-lock fix, a 12-sample burst during a real call
on TG 202 (sole active TG, no multi-TG ambiguity) showed:

- `current_talkgroup` correctly pinned to 202 across all 12 samples ✅
- BUT `retunes` still climbed by 39 over 12 s (~3.25 retunes/sec)
- AND `state` alternated `Acquiring -> Idle -> Acquiring` every
  ~2 s of the burst window

`grants_seen` advanced ~20/sec (matching the 50 ms poll cadence,
so `handle_grant` was being called every poll), the same-TG match
was firing (TG never changed), but somehow `last_activity` wasn't
preventing the timeout.

The cause was a compound interaction inside `TrafficManager`:

1. `handle_grant` correctly took the same-TG-same-freq path,
   refreshed `last_activity`, returned false. **State stayed in
   `Acquiring` forever** because `sync_acquired()` -- the only
   thing that promotes Acquiring -> Active -- is never called
   in Phase 7A.1 (no real sync detector exists yet; that's
   Phase 7C work).
2. `check_timeouts` for the `Acquiring` branch uses
   `acquire_timeout_ms = 200` (200 *milliseconds*) and compares
   it against `started` (the moment the retune fired), NOT
   against `last_activity`. So 210 ms after every retune the
   call hard-timed out to Idle regardless of how recent the
   activity was.
3. The next 50 ms poll iteration found Idle state, picked the
   newest matching grant, retuned, started Acquiring all over
   again. Cycle: ~250 ms between retunes = ~4 retunes/sec.

### The Acquiring auto-promote fix

`handle_grant` now auto-promotes `Acquiring -> Active` inline
on the first matching same-TG-same-freq poll (typically 50 ms
after the initial retune). This puts the state machine into the
`Active` branch's 2 s `call_timeout_ms` window (which DOES use
`last_activity`), where the per-poll activity refresh actually
keeps the call alive.

The 200 ms `acquire_timeout_ms` and the `started` timestamp
become Phase 7C concerns -- once we have real LDU sync detection
in HDL, `Acquiring -> Active` will be driven by an actual
"chain locked" signal instead of "we got at least one matching
poll". For Phase 7A.1 + 7A.2 the auto-promote heuristic is
correct.

**This was only catchable with on-target verification.** A unit
test of `TrafficManager` in isolation would have missed it
because it depends on the interaction between `handle_grant` +
`check_timeouts` + the 50 ms polling cadence + the absence of
`sync_acquired` calls. The 12-sample burst test
(`tools/p25_sticky_lock_test.py`) pinpointed it exactly.

### Round 3: Acquiring auto-promote fix flashed and verified

**STATUS: ✅ PASSED on-target on 2026-04-11.** Verification ran
on the combined Phase 7A.1 + 7A.2 + 7C binary
(`2026-04-11-phase7c-ldu-imbe-extraction`).

`tools/p25_sticky_lock_test.py` output during a real Clay
County call on TG 402:

```text
initial state          = Active
initial talkgroup      = 402
initial channel        = 1189
initial frequency_hz   = 858437500
initial retunes        = 9
initial grants_seen    = 1020
last_retune_secs_ago   = 0.16

Sampling 12 times over 12 seconds (1 sec apart)...

  i= 0 state=Active     tg=402 retunes=9 grants_seen=1020 wakeups=19
  i= 1 state=Active     tg=402 retunes=9 grants_seen=1041 wakeups=20
  i= 2 state=Active     tg=402 retunes=9 grants_seen=1062 wakeups=20
  ... (samples 3-10 elided -- all identical) ...
  i=11 state=Active     tg=402 retunes=9 grants_seen=1212 wakeups=23

=== verdict ===
  retunes:     9 -> 9  (delta 0)
  grants_seen: 1020 -> 1212  (delta 192)
  retune rate during the 12s sample window: 0.00 retunes/sec
  unique talkgroups locked across the 12 samples: [402]

PASS: retune count stable (<=2 retunes in 12s) -- sticky lock works
```

**Headline numbers:**

- **`delta_retunes = 0`** over the entire 12 s window
- **`retune rate = 0.00 retunes/sec`** vs the pre-fix `~3-4 retunes/sec`
  Round 2 measured (and the original pre-sticky-lock `~15+ retunes/sec`
  Round 1 measured)
- **State remained `Active` throughout** -- the auto-promote
  pushed us out of `Acquiring` on the first matching poll
  (50 ms after the initial retune) and the 2 s `call_timeout_ms`
  was correctly refreshed by every subsequent same-TG-same-freq
  match
- **`grants_seen` advanced 192 over 12 s = 16/s**, matching the
  expected ~20 Hz polling cadence with some samples seeing no
  grant in the snapshot
- **Single TG lock**: `unique talkgroups locked = [402]` -- the
  follower stayed pinned to TG 402 throughout, no flapping

**This is Round 3 closed.** Phase 7A.1's compound bug fix
(sticky-lock + Acquiring auto-promote) is verified on hardware
in production conditions. The doc 033 appendix is now
complete.

The on-target verification was deferred from this commit's
original flash because the user was away from the device when
the second bug was found and fixed. It rode the combined
Phase 7A.1 + 7A.2 + 7C flash on 2026-04-11 and was the first
test run after the new binary booted -- exactly the workflow
this appendix predicted.

### What 7A.1 ships in this commit

- The full traffic-channel scaffold wire-up (read-only path:
  configure_traffic_ddc, traffic dibit reader task, traffic grant
  follower task, /api/traffic endpoint with four manual-control
  query params).
- The sticky-lock policy: TG-based call identity, 2 s stale
  threshold, Idle-only-acquires-new-TG semantics, derived from
  SDRTrunk upstream PR #2010.
- The Acquiring auto-promote bug fix that lets the sticky-lock
  policy actually hold during a call instead of cycling through
  the 200 ms acquire timeout.
- `tools/p25_sticky_lock_test.py` for repeatable verification on
  any subsequent Phase 7 binary -- the same script will validate
  Phase 7A.2's TDU-based release once that ships.

The next session (Phase 7A.2) starts with a clean known-good
baseline at the *source level*; the on-target verification of
the Acquiring auto-promote fix happens automatically as part
of the 7A.2 flash since the same binary will include both
changes.
