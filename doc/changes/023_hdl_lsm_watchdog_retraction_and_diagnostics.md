# 023 -- HDL LSM watchdog retraction + crash-transition diagnostic instrumentation

**Date:** 2026-04-10
**Phase:** Phase 6E.6e diagnostic follow-up
**Branch:** fishball-p25
**Status:** PS-side only; commit ready, awaiting Tezuka rebuild + flash

---

## TL;DR

Two things in one commit, both `p25-httpd` only (no HDL or
bitstream changes needed):

1. **Retract the PS-side `sdr_reset` watchdog** from commit `0ef0d09`.
   It is fundamentally unsafe -- pulsing `sdr_reset` mid-operation
   causes a hard kernel panic reboot via AXI HP deadlock. It also
   has been silently broken since shipped: in the chain's degraded
   state the lsm_registers AXI CDC returns shifted/wrong data on
   reads, so the watchdog's read-modify-write `modify()` reads
   garbage and writes garbage back, never actually toggling the
   sdr_reset bit.

2. **Add a 32-deep NID event ring buffer** that captures the last
   32 NID events with full state (NAC, DUID, n_errors, sync_distance,
   drop_count, pll_dbg, sp_dbg, t_ms_since_boot) and dumps it
   unconditionally on the FIRST heartbeat where `nid_evts == 0`
   after a healthy run. This captures the exact NID events leading
   up to the transition from healthy to stalled, with no log
   throttling.

3. **Add `iq_dma` health tracking to the heartbeat**: per-second
   address advance rate (KB/s), buffer rollover count, and overflow
   tick count. The previous heartbeat only watched the LSM dibit
   ring; we need to know whether `iq_dma` is also degrading at
   the same time as the LSM path (which would localize the failure
   to a shared-AXI / shared-DDR backpressure issue rather than
   anything specific to the LSM datapath).

---

## How we got here

The 2026-04-10 CORDIC bake (commits `47c9cc5` + `cfd691f`)
successfully closed the slip resistance bug at the algorithm
level: the on-target test against Clay County NAC `0x8A1` showed
**150+ seconds of fully healthy decoding** at near-100 % NID
validity (vs ~18 seconds before sticking on the previous bake).
The CORDIC fix delivered.

After ~150 seconds the chain transitioned to a stalled state
where:

- `lsm_dibit_overflow` latched on every poll tick
- `iq_dma` throughput dropped from 65 k samples/s → 45 k samples/s
- The Phase 6D Rust LSM pipeline reported "STALLED"
- The HDL LSM heartbeat reported nonsense values:
  `pll[30881,30881] sp[0,0] best_sync_dist=90 nid_evts=0`
- `recoveries: 2` -- the watchdog believed it had fired, but the
  system did NOT reboot

Direct `devmem` reads of the lsm_registers bank in the stalled
state revealed the AXI register CDC was returning **shifted /
wrong data**:

```text
                   stalled state           healthy state (post-reboot)
lsm_control(0xa0)  0x00070000 ✗            0x00000003 ✓
lsm_nid    (0xa8)  0x360A2183 ✗            0x000078A1 ✓
lsm_drop_count     0x000000A8              0x00000000
lsm_dibit_next     0x000078A1 ✗            0x1A001E00 ✓
lsm_debug  (0xb4)  0x00070000 ✗            0x0932ECE0 ✓
```

Decoding `0x00070000`: bits 18, 17, 16 set. This is **exactly the
lsm_status pattern with `lsm_dibit_overflow` (bit 18) set +
`sync_distance` (bits [17:11]) = 96**. So `lsm_control` reads
were returning `lsm_status` data. Address-decode is broken in
the degraded state.

The "stuck" `pll = 30881` value the heartbeat was reporting was
**not real HDL state** -- it was the heartbeat reading
`lsm_debug` and getting back `lsm_nid`'s value (the latched
`(duid<<12) | nac = 0x78A1` = 30881) because of CDC corruption.
The actual `pll_reg` in HDL may have been at a perfectly normal
value; we just can't read it correctly through the broken CDC.

## Why `sdr_reset` is fundamentally unsafe

User direct devmem write to test:

```bash
devmem 0x7C460008 32 0x1     # set sdr_reset
```

→ **Board immediately rebooted.**

Mechanism:

1. The write sets the `sdr_reset` bit in the s_axi_lite_clk-domain
   control register.
2. The bit propagates through the FFSynchronizer (in
   `p25_top.elaborate`) into `ResetSignal('sync')`, the reset
   signal of the maia_sdr_clk core domain.
3. All `m.d.sync` flops in the core domain reset to their init
   values, INCLUDING the `iq_dma` / `lsm_dibit_dma` /
   `traffic_dma` AXI master state machines that may be mid-burst
   on AXI HP at that exact moment.
4. The AW phase of the in-flight burst is forgotten by the FPGA,
   but the PS DDR controller is still waiting for `WLAST=1` +
   `BVALID=1` to retire the transaction.
