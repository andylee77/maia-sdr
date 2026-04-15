# Phase 8 — HDL LSM Review + Runtime Reset Plan

Date: 2026-04-14

## Summary

Full HDL review of the LSM demod chain after Phase 7F (capture + BCH
sweep tooling) confirmed that the traffic-side audio quality problem
is **not** in the software framer, BCH decoder, or DUID extraction.
It is in the **traffic LSM HDL chain's lack of a runtime reset path**
combined with the PS never gating the LSM chain per-call. Phase 8
(8A + 8B + 8C) is the plan to fix it.

## Observation that started the investigation

After flashing `2026-04-14-idle-gate-sync-threshold` (which added the
sync-threshold override, ImbeForwarder Idle gate, manual encryption
blocklist, and per-call sync-threshold control), the audio was still
mostly broken: 1 in 20 calls gave intelligible audio, the rest were
robotic / noisy / short blips. Stats:

```text
HDL heartbeat:    LDU=50,  TDU/TDU_LC=149   ratio 1:3
Software framer:  LDU=45,  TDU/TDU_LC=176   ratio 1:3.9
```

Both decoder paths (HDL gateware BCH + PS software BCH) independently
agree: the NIDs are ~75 % TDU_LC-classified. Since they share the
same dibit source but use independent BCH implementations, the
corruption must be **upstream of BCH** — in the dibit stream itself.

## Dibit-level evidence

`/api/nid_capture` batch capture + `tools/p25_nid_analyze.py` BCH-t
sweep over 256 traffic-side NIDs:

```text
raw-duid (pre-BCH, straight off the dibit stream):
  HDU  (0x0):   5  ( 2.0%)
  TDU  (0x3):   5  ( 2.0%)
  LDU1 (0x5):  34  (13.3%)
  LDU2 (0xA):  40  (15.6%)
  TSDU (0x7):   1  ( 0.4%)
  ? (noise):   17  ( 6.7%)
  TDU_LC (0xF): 153 (59.8%)  ← already 60 % TDU_LC before BCH

bch-duid (post-BCH at t=11):
  HDU:     6 ( 2.3%)
  TDU:     3 ( 1.2%)
  LDU1:   36 (14.1%)
  LDU2:   36 (14.1%)
  TDU_LC: 153 (59.8%)
  rejected: 22 ( 8.6%)
```

BCH is faithfully preserving the pre-BCH distribution (post ≈ pre).
t-sweep from 11 → 1 barely moves the ratio — lowering BCH rejection
threshold can't fix this.

Control side under the **same framer code and same BCH**: 256/256 →
TSDU, 100 % correct. So the framer logic is fine.

The **sync distance histograms** are the smoking gun:

```text
control:  {0: 186, 1: 22, 2: 14, 3: 15, 4: 11, 5: 6, 6: 2}   — decaying tail (real)
traffic:  {0: 185, 1: 19, 2: 12, 3: 10, 4: 10, 5: 10, 6: 10}  — flat tail (noise)
```

Control has a **decaying** tail past `dist=1` — consistent with a
stable PLL producing marginal-but-real syncs at `dist=2..6`. Traffic
has a **flat** tail from `dist=2` onward — the uniform-noise
signature of a PLL that is never settled. Each flat-distribution hit
turns into a random-looking NID that BCH corrects to whichever
codeword happens to sit closest in Hamming space — and the all-ones
`NAC=0xFFF DUID=0xF` codeword has a large basin of attraction.

## Root cause (two independent gaps)

### Gap 1 — PS never gates the traffic LSM chain per-call

The PS thinks it's pausing the traffic chain between calls via
`set_traffic_demod_enable(false)`. It isn't. Two separate enable
registers exist:

| Register field | What it gates | Wired in HDL at |
|---|---|---|
| `traffic_demod_control.demod_enable` | C4FM traffic DMA only | `p25_top.py:979` |
| `traffic_lsm_control.traffic_lsm_enable` | LSM chain strobe at decimator | `p25_top.py:1043` |

`traffic_lsm_enable` is set to 1 exactly once at boot in
`p25-httpd/src/main.rs:670` and **never toggled again**. The
follower task's retune path at `main.rs:2213-2218` and timeout
handler at `main.rs:2138` only touch `traffic_demod_enable` — the
C4FM bit. The LSM chain has been running continuously since boot,
processing whatever the traffic DDC spits out, regardless of
follower state. Between calls it produces the phantom NID events
we've been seeing.

### Gap 2 — LSM chain has no runtime reset path

The stateful registers in the LSM chain all use `init=…` with
`reset_less=True`:

| Module | Stateful register | Reset path |
|---|---|---|
| `LsmPllUpdate` | `pll_reg: Signal(signed(pll_width), init=0, reset_less=True)` | **none** — only FPGA reconfig |
| `LsmTimingInterp` | `sample_point: Signal(signed(18), init=sample_point_init, reset_less=True)` + IQ FIFO | **none** |
| `LsmDiffDemodSlicer` | `prev_i`, `prev_q` | **none** |
| `LsmSyncNidExtract` | `sync_reg`, `reg_fill`, FSM state | clears `sync_reg=0` on `EMIT` only |
| `LsmNidBchFec` | in-flight sweep state | none |

`reset_less=True` tells Amaranth: "this register has no hardware
reset wire, use `init=` only at configuration time". The PS has
**zero** ways to force these registers back to their init values
during operation.

Why this matters: when the follower retunes the traffic DDC from
`858.4375 MHz` to `858.4625 MHz`, the CORDIC PLL accumulator still
holds the phase-error it settled to on the old carrier. The new
carrier has a different residual offset; the PLL has to
*re-converge*. During the re-convergence transient (hundreds of
ms), the slicer emits corrupted dibits → sync correlator matches
noise at various Hamming distances → noise-derived NIDs → BCH
converges on the nearest codeword → mostly `DUID=0xF=TDU_LC`.

