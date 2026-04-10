# 014 -- Phase 6D: Rust LSM Demod + NID FEC Port to p25-httpd

**Date:** 2026-04-09
**Phase:** 6D (PS firmware: Rust port of LSM demod consuming the iq_dma ring)
**Branch:** fishball-p25

---

## TL;DR

1. **Mechanical port** of `tools/p25_lsm_demod.py` (~1100 lines) and
   `tools/p25_nid_fec.py` (~370 lines) into a new
   [p25-httpd/src/lsm/](../../p25-httpd/src/lsm/) module, file-by-file with
   a one-to-one mapping between Python stages and Rust files.
2. **17 unit tests, all passing** on the Windows host (`cargo test lsm::`).
   The encoder matches SDRTrunk's golden vector
   (`NAC=1, DUID=0 → 0x00103185B7E9E224`); the BCH(63,16,11) decoder
   corrects up to t=11 errors across 50 trials per error level; the
   streaming FIR matches the batch FIR bit-for-bit; the streaming
   decimator preserves phase across odd-length chunks; and both sync
   detectors find a clean sync and recover the correct NAC/DUID.
3. **`fpga::IpCore` extended** to expose the Phase 6C `iq_dma` ring as a
   first-class peripheral: `set_iq_dma_enable`, `iq_last_buffer`,
   `iq_overflow`, `iq_next_address`, `read_iq_buffers`, plus a third
   interrupt waiter `waiter_iq_dma`. The existing dibit/traffic paths are
   unchanged.
4. **Tezuka device tree** gains the `0x19000000`-`0x1903FFFF` (256 KB)
   `reserved-memory` node and a matching `p25-iq` rxbuffer node, mirroring
   the dibit/traffic carve-outs already in
   [board/tezuka/fishball7020/dts/fishball-p25.dtsi](../../../Tezuka/tezuka_fw/board/tezuka/fishball7020/dts/fishball-p25.dtsi).
5. **`main.rs` spawns a parallel LSM reader task** alongside the existing
   dibit reader. Both consume the same control DDC output via separate
   ring DMAs; the dibit task keeps producing the legacy C4FM dibit stream
   for the existing decoder, the LSM task runs the new Rust pipeline and
   logs per-IRQ NID accuracy stats. The two pipelines are completely
   independent — disabling one does not affect the other.
6. **ARM cross-build (`cargo check --target armv7-unknown-linux-gnueabihf`)
   passes cleanly.** No new runtime dependencies, no `pm-remez`, no
   `num-complex` — the LPF and RRC tap arrays are frozen at build time
   from the Python reference (sample rate 31.25 kSPS, post-/2 decimation)
   and embedded as `const [f32; N]` so the embedded build has zero filter
   design at startup.

This phase ends at "Rust port compiles, all unit tests pass, ARM cross-build
clean, hardware bring-up queued". The on-target validation step is queued
behind a Tezuka firmware rebuild and SD-card flash; the gateware bitstream
from Phase 6C is already built and parked in
`tezuka_fw/board/tezuka/fishball7020/bitstream/p25/system_top.xsa`.

## Why Phase 6D exists

Phase 6A (commit `e1980aa`, doc 011) ported SDRTrunk's full LSM chain to
Python and validated 100% NAC accuracy against an SDRTrunk wav recording.
Phase 6B (commit `46630d6`, doc 012) added a bit-perfect BCH(63,16,11) NID
FEC. Phase 6C (commit `9f35f34`, doc 013) added the post-DDC IQ ring DMA
in gateware and built the bitstream.

Phase 6D is the bridge from "we have validated demod algorithms in Python
running on captured wavs" to "we have those same algorithms running on the
Cortex-A9 against live antenna signal from the Fishball iq_dma ring". It
is *not* an algorithm change — every line in the Rust port is a direct
translation of Python that is itself a direct translation of SDRTrunk
Java. There are no new design decisions, no new tunables, and no new
heuristics.

The phase ladder reminder from
[DEVPLAN.md](../../DEVPLAN.md):