5. AXI HP slave hangs waiting for the handshake that never comes.
6. Kernel watchdog detects the AXI deadlock and panics.
7. Hard reboot.

This is not fixable in the watchdog design. `sdr_reset` is a
**boot-time-only** reset, not a runtime recovery mechanism. Any
future runtime recovery has to:

- Quiesce the AXI HP DMA masters first (let in-flight writes
  drain to completion)
- Then assert reset only on the LSM datapath (not the DMA master
  state machines)
- Then de-assert and re-arm the masters

That's substantially more complex than a single bit pulse and
requires HDL gateware changes (separate reset domains for the
demod chain vs the AXI master state). Out of scope for now.

## Why the watchdog was silently broken

Two compounding bugs:

**Bug A: read-modify-write in the broken-CDC state.** The
`reset_and_reinit()` function used PAC `modify()`:

```rust
self.registers.control().modify(|_, w| w.sdr_reset().set_bit());
```

`modify()` does a read first, modifies the in-memory value, then
writes back. In the chain's degraded state the lsm_registers
CDC returns shifted/wrong data; if the AXI register read for
`control` was also corrupted (we don't yet know the exact
boundary), the modify reads garbage and writes garbage back. The
sdr_reset bit may never have actually toggled.

**Bug B: even when it does toggle, it crashes.** The user's
direct devmem test confirmed that toggling sdr_reset on a stuck
system DOES crash the board. So even if `modify()` had worked
correctly, the watchdog firing would have rebooted the board on
every recovery.

Net effect: from the user's perspective the watchdog was firing
(`recoveries: 2` in the heartbeat) but doing nothing useful. The
counter was incrementing but the underlying hardware was
unchanged. From doc 023's claimed behaviour ("HDL LSM chain
should re-acquire within the next ~1 second"), this is the worst
possible failure mode -- silent no-op while pretending to work.

## What this commit changes

### `p25-httpd/src/main.rs`

Heartbeat task rewritten:

- Watchdog code DELETED (`STUCK_WINDOWS_THRESHOLD`,
  `last_recovery`, `recovery_count`, the `if stuck_windows >=
  ... { reset_and_reinit() }` block).
- New per-tick fields added to the snapshot read: `iq_overflow`,
  `iq_last_buffer`, `iq_next_addr`. Read coherently under the
  same mutex lock as the LSM status snapshot.
- New per-second iq health bookkeeping:
  - `hb_iq_addr_advance` -- bytes the iq_dma AW pointer advanced
    in the last second. Healthy = ~250 KB/s (62.5 kSPS * 4
    bytes/sample).
  - `hb_iq_buffer_changes` -- how many times last_buffer rolled
    over. Healthy = ~4-8/s (matching iq_dma sub-buffer count).
  - `hb_iq_overflow_ticks` -- count of poll ticks where iq
    overflow was latched. Healthy = 0.
- Per-tick iq_overflow warning, throttled to 1 Hz so a stuck
  overflow doesn't drown the log.
- The lsm_dibit_overflow per-tick warning is also now throttled
  to 1 Hz (was per-tick = 60 warnings/second when stuck).
- Heartbeat line format extended:
  ```text
  HB Nt: pll[lo,hi] sp[lo,hi] best_sync_dist=N
         bch_busy=N in_window=N nid_evts=N
         dibit_overflow=N iq_overflow=N
         iq_kbps=N iq_buf_rolls=N
         (window NIDs: V/E valid; cum NIDs: V/E)
  ```
  (`recoveries:` field removed.)

NID event ring buffer added:

- 32-deep ring of `NidRingEntry` structs, captured on EVERY
  `nid_event` strobe regardless of throttling.
- Each entry has full state: seq, t_ms_since_boot, NAC, DUID,
  valid, n_errors, sync_distance, drop_count, pll_dbg, sp_dbg.
- `crash_dump_armed = true` after the first NID event.
- On the FIRST heartbeat where `hb_window_event_count == 0`
  AFTER `crash_dump_armed`, dump the entire ring in
  chronological order (oldest -> newest) at WARN level. Fires
  exactly once per boot.

The dump format:

```text
WARN p25_hdl_lsm: CRASH TRANSITION: HDL LSM chain produced 0
NID events in the past 1s window after a healthy run --
dumping the last 32 NID events from the ring buffer:
WARN p25_hdl_lsm:   ring[ 0] t= 13874ms #    1 nac=0x8A1 duid=7 valid= true n_errors= 0 sync_dist= 0 drop_count=    0 pll= -4897 sp=  3934
WARN p25_hdl_lsm:   ring[ 1] t= 13955ms #    2 nac=0x8A1 duid=7 valid= true n_errors= 0 sync_dist= 0 drop_count=    0 pll= -5003 sp=  6870
... (up to 32 entries) ...
WARN p25_hdl_lsm:   ring[31] t=153420ms #  876 nac=0xE52 duid=4 valid=false n_errors=14 sync_dist= 4 drop_count=  168 pll= -8579 sp=  1035
WARN p25_hdl_lsm: CRASH TRANSITION: end of ring dump. Chain will
likely remain stuck until power cycle.
```

