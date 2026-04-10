# 020 -- Phase 6C follow-up: IQ/dibit packer `overflow` must be a pulse, not a latched level

**Date:** 2026-04-10
**Phase:** Phase 6C follow-up (was on the deferred list in doc 014)
**Branch:** fishball-p25
**Status:** HDL + PS-side hotfix DONE; bitstream B rebake in progress

---

## The bug

[`p25-httpd/src/main.rs`](../../p25-httpd/src/main.rs) called
`pipeline.reset()` on every LSM IRQ because `IpCore::iq_overflow()`
returned `true` every time the PS polled it, even on a perfectly
healthy RF feed. That in turn destroyed the Rust LSM pipeline's
accumulated state (streaming FIR delay lines, /2 decimator phase,
Gardner TED history, Costas PLL accumulator, sync-detector running
buffer) on every ~128 ms sub-buffer -- so the pipeline never got to
accumulate enough samples to lock onto a signal.

This was visible in the dashboard as `overflow_resets` ticking up
monotonically at ~7.6 Hz (one per sub-buffer) regardless of input
signal strength, and was flagged in
[doc 014 follow-ups](014_phase6d_lsm_rust_port.md#still-deferred) as
"spurious 'overflow latched' on every sub-buffer despite correct
sample throughput. Not blocking -- sample math proves no actual loss."

## The root cause

[`maia-hdl/p25_hdl/iq_packer.py`](../../maia-hdl/p25_hdl/iq_packer.py)
(and [`dibit_packer.py`](../../maia-hdl/p25_hdl/dibit_packer.py), which
has the same pattern) used to do this on an overflow trigger:

```python
with m.If(holding_valid):
    m.d.sync += self.overflow.eq(1)
```

There was no other driver for `self.overflow`, so once it latched to
1 it stayed at 1 forever. The class docstring even claimed this was
correct: *"never self-clears in the gateware; the AXI-Lite Rsticky
field clears it on PS read."*

The problem is that claim is wrong about how
[`maia_hdl.register.Registers`](../../maia-hdl/maia_hdl/register.py)
implements `Rsticky`:

```python
# every cycle: accumulate any high input into the sticky field
m.d.sync += sticky.eq(sticky | self[field.name])
...
# on PS read: replace the sticky accumulator with the CURRENT input
with m.If(self.ren):
    m.d.sync += rfield.eq(self[field.name])
```

The read-clear path is `sticky := input`, **not** `sticky := 0`. So
if the source signal `iq_packer.overflow` is stuck high at the moment
of read, the sticky accumulator gets re-loaded with 1 on that cycle
and immediately accumulates high again on the next cycle. Reads never
actually clear the bit -- they only replace the accumulator with
whatever the input happens to be right now.

This is fine (and by design) for signals that naturally return to 0
after the trigger event; the Rsticky layer lets you catch transient
pulses that would otherwise slip between polls. But for a packer that
latches its own output to 1 forever, Rsticky + latched-level is
**broken**: you get exactly one "real" event followed by an infinite
stream of spurious reads-show-high.

## Why this didn't surface in dibit_packer

`dibit_packer.overflow` has the same latent bug but it was never
observed in practice. The dibit rate is 4800 sym/s = 150 words/s =
~1.28 KB/s, and the HP1 SmartConnect budget is ~1.7 GB/s -- the DMA
is always ready, so the trigger condition (`holding_valid` still set
when the next word lands) never fires even once. Zero triggers =
zero latched state = no bug visible.

`iq_packer.overflow` fires at least once on every boot (probably on
the first ring wrap-around, where the AW channel stalls for a cycle
while the write address rolls over). After that single legitimate
trigger, the latched level means every subsequent PS read sees 1.

## The fix

Add a default `m.d.sync += self.overflow.eq(0)` at the top of
`elaborate()` in both packers. Amaranth's last-assignment-wins
semantics means the conditional `self.overflow.eq(1)` inside the
trigger branch still takes effect -- it just only holds for one
cycle, and then the default takes over again. The Rsticky wrapper
then correctly accumulates those one-cycle pulses and clears on
read.

Diff (both files, same pattern):

```python
         # Handshake: clear valid when DMA accepts the word.
         with m.If(holding_valid & self.stream_ready):
             m.d.sync += holding_valid.eq(0)

+        # Default overflow to 0 every cycle so the only time it is
+        # high is the single cycle after a trigger event -- see the
+        # class docstring for why this MUST be a pulse and not a
+        # latched level.
+        m.d.sync += self.overflow.eq(0)
+
         with m.If(self.strobe_in):
             ...
             with m.If(holding_valid):
                 m.d.sync += self.overflow.eq(1)   # one-cycle pulse now
```

I also fixed `dibit_packer.py` at the same time even though its bug
has never manifested -- the fix is mechanically identical and keeping
the two packers consistent is cheaper than remembering which one has
which semantics.

## Test guard

Added `test_overflow_is_pulse_not_latched` to both
[`test_iq_packer.py`](../../maia-hdl/test/test_iq_packer.py) and
[`test_dibit_packer.py`](../../maia-hdl/test/test_dibit_packer.py).
This test fires enough strobes under back-pressure to guarantee at
least one overflow trigger, then samples `overflow` every cycle as
the trigger fires, then verifies:

1. `overflow` actually fired at least once (sanity -- a broken test
   harness that never triggers would silently pass the main assertion).
2. After the strobes stop, `overflow` is 0 within at most one tick.
3. After back-pressure is released and eight more ticks elapse,
   `overflow` has stayed at 0 for every one of those cycles.

Fail mode: if someone ever reintroduces the latched-level bug, the
counter of "cycles seen high" ticks up past a small threshold and
the test fails with an explicit reference back to this doc. This is
the best guard against re-introduction because the old test
(`test_overflow_sticky`) actively asserted the *wrong* behaviour --
"overflow stays set after stream_ready re-asserts" -- so a purely
regression-driven fix would silently break it.

The old `test_overflow_sticky` tests have been renamed + rewritten
rather than deleted so the test-name history still links back to
this investigation.

## PS-side companion fix (defence in depth)

Even with the HDL fix, the PS-side behaviour in
[`p25-httpd/src/main.rs`](../../p25-httpd/src/main.rs) was also
wrong: it was calling `pipeline.reset()` on any overflow, which is
the wrong reaction because "sample math proves no actual data loss"
(doc 014). The right reaction is to log + count the event and let
the pipeline keep running -- an actual data-loss event would need to
be detected at a higher layer (a gap in sample timestamps or a
backward jump in the FPGA's AW address counter), not inferred from
the overflow Rsticky bit.

Changed the Phase 6D LSM reader task to:

```rust
if overflow {
    tracing::warn!(
        target: "p25_lsm",
        "iq_dma overflow latched -- counting, NOT resetting pipeline (see doc 020)"
    );
    lsm_stats_task.lock().await.record_overflow();
    // NOTE: no pipeline.reset() -- see long comment in main.rs
}
```

The counter still ticks up so the dashboard's `overflow_resets`
field (the name is now historical but kept for API compatibility)
still shows the PS how often the bit fired. After the HDL hotfix +
Vivado rebake, this should stay at 0 on a healthy RF feed. If it
starts climbing on real hardware, that's a real back-pressure event
and the investigation is: why is HP1 stalling?

## Files changed

```text
maia-hdl/p25_hdl/iq_packer.py                  +17 / -6   (pulse default + docstring)
maia-hdl/p25_hdl/dibit_packer.py               +23 / -4   (pulse default + docstring)
maia-hdl/test/test_iq_packer.py                +73 / -21  (pulse test + rewritten overflow_sticky)
maia-hdl/test/test_dibit_packer.py             +70 / -14  (pulse test + rewritten overflow_flag)
p25-httpd/src/main.rs                          +18 / -6   (remove pipeline.reset on overflow)
maia-hdl/ip/p25-core/default/p25_core.v        REGEN      (second pass with the fix)
p25-httpd/p25-pac/p25.svd                      REGEN      (no content change -- same 27 regs)
p25-httpd/p25-pac/src/lib.rs                   REGEN      (no content change)
doc/changes/020_iq_dibit_packer_overflow_pulse.md  NEW    (this doc)
```

The `p25.svd` and PAC regen outputs are byte-for-byte identical to
the pre-fix versions because this is a gate-level fix inside the
packer's `elaborate()` -- it does not touch the register layout.
They only move in the commit because `build_hdl.bat --p25` always
re-runs svd2rust and we copy the outputs back regardless.

## Verification

### Unit tests

```text
$ python -m unittest test.test_iq_packer test.test_dibit_packer
Ran 13 tests in 0.115s
OK

$ python -m unittest test.test_iq_packer test.test_dibit_packer \
    test.test_c4fm_demod test.test_symbol_timing
Ran 25 tests in 0.352s
OK
```

Before the fix, `test_overflow_is_pulse_not_latched` did not exist
and the old `test_overflow_sticky` passed by actively checking for
the bug. After the fix, the new pulse test passes and nothing else
regresses.

### ARM cross-check

```text
$ cd p25-httpd && cargo check --target armv7-unknown-linux-gnueabihf
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.93s
```

27 pre-existing warnings, no errors. The `main.rs` change is a pure
deletion of the `pipeline.reset()` call + a longer comment.

### On-target (queued behind bitstream B bake)

With bitstream B loaded and Tezuka booted, on a healthy RF feed:

- `/api/lsm` dashboard field `overflow_resets` should stay at 0 for
  the first minute of uptime and climb only on actual HP1 stalls.
  (Before this fix, it climbed at ~7.6 Hz on every boot regardless.)
- Rust LSM pipeline's `hard_events` / `soft_events` counters should
  start accumulating within a few seconds of enable -- the pipeline
  now keeps its lock state across sub-buffer boundaries.
- `/api/lsm` `top_nacs` should start showing `0x8A1` entries within
  seconds on the Clay County control channel. (Before this fix, the
  constant reset kept wiping the top-nac histogram too.)
- HDL LSM path (6E.9/6E.10) has its own `lsm_status.nid_event`
  polling + `lsm_dibit_dma` ring and does not share state with the
  Rust Phase 6D path, so it's unaffected by this fix either way.
  It's a useful independent sanity check during bring-up because
  both paths should emit the same NID stream on the same RF feed.

## Related

- [doc 013](013_phase6c_iq_dma.md) -- Phase 6C post-DDC IQ ring DMA
  (where iq_packer was introduced)
- [doc 014](014_phase6d_lsm_rust_port.md) -- Phase 6D Rust LSM port
  (where the spurious-overflow bug was first observed and deferred)
- [doc 018](018_phase6e9_lsm_top_integration.md) -- Phase 6E.9 wiring
  `LsmDemod` into `p25_top.py` (instantiates a second `DibitPacker`
  for `lsm_dibit_dma`, which inherits this fix)
- [doc 019](019_phase6e10_vivado_bake.md) -- Phase 6E.10 Vivado bake
  (the first bitstream after this fix is bitstream B from that phase)