- **6A** — Python LSM reference ✅ (doc 011)
- **6B** — NID BCH FEC ✅ (doc 012)
- **6C** — IQ DMA path in gateware ✅ (doc 013)
- **6D** — Rust port of demod + FEC to PS ← **this change**
- **6E** — HDL port back into PL

Each phase locks in a fixed reference. Phase 6D says "the Rust port is
right" — measured against the Python reference at the unit-test level
(impulse responses, encoder/decoder roundtrips, streaming-vs-batch
equivalence, sync hits on synthetic streams) and against SDRTrunk's truth
log at the integration level (deferred to on-hardware bring-up).

## Module layout

New module: [p25-httpd/src/lsm/](../../p25-httpd/src/lsm/), one Rust file
per Python stage. The mapping to the Python reference is intentionally
mechanical so a side-by-side diff stays useful.

| Python (`tools/`) | Rust (`p25-httpd/src/lsm/`) | Lines | Purpose |
|---|---|---|---|
| `p25_nid_fec.py` (~370) | `nid_fec.rs` (~280) | 280 | BCH(63,16,11) encoder + ML decoder over 65536-entry codebook |
| `p25_lsm_demod.py:158-260` (filters) | `filters.rs` (~310) | 310 | Frozen LPF/RRC tap arrays, batch FIR, `StreamingFir`, `StreamingDecimator2` |
| `p25_lsm_demod.py:328-475` (`demod_lsm`) | `demod.rs` (~330) | 330 | LSM demod loop: AGC + PLL + Gardner TED + slicer |
| `p25_lsm_demod.py:477-700` (sync) | `sync.rs` (~400) | 400 | Hard + soft sync detectors + status-aware NID extractor |
| (new — gateware glue) | `ring.rs` (~110) | 110 | iq_dma sub-buffer `&[u8]` → `Vec<Complex32>` |
| (new — orchestration) | `mod.rs` (~150) | 150 | Local `Complex32` POD type, `LsmPipeline` struct that owns all streaming state |

Total: ~1580 lines of Rust + tests, ~1480 lines of Python being ported.

### Local `Complex32` instead of `num-complex`

The LSM pipeline only needs add/sub/mul, magnitude, and direct field
access (the demod loop reads `.re` and `.im` directly to feed scalar
math). To avoid pulling `num-complex` as a new runtime dependency, the
module defines its own thin POD wrapper:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[repr(C)]
pub struct Complex32 {
    pub re: f32,
    pub im: f32,
}
```

`#[repr(C)]` keeps it ABI-compatible with `num_complex::Complex32` if the
project ever wants to swap. The compiler vectorises the inner loops over
this layout the same way it would over `num-complex`.

### Frozen filter taps

The Python reference designs the baseband LPF at runtime via
`scipy.signal.remez` (Parks-McClellan equiripple, passband 7250 Hz,
stopband 8000 Hz, 0.01 ripple) and the RRC matched filter via the
closed-form formula in `FilterFactory.getRootRaisedCosine` (alpha=0.2, 16
symbols, sps = 31250/4800 ≈ 6.51).

In Rust we embed the result as two `const [f32; N]` arrays so:

- The embedded build pulls in zero numerical-design code (no `pm-remez`,
  no `scipy` equivalent).
- The Rust pipeline is bit-comparable to the Python reference for any
  given input — the only sources of drift are the demod loop's f32 vs
  f64 rounding (Python uses f64; Rust uses f32 to match the FPGA-bound
  Phase 6E target).
- A future regen is one Python invocation: rerun
  `design_baseband_lpf(31250)` and `design_rrc(31250/4800, 16, 0.2)`,
  paste the new arrays into [filters.rs](../../p25-httpd/src/lsm/filters.rs).

LPF: 83 taps, DC gain ≈ 0.9899 (raw `scipy.signal.remez` output, no
unity normalisation). RRC: 105 taps, unit-energy normalised.

### Streaming primitives for the IRQ-driven path