Compare entries N..N+5 in the tail of the ring -- if `n_errors`
or `sync_distance` is rising while `pll_dbg` is railing at
`-8579` or `+8579` (= ±MAX_PLL_ABS_Q13 = ±π/3), the chain is
slipping and the CORDIC needs further work. If `drop_count` is
spiking, the BCH is being flooded. If `sp_dbg` is going
negative or zeroing, the LsmTimingInterp pipeline is misbehaving.

### `p25-httpd/src/fpga.rs`

`reset_and_reinit()` function DELETED. Replaced with a long
comment block explaining why it was deleted, what it tried to
do, and what a future safer recovery mechanism would need to
look like.

This means the only way to recover from the stalled state is
now a power cycle. That is the honest characterization of
what's actually possible right now.

### `p25-httpd/src/main.rs` (already does the work)

No code change to `fpga.rs` consumers since the only caller of
`reset_and_reinit()` was the watchdog, which is now deleted.

## Verification

```text
cargo check                                        clean
cargo check --target armv7-unknown-linux-gnueabihf clean
```

(Both targets had pre-existing dead-code warnings unrelated to
this commit; no new warnings, no errors.)

No HDL changes, no Vivado bake. Tezuka rebuild + flash only.

## Expected on-target behaviour

After flashing this PS image with the same bitstream we already
have on the SD card:

1. **Boot**: same healthy 150 seconds we already saw.
2. **Heartbeats during the healthy phase**: same content as before,
   PLUS new fields `iq_overflow=0 iq_kbps=~244 iq_buf_rolls=4-8`.
   (Use these to verify the iq_dma is genuinely doing 250 KB/s
   in the healthy phase, which is the baseline.)
3. **Crash transition (around T=150s)**: heartbeat with
   `nid_evts=0` for the first time. Immediately followed by the
   CRASH TRANSITION ring dump showing the last 32 NID events.
   This is the data we need.
4. **Post-crash heartbeats**: keep emitting (with `iq_kbps`
   showing the degraded rate so we know if iq_dma is also
   degraded). NO recoveries field. NO sdr_reset pulse. The
   chain stays stuck until you power cycle.
5. **`p25_hdl_lsm: lsm_dibit_overflow latched`** warnings now at
   1 Hz instead of 60 Hz, so the log is readable.

## What we'll learn

From the ring dump alone:

- **Was there a clean transition or a gradual degradation?**
  If the last 5 NIDs are healthy and the 6th is missing, it's a
  sudden event (likely a noise burst or AXI deadlock). If there's
  a gradual rise in `n_errors` and `sync_distance` over the last
  10-20 NIDs, the CORDIC is slow-drifting (need more loop work).
- **Does `pll_dbg` rail at the clamp before the failure?**
  If yes, the CORDIC has a slow integrator drift the linearised
  form didn't have. If no, the failure isn't PLL related.
- **Does `drop_count` climb before the failure?**
  If yes, the sync detector is firing while BCH is busy -- means
  many false sync hits, suggesting dibit corruption upstream.
- **Does `sp_dbg` (sample_point) drift toward 0?**
  If yes, my LsmTimingInterp 2-stage pipeline (commit `cfd691f`)
  has a Gardner TED race condition or similar, and we need to
  fix the source.

From the iq_dma rate alone:

- **If iq_dma rate stays at 244 KB/s after the crash**: the
  failure is specific to the LSM datapath; iq_dma is fine.
- **If iq_dma rate drops in lockstep with the LSM crash**: the
  failure is upstream of both, in the maia_sdr_clk domain or in
  the AXI HP backpressure path. Different fix class.
- **If iq_overflow latches before the LSM crash**: the iq_dma
  ring is overflowing first, which causes downstream
  back-pressure, which kills the LSM chain. Different root
  cause again.

## What this does NOT do

- Does NOT fix the underlying ~150s crash. We don't know enough
  yet to fix it -- the diagnostics in this commit are the data
  we need to characterize the failure mode.
- Does NOT add a working recovery mechanism. Recovery requires
  HDL gateware changes (separate reset domains) and is out of
  scope for this commit.
- Does NOT touch the HDL or rebake the bitstream. The bitstream
  we have on the SD card is fine -- the algorithm works for
  150 seconds.

## Next steps

1. Tezuka rebuild + flash this PS image.
2. Boot and let the chain run until the crash transition.
3. Capture the ring dump from `/var/log/p25-httpd.log`.
4. Send the dump back; we'll analyze it and design the actual
   fix.

If the ring dump shows a slow drift (last 10 NIDs gradually
degrading), the fix is in the CORDIC loop -- maybe AGC, maybe
loop bandwidth tuning, maybe a longer integrator clamp.

If the ring dump shows a clean cliff (NIDs healthy until the
last one, then nothing), the fix is somewhere else -- AXI
backpressure, IRQ handler stall, DMA pointer race, etc.

Either way the diagnostic data tells us where to look.