## Why control-side works and traffic-side doesn't

The **control DDC never retunes**. It's fixed at the control
channel frequency since boot. Its PLL locked once months ago. The
traffic DDC retunes on every grant, sometimes multiple times per
second, and its PLL is almost always in a transient state.

This is a **completeness gap in the Phase 7A.2 HDL port**, not a
typo or a math error. The HDL module wiring is byte-for-byte
identical between control and traffic chains. The modules are
correctly written — they just lack the runtime-reset plumbing that
a *retunable* chain needs.

## Fix plan

### Phase 8A — HDL runtime reset plumbing (gateware)

Add a `reset_in: Signal()` port to `LsmDemod` and propagate down
through `LsmDemodLoop` to `LsmPllUpdate`, `LsmTimingInterp`,
`LsmDiffDemodSlicer`, and across to `LsmSyncNidExtract` /
`LsmNidBchFec`. Inside each module, when `reset_in` is asserted
(1 cycle), clear the stateful registers to their init values.

Add a new W1P register field `traffic_lsm_control.traffic_lsm_reset`
wired to a 1-cycle strobe generator that drives
`traffic_lsm_demod.reset_in`. Mirror on the control side
(`lsm_control.lsm_reset` → `lsm_demod.reset_in`) — trivially
zero-cost and future-proofs against Phase 7G channel-hopping.

See `DEVPLAN.md` Phase 8A for the full file list, unit test plan,
and acceptance criteria.

### Phase 8B — PS integration

With the new reset pin available, the retune path becomes:

```rust
pub fn retune_traffic_chain(&self, freq_hz: u64) -> Result<()> {
    self.set_traffic_lsm_enable(false);        // freeze chain
    self.set_traffic_ddc_frequency(freq_hz)?;   // new NCO
    self.pulse_traffic_lsm_reset();             // clear PLL / timing / sync
    self.set_traffic_lsm_enable(true);          // re-enable
    self.set_traffic_demod_enable(true);        // C4FM too
    Ok(())
}
```

On Idle → timeout or encrypted tear-down, call `pause_traffic_chain`
which writes both `traffic_lsm_enable = 0` and
`traffic_demod_enable = 0`. Between calls, neither chain runs.

### Phase 8C — LSM chain local clock domain (architectural)

Move each `LsmDemod` instance into its own
`m.domains.lsm_<side> = ClockDomain(local=True)` with reset wired
to `~<side>_lsm_enable`. Disabling the enable becomes equivalent to
a full reset, giving us "disable = full reset" semantics at zero
runtime cost. Lays groundwork for Phase 7G (channelizer +
multi-channel follower) where each LDU slot will want its own
clock domain.

## Artefacts left in place from Phase 7F debug session

These are already flashed on the board and in source, and are
independently useful. Phase 8 doesn't touch them:

- `/api/nid_capture?side=control|traffic&arm=1&limit=N&clear=1` —
  NID batch capture ring, dumps JSON of the last N captured NIDs
  with pre-BCH and post-BCH DUIDs, sync distances, raw dibits.
- `/api/bch_t?side=traffic|control&value=N` — runtime BCH-t
  override (per-decoder). Useful for future sensitivity sweeps.
- `/api/sync_tune?side=traffic|control&threshold=N` — per-decoder
  sync-distance threshold override.
- `/api/encrypted_tgs` — manual encryption blocklist for sites
  that never set `service_options.encrypted` on grants.
- `tools/p25_nid_analyze.py` — offline BCH-t sweep analysis tool.
- `ImbeForwarder::imbe_frames_dropped_idle` counter — phantom
  frame drops when `current_talkgroup == 0`.
- `ControlChannelDecoder::reset_framer_state()` — framer-only
  reset called on retune (independent of the new HDL reset).
- Event log ring + `/api/log` + Logs tab in the dashboard.

Once Phase 8 lands, the decoder framer reset and the phantom-drop
counter become redundant belts-and-suspenders (the HDL reset will
mean there are no phantom frames to drop), but they don't hurt to
leave in place.

## Current board state at end of Phase 7F

- BUILD_TAG: `2026-04-14-idle-gate-sync-threshold`
- Manual encryption blocklist: `[402, 414, 430, 433, 600, 700]`
- Traffic sync threshold override: 2 (reset to default via
  `PUT /api/sync_tune?side=traffic&threshold=reset` before Phase 8
  testing to get a clean baseline)
- Last-observed audio quality: 1 clear call in ~20, rest garbled

## Entry point for a new session

1. Read this change doc + `DEVPLAN.md` Phase 8 section.
2. Start Phase 8A with `maia-hdl/p25_hdl/lsm_pll_update.py` — add
   the `reset_in` port and the `pll_reg` clear path.
3. Run the existing PLL unit test to confirm no regression:
   `pytest -k pll_update` (MAIA_HDL_SLOW_TESTS=1 for the full
   convergence regression).
4. Work outward through the module tree, one file per commit,
   adding `reset_in` at each level.
5. Last: wire the new `traffic_lsm_reset` register in `p25_top.py`,
   regen SVD + PAC, `./build_fpga.bat --p25`.
6. Phase 8B: edit `p25-httpd/src/fpga.rs` to add the new helpers,
   update the follower retune path in `main.rs`, bump BUILD_TAG,
   flash.
7. Verify by re-running the capture sweep — post-BCH DUID histogram
   should show real LDU dominance and near-zero false TDU_LC.