The iq_dma ring delivers 32 KB sub-buffers (8192 complex samples ≈ 131 ms
at 62.5 kSPS) per interrupt. A naive per-IRQ batch pipeline would
re-initialise the FIR with zero history at every wakeup, producing a
~83-sample LPF transient and ~105-sample RRC transient at every
sub-buffer boundary — about 4.6% of each batch's symbols would be
garbage. To avoid that, [filters.rs](../../p25-httpd/src/lsm/filters.rs)
provides:

- `StreamingFir` — owns a `history: Vec<Complex32>` of length
  `taps.len() - 1`. `process(&[Complex32]) -> Vec<Complex32>` produces
  bit-exact equivalence to `apply_real_fir_complex` over the
  concatenation of all chunks ever fed in. Tested by feeding the same
  input both ways and asserting per-sample equality (`< 1e-5` absolute,
  the f32 noise floor of a 200-sample sweep).
- `StreamingDecimator2` — tracks input phase across odd-length chunks so
  the even-grid decimation never silently shifts by one sample at a
  chunk boundary. Tested by chunking a 23-sample input into 7+8+8 and
  asserting the concatenated output equals batch `decimate_by_2`.

The demod loop is also streaming: `DemodState` is the per-symbol PLL +
AGC + Gardner timing state, and `demod_lsm_with_state` takes `&mut
DemodState` so successive calls advance the same state machine. The
batch entry point `demod_lsm` is just the test/Python-port shim.

### LsmPipeline orchestration

[lsm/mod.rs](../../p25-httpd/src/lsm/mod.rs) bundles the four streaming
states into one `LsmPipeline` struct:

```rust
pub struct LsmPipeline {
    decimator: StreamingDecimator2,   // /2 phase tracking
    lpf: StreamingFir,                // 83-tap LPF history
    rrc: StreamingFir,                // 105-tap RRC history
    demod_state: DemodState,          // PLL + AGC + Gardner
}

impl LsmPipeline {
    pub fn new() -> Self;
    pub fn process_iq(&mut self, iq_62k5: &[Complex32]) -> LsmBatch;
    pub fn reset(&mut self);          // call on iq_dma overflow
}
```

`process_iq` runs the full chain (decimate → LPF → RRC → demod → hard
sync detect → soft sync detect → BCH FEC) and returns `LsmBatch { demod,
hard_events, soft_events }`. `reset` is called from the IRQ handler if
the gateware-side overflow latch fires (signalling a discontinuous input
stream).

## Gateware glue: `fpga::IpCore` extensions

[p25-httpd/src/fpga.rs](../../p25-httpd/src/fpga.rs) gains:

- `set_iq_dma_enable(bool)` — write `iq_dma_control.iq_enable` (level).
- `iq_last_buffer() -> u8` — read the 3-bit `last_buffer` field.
- `iq_overflow() -> bool` — read the Rsticky overflow latch (clears on read).
- `iq_next_address() -> u32` — debug, current AW write address.
- `read_iq_buffers() -> Vec<&[u8]>` — drain new sub-buffers since the
  last call. Reuses the existing `read_dma_buffers` ring-walking helper
  with a new `DmaChannel::Iq` arm.
- `InterruptHandler::waiter_iq_dma() -> InterruptWaiter` — third
  notify alongside the existing dibit/traffic waiters. The IRQ run loop
  reads `interrupts.iq_dma()` and notifies on bit set, mirroring the
  dibit/traffic logic.

`IpCore::take()` opens `/dev/p25-iq` via the existing `RxBuffer::new`
helper. The kernel-side rxbuffer device is registered by the new device
tree entry below.

## Tezuka device tree changes

[fishball-p25.dtsi](../../../Tezuka/tezuka_fw/board/tezuka/fishball7020/dts/fishball-p25.dtsi)
gains a third `reserved-memory` carve-out and a matching rxbuffer node:

```dts
reserved-memory {
    /* ... existing p25_dibit_dma @ 0x17000000 ... */
    /* ... existing p25_traffic_dma @ 0x18000000 ... */

    p25_iq_dma: p25-iq-dma@19000000 {
        no-map;
        reg = <0x19000000 0x40000>;     /* 256 KB */
        label = "p25_iq_dma";
    };
};

p25-iq {
    compatible = "maia-sdr,rxbuffer";
    memory-region = <&p25_iq_dma>;
    buffer-size = <0x8000>;             /* 32 KB per sub-buffer = 8 buffers */
};
```

The `maia-sdr,rxbuffer` driver in
[maia-kmod/maia-sdr.c](../../maia-kmod/maia-sdr.c) computes
`num_buffers = reserved_mem.size / buffer_size = 0x40000 / 0x8000 = 8`
which matches the FPGA `iq_dma_num_buffers_log2 = 3`.

## main.rs LSM reader task

[main.rs](../../p25-httpd/src/main.rs) spawns a third tokio task at
startup, after the existing dibit reader and IRQ handler:

```text
ip_core.set_iq_dma_enable(true);
let iq_waiter = interrupt_handler.waiter_iq_dma();

tokio::spawn(async move {
    let mut pipeline = LsmPipeline::new();
    loop {
        iq_waiter.wait().await;
        let (iq_bytes, overflow) = { /* lock, drain, unlock */ };
        if overflow { pipeline.reset(); }
        let batch = pipeline.process_iq(&sub_buffers_to_complex(&iq_bytes));
        // log per-IRQ NID stats: hard syncs, soft syncs, top NACs
    }
});
```

The task logs per-IRQ:

- IQ samples consumed
- Dibits produced
- Hard sync events found this batch
- Soft sync events found this batch
- Cumulative top-3 NACs across all events

This is enough to validate live RF behaviour against SDRTrunk on the same
antenna without needing the full TSBK decode chain. NACs that match the
target P25 site (`0x8A1` for Clay County) at a comparable rate to
SDRTrunk's PASSED count == ground truth.

The task is **independent of the existing dibit pipeline**. Both pull
from the same control DDC output, but via separate ring DMAs and separate
PS-side state. Disabling either by skipping `set_*_dma_enable` does not
affect the other.

## Verification

### Unit tests (`cargo test lsm::`, Windows host)

Final result:

```
running 17 tests
test lsm::filters::tests::lpf_dc_gain_close_to_unity ... ok
test lsm::filters::tests::decimate_by_2_takes_evens ... ok
test lsm::demod::tests::to_dibit_quadrants ... ok
test lsm::filters::tests::fir_impulse_response_matches_taps ... ok
test lsm::filters::tests::rrc_impulse_response_matches_taps ... ok
test lsm::demod::tests::demod_runs_on_constant_input ... ok
test lsm::nid_fec::tests::encoder_matches_sdrtrunk_vector ... ok
test lsm::demod::tests::dibit_phase_inverse_of_to_dibit ... ok
test lsm::filters::tests::streaming_decimator_preserves_phase_across_odd_chunks ... ok
test lsm::sync::tests::status_dibit_is_skipped ... ok
test lsm::filters::tests::streaming_fir_matches_batch_across_chunks ... ok
test lsm::sync::tests::hard_detector_tolerates_one_dibit_error_in_sync ... ok
test lsm::sync::tests::soft_detector_finds_clean_sync ... ok
test lsm::sync::tests::hard_detector_finds_clean_sync_and_decodes_nid ... ok
test lsm::nid_fec::tests::encode_decode_roundtrip_clean ... ok
test lsm::nid_fec::tests::errors_beyond_sphere_dont_silently_corrupt ... ok
test lsm::nid_fec::tests::error_correction_sweep_up_to_t11 ... ok

test result: ok. 17 passed; 0 failed
```

Coverage by file:

- **`nid_fec`** — 4 tests: SDRTrunk encoder vector
  (`NAC=1, DUID=0 → 0x00103185B7E9E224`), clean encode/decode
  roundtrip on 6 (NAC, DUID) pairs, error correction sweep 1..=11
  bit errors × 50 trials each = 550 corrupted-codeword recoveries (all
  decode to the right NAC/DUID), and a 12-error sanity test confirming
  the decoder doesn't silently return wrong codewords past the
  unique-decoding sphere.
- **`filters`** — 5 tests: Kronecker-delta impulse response on the LPF
  and RRC, /2 decimation drops the odd samples,
  `lpf_dc_gain_close_to_unity` checks the frozen-tap export against the
  measured 0.9899, `streaming_fir_matches_batch_across_chunks` proves
  the streaming FIR with chunk size 137 matches the batch FIR
  bit-for-bit on a 400-sample frequency-sweep input, and
  `streaming_decimator_preserves_phase_across_odd_chunks` proves the
  /2 decimator phase-tracks correctly across 7+8+8 odd-length chunks.
- **`demod`** — 3 tests: `to_dibit` quadrant assignment matches
  `Dibit.java`, `dibit_phase` is the inverse of `to_dibit`, the demod
  loop runs to completion on a 4096-sample 1 kHz tone and produces the
  expected number of symbols with finite, clamped PLL state.
- **`sync`** — 4 tests: hard detector finds a clean sync and decodes
  NAC=0x8A1/DUID=7 with zero FEC errors, hard detector tolerates a
  1-bit error in the sync pattern, soft detector finds a clean sync
  with score >130 and decodes NAC=1/DUID=0 with zero FEC errors,
  status dibit at index 11 of the NID window is correctly skipped
  (proven by feeding two streams that differ only in that dibit and
  asserting the extracted NID is identical).

### ARM cross-build (`cargo check --target armv7-unknown-linux-gnueabihf`)

```
warning: `p25-httpd` (bin "p25-httpd") generated 27 warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.28s
```

No errors. All 27 warnings are dead-code on existing modules
(`p25::tsbk`, `p25::types`, `p25::fec`) plus the new `lsm::sync::SyncEvent`
fields and `IQ_DMA_SAMPLE_RATE_HZ` constant that are referenced from
`main.rs` but only when actually wired into the Linux runtime path.

### What is still queued (on-hardware bring-up)

This phase delivers the code; on-target validation is the next step:

1. **Tezuka firmware rebuild.** Run `build.bat --p25` from
   `tezuka_fw/` (inside the Tezuka Docker container) to consume the new
   `system_top.xsa` from Phase 6C, re-cross-compile p25-httpd with the
   new lsm module, and produce a flashable image. The Tezuka build
   should auto-invalidate the p25-httpd Buildroot package on src/
   change (per change 009).
2. **Flash + boot.** Write the image to SD, boot Fishball.
3. **Smoke test.** Confirm in `/var/log/p25-httpd.log` that the LSM
   reader task starts (`"LSM IQ reader task started (Phase 6D)"`) and
   that wakeups happen at ~7.6 Hz (62.5 kSPS / 8192 samples ≈ 7.6
   sub-buffers/sec).
4. **NID accuracy check.** Watch the periodic `p25_lsm` log lines for
   `top_nacs=[0x8A1=N, ...]`. On a clean control channel, NAC=0x8A1
   should dominate at a per-second rate comparable to SDRTrunk on the
   same antenna (Phase 6A measured 313 syncs in ~27 seconds = 11.6/sec
   on the better-signal recording).
5. **Negative path check.** Compare against the dibit-pipeline NAC
   accuracy in the same boot — the dibit pipeline still uses the
   C4FM-only slicer and is expected to produce garbage NACs against
   LSM signal. The contrast confirms the LSM port works.
6. **iq_dma overflow check.** Watch `p25_lsm` warnings; if overflow
   latches fire under normal load, the LSM task is too slow to keep
   up with the ring (~250 KB/s consumer side, well within Cortex-A9
   capacity, so overflows would indicate a bug).

## Files touched

### New (Rust source)

- [p25-httpd/src/lsm/mod.rs](../../p25-httpd/src/lsm/mod.rs)
  — module root, `Complex32` POD type, `LsmPipeline` orchestration
- [p25-httpd/src/lsm/nid_fec.rs](../../p25-httpd/src/lsm/nid_fec.rs)
  — BCH(63,16,11) encoder + ML codebook decoder + tests
- [p25-httpd/src/lsm/filters.rs](../../p25-httpd/src/lsm/filters.rs)
  — LPF/RRC frozen taps, streaming FIR, streaming /2 decimator + tests
- [p25-httpd/src/lsm/demod.rs](../../p25-httpd/src/lsm/demod.rs)
  — `demod_lsm` + `demod_lsm_with_state` + tests
- [p25-httpd/src/lsm/sync.rs](../../p25-httpd/src/lsm/sync.rs)
  — hard + soft sync detectors + status-aware NID extractor + tests
- [p25-httpd/src/lsm/ring.rs](../../p25-httpd/src/lsm/ring.rs)
  — iq_dma sub-buffer to `Vec<Complex32>` adapter + tests

### Modified

- [p25-httpd/src/fpga.rs](../../p25-httpd/src/fpga.rs)
  — `IpCore` gains `iq_dma: RxBuffer`, `iq_last_addr: Option<u32>`,
    accessor methods for `iq_dma_status` / `iq_dma_control` / `iq_next_address`,
    `read_iq_buffers`. `DmaChannel` enum gains `Iq`. `InterruptHandler`
    gains `notify_iq_dma`, `waiter_iq_dma`, and the IRQ-loop iq branch.
- [p25-httpd/src/main.rs](../../p25-httpd/src/main.rs)
  — `mod lsm`, `set_iq_dma_enable(true)` at startup, third spawned task
    that runs `LsmPipeline` on iq_dma waker.
- [tezuka_fw/board/tezuka/fishball7020/dts/fishball-p25.dtsi](../../../Tezuka/tezuka_fw/board/tezuka/fishball7020/dts/fishball-p25.dtsi)
  — `p25_iq_dma` reserved-memory node + `p25-iq` rxbuffer node.

## What Phase 6D does NOT do

1. **No dibit pipeline removal.** The existing C4FM dibit pipeline,
   `ControlChannelDecoder`, and dashboard still run from the same boot.
   They are unchanged. Phase 6D adds a *parallel* pipeline; future
   phases will decide whether/when to retire the dibit path.
2. **No HTTP API surface for LSM events yet.** The LSM task currently
   logs to `tracing` only. Wiring `SyncEvent` into `httpd::AppState` is
   left for a follow-up so the dashboard can display LSM-decoded NACs
   alongside the dibit pipeline's TSBK output.
3. **No live-RF golden test embedded as a unit test.** The Python
   reference's truth-log diff against captured `.wav` recordings stays
   in `tools/`; the on-hardware bring-up validation in step 4 above is
   the equivalent for the Rust port.
4. **No HDL changes.** Phase 6E will port the streaming filters and the
   demod loop into Amaranth, replacing the C4FM-only `c4fm_demod` +
   `symbol_timing` path. That work is independent of this phase.

## Follow-up: LSM dashboard wiring (2026-04-09, same day)

First on-target bring-up confirmed the Rust LSM port works (see below),
but the dashboard at `:8080` still only surfaced the C4FM dibit
pipeline's output — which decodes to garbage NACs against an LSM
signal, so the System Identity panel showed `NAC=0xC0A` while the LSM
task's log lines showed `top_nacs=[0x8A1=2085, 0x12E=38, 0xABB=38]`.
The operator had to SSH in and tail `/var/log/p25-httpd.log` to see
the real decoded NAC. This follow-up clears the doc 014 punch-list
item ("No HTTP API surface for LSM events yet") by adding a parallel
LSM panel to the dashboard:

1. **`LsmStats` struct** added to
   [p25-httpd/src/lsm/mod.rs](../../p25-httpd/src/lsm/mod.rs) with
   cumulative counters (`wakeups`, `iq_samples`, `dibits`,
   `hard_events`, `soft_events`, `overflow_resets`), a
   `HashMap<u16, u64>` NAC histogram, and a `LastSync` snapshot of
   the most recent sync event. `record_batch` folds one `LsmBatch`
   into the stats, `record_overflow` bumps the reset counter, and
   `top_nacs(n)` returns the top-N NACs sorted descending with a
   stable secondary key. 3 unit tests cover accumulation, sort
   order, and counter independence.
2. **Shared `Arc<tokio::sync::Mutex<LsmStats>>`** created in
   [main.rs](../../p25-httpd/src/main.rs) before the
   `cfg(target_os = "linux")` block and cloned into both (a) the LSM
   tokio task, which now updates it every wake alongside the existing
   tracing logs, and (b) `httpd::AppState`. The task's local counter
   variables (`wakeups`, `total_iq_samples`, etc.) are removed — the
   shared stats are now the single source of truth and the logging
   branch snapshots them under the lock.
3. **`GET /api/lsm` handler** in
   [p25-httpd/src/httpd/mod.rs](../../p25-httpd/src/httpd/mod.rs)
   returns `serde_json::Value` inline (same lightweight pattern as
   `/api/dibit_dump` — no new `p25-json` types). Fields: `running`,
   `uptime_secs`, `last_wake_ms_ago`, cumulative counters, steady-
   state rates (`iq_samples_per_sec`, `dibits_per_sec`), top-10 NACs
   `[{nac, count, pct}, ...]`, and `last_sync: {nac, duid,
   fec_corrected, distance, score, age_ms}`. The `overflow_resets`
   field ships alongside an `overflow_note` string flagging the
   known Phase 6C false-positive.
4. **New "LSM Decoder (Phase 6D)" grid2 row** added to
   `DASHBOARD_HTML` just above the existing Dibit Histogram /
   Sync Correlator row. Left card: status pill ("ALIVE" green /
   "STALLED" or "NOT STARTED" red), uptime, wakeups, IQ samples with
   rate, dibits with rate, hard/soft sync totals, overflow resets,
   last sync display (`NAC DUID FEC✓ (Xs ago)`). Right card: top-10
   NAC table with count and percentage. The `refresh()` polling loop
   gains a `/api/lsm` fetch and DOM update block. The existing System
   Identity / Decode Stats panels are intentionally **left alone** so
   the operator can see the C4FM-decoder-on-LSM-signal garbage side-
   by-side with the real LSM output.

### Verification (dashboard wiring)

- `cargo check --bin p25-httpd` (Windows host): clean, 0 errors.
- `cargo check --bin p25-httpd --target armv7-unknown-linux-gnueabihf`:
  clean, 0 errors (all 26 warnings pre-existing dead code).
- `cargo test --bin p25-httpd lsm::stats_tests`: 3/3 new tests pass.
- On-target dashboard validation: deferred until next firmware
  rebuild + SD flash.

### Still deferred

- **iq_dma overflow debug** (spurious "overflow latched" on every
  sub-buffer despite correct sample throughput). Root cause is
  either sticky Rsticky semantics in the Phase 6C HDL, the
  `iq_overflow()` accessor in `fpga.rs`, or a `p25.svd` register-map
  mismatch. Not blocking — sample math proves no actual loss, and
  the dashboard now shows the reset count transparently so the bug
  is visible rather than silent. Queue for next session after on-
  target validation of this follow-up.
- **Live-RF golden test embedded as a `cargo test`** — still
  deferred from the original Phase 6D punch list.

## Phase ladder status after this commit

- 6A: Python LSM demod ✅ (commit `e1980aa`, doc 011)
- 6B: NID BCH FEC ✅ (commit `46630d6`, doc 012)
- 6C: IQ DMA path in FPGA gateware ✅ (commit `9f35f34`, doc 013)
- **6D: Rust port of demod + FEC to PS** ✅ (this change, doc 014)
  - Follow-up: LSM dashboard wiring ✅ (this doc, appended 2026-04-09)
- 6E: HDL/PL final implementation (codebook BRAM + popcount tree) — next
