# Maia SDR -- Changelog (andylee77 fork)

Tracking log for the `andylee77/maia-sdr` fork.
Upstream: [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr) (originally [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr))

---

## [2026-04-11] Phase 7A.2 -- LSM demod chain on traffic side + HDU/TDU/LDU dispatch

**Branch:** fishball-p25
**Related:** `doc/changes/034_phase7a2_lsm_traffic_chain_and_tdu_hdu.md`

Mirrors Phase 6E.9 on the traffic side: a parallel LSM demod chain
sits beside the existing C4FM traffic chain on the traffic DDC
output, identical to the control-side LSM chain. The new chain
produces NID events (NAC + DUID + BCH validity + sync distance)
which the PS-side heartbeat task polls at 16 ms cadence and
dispatches by DUID to the appropriate TrafficManager handler:

| DUID | Name | Dispatch |
|------|------|----------|
| `0x0` | HDU (Header) | `hdu_received(now, nac)` |
| `0x3` | TDU | `tdu_received(now, nac, false)` |
| `0x5` | LDU1 | `ldu_received(now, nac, false)` |
| `0xA` | LDU2 | `ldu_received(now, nac, true)` |
| `0xF` | TDU_LC | `tdu_received(now, nac, true)` |

This is the FPGA prerequisite for HDU + TDU detection on followed
voice channels. With TDU detection in place, the TrafficManager
gains a **2 s post-TDU hold window** (matches SDRTrunk PR #2010 /
commit `1b3ce431` `STALE_EVENT_THRESHOLD_MS = 2000`) so that PTT
releases between speakers in a multi-speaker conversation reuse
the same slot instead of fragmenting into separate calls. Phase
7C will tap the new `traffic_lsm_dibit_dma` ring in parallel for
IMBE frame extraction, and Phase 7D will add the IMBE -> PCM
vocoder.

**HDL changes:**

- `maia-hdl/p25_hdl/config.py`: new
  `traffic_lsm_dibit_dma_address = 0x1B00_0000` constant +
  validate() assertion.
- `maia-hdl/p25_hdl/p25_top.py`: new constructor instantiations
  (`traffic_lsm_decimator`, `traffic_lsm_lpf`, `traffic_lsm_rrc`,
  `traffic_lsm_demod`, `traffic_lsm_dibit_packer`,
  `traffic_lsm_dibit_dma`), new `traffic_lsm` register bank at
  offset 0xC0 (bank 6) with bit-identical layout to the
  control-side `lsm` bank, new `m_axi_traffic_lsm_dibit` AXI
  master, new `interrupts.traffic_lsm_dibit_dma` field, register
  crossbar update for `addr_bank == 0b110`. The
  `elaborate()` chain wiring mirrors lines 664-781 of the
  control-side LSM chain exactly, just fed by `traffic_ddc.re_out`
  instead of `ddc.re_out`.
- `maia-hdl/projects/fishball7020_p25/system_bd.tcl`: new
  `ad_mem_hp1_interconnect` line for
  `p25_core/m_axi_traffic_lsm_dibit`.
- `maia-hdl/ip/p25-core/default/p25_core.v`: regenerated from
  Amaranth (54888 lines, +16K from Phase 7A.1).

**PS Rust changes:**

- `p25-httpd/p25-pac/p25.svd` + `src/lib.rs`: regenerated via
  `svd2rust` to expose the new `traffic_lsm_*` register accessors.
- `p25-httpd/src/p25/traffic_manager.rs`: new fields
  (`post_tdu_hold_until`, `last_duid`, `last_nac`, `hdus_seen`,
  `tdus_seen`, `ldus_seen`), new methods (`hdu_received`,
  `tdu_received`, `ldu_received`, `post_tdu_hold_remaining_ms`),
  modified `note_activity()` (now also clears the post-TDU hold),
  modified `check_timeouts()` (honours the post-TDU hold window
  with priority over the call_timeout_ms fallback). The
  Phase 7A.1 Acquiring auto-promote bug fix is still present and
  carries over.
- `p25-httpd/src/fpga.rs`: new
  `set_traffic_lsm_enable/dibit_dma_enable/dc_block_enable`
  helpers, `traffic_lsm_control_readback`, `traffic_lsm_status`,
  `traffic_lsm_nid`, `traffic_lsm_drop_count`,
  `traffic_lsm_dibit_last_buffer/next_address`,
  `traffic_lsm_debug`, new `traffic_lsm_dibit_dma: RxBuffer`
  field opened from UIO device `p25-traffic-lsm-dibit`,
  new `DmaChannel::TrafficLsmDibit` variant + branch in
  `read_dma_buffers`, new `notify_traffic_lsm_dibit_dma` /
  `waiter_traffic_lsm_dibit_dma` for the IRQ source, plus IRQ
  counter and log line wiring.
- `p25-httpd/src/main.rs`: extended `IrqStats` with
  `traffic_lsm_dibit` field, new traffic LSM chain init at
  startup (enable + dibit DMA + DC blocker, with readback
  verification), new traffic LSM heartbeat task (polls
  `traffic_lsm_status` at 16 ms cadence, dispatches NID events by
  DUID), BUILD_TAG bumped to
  `2026-04-11-phase7a2-traffic-lsm-chain-and-tdu-hdu`.
- `p25-httpd/src/httpd/mod.rs`: extended `/api/traffic` snapshot
  with `last_duid` / `last_duid_hex` / `last_duid_label` /
  `last_nac` / `last_nac_hex` / `hdus_seen` / `ldus_seen` /
  `tdus_seen` / `post_tdu_hold_remaining_ms` / `traffic_lsm_chain`
  (full new register bank readback) / `irq.traffic_lsm_dibit_total`.

**Tezuka side (separate repo):**

- New device-tree carve-out for
  `p25_traffic_lsm_dibit_dma@1b000000` so the rxbuffer kernel
  module exposes a `p25-traffic-lsm-dibit` UIO device. Mirrors
  the existing `p25_lsm_dibit_dma@1a000000` carve-out.

**Documentation:**

- `doc/changes/034_phase7a2_lsm_traffic_chain_and_tdu_hdu.md`
  (new).
- `doc/P25_API.md` -- new "Phase 7A.2 additions" section under
  `/api/traffic`.
- `doc/P25_ADDRESS_MAP.md` -- new bank 6 detail section, new
  IRQ table row, new HP1 master row, new DDR carve-out row.
- `tools/p25_status_and_next_step.py` -- Phase 7A.2 ROADMAP
  entry's check() now actually verifies
  `/api/traffic.traffic_lsm_chain.enabled == true` instead of
  always returning false.

**Verification (pending):** combined Phase 7A.1 sticky-lock fix +
Phase 7A.2 LSM traffic chain will be verified together on the
next flash. Acceptance criteria:

1. `/api/traffic.traffic_lsm_chain.enabled == true`
2. NID events arrive at the heartbeat dispatcher (visible in
   `last_duid_label` rotating through HDU / LDU1 / LDU2 / TDU)
3. `hdus_seen + ldus_seen + tdus_seen` grows during a real call
4. TDU release is sub-second (visible in `post_tdu_hold_remaining_ms`
   counting down from 2000 -> 0 after TDU)
5. `tools/p25_sticky_lock_test.py` reports
   `delta_retunes <= 2` over the 12 s sample window during a
   real call (validates the Phase 7A.1 Acquiring auto-promote
   fix on hardware)
6. NID CRC pass rate on the traffic LSM chain matches the
   control-side ~85% per-block when locked on a known voice
   channel

---

## [2026-04-11] Phase 7A.1 -- Traffic-channel grant follower scaffold + sticky-lock policy

**Branch:** fishball-p25
**Related:** `doc/changes/033_phase7a1_traffic_scaffold_wire_up.md`

First step of Phase 7 (voice channel follow + audio out). Wires
the **already-existing** C4FM traffic-channel scaffold (HDL chain
from doc 007 + `fpga.rs` traffic helpers + `p25/traffic_manager.rs`
state machine, all sitting dormant since Phase 4) into the live
`p25-httpd` process so that:

- The traffic DDC is configured at startup (the existing
  `configure_ddc()` only set up the control DDC; the new
  `configure_traffic_ddc()` mirrors the same decimation /
  operations / bypass writes against the `traffic_*` register bank,
  no coefficient loading because the FIR ROM is shared at the HDL
  level).
- A new traffic dibit reader task drains the `traffic_dma` ring
  on every IRQ, builds a per-dibit histogram in `TrafficStats`,
  and pets the `TrafficManager` activity timer.
- A new traffic grant follower task polls
  `lsm_decoder.grants` at 50 ms cadence and retunes the traffic
  DDC to follow active calls.
- A new `/api/traffic` endpoint surfaces TrafficManager state,
  the dibit histogram, the traffic_dma IRQ counter, and four
  manual-control query params (`?reset_stats=1`,
  `?follower=on/off`, `?retune_hz=N`, `?demod_enable=0/1`)
  processed in fixed order.

**Sticky-lock policy from SDRTrunk upstream PR #2010 (commit `1b3ce431`):**
The initial naive newest-by-timestamp follower thrashed the
singleton DDC across multiple simultaneously-active TGs (~15+
retunes/sec observed on Clay County). Replaced with sticky-lock:
TG-based call identity (matching SDRTrunk's
`isSameCallCheckingToOnly()`), 2 s stale eviction threshold
(matching `STALE_EVENT_THRESHOLD_MS = 2000`), and a polling-task
gate that only accepts new TGs when state is Idle. Same-TG
different-frequency falls through to retune (handles network
channel reassignment mid-call). Plus an `Acquiring -> Active`
auto-promote in `handle_grant` that fixes a compound bug where
the 200 ms `acquire_timeout_ms` was hard-timing-out every call
because no real sync detector exists yet (Phase 7C).

**No FPGA bake required.** The traffic chain has been in the
HDL since Phase 4 (doc 007) and is already in the bitstream
from `tezuka_fw@08f7607`. Only `p25-httpd` needed a Tezuka
rebuild + flash to pick up the new endpoint and tasks.

Files touched:

- `p25-httpd/src/fpga.rs` -- new `configure_traffic_ddc()`.
- `p25-httpd/src/p25/traffic_manager.rs` -- removed
  `#![allow(dead_code)]`, added metrics fields and accessors,
  TG-based call identity in `handle_grant`, 2 s
  `call_timeout_ms`, `Acquiring -> Active` auto-promote.
- `p25-httpd/src/main.rs` -- new `TrafficStats`, two new tokio
  tasks (dibit reader + grant follower), traffic DDC startup
  configuration, BUILD_TAG bump to
  `2026-04-11-phase7a1-traffic-scaffold-wire-up`.
- `p25-httpd/src/httpd/mod.rs` -- extended `AppState`, new
  `/api/traffic` endpoint with four manual-control query params.
- `doc/P25_API.md` -- documented `/api/traffic` + bumped route
  count to 21.
- `doc/changes/033_phase7a1_traffic_scaffold_wire_up.md` -- new
  doc with the discovery, design decisions, the sticky-lock
  derivation from SDRTrunk PR #2010, the Acquiring bug story,
  and the on-target verification appendix.
- `tools/p25_status_and_next_step.py` -- new Phase 7A.1 ROADMAP
  entry plus restaged Phase 7A.2 -> 7H entries; also fixed two
  pre-existing brittle build-tag-string checks (Phase 6F
  source-preservation and Phase 6G.1 DC blocker) by replacing
  them with functional checks against `/api/grants[].source`
  and `/api/lsm_control.lsm_dc_block_enable`.
- `tools/p25_sticky_lock_test.py` -- new verification script
  that polls `/api/traffic` until an active call is seen, then
  takes a 12-sample burst to confirm `retunes` stays flat.

**Verification:** Round 1 (scaffold) verified on hardware -- all
seven acceptance criteria from doc 033 pass. Round 2 (sticky
lock) verified the TG pin holds. Round 3 (Acquiring auto-promote
fix) deferred to the next flash since the user was away from
the device when the second bug was found and fixed; the same
binary that ships Phase 7A.2 will validate the auto-promote
behavior automatically.

---

## [2026-04-11] Phase 6 closeout -- LSM trunking control channel COMPLETE

**Branch:** fishball-p25
**Related:** `doc/changes/032_phase6_closeout.md`

Phase 6 (the multi-month port of an LSM Simulcast P25 control
channel decoder onto the Fishball Z7020) is **DONE**. The Clay
County NAC 0x8A1 control channel is decoded end-to-end on the
FPGA + ARM PS at ~76-80 % steady-state TSBK CRC pass with ~88 %
of CRC-OK blocks dispatching as structured TsbkMessage events,
TG dedup + source-RadioId preservation in the active grants
table, and a runtime DC blocker A/B knob.

This commit closes out the phase with three small additions
and a documentation sweep:

### New endpoint: `/api/lsm_control` (Phase 6G.2)

`p25-httpd/src/httpd/mod.rs` adds a new GET handler that
reads back all three `lsm_control` register bits
(`lsm_enable`, `lsm_dibit_dma_enable`, `lsm_dc_block_enable`)
and exposes a `?dc_block=0|1` query-param shortcut for
toggling the DC blocker without ssh + devmem. The handler
takes the `ip_core` lock once and does the optional write +
the readback under it so a write+read sequence is atomic.
Closes the doc 031 verification gap that previously required
shell access on the board for runtime A/B testing.

The two other lsm_control bits are intentionally read-only
from this endpoint -- flipping them at runtime would tear
down the radio for no debugging benefit, and the devmem
escape hatch is still there.

### Documentation sweep

- New `doc/changes/032_phase6_closeout.md` -- canonical
  "Phase 6 is done, here's what shipped, here's what was
  consciously deferred, here's Phase 7" reference. Includes
  the full sub-phase rollup (6A through 6G.2), the deferred-list
  with decision references, the final commit chain, and a
  Phase 7A-7E sketch for the next session.
- `DEVPLAN.md` -- updated implementation order section to
  reflect Phase 6 completion (was stale past Phase 5 since
  the original C4FM-only redirect). Now shows Phase 6 sub-phases
  6A-6G.2 marked done with brief descriptions, and Phase 10
  added as the explicit Phase 7 voice-channel-follow next
  step with sub-phases 7A-7E.
- `doc/P25_API.md` -- new `/api/lsm_control` section, updated
  route count from 19 to 20, removed `/api/lsm_control` from
  the "endpoints we don't have" table.
- `tools/p25_status_and_next_step.py` -- ROADMAP[] entry for
  Phase 6G.2 now probes the live `/api/lsm_control` endpoint
  to verify the new binary is on the box. New
  `render_lsm_control` section in the snapshot output.

### Memory

- New `project_phase7_entry_point.md` memory replaces the
  obsolete `project_phase6e_entry_point.md` and
  `project_phase6f_entry_point.md` files (both were full of
  historical Phase 6F.x debug detail that lives in the change
  docs now). The new memory is forward-looking: where Phase 6
  ended, what the Phase 7A-7E plan is, and the recommended
  fresh-session entry point.

### Status

Source ships in this commit. **One more Tezuka rebuild + flash
is needed** to get this binary onto the board (no FPGA bake --
the bitstream from `tezuka_fw@08f7607` is unchanged). After
flashing, `/api/system.build` will report the new
`phase6-closeout` tag and
`tools/p25_status_and_next_step.py` will advance the
next-step pointer to Phase 7A.

---

## [2026-04-11] Phase 6G.1 -- HDL DC blocker on the LSM IQ input

**Branch:** fishball-p25
**Related:** `doc/changes/031_phase6g1_hdl_dc_blocker.md`

First commit of the doc 030 PL port roadmap. Adds a pair of one-pole
leaky-integrator DC blockers (one each for I and Q) at the very
front of `LsmDemod`, runtime bypassable through a new
`lsm_control.lsm_dc_block_enable` register bit. The slicer was
running on a slightly DC-biased input, which gave it a 60/40
inner/outer dibit ratio for the first 2-3 minutes after PLL start
until the loop slowly absorbed the bias on its own. With the
front-end DC blocker enabled the slicer never sees the bias in
the first place and the loop should lock immediately from cold boot.

### HDL changes

- New `LsmDcBlocker` Elaboratable in `maia-hdl/p25_hdl/lsm_dc_blocker.py`.
  One-pole leaky integrator with `alpha = 1 - 2^-7` (~39 Hz cutoff at
  31.25 kSPS, ~4 ms time constant). Pure shifts and adds, no DSPs,
  no BRAM. Saturated signed-16 output. Runtime bypass via `enable_in`.
  ~6 LUTs per instance.
- `LsmDemod` instantiates two of them at the front, with a new
  `dc_block_enable` top-level input. The blockers add one cycle of
  latency on the IQ path, which is invisible to `LsmTimingInterp`.
- `p25_top.py` adds a new `lsm_dc_block_enable` field at
  `lsm_control[2]`, default 0 (matches the existing `lsm_enable`
  convention), wired through to `LsmDemod.dc_block_enable`.

### PS changes

- `p25-pac` SVD updated; PAC regenerated with `svd2rust 0.33.5`.
- `fpga.rs`: new `set_lsm_dc_block_enable(bool)` helper;
  `lsm_control_readback()` extended to return the new bit.
- `main.rs`: control DDC startup now calls
  `set_lsm_dc_block_enable(true)` alongside the existing
  `set_lsm_enable(true)` / `set_lsm_dibit_dma_enable(true)`. The
  startup readback log line includes the new bit, and a
  `tracing::warn!` fires if the readback comes back false (with
  the explicit warning that the PLL acquisition transient will be
  2-3 minutes instead of a few seconds, so this isn't a silent
  regression).

### Tests

- New `test/test_lsm_dc_blocker.py`: 4 unit tests (step response
  bit-exact against a Python reference + decay below 0.5% of input;
  passband 1 kHz unattenuated; bypass passes DC through unchanged;
  strobe lockstep with input).
- New `test_dc_blocker_absorbs_constant_iq_bias` regression in
  `test/test_lsm_demod.py`: drives `LsmDemod` with the synthetic
  golden + a 6%-of-fullscale DC bias on both I and Q; verifies the
  dibit pass-through still produces a sensible dibit count.
- All 9 LSM demod / DC blocker tests pass; `cargo check` on
  `p25-httpd` is clean.

### Doc

- `doc/changes/031_phase6g1_hdl_dc_blocker.md` -- full design rationale,
  fixed-point format, on-target verification plan.
- `doc/P25_ADDRESS_MAP.md` -- documents the new `lsm_dc_block_enable`
  field at `lsm_control[2]`.

### Status

HDL + PS source ships in this commit. **Next:** rebuild bitstream
via `build_fpga.bat --p25`, commit the binary artefact separately
(per the build/commit-sequencing rule), then on-target A/B
verification (blocker on vs off) to confirm the cold-boot lock
time drops from 2-3 minutes to a few seconds.

---

## [2026-04-11] Phase 6F.11 -- PS at 100% (5 new opcode parsers + API merge)

**Branch:** fishball-p25
**Related:** `doc/changes/030_phase6f11_ps_complete_and_pl_port_roadmap.md`

Finishes the PS Rust side of the Fishball P25 LSM control channel
decoder. Adds the top 5 unparsed opcodes from the 6F.10
verification, extends `SystemIdentity` with the new state fields,
and unions both decoders' state in the dashboard handlers so the
operator sees the best of both pipelines while the per-pipeline
diagnostic split stays intact in `/api/decoder_compare`.

### Five new opcode parsers

All using SDRTrunk-style absolute-bit-position layouts via the
existing `TsbkBlock::bits()` helper:

| Opcode | Name | What it brings |
|---|---|---|
| 0x05 | UU_ANS_REQ | Private call paging (target + source radio IDs) |
| 0x09 | TELE_INT_VCH_GRNT_UPDT | Telephone interconnect grant update |
| 0x16 | SNDCP_DCH_ANN_EX | SNDCP packet-data channels (DL + UL) |
| 0x30 | TDMA_SYNC_BCST | System date/time + microslot rollover |
| 0x39 | SEC_CCH_BROADCST | Backup primary control channels A/B |

### Extended `SystemIdentity`

New optional fields populated by the new parsers: `secondary_cch_a/b`,
`sndcp_downlink/uplink_channel`, `last_sync_clock`. `p25-json::SystemInfo`
grows matching `Option<String>` fields with `serde` skip-if-none.

### API-level merge of both decoders

`/api/system`, `/api/grants`, `/api/bands` now read BOTH
`lsm_decoder` and `iq_lsm_decoder` and union the state:

- `/api/system` picks the most-populated value per field via
  `pick(a, b) = a.or(b)`
- `/api/grants` unions grants by channel, picks the YOUNGER on
  duplicates
- `/api/bands` unions frequency band entries by identifier

`/api/decoder_compare` is INTENTIONALLY unchanged to keep the
per-pipeline diagnostic A/B comparison from 6F.4-6F.10. See doc 030
for the design discussion.

### Final on-target numbers (92 s post PLL lock)

| Pipeline | TSDU/s | Block/s | CRC OK/s | Pass% |
|---|---:|---:|---:|---:|
| `ps_lsm` | 10.17 | 30.50 | **25.04** | 82.1% |
| `ps_iq_lsm` | 10.23 | 30.70 | **16.55** | 53.9% |
| **COMBINED** | — | **61.20** | **41.59** | — |

- **87.6 % opcode coverage** of CRC-OK blocks (2021/2308 parsed)
- **Top 9 opcodes all `parsed: yes`** in `/api/tsbk_opcodes`
- **3 simultaneous active grants decoded** (TG 300/433/402)
- **`bands_known = 6`** (FDMA + TDMA via merge)

### PS side is feature-complete

Doc 030 captures what "PS at 100 %" means concretely + the PL port
roadmap for Phase 6G:

1. **HDL DC blocker** (top priority) -- shrinks PLL acquisition
   transient from 2-3 min to seconds, fixes the 60/40 inner/outer
   slicer ratio that costs us ~70 % of syncs during transients
2. **(possibly) soft sync correlator into PL HDL** -- moderate
   value, would let us retire the parallel `iq_lsm_decoder` pipeline
3. **Multi-channel decode for trunking failover** -- only if
   we have a real failover need
4. **TSBK status/Viterbi feed** -- defer indefinitely, not
   CPU-limited

What stays in PS forever: BCH NID FEC, Trellis Viterbi, TSBK CRC,
all opcode parsers, dashboard / WebSocket / API. What we won't do:
port BCH FEC to HDL (already there + PS port is faster), port
opcode parsers to HDL (high-level state machine work), kill the
parallel-decoder architecture as a "cleanup" (lose diagnostic
A/B value the 6F.4-6F.10 saga depended on).

Tests: cargo test = 52 green.

Build tag: 2026-04-11-phase6f.11-five-new-opcode-parsers-and-api-merge

---

## [2026-04-11] Phase 6F.5 → 6F.9 throughput breakthrough (PS LSM decoder)

**Branch:** fishball-p25
**Related:** `doc/changes/029_phase6f5_through_6f9_throughput_breakthrough.md`

Five flash cycles of throughput tuning + diagnostic infrastructure
that took the PS LSM decoder from 2.3 useful messages/sec to a
steady-state ~14.8 parsed messages/sec across 8 opcode types.
Both stretch targets (30 TSBK/sec, 10 msg/sec) MET.

### Final on-target numbers (130s steady-state, post PLL lock)

| Pipeline | TSDU/s | Block attempts/s | CRC OK/s | Pass% |
|---|---:|---:|---:|---:|
| **ps_lsm** (HDL slicer + dibit hard sync) | **12.4** | **37.2** | **34.1** | **91.7%** |
| **ps_iq_lsm** (raw IQ + soft sync, NEW) | **11.6** | **21.5** | **19.8** | **92.3%** |
| **COMBINED** | **24.0** | **58.7** | **53.9** | 92% |

### What landed in each phase

- **6F.5 -- TDMA IDEN_UPDATE offset fix.** SDRTrunk's
  `FrequencyBandUpdateTDMA.getTransmitOffset()` multiplies by
  `getChannelSpacing()`, NOT by 250 kHz like FDMA/VUHF. We were
  reporting -780 MHz on Clay County's TDMA bands instead of the
  SDRTrunk-correct -39 MHz. One-line fix in
  `decode_iden_update_tdma`.
- **6F.6 -- sync distance histogram.** New
  `sync_distance_hist[25]` field on `ControlChannelDecoder` that
  buckets every observed sync distance. Exposed via
  `/api/lsm_dibit_dump`. The diagnostic that revealed the PLL
  acquisition transient was distorting all the early throughput
  measurements.
- **6F.7 -- runtime tunable threshold + sweep tool.** New
  `RUNTIME_SYNC_THRESHOLD: AtomicU32`, two new GET endpoints
  (`/api/sync_tune?threshold=N` and `/api/decoder_reset`), new
  `tools/p25_sync_sweep.py` automated threshold sweep tool. All
  endpoints accept GET-with-query-params so they work from a
  plain browser bar / curl.
- **6F.8 -- decoder_reset bug fix.** New
  `ControlChannelDecoder::reset_diagnostics()` method that clears
  EVERY per-run counter / histogram in one place. The 6F.7
  handler had missed `sync_hits`, `total_dibits`, `dibit_hist`,
  `recent_dibits`, and `raw_duid_hist`, which made the sweep tool
  conflate lifetime average with per-window throughput.
- **6F.9 -- IQ-LSM parallel decoder.** New
  `process_directed_tsdu()` method on `ControlChannelDecoder`
  that skips the Hunting state machine and runs NID + multi-block
  TSBK decode directly on a caller-supplied dibit buffer. New
  `iq_lsm_decoder` field on `AppState`, fed by Phase 6D's
  `LsmPipeline` running on raw IQ -- soft sync events from
  `find_sync_events_soft` get dispatched into the directed-decode
  path with a 400-dibit cross-batch carry-over so events near a
  batch boundary still find their full 336-dibit body. Third
  parallel TSBK pipeline alongside the legacy C4FM and HDL LSM
  decoders. Robust against future HDL slicer regressions.

### Diagnostic infrastructure additions

New / updated REST endpoints:

- `GET /api/sync_tune` -- read current threshold + cumulative
  histogram
- `GET /api/sync_tune?threshold=N` -- write new threshold
- `GET /api/decoder_reset` -- clear all counters for clean
  measurement window
- `POST /api/decoder_reset` -- HTTP-method-correct alias
- `PUT /api/sync_tune?threshold=N` -- HTTP-method-correct alias
- `GET /api/decoder_compare` now includes `ps_iq_lsm` slice
- `GET /api/lsm_dibit_dump` now includes `sync.distance_hist[25]`

New tools:

- `tools/p25_sync_sweep.py` -- walks a list of thresholds with
  reset between each, prints comparison table
- `tools/p25_check_phase6f4.py` -- now displays "IQ-LSM decoder"
  section + "Sync distance histogram" section

### Lessons learned (saved to memory)

The biggest single throughput improvement wasn't any of the
threshold tuning or parser fixes -- it was waiting for the LSM
PLL to fully converge. On the Fishball P25 LSM signal the PLL
takes 2-3 minutes after a flash to settle, and during that
transient the slicer produces a 60/40 inner/outer dibit ratio
with ~5 bit errors per sync window. Every measurement in the
first 90 seconds shows ~22% CRC pass rate which **looks like** a
fundamental signal-quality ceiling but is actually just PLL
hunting noise.

I burned three flash cycles tuning sync threshold trying to
"fix" what was just transient noise. The right thing was to
wait, not flash. Saved as
`feedback_pll_acquisition_transient` memory.

### Tests

`cargo test p25::` = 28 green at every phase. Full crate = 52
green. No new tests because all the changes are diagnostic
infrastructure or parallel pipelines that share the existing
tested parser code.

### Open follow-ups still on the queue (none are blockers)

- iq_lsm cross-batch defer (6F.10) -- push `ps_iq_lsm` from 1.86
  blocks/TSDU to 3.0
- Bump `max_recent` 100 → 1000 + fix verification script
  `messages/sec` calculation
- Add parsers for SNDCP_DCH_ANN_EX (0x16), TDMA_SYNC_BCST (0x30),
  SEC_CCH_BROADCST (0x39), UU_ANS_REQ (0x05),
  TELE_INT_V_CH_GRANT_UPDT (0x09) -- biggest unparsed buckets,
  ~30 lines each
- Merge dashboard system identity from BOTH lsm decoders
- HDL DC blocker (long-term DEVPLAN item)

---

## [2026-04-11] Phase 6F.3 multi-block TSBK2 / TSBK3 support (PS LSM decoder)

**Branch:** fishball-p25
**Related:** `doc/changes/027_phase6f3_multi_block_tsbk.md`

Adds end-to-end multi-block TSBK support to the PS LSM software
decoder. Phase 6F.2j (doc 026) shipped a working TSBK1 reader that
populates the System Identity card, but it stopped after one block
per TSDU and dropped ~2/3 of on-air TSBK content because most TSDUs
on the Clay County test target are TSBK1+TSBK2+TSBK3 multi-block
frames.

This phase generalises the deinterleaver and the
`ReadingDataUnit` arm of the state machine to handle 1, 2, or 3
TSBK blocks per TSDU. After each successful block decode the
state machine inspects the `LB` (last block) header bit and
either extends `du_expected_len` to the next block boundary
(231 dibits for TSBK2, 303 for TSBK3) or returns to Hunting.
Trellis or CRC failure on any block also returns to Hunting,
matching SDRTrunk's framer behavior.

Expected impact on the Clay County test target after on-target
verification: roughly **3x more TSBK messages decoded per second**,
and the previously stuck `bands_known` and `active_grants` counters
should start populating now that `IDEN_UPDATE` and
`GRP_VOICE_CHAN_GRANT` TSBKs riding in TSBK2/TSBK3 slots are
finally being read.

### Code

- `p25-httpd/src/p25/fec.rs` -- `TsduDeinterleaver` rewritten
  with `body_dibits_for_blocks(num_blocks) -> Option<usize>` and
  `deinterleave_multi(tsdu_dibits, num_blocks) -> Vec<u8>`. Status
  positions are pre-computed for the period-36 schedule (max 9
  positions for TSBK3). The trellis test helper was also lifted
  out of `mod tests` to module level (`trellis_encode_block` +
  new `trellis_encode_bytes` wrapper) so cross-module e2e tests
  can build real TSBK frames.
- `p25-httpd/src/p25/control_channel.rs` -- new field
  `tsdu_blocks_decoded`, renamed `process_tsdu` →
  `process_tsdu_block` returning `bool` (`true` = done, `false` =
  need more dibits), state machine inspects the return value to
  decide whether to extend or transition to Hunting. Aligned
  capture finalization moved into `finalize_capture(...)` helper.

### Tests

Two new e2e tests in `control_channel.rs::tests`:

- `test_multi_block_tsbk_e2e` -- builds a real
  `TSBK1=NET_STS_BCST(LB=0)` + `TSBK2=RFSS_STS_BCST(LB=1)` body,
  verifies both blocks decode and dispatch their messages.
- `test_single_block_tsbk_terminates_on_lb1` -- regression guard
  that confirms `LB=1` on TSBK1 correctly stops without
  consuming dibits from the next sync window.

Two new fec.rs tests for the multi-block deinterleaver:

- `test_tsdu_deinterleave_two_blocks` (231 raw → 196 trellis)
- `test_tsdu_deinterleave_three_blocks` (303 raw → 294 trellis)
- `test_body_dibits_for_blocks_table`

`cargo test p25::` runs 28 tests, all green. Full crate test
suite (52 tests) green.

---

## [2026-04-10] Phase 6E.10 PS-side scaffolding + iq/dibit packer overflow HDL hotfix

**Branch:** fishball-p25
**Related:** Tezuka `doc/changes/004_p25_lsm_dibit_dma_reserved_memory.md`

Bring-up companion to the Phase 6E.10 Vivado bake below. Adds the
p25-httpd PS-side accessors and tasks that let Linux userspace
actually exercise the new HDL LSM chain on hardware, the Tezuka DT
carve-out that makes the new `lsm_dibit_dma` ring visible as
`/dev/p25-lsm-dibit`, and a long-deferred Phase 6C hotfix to the
iq/dibit packer `overflow` semantics that was silently wrecking the
Phase 6D Rust LSM pipeline on every boot.

See `doc/changes/020_iq_dibit_packer_overflow_pulse.md` for the
overflow hotfix investigation + fix, and the existing `018`/`019`
docs for the Phase 6E.9/6E.10 HDL context.

### p25-httpd PS-side scaffolding for the HDL LSM chain

- `p25-httpd/src/fpga.rs`: `IpCore` grows a fourth `lsm_dibit_dma`
  `RxBuffer` opened against the new `/dev/p25-lsm-dibit` chardev and
  a full set of `lsm_*` register accessors against the regenerated
  PAC -- `set_lsm_enable`, `set_lsm_dibit_dma_enable`, `lsm_status`
  (returning a new `LsmStatusSnapshot` struct that captures all 7
  fields of the register in one bus read so the PS sees a coherent
  per-NID-event picture), `lsm_nid`, `lsm_drop_count`,
  `lsm_dibit_last_buffer`, `lsm_dibit_next_address`, `lsm_debug`,
  `read_lsm_dibit_buffers`. `DmaChannel` gains an `LsmDibit` variant
  wired through the existing `read_dma_buffers` helper.
- `InterruptHandler` gains `notify_lsm_dibit_dma` + matching
  `waiter_lsm_dibit_dma()`, and the IRQ fanout loop decodes the new
  bit 3 (`interrupts.lsm_dibit_dma`) alongside the existing three.
  NID events themselves are deliberately NOT IRQ-driven -- the 60 Hz
  polling loop below catches every ~14 ms NID with plenty of headroom.
- `p25-httpd/src/main.rs`:
  - Boot sequence enables the HDL LSM chain alongside C4FM + iq_dma:
    `ip_core.set_lsm_enable(true)` +
    `ip_core.set_lsm_dibit_dma_enable(true)`.
  - New "HDL LSM dibit reader" task drains the `lsm_dibit_dma` ring
    on every IRQ and histograms the dibit distribution for bring-up
    sanity. It deliberately does NOT feed the dibits into the Phase
    2A C4FM control-channel decoder -- LSM has different symbol-phase
    timing so cross-feeding would corrupt the working decoder's state.
    A dedicated LSM TSBK decoder is a Phase 6F follow-up.
  - New "HDL LSM NID poller" task polls `lsm_status.nid_event` at
    60 Hz (16 ms tick). On each fired event it reads the coherent
    snapshot (lsm_status + lsm_nid + lsm_drop_count + lsm_debug in
    one pass under the mutex), logs NAC/DUID/`n_errors`/
    `sync_distance`/`drop_count`/`pll_dbg`/`sample_point_dbg`
    throttled to 5 Hz (always logs the first 10 events), and warns
    on `lsm_dibit_overflow` latches or `drop_count` bumps.
  - The Phase 6D Rust LSM path keeps running in parallel as an
    independent sanity check. Both pipelines consume the same
    control DDC output and should emit identical NID streams on the
    same RF feed -- useful A/B during bring-up. Retiring the Phase
    6D PS-side path is a Phase 6F decision once both converge.
  - Phase 6 stats task is extended to also log
    `lsm_dibit_last_buffer` and `lsm_dibit_next_address`.

### Tezuka DT carve-out for `/dev/p25-lsm-dibit`

Added to `board/tezuka/fishball7020/dts/fishball-p25.dtsi` alongside
the existing dibit / traffic / iq entries (kernel-side counterpart
in `andylee77/tezuka_fw`):

```dts
p25_lsm_dibit_dma: p25-lsm-dibit-dma@1a000000 {
    no-map;
    reg = <0x1a000000 0x8000>;
    label = "p25_lsm_dibit_dma";
};

p25-lsm-dibit {
    compatible = "maia-sdr,rxbuffer";
    memory-region = <&p25_lsm_dibit_dma>;
    buffer-size = <0x1000>;
};
```

32 KB region, 8 x 4 KB sub-buffers -- mechanically identical to the
existing C4FM `p25_dibit_dma` ring, just at a new base. Mirrors the
FPGA-side `lsm_dibit_dma_address = 0x1A00_0000` from
`maia-hdl/p25_hdl/config.py`. Tezuka commit lands separately in
`andylee77/tezuka_fw/doc/changes/004_p25_lsm_dibit_dma_reserved_memory.md`.

### Phase 6C iq/dibit packer overflow HDL hotfix (doc 020)

Long-deferred bug from doc 014 follow-ups: `iq_dma` overflow latch
fired on every sub-buffer on hardware, causing
`p25-httpd/src/main.rs` to call `pipeline.reset()` on the Phase 6D
Rust LSM pipeline every ~128 ms, which wiped accumulated streaming
FIR delay lines / /2 decimator phase / Gardner TED history / Costas
PLL accumulator / sync-detector state before the pipeline had any
chance to converge. On-target symptom: `overflow_resets` ticking up
monotonically at ~7.6 Hz regardless of signal strength, and the
Phase 6D Rust LSM pipeline never locking onto the Clay County
control channel despite the Python reference doing so cleanly on
the same wav capture.

**Root cause.** `maia-hdl/p25_hdl/iq_packer.py` (and
`dibit_packer.py` -- same pattern) drove `self.overflow.eq(1)` on
the trigger condition but never cleared it anywhere else, so once
latched the signal was stuck high forever. The `maia_hdl.register`
`Rsticky` wrapper implements read-clear as `sticky := input` (not
`sticky := 0`), because the intended semantics are "one-cycle pulse
in, accumulated + clear-on-read sticky out". With a stuck-high
input, reads "clear" the accumulator by replacing it with the
current input value (still 1), and the next cycle's
`sticky := sticky | input` re-accumulates it immediately. The PS
never sees the bit clear. The class docstring even documented this
backwards (*"never self-clears in the gateware; the AXI Rsticky
layer is the only clear path"*).

**Fix.** Add a default `m.d.sync += self.overflow.eq(0)` at the top
of `elaborate()` in both packers. Amaranth's last-assignment-wins
means the conditional `self.overflow.eq(1)` inside the trigger
branch still fires, but now only for one cycle; the Rsticky wrapper
accumulates the pulse and clears it correctly on PS read.

`dibit_packer` had the same latent bug but it had never triggered
in practice: at 4800 sym/s on a 1.7 GB/s HP1 budget, the trigger
condition (previous word still waiting for `stream_ready` when the
next word arrives) effectively never fires. Fixed anyway for
consistency.

### Overflow regression guard + test rewrite

Added `test_overflow_is_pulse_not_latched` to both
`test_iq_packer.py` and `test_dibit_packer.py`. The new test fires
enough strobes under back-pressure to guarantee at least one
overflow trigger, samples `overflow` every cycle during the burst,
then verifies: (1) overflow actually fired at least once, (2) it
fell to 0 within one tick after the strobe burst stopped, (3) it
stayed at 0 for 8 ticks after back-pressure was released. Fail
mode: cycle-high counter bumps past a small threshold with an
explicit "this is the Phase 6C latched-level bug (see doc 020)"
assertion message.

The old `test_overflow_sticky` in both files actively asserted the
*wrong* behaviour (*"overflow should be sticky in gateware"*) and
therefore could never have caught the bug via pure regression
testing. Rewrote both `test_overflow_flag` tests to sample cycle
by cycle instead of only at the end of the burst, so a 1-cycle
pulse still passes.

### PS-side `main.rs` companion fix (defence in depth)

Even with the HDL fix, the old PS-side behaviour was wrong: it
called `pipeline.reset()` on any overflow bit, which throws away
legitimate LSM lock state. The right reaction is log + count + keep
running, because doc 014 proved the sample math shows no actual
data loss when the bit fires. A genuine back-pressure event that
caused actual sample loss would need a higher-layer detection
(gap in sample timestamps or backward jump in the FPGA's AW
address counter), not inference from the Rsticky.

### Regenerated HDL artefacts

Re-ran `./build_hdl.bat --p25 --verilog-only` to roll the packer
fix into `p25_core.v`. `p25.svd` and `p25-pac/src/lib.rs` are
regenerated but their content is byte-identical to the post-6E.10
version -- this is a gate-level internal change inside the packer's
`elaborate()` that does not touch any register layout.

### Verification

- `python -m unittest test.test_iq_packer test.test_dibit_packer`
  -- 13/13 pass.
- `python -m unittest test.test_iq_packer test.test_dibit_packer
  test.test_c4fm_demod test.test_symbol_timing` -- 25/25 pass, no
  regressions in the wider p25_hdl suite.
- Host-side `cargo check` on the p25-httpd workspace -- clean (90
  pre-existing warnings, no errors).
- ARM cross-check `cargo check --target
  armv7-unknown-linux-gnueabihf` -- clean (27 pre-existing warnings,
  no errors). New `fpga.rs` + `main.rs` paths compile under
  `cfg(target_os = "linux")`.
- First Vivado bake (bitstream A, pre-fix) completed cleanly at
  `maia-hdl/projects/fishball7020_p25/fishball_p25.sdk/system_top.xsa`
  -- kept as a baseline for comparison, not intended for flashing.
- Second Vivado bake (bitstream B, with fix) running in background.

### What gets flashed

On-target testing should use **bitstream B** (post-fix) plus the
p25-httpd binary from this commit plus the Tezuka firmware that
picks up `004_p25_lsm_dibit_dma_reserved_memory.md`. With all three
in place:

- `overflow_resets` in `/api/lsm` should stay at 0 for the first
  minute of uptime and climb only on actual HP1 stalls (vs the
  ~7.6 Hz boot-constant of the old bitstream).
- The Phase 6D Rust LSM pipeline should start accumulating
  `hard_events` / `soft_events` within seconds of enable as the
  streaming state is no longer being wiped.
- The Phase 6E.9 HDL LSM path should start logging NID events at
  the expected ~14 ms cadence with `nac=0x8A1`, `valid=true`,
  `n_errors<=11`, `drop_count=0`, and `lsm_dibit_overflow` not
  latching.
- Both decoders should agree on the same NIDs on the same RF
  capture, cross-validating the HDL port of 6E.0-6E.9 against the
  Phase 6D Rust reference.

---

## [2026-04-10] Phase 6E.10: Vivado bake artefacts (regen Verilog + PAC, wire `m_axi_lsm_dibit` through TCL)

**Branch:** fishball-p25

Phase 6E.10 closes out the **HDL side** of Phase 6E. The 6E.9 source already
wired `LsmDemod` into `P25Core` at the Amaranth level; this sub-phase
regenerates the binary HDL artefacts that Vivado actually consumes
(`p25_core.v`, `p25.svd`, `p25-pac/src/lib.rs`) and adds the two TCL
one-liners that surface the new `m_axi_lsm_dibit` AXI master to the IP
packager and the block-design HP1 SmartConnect. After this commit, running
`build_fpga.bat --p25` produces a bitstream that contains the full LSM HDL
chain plus its parallel dibit DMA ring.

See `doc/changes/019_phase6e10_vivado_bake.md` for the full design log.

### Regenerated HDL artefacts

`build_hdl.bat --p25 --verilog-only` (Docker, ~1 min wall clock) refreshed
all three downstream artefacts from the 6E.9 Amaranth source:

- **`maia-hdl/ip/p25-core/default/p25_core.v`** -- 770 KB / 21,896 lines
  (pre-6E.7) -> 1.30 MB / 37,420 lines. The ~70 % growth is dominated by
  `LsmDemod` and its 11 child Elaboratables, plus the second
  `DibitPacker`/`DmaStreamRingWrite` pair for `lsm_dibit_dma`, plus the
  new `lsm` register bank with its `RegisterCDC`.
- **`p25-httpd/p25-pac/p25.svd`** -- 17 KB / 21 registers (pre-6E.9) ->
  22 KB / 27 registers. Adds the six `lsm_*` registers from the new bank 5
  (`lsm_control`, `lsm_status`, `lsm_nid`, `lsm_drop_count`,
  `lsm_dibit_next`, `lsm_debug`) at offsets `0xa0..0xb4`. The first 21
  registers are byte-for-byte unchanged so existing PAC consumers are
  unaffected.
- **`p25-httpd/p25-pac/src/lib.rs`** -- auto-regenerated by `svd2rust 0.33.5`
  inside the Docker `build_hdl.sh` pass. Compiles cleanly under
  `cargo check` (33 lifetime-elision warnings, all in the new `lsm_*`
  accessors, matching the same pre-existing svd2rust pattern that already
  affects every other bank in the file).

### TCL wiring for `m_axi_lsm_dibit`

- **`maia-hdl/projects/fishball7020_p25/system_bd.tcl`** -- one extra
  `ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 p25_core/m_axi_lsm_dibit`
  call alongside the existing three masters. `ad_mem_hp1_interconnect` is
  idempotent, so the new master simply becomes a fourth slave port on the
  same HP1-bound SmartConnect that already hosts `m_axi_dibit`,
  `m_axi_traffic`, and `m_axi_iq`. HP1 budget at ~1.7 GB/s absorbs the
  additional ~1.28 KB/s from the LSM dibit ring at well below 0.001 %
  utilisation.
- **`maia-hdl/ip/p25-core/package_ip.tcl`** -- one extra
  `ipx::associate_bus_interfaces -busif m_axi_lsm_dibit -clock clk` so the
  Vivado IP packager places the new master in the 100 MHz core clock
  domain (`maia_sdr_clk/clk_out1`). Without this line the IP packager
  would emit the master as an unclocked port and the block design would
  refuse to auto-connect it.

### Verification

- Docker regen: 27 expected registers in the SVD dump, all at the offsets
  the address-map doc predicted; `p25_core.v` lands at the expected size.
- `rg -c m_axi_lsm_dibit p25_core.v` returns 190 (top-level port + AXI
  channel signals through several wrapper levels).
- `cd p25-httpd/p25-pac && cargo check` finishes in 0.77 s with 33
  pre-existing-pattern warnings, no errors.
- `cd p25-httpd && cargo check --workspace` finishes in 9.42 s with 90
  pre-existing warnings, no errors -- existing C4FM/iq_dma/Phase 6D LSM
  PAC consumers all compile unchanged against the regenerated PAC.
- Amaranth HDL test suites (49/49 LSM HDL + 17/17 older P25 HDL) not
  re-run: no `.py` source touched in this sub-phase, so re-running them
  would be a no-op against the same 6E.9 source the doc 018 verified.

### What 6E.10 does *not* cover (user-side hand-off)

Three remaining items live on the user's side because they need
Vivado 2023.2 + the Fishball Z7020 hardware:

1. `build_fpga.bat --p25` -- the actual Vivado IP packaging + synth + PAR
   that produces the `.xsa`. Expected wall-clock time is ~20 % longer
   than the pre-6E.9 baseline because the Verilog is ~70 % larger. Per
   the build/commit sequencing rule, the resulting `.xsa` should land in
   a separate "shipping artefact" commit before it goes into a Tezuka
   firmware image.
2. Tezuka device-tree carve-out for `p25_lsm_dibit_dma@1a000000`
   (32 KB region, 32 KB alignment) alongside the existing `iq_dma` /
   `traffic_dma` / `dibit_dma` reserved-memory entries. Mechanically
   identical to the Phase 6C `iq_dma` carve-out, just at a new base.
3. On-target validation against Clay County NAC 0x8A1: with
   `lsm_control.{lsm_enable, lsm_dibit_dma_enable} = 1`, point the
   control DDC at 860.9625 MHz and confirm `lsm_status.nid_event` is
   firing at the expected ~14 ms cadence with `nid_valid == 1`,
   `n_errors <= 11`, `lsm_nid.nac == 0x8A1`, `drop_count == 0`, and
   `lsm_dibit_overflow` not latching.

The "AGC deferred to 6E.6.5" caveat from doc 015 is still in force; the
Clay County signal is strong enough that SDRTrunk decodes it without
explicit AGC, so on-target smoke against this specific site should still
pass even though `LsmDemod` does not yet have a runtime AGC.

### Phase ladder status (post-6E.10)

- 6A: Python LSM demod -- DONE
- 6B: NID BCH FEC -- DONE
- 6C: IQ DMA path in FPGA gateware -- DONE
- 6D: Rust LSM port to PS -- DONE
- 6E.0-6E.6: HDL front end + demod loop -- DONE
- 6E.6.5: AGC in HDL -- deferred follow-up
- 6E.7: BCH FEC in HDL -- DONE
- 6E.8: LsmDemod top-level (sync detect + NID pipeline) -- DONE
- 6E.8.5: Soft sync detector -- deferred follow-up
- 6E.9: wire LsmDemod into p25_top.py alongside C4FM -- DONE
- **6E.10: regen `p25_core.v` + Vivado wiring -- DONE on the HDL side (THIS commit)**
  - Vivado synth via `build_fpga.bat --p25` -- pending user action
  - Tezuka DT carve-out for `lsm_dibit_dma` -- pending user action
  - On-target validation against NAC 0x8A1 -- pending user action

Phase 6F (PS-side LSM HDL consumer + dashboard wiring) is the natural
follow-up once the on-target smoke passes.

---

## [2026-04-10] Phase 6E.9: Wire LsmDemod into p25_top.py alongside C4FM

**Branch:** fishball-p25

Phase 6E.9 is complete. The standalone `LsmDemod` Elaboratable from
Phase 6E.8 is now plumbed into `P25Core` so the top-level FPGA design
runs the C4FM and LSM demod chains in parallel on the control channel.
After this sub-phase the only Phase 6E HDL work remaining is the Vivado
bake (6E.10). See `doc/changes/018_phase6e9_lsm_top_integration.md` for
the full design log.

### New top-level pipeline

The control DDC output (62.5 kSPS, 16-bit signed I+Q) now drives both
chains in parallel:

```text
control DDC -> /2 decimator -> 83-tap LPF -> 105-tap RRC -> LsmDemod
                                                              |
                                                              +--> lsm_dibit_packer -> lsm_dibit_dma
                                                              +--> NID event registers
```

The C4FM chain (`C4FMDemod` -> `SymbolTimingRecovery` -> `dibit_packer`
-> `dibit_dma`) is unchanged. The traffic channel stays C4FM-only --
adding LSM there is a follow-up phase.

### New DDR carve-out + AXI master

`P25Config.lsm_dibit_dma_address = 0x1A00_0000`, 32 KB total ring
(8 sub-buffers x 4 KB), naturally aligned. New `m_axi_lsm_dibit` AXI
master added to `P25Core.ports()` for Vivado IP packaging in 6E.10.
This is a deliberately parallel ring (rather than muxing the existing
`dibit_dma`) so the PS can drain both rings simultaneously and A/B C4FM
vs LSM on the same RF capture without disturbing either chain. Cost is
~1.28 KB/s on HP1 -- well below 0.001 % of HP1 budget.

### New `lsm` AXI register bank (bank 5, byte base `0x7C46_00A0`)

Six registers in an 8-slot bank (3-bit reg field), 2 slots free for
future expansion:

- **`lsm_control`** -- `lsm_enable` (RW, master enable for the entire
  LSM chain; gates the strobe at the front of `LsmDecimator2` so all
  downstream blocks go quiescent when 0) + `lsm_dibit_dma_enable` (RW,
  enables the lsm_dibit_dma ring's AW channel).
- **`lsm_status`** -- `bch_busy` (R), `in_nid_window` (R, useful as a
  "have lock" indicator), `nid_event` (Rsticky, latches each
  `nid_event_strobe`, clears on read), `nid_valid` (R, latched), and
  the latched 7-bit fields `n_errors` and `sync_distance`. Also packs
  the Rsticky `lsm_dibit_overflow` flag.
- **`lsm_nid`** -- latched `nac` (12 bits) + `duid` (4 bits) for the
  most recent BCH-decoded NID event.
- **`lsm_drop_count`** -- 16-bit saturating count of NIDs the sync
  detector emitted while `bch_busy` was high (should always read 0 in
  normal operation), plus the LSM dibit DMA `last_buffer` index.
- **`lsm_dibit_next`** -- AW write address inside the LSM dibit ring
  (debug only).
- **`lsm_debug`** -- snapshots of `pll_dbg` (signed Q2.13) and the top
  16 bits of `sample_point_dbg` (signed Q4.10), useful for dashboard
  Costas-loop and Gardner-timing traces.

The five "latched" NID-event fields (`nid_valid`, `n_errors`,
`sync_distance`, `nac`, `duid`) live in local `Signal()`s that are
updated on each `nid_event_strobe` pulse, so the PS sees a coherent
snapshot per event. The `nid_event` Rsticky bit tells the PS *which*
snapshot is current; reading `lsm_status` clears it.

### New IRQ bit

Bank 0 `interrupts` register grows a fourth Rsticky bit at offset 3:
`lsm_dibit_dma`, fed by `lsm_dibit_dma.interrupt`. NID events themselves
are PS-polled via `lsm_status.nid_event` rather than IRQ-driven, because
at one NID per ~14 ms a 60 Hz dashboard poll catches every event without
burning IRQ overhead.

### Verification

- `P25Core(P25Config())` constructs cleanly, generates a ~22 KB SVD
  (up from ~17 KB) covering the new bank, and elaborates to ~1.5 MB
  Verilog (up from ~1.27 MB).
- 49/49 LSM HDL tests pass in ~135 s (no regressions vs Phase 6E.8;
  the same 2 slow BCH sweeps are still gated behind
  `MAIA_HDL_SLOW_TESTS=1`).
- 17/17 older P25 HDL tests pass (`test_c4fm_demod`, `test_dibit_packer`,
  `test_symbol_timing`).

No new test files in 6E.9 -- the building blocks are exhaustively
tested in their own benches, and the integration is purely top-level
wiring covered by the elaboration smoke test.

### Resource estimate (Z7020, post-6E.9)

| Component                                     | DSP48  | BRAM18 | LUT     | FF    |
|-----------------------------------------------|--------|--------|---------|-------|
| C4FM chain (control + traffic, unchanged)     | ~10    | 0      | ~2000   | ~1500 |
| Maia DDC + DMA infra (unchanged)              | ~30    | ~10    | ~5000   | ~2500 |
| LSM front end (decimator + LPF + RRC)         | ~2     | 0      | ~150    | ~250  |
| LSM demod (`LsmDemod`)                        | ~30    | 2      | ~3940   | ~1730 |
| LSM dibit packer + DMA                        | 0      | 0      | ~100    | ~50   |
| LSM register bank                             | 0      | 0      | ~80     | ~150  |
| **6E.9 grand total**                          | **~72**| **~12**| **~11270**| **~6180** |

That's ~33 % DSP48, ~9 % BRAM18, ~21 % LUT, ~6 % FF on Z7020 -- comfortable
margins for the Vivado bake in 6E.10 even after PAR overhead.

---

## [2026-04-10] Phase 6E.8: LSM Demod Top-Level (sync detect + NID pipeline)

**Branch:** fishball-p25

Phase 6E.8 is complete. The complete LSM demod chain (IQ -> dibits ->
sync detect -> NID extract -> BCH decode -> NAC/DUID) now lives behind
a single top-level Elaboratable, `LsmDemod`. After this sub-phase the
only HDL work remaining for Phase 6E is wiring `LsmDemod` into
`p25_top.py` (6E.9) and the Vivado bake (6E.10). See
`doc/changes/017_phase6e8_lsm_demod_top.md` for the full design log.

### New modules

- **`maia-hdl/p25_hdl/lsm_sync_nid_extract.py`** -- 48-bit hard sync
  detector + status-skipping NID extractor. Streaming HDL equivalent
  of `find_sync_events_hard()` + `extract_nid_skipping_status()` from
  `p25-httpd/src/lsm/sync.rs`. State machine: IDLE shifts dibits into
  a 48-bit register, gates the threshold check on a fill counter
  (no false-trigger before the register has 24 dibits), checks
  `popcount(reg ^ FRAME_SYNC_DIBIT_PATTERN) <= 4`, then enters
  COLLECT_NID for 33 dibits, skipping index 11 (the status dibit) and
  packing the remaining 32 dibits into a 64-bit NID word MSB-first.
  EMIT pulses `nid_strobe` for one cycle and clears the sync register
  to suppress re-trigger. Hard detector only -- the Rust soft detector
  is deferred to an optional 6E.8.5 sub-phase because it would need a
  CORDIC `atan2` tap on the differential demod output.

- **`maia-hdl/p25_hdl/lsm_nid_pipeline.py`** -- thin wrapper that
  chains `LsmSyncNidExtract` + `LsmNidBchFec` and owns the start/done
  handshake plus a 16-bit saturating NID-drop counter. The handshake
  feeds `nid_strobe` to `bch.start` gated by `~bch.busy`; if a NID
  arrives mid-decode it's silently dropped and the counter is bumped
  (NIDs are spaced ~14 ms apart and BCH takes ~656 us, so this should
  never fire on real RF). Extracted from `LsmDemod` for testability:
  the entire control logic of 6E.8 lives here, in one self-contained
  Elaboratable that can be integration-tested without dragging the
  IQ-to-dibit demod loop into the test bench.

- **`maia-hdl/p25_hdl/lsm_demod.py`** -- top-level `LsmDemod`.
  Instantiates `LsmDemodLoop` + `LsmNidPipeline` side by side, passes
  the dibit stream straight through to the existing `dibit_dma` path
  so the existing dibit consumer keeps working unchanged, and surfaces
  the new NID event outputs (`nid_event_strobe`, `nac_out`, `duid_out`,
  `n_errors_out`, `valid_out`, `sync_distance_out`, `in_nid_window`,
  `bch_busy`, `nid_drop_count`) plus the existing `pll_dbg` /
  `sample_point_dbg` debug taps from the demod loop.

### Tests

- **`maia-hdl/test/test_lsm_sync_nid_extract.py`** -- 6 standalone
  tests for the sync detector + NID extractor (no BCH cost):
  clean sync + clean NID for NAC=0x8A1/DUID=7, 1-dibit error in the
  sync pattern (mirrors the Rust 1-error tolerance test),
  status-dibit-skip equivalence, back-to-back sync events, no
  false-trigger before the 24-dibit register fills, and
  `in_nid_window` waveform tracking. Total runtime <0.5 s.

- **`maia-hdl/test/test_lsm_nid_pipeline.py`** -- 1 integration
  test: drives a constructed sync + clean NID dibit stream through
  `LsmNidPipeline` and verifies one `nid_event_strobe` fires with
  the right (NAC, DUID, n_errors=0, valid=1, sync_distance=0), and
  `nid_drop_count` stays at 0. ~13 s sim time (one BCH decode at the
  decoder's 65,538-cycle serial sweep).

- **`maia-hdl/test/test_lsm_demod.py`** -- 1 wiring test: drives
  the existing `demod_loop_synthetic.json` IQ golden through
  `LsmDemod` and verifies the dibit pass-through still produces ~254
  dibits AND the NID pipeline stays quiescent (no `bch_busy`, no
  `in_nid_window`, no `nid_event_strobe`, `nid_drop_count == 0`)
  because the synthetic golden has no sync pattern. The standalone
  per-dibit accuracy vs truth is covered by `test_lsm_demod_loop`
  and is not re-checked here. ~1.7 s sim time.

### Test results

```text
$ python -m unittest test.test_lsm_decimator test.test_lsm_fir \
    test.test_lsm_timing_interp test.test_lsm_diff_demod_slicer \
    test.test_lsm_gardner_ted test.test_lsm_pll_update \
    test.test_lsm_pll_rotate test.test_lsm_demod_loop \
    test.test_lsm_nid_bch_fec test.test_lsm_sync_nid_extract \
    test.test_lsm_nid_pipeline test.test_lsm_demod
...
Ran 49 tests in 172.729s
OK (skipped=2)
```

Up from 41/41 in 6E.7. Two skips are the slow-mode BCH sweeps from
6E.7 still gated behind `MAIA_HDL_SLOW_TESTS=1`. All previous LSM
tests still pass unchanged.

### Resource estimate (Z7020)

| Component | DSP48 | BRAM18 | LUT | FF |
|---|---|---|---|---|
| LsmDemodLoop (6E.6d) | ~30 | 2 | ~3500 | ~1500 |
| LsmSyncNidExtract (6E.8a) | 0 | 0 | ~50 | ~130 |
| LsmNidBchFec (6E.7) | 0 | 0 | ~340 | ~50 |
| LsmNidPipeline + LsmDemod glue | 0 | 0 | ~40 | ~50 |
| **LsmDemod total** | **~30** | **2** | **~3930** | **~1730** |

~14% of Z7020 DSP48, 1.4% of BRAM18, ~7% of LUT/FF for one full LSM
channel. Plenty of room for the existing C4FM chain, the Phase 6C
IQ DMA, the Maia SDR base platform, and any future AGC follow-up.

### Out of scope (deferred)

- AGC in HDL (still 6E.6.5)
- Soft sync detector (new 6E.8.5 -- only if hard detector under-
  performs on real RF)
- `p25_top.py` integration (6E.9)
- Vivado bitstream bake + on-target validation (6E.10)

### Status

Phase 6E.8 ends at "the entire LSM demod chain is one Elaboratable,
the dibit pass-through is verified against the synthetic golden, the
sync detect + NID extract + BCH chain is verified end-to-end on a
constructed clean stream, all 49 LSM HDL tests pass". Phase 6E.9 --
wiring `LsmDemod` into `p25_top.py` alongside the existing C4FM chain
and surfacing the new NID event outputs as AXI registers -- is up
next.

---

## [2026-04-10] Phase 6E.7: NID BCH(63,16,11) FEC in HDL

**Branch:** fishball-p25

Phase 6E.7 is complete. The NID BCH(63,16,11) maximum-likelihood
decoder is now in PL fabric, finishing the Phase 6E LSM
synchronisation chain in hardware. The remaining 6E sub-phases are
the top-level `LsmDemod` wrapper (6E.8), the `p25_top.py` integration
(6E.9), and the Vivado bake (6E.10). See
`doc/changes/016_phase6e7_bch_fec.md` for the full design log.

### Architectural deviation from doc 015 (with user approval)

The original Phase 6E plan called for a **65,536-entry codebook in
BRAM** (~4.2 Mbit ~= 84% of Z7020 BRAM) plus a popcount tree and a
running min. That works in software (Phase 6D's `nid_fec.rs` keeps a
`[u64; 65536]` static codebook) but is unworkably tight on the Z7020
fabric -- it leaves almost no headroom for the existing C4FM chain,
the rest of the LSM front end, or future expansion.

`LsmNidBchFec` instead **computes each codeword on the fly** from a
16-bit counter and the constant 16x48-bit generator matrix:

```text
parity = XOR over { GEN[i] : data[15-i] == 1 }   # 48-bit
cw     = (data << 48) | parity                   # 64-bit
diff   = cw ^ received_nid_latched               # 64-bit
dist   = popcount(diff)                          # 7-bit
```

Per-cycle update of the running min over 65,536 sweep cycles. Same
algorithm, same correction strength (t=11, identical to a
Berlekamp-Massey decoder within the unique-decoding sphere), same
cycle budget (~656 us per decode at 100 MHz, well under the ~14 ms
NID rate). The trade is ~1% of Z7020 LUT/FF for 0 BRAM and 0 DSP.

### Modules

- **`maia-hdl/p25_hdl/lsm_nid_bch_fec.py`** -- new
  `LsmNidBchFec` Amaranth module. Inputs: `start`, `received_nid[64]`.
  Outputs: `done` (1-cycle pulse), `nac_out[12]`, `duid_out[4]`,
  `n_errors_out[7]`, `valid_out`, `busy`. State machine: IDLE -> SWEEP
  -> IDLE. Uses a 17-bit counter so bit 16 cleanly signals "swept all
  65,536 codewords". The combinational parity tree is pure LUT
  (sparse generator means each output bit is the XOR of ~8 data bits
  -- ~2 LUT levels). Includes a software `encode_nid()` reference
  used by the test bench.

### Tests

- **`maia-hdl/test/test_lsm_nid_bch_fec.py`** -- 7 new tests
  (5 default + 2 slow-mode opt-in):

  - `test_encoder_reference_matches_sdrtrunk_vector` -- locks the
    generator matrix and bit ordering against the SDRTrunk-published
    golden `encode_nid(1, 0) == 0x00103185B7E9E224`. Pure-software,
    runs in milliseconds. Catches any future drift.
  - `test_encoder_data_field_layout` -- pure-software check that the
    16 data bits land in bits 48..63 of the codeword.
  - `test_clean_codeword_and_done_strobe` -- single clean decode for
    NAC=0x8A1 / DUID=7. Captures the `done` pulse width (must be
    exactly 1 cycle) and the `busy` waveform across the full sweep
    in the same simulation to avoid spinning a second `Simulator`.
  - `test_single_bit_error_sample_positions` -- flips one bit at
    each of {0, 15, 16, 47} (data MSB, data/parity boundary, parity
    LSB+1) and verifies the decoder corrects each. Also exercises
    the start-after-done path by reusing one DUT across decodes.
  - `test_error_correction_at_t1_t6_t11` -- three corrupted
    codewords with deterministic xorshift bit patterns at the easy
    edge (t=1), mid-range (t=6), and the corner of the
    unique-decoding sphere (t=11). PRNG seed
    `0xDEAD_BEEF_CAFE_BABE` matches the Rust
    `error_correction_sweep_up_to_t11` test for cross-debugging.

  Default suite: ~7 HDL decodes, ~90 seconds total. The two
  slow-mode tests (`test_error_correction_sweep_up_to_t11` with
  55 decodes, `test_all_64_single_bit_positions` with 64 decodes)
  are gated by `@unittest.skipUnless(MAIA_HDL_SLOW_TESTS=1)` --
  ~25 min combined sim time, run before bake or on CI.

### Test results

```text
$ python -m unittest test.test_lsm_decimator test.test_lsm_fir \
    test.test_lsm_timing_interp test.test_lsm_diff_demod_slicer \
    test.test_lsm_gardner_ted test.test_lsm_pll_update \
    test.test_lsm_pll_rotate test.test_lsm_demod_loop \
    test.test_lsm_nid_bch_fec
...
Ran 41 tests in 195.545s
OK (skipped=2)
```

Up from 34/34 LSM HDL tests in Phase 6E.0-6E.6. Two skips are the
slow-mode opt-in BCH sweeps. All previous LSM tests still pass
unchanged.

### Resource estimate (Z7020)

| Resource | LsmNidBchFec |
|---|---|
| BRAM18 | **0** |
| DSP48E1 | **0** |
| LUT (parity tree + XOR + popcount + compare) | ~340 |
| FF (counter + best_dist + best_data + outputs) | ~50 |

Total `LsmNidBchFec` cost: <1% of Z7020. Combined with the Phase
6E.0-6E.6 LSM front end + demod loop (~30 DSP48 + 2 BRAM18) the
full LSM chain is comfortably under 15% of Z7020 DSP and ~1.5% of
BRAM, leaving plenty of room for the existing C4FM chain and future
work.

### Out of scope (deferred)

- AGC in HDL (still 6E.6.5)
- Top-level `LsmDemod` wrapper (6E.8)
- Wiring into `p25_top.py` (6E.9)
- Vivado bitstream bake + on-target validation (6E.10)

### Status

Phase 6E.7 ends at "all 41 LSM HDL tests pass, BCH decoder is
bit-exact with the Rust ML decoder within the unique-decoding sphere,
zero BRAM and zero DSP cost". Phase 6E.8 -- top-level `LsmDemod`
Amaranth module that assembles the front end + demod loop + BCH FEC
behind a single Elaboratable -- is up next.

---

## [2026-04-09] Phase 6D: Rust LSM Demod + NID FEC Port to p25-httpd

**Branch:** fishball-p25

Phase 6D is complete (Rust port compiles, all unit tests pass, ARM
cross-build clean; on-target validation queued behind the next Tezuka
firmware rebuild). Mechanical port of the validated Phase 6A/6B Python
prototype into the embedded p25-httpd Rust workspace, file-by-file with a
one-to-one mapping between Python stages and Rust files. The new pipeline
runs in parallel with the existing C4FM dibit reader, consuming the
Phase 6C `iq_dma` ring directly. See `doc/changes/014_phase6d_lsm_rust_port.md`
for the full write-up.

- **`p25-httpd/src/lsm/`** -- new module, ~1580 lines of Rust + tests:
  - `nid_fec.rs` (~280 lines) -- BCH(63,16,11) encoder + ML codebook
    decoder. Generator matrix copied verbatim from
    `BCH_63_16_23_P25_Test.java` (octal literals → `u64`). Codebook is
    `OnceLock<Box<[u64; 65536]>>`, built lazily on first decode call
    (~512 KB resident, <10 ms build on Cortex-A9).
  - `filters.rs` (~310 lines) -- frozen LPF (83 taps Parks-McClellan) and
    RRC (105 taps closed-form) `const [f32; N]` arrays designed at
    31.25 kSPS via the existing `tools/p25_lsm_demod.py`. Plus
    `apply_real_fir_complex` (batch), `StreamingFir` (with `(taps.len()-1)`
    history for boundary-transient-free chunking), `decimate_by_2` (batch),
    and `StreamingDecimator2` (phase-tracking across odd-length chunks).
  - `demod.rs` (~330 lines) -- verbatim port of `demod_lsm()` and the
    SDRTrunk `P25P1DemodulatorLSM.process()` it descends from. Variable
    names match the Java source. AGC + PLL + Gardner TED + slicer.
    `DemodState` is exposed so the streaming variant
    `demod_lsm_with_state` can preserve loop state across iq_dma
    sub-buffer boundaries.
  - `sync.rs` (~400 lines) -- hard + soft sync detectors. Hard is the
    sliding 48-bit Hamming-distance correlator (`SYNC_THRESHOLD = 4`).
    Soft is the port of `P25P1SoftSyncDetectorScalar` correlating
    against the 24 ideal `±3π/4` sync phases (`SYNC_SCORE_THRESHOLD =
    60.0`). Both share the same status-dibit-aware NID extractor that
    skips the 33-dibit-window position 11.
  - `ring.rs` (~110 lines) -- iq_dma sub-buffer `&[u8]` to
    `Vec<Complex32>` adapter. Decodes the FPGA's
    `{im[1], re[1], im[0], re[0]}` 64-bit word as four little-endian
    `i16` samples, normalises to ±1.0.
  - `mod.rs` (~150 lines) -- module root, local
    `Complex32 { re: f32, im: f32 }` POD type (avoids new `num-complex`
    runtime dep), `LsmPipeline` orchestrator that owns the streaming
    decimator + LPF + RRC + demod state and exposes `process_iq()` /
    `reset()`.
- **`p25-httpd/src/fpga.rs`** -- `IpCore` gains `iq_dma: RxBuffer`,
  `iq_last_addr: Option<u32>`, `set_iq_dma_enable`, `iq_last_buffer`,
  `iq_overflow`, `iq_next_address`, `read_iq_buffers`. New `DmaChannel::Iq`
  arm in `read_dma_buffers`. `InterruptHandler` gains `notify_iq_dma`,
  `waiter_iq_dma`, and the IRQ-loop iq branch (bit 2 of `interrupts`).
- **`p25-httpd/src/main.rs`** -- `mod lsm`, `ip_core.set_iq_dma_enable(true)`
  at startup, third tokio task that runs the LSM pipeline on iq_dma
  wakeups. Snapshots `read_iq_buffers()` + `iq_overflow()` under the lock,
  drops the lock before CPU work, runs the streaming pipeline, and logs
  per-IRQ NID stats (hard/soft sync counts, cumulative top-3 NACs). The
  task resets all streaming state if the gateware overflow latch fires.
  Independent of the existing dibit reader -- both pull from the same
  control DDC output via separate ring DMAs and separate PS state.
- **`tezuka_fw/board/tezuka/fishball7020/dts/fishball-p25.dtsi`** --
  third `reserved-memory` entry `p25_iq_dma: p25-iq-dma@19000000`
  (256 KB, `no-map`) and a matching `p25-iq` rxbuffer node with
  `buffer-size = <0x8000>` (32 KB sub-buffer × 8 = 256 KB ring, matches
  FPGA `iq_dma_num_buffers_log2 = 3`).
- **Verification on Windows host:** **17/17 lsm unit tests pass**
  (`cargo test lsm::`). The encoder produces SDRTrunk's golden vector
  bit-for-bit (`encode_nid(1, 0) == 0x00103185B7E9E224`). The BCH decoder
  recovers all 550 corrupted codewords across the 1..=11 error sweep
  (50 trials per error level). The streaming FIR matches the batch FIR
  to <1e-5 absolute on a 400-sample frequency-sweep input chunked at
  position 137. The streaming /2 decimator preserves the even-grid phase
  across 7+8+8 odd-length chunks. Both sync detectors find a clean sync
  in a synthetic dibit stream and the BCH decoder recovers the embedded
  NAC/DUID with zero errors. The status dibit at NID-window index 11 is
  proven to be skipped (two streams differing only in that dibit produce
  identical extracted NIDs).
- **ARM cross-build clean:** `cargo check --target armv7-unknown-linux-gnueabihf`
  produces 27 warnings (all dead-code on existing modules) and zero
  errors. No new runtime dependencies (`pm-remez` and `num-complex` not
  pulled in).
- **Pending verification:** Tezuka firmware rebuild (`build.bat --p25`
  inside the Tezuka Docker container) to consume the Phase 6C XSA + the
  new lsm module, SD-card flash, on-target smoke test (LSM reader task
  starts, iq_dma wakeups arrive at ~7.6 Hz, NAC=0x8A1 dominates the
  cumulative histogram at a per-second rate comparable to SDRTrunk on
  the same antenna).

Phase 6D ends at "Rust port compiles, all unit tests pass, ARM
cross-build clean". Phase 6E -- HDL port of the streaming filters and
the demod loop into Amaranth, replacing the C4FM-only path -- is the
next step on the ladder.

---

## [2026-04-09] Phase 6C: P25 Post-DDC IQ Ring DMA in FPGA Gateware

**Branch:** fishball-p25

Phase 6C is complete (gateware logic + Verilog/SVD/PAC regen). Adds a third
ring DMA inside the P25 IP core that streams the control DDC's post-decimation
IQ output (62.5 kSPS, 16-bit signed I/Q, two samples per 64-bit word) to a
reserved DDR carve-out at `0x1900_0000`. This is the bridge that lets Phase
6D's Rust port of the validated Python LSM demod consume live antenna data
without disturbing the existing dibit pipeline. See
`doc/changes/013_phase6c_iq_dma.md` for the full write-up and
`doc/P25_ADDRESS_MAP.md` for the canonical address-space tables.

- **`maia-hdl/p25_hdl/iq_packer.py`** -- new ~120-line `IQPacker` Amaranth
  module. Buffers two consecutive `(re, im)` pairs into a 64-bit AXI4-Stream
  word `{im[1], re[1], im[0], re[0]}` (sample 0 in low half). Mirrors
  `DibitPacker`'s handshake + sticky-overflow conventions exactly.
- **`maia-hdl/p25_hdl/p25_top.py`** -- third tap of the control DDC output
  (alongside `c4fm_demod` and `symbol_timing`); new `iq_dma` instance of
  `DmaStreamRingWrite` exposing `m_axi_iq`; new 5th register bank `iq` at
  byte offset `0x80` containing `iq_dma_status`/`iq_dma_control`/
  `iq_next_address`. The bank decoder was widened from `address[3:5]`
  (4 banks) to `address[3:6]` (8 banks max) -- no `axi4_awidth` change
  needed, the existing 7-bit word address has plenty of headroom.
- **`maia-hdl/p25_hdl/config.py`** -- new `iq_dma_*` fields with the full
  bandwidth math (250 KB/s, 256 KB ring = 8 x 32 KB sub-buffers, ~128 ms
  per sub-buffer interrupt, ~1 s of IQ in flight) and an alignment assert
  in `validate()`.
- **`maia-hdl/ip/p25-core/package_ip.tcl`** -- one new
  `ipx::associate_bus_interfaces -busif m_axi_iq -clock clk` line.
- **`maia-hdl/projects/fishball7020_p25/system_bd.tcl`** -- one new
  `ad_mem_hp1_interconnect` line. SmartConnect on HP1 now arbitrates
  three masters (`m_axi_dibit` + `m_axi_traffic` + `m_axi_iq`); HP1 budget
  at ~1.7 GB/s absorbs the new ~250 KB/s consumer with ~0.015% utilisation.
- **`maia-hdl/test/test_iq_packer.py`** -- new pure-Python pysim test (uses
  `amaranth.sim.Simulator`, mirrors `test_dibit_packer.py`). 7 tests
  covering single/multi-pair packing, two's complement extremes, no-strobe
  quiescence, backpressure handshake, and the sticky overflow flag. All 7
  pass on the Windows host.
- **`maia-hdl/test_cocotb/iq_packer/`** -- new cocotb scaffold (Makefile,
  verilog.py, tb.v, test_iq_packer.py) ready to run in WSL Ubuntu or
  Docker as a CI step.
- **`doc/P25_ADDRESS_MAP.md`** -- new canonical address-map document.
  Single source of truth for DDR carve-outs, AXI-Lite register banks, and
  IRQ assignments. Per the doc-as-we-go discipline, written *before* the
  wiring code so the address-map decisions were committed in writing
  before they hardened.
- **Verification on Windows host:** 7/7 pysim tests pass. Full P25Core
  Amaranth elaboration succeeds. `build_hdl.sh --verilog-only --p25` (run
  in the project's Python 3.11 Docker container) regenerates `p25_core.v`
  (22046 lines, all 20 `m_axi_iq_*` ports declared), `p25.svd` (with the
  three new registers at `0x80`/`0x84`/`0x88`), and `p25-pac/src/lib.rs`
  via `svd2rust v0.33.5` cleanly. Phase 6D's Rust port can `use p25_pac::iq`
  immediately.
- **Pending verification:** Vivado bitstream synth (`build_fpga.bat --p25`,
  ~30-60 min on host Vivado 2023.2) and on-hardware smoke test (load
  bitstream, devmem to enable `iq_dma_control`, mmap `0x1900_0000`, dump
  sub-buffers, feed to `tools/p25_lsm_demod.py`, check NAC accuracy).

Phase 6C ends at "gateware logic verified, Verilog regenerates, register
PAC regenerates". Phase 6D -- Rust port of the LSM demod to the Cortex-A9,
fed by this new IQ ring -- is the next step on the ladder.

---

## [2026-04-09] Phase 6B: P25 NID BCH(63,16,11) FEC Validated Against SDRTrunk

**Branch:** fishball-p25

Phase 6B is complete. NID forward error correction is now in the validated
Python reference, and on the better-signal test recording our prototype
produces an exact sync count match to SDRTrunk (313/313) with 100% NAC
accuracy after FEC. See `doc/changes/012_p25_nid_bch_fec.md` for the full
write-up.

- **`tools/p25_nid_fec.py`** -- new ~280-line standalone module. Encoder
  is verbatim from SDRTrunk's `BCH_63_16_23_P25_Test.java` (16-row
  generator matrix in octal + 5-line systematic encoding loop). Decoder
  uses maximum-likelihood nearest-neighbour search across the 65,536-entry
  codebook -- mathematically identical to BCH decoding within the unique-
  decoding sphere, ~30 lines vs ~600 for a Berlekamp-Massey + Chien search
  port from the Linux-derived `BCH.java` base class.
- **Encoder bit-perfect**: `encode_nid(NAC=1, DUID=0)` produces
  `0x00103185B7E9E224` exactly, matching SDRTrunk's documented test vector
  in `BCH_63_16_23_P25_Test.java:38`.
- **Decoder bit-perfect**: synthetic 1-11 bit error injection at all
  positions, 100 trials each: 1100/1100 corrected. At 12 errors,
  200/200 declared uncorrectable -- exactly the (63,16,d=23) bound.
- **`tools/p25_lsm_demod.py`** -- BCH FEC integrated into both sync
  detector paths. `SyncEvent` now carries `nid_raw`, `nac_fec`, `duid_fec`,
  `fec_errors` (-1 if uncorrectable). New "after BCH(63,16,11) FEC" section
  in the report shows correctable count, bit-error histogram, and the
  "NAC among correctable" metric (the right way to measure FEC quality
  while ignoring false sync hits).
- **End-to-end validation**:
  - 175119 wav (better signal): 313/313 syncs, 313/313 correctable,
    313/313 NAC=0x8A1 after FEC. **Exact match to SDRTrunk truth log.**
  - 163748 wav (noisier): 339/335 syncs, 330/339 correctable, 330/330
    NAC=0x8A1 *among correctable*. The 9 uncorrectable events are
    dominated by 4 false sync hits beyond truth + 5 PLL-slip events;
    not a FEC bug.
- **Pyradio evaluation**: started this session by reading the user's
  pre-existing pyradio P25 port at `~/Downloads/sdrtrunk-master/docs/pyradio/`
  to assess whether to merge it. Two findings: (1) pyradio's LSM demod
  loop is algorithmically equivalent to ours but uses `MAX_PLL = π`
  instead of SDRTrunk's `π/3` (deviation comment cites Pluto crystal
  offset); ours is more faithful. (2) **pyradio's `decode_p25_nid`
  uses RS(24,12,13) over GF(2^6), NOT the BCH(63,16,11) that SDRTrunk
  actually implements**, has zero unit tests for the FEC, and the
  pyradio author's own skeleton at `p25_pure_python.py` mislabels which
  code goes where. Per the project mandate ("if pyradio diverges from
  SDRTrunk, prefer SDRTrunk"), this session ports BCH directly from
  SDRTrunk Java rather than trusting pyradio's RS substitute.
- **TSBK parser deferred**: cherry-picking pyradio's TSBK parser would
  require ~1000 lines of trellis decoder + deinterleaver + CRC + opcode
  dispatch + an event/identifier framework. Significantly bigger lift
  than the FEC was. Recommend a dedicated future session.

## [2026-04-09] Phase 6: P25 LSM Demodulator -- Validated Python Reference

**Branch:** fishball-p25

The Fishball P25 target site is **LSM Simulcast**, not C4FM. All P25 systems
within RF range of the user's location are LSM. The existing C4FM-only Phase 1
gateware cannot decode LSM regardless of how we tune the existing slicer --
LSM is pulse-shaped CQPSK with data in carrier *phase*, requiring an RRC
matched filter and a decision-directed PLL that the current architecture
lacks. See `doc/changes/011_p25_lsm_python_reference.md` for the full
diagnosis and the validated Python reference port.

This change establishes the project's new direction:

- **Read SDRTrunk's LSM source line by line** (`P25P1DecoderLSM.java`,
  `P25P1DemodulatorLSM.java`, `Dibit.java`, `P25P1MessageFramer.java`,
  `P25P1SoftSyncDetector.java`, `BCH_63_16_23_P25.java`) and document
  every constant and DSP block.
- **`tools/p25_lsm_demod.py`** -- new self-contained ~900-line Python port
  of the full LSM chain: half-band decimation -> Parks-McClellan baseband
  LPF -> RRC matched filter -> demod loop with AGC + PLL + Gardner TED +
  atan2 slicer -> hard + soft sync detectors -> status-aware NID extractor
  (skip dibit 11). Variable names mirror the Java source for direct diff.
  Includes a 6-panel Matplotlib `--plot` dashboard for visual diagnosis.
- **Validated bit-exact against SDRTrunk** on a 27-second .wav recording
  the user captured via their Pluto + SDRTrunk + custom pluto_server.py
  bridge: 339 sync events vs 335 in SDRTrunk's truth log (101.2% recall),
  91% at perfect Hamming distance 0, NAC = 0x8A1 in 93.5% of detections,
  DUID = 0x7 in 95.9% of detections. Symbol rate within 0.006% of nominal.
- **`tools/monitor_p25_decoder.py`** -- new tool for live polling of the
  on-target Fishball `/api/stats` and `/api/dibit_dump` endpoints into
  JSONL snapshots. Used during the failed slow-convergence-tracking
  detour but kept as the standard "watch the decoder over time" utility.
- **`maia-hdl/p25_hdl/p25_top.py`** -- corrected the docstring's wrong
  claim that the existing slicer is "unified for C4FM and LSM". It is
  not. Documented the architectural delta (RRC + PLL missing) and the
  recommended fix path (PS Rust -> HDL).
- **DEVPLAN.md** -- new Phase 6 with a 5-step ladder (6A Python reference
  done, 6B BCH FEC, 6C IQ DMA in FPGA, 6D Rust on PS, 6E HDL/PL final).
  Each step locks in a fixed reference for the next, so each one has at
  most one degree of freedom and a known-good target.

The validated Python reference unblocks the rest of the project. Phase 6B
through 6E now become mechanical port-and-test exercises with bit-exact
targets, instead of "design and pray."

## [2026-04-09] Phase 5 (final): P25 Decoder Observability + Init Hardening

**Branch:** fishball-p25

A bundle of small but high-leverage diagnostic-infrastructure changes that
turned the P25 decoder from a black box into something we could actually
debug from a browser. See `doc/changes/010_p25_decoder_observability.md`
for the per-item rationale.

The headline finding: `S60p25-httpd` was launching the daemon via
`start-stop-daemon -b`, which detaches the process and closes its standard
streams under busybox. **Every `tracing::info!` we have ever written has
been going straight to /dev/null.** Fixed by wrapping the binary in
`sh -c 'exec ... >> /var/log/p25-httpd.log 2>&1'`. The same bug exists in
`S60maia-httpd` (Maia's init script) and is worth fixing in `fishball-dev`
in a follow-up.

Other items in this bundle:

- **Explicit `EnvFilter` setup** in `main.rs` so the default tracing
  filter is `info,p25_httpd=info` and not whatever `fmt::init()`'s
  undocumented default is.
- **`/api/stats` exposes AD9361 RX gain + RSSI.** No more "ssh in and
  cat sysfs" while debugging.
- **`/api/dibit_dump` exposes inner/outer histogram percentages and a
  raw on-air DUID histogram.** The DUID histogram in particular was
  the diagnostic that broke open the LSM debug session -- it showed a
  near-uniform spread across all 16 nibble values (TSDU at 4-5%
  instead of expected ~100%), immediately implicating the demodulator
  architecture.
- **`SYNC_THRESHOLD` widened from 4 to 10** with a long comment
  explaining why and when it should drop back. Temporary diagnostic
  measure -- not a fix.
- **Periodic `expire_grants(30)`** task in `main.rs` so the dashboard's
  Active Grants count doesn't grow forever once decoding works.
- **NID DUID hardcode hack** in `decode_nid` to flush out downstream
  bugs faster while the demod is still broken. Diagnostic raw_duid
  histogram preserves the actual on-air values so we can observe the
  bit-error pattern. Replaced by proper BCH(64,16) FEC in change 011's
  follow-up work (Phase 6B).
- **Category-1 cleanups**: drop unused imports (`put`, `TsbkMessage`),
  module-level `#![allow(dead_code)]` for traffic-following placeholder
  code, remove dead `tracing::debug!` in `fpga.rs`, fold the NCO
  frequency into the existing `configure_ddc` info log.

## [2026-04-09] Phase 5 (cont.): Build Verilog Staleness Detection

**Branch:** fishball-p25

Added automatic staleness detection for Amaranth-generated IP Verilog in
`build_fpga.bat`. Previously the script only checked whether `p25_core.v` /
`maia_sdr.v` existed, so any edit to `p25_hdl/*.py` or `maia_hdl/*.py` after
the first build would silently bake pre-edit logic into new bitstreams that
looked fresh by mtime. See `doc/changes/009_build_verilog_staleness.md`.

This was discovered while debugging why the P25 dibit slicer fix (symbol-rate
differential, commits 3f1bff7 and 94faae9) wasn't affecting the on-target
FPGA behaviour. The bitstream was built 15 minutes *after* the fix was
committed, but Vivado had picked up the old `p25_core.v` generated 90 minutes
*before* the fix. The symbol-rate slicer logic never made it into the
bitstream, and the on-target dibit histogram exactly matched the pre-fix
failure mode fingerprint recorded in the `p25_top.py` docstring.

- **New:** `tools/check_verilog_stale.ps1` -- PowerShell helper that compares
  generated `.v` mtime against the maximum mtime of `*.py` files under one or
  more source directories. Outputs `MISSING`, `STALE`, or `FRESH`.
- **Changed:** `build_fpga.bat` Step 2 now calls this helper for both the
  Maia SDR and P25 IP Verilog. P25 checks against both `p25_hdl` and
  `maia_hdl` (since `p25_top.py` imports DDC, registers, DMA, and CDC from
  `maia_hdl`). On `STALE` or `MISSING`, it automatically invokes
  `build_hdl.bat --verilog-only [--p25]` in Docker before running Vivado.
- **No API change:** users still run `build_fpga.bat --p25` as the single
  command. The `--verilog-only` flag on `build_hdl.bat` remains as an
  internal mechanism and is no longer user-facing.

---

## [2026-04-09] Phase 5: Build Pipeline, Register CDC, Ring DMA, First Hardware Boot

**Branch:** fishball-p25

First successful hardware boot of the P25 FPGA bitstream on the Fishball Z7020,
followed by a same-day refactor of the DMA path from one-shot to a ring buffer
architecture. Also fixed multiple build pipeline issues from the standalone
repo migration, corrected the SVD register map, added DDC FIR coefficient
initialization, and fixed a register clock-domain-crossing bug.

### Build Pipeline Fixes

- Fixed P25 Verilog generation to use Docker (ext4 filesystem avoids NTFS pip issues)
- Fixed CMD escaping in `build_fpga.bat` for Docker invocation
- Added missing ADI library builds: `util_clkdiv`, `util_rfifo`, `util_wfifo`
- Fixed stale TCL/Makefile paths left over from standalone repo migration
- Added `.gitattributes` enforcing LF line endings on `.sh` files
- Added skip-if-built logic for incremental ADI library builds (and ADI library packages)
- `build_hdl.sh` now auto-generates `p25.svd` when `--p25` is set and regenerates `p25-pac/src/lib.rs` via a downloaded `svd2rust` binary, so SVD/PAC stay in sync with Amaranth HDL without manual steps

### Tezuka Firmware Fixes

- Added XSA cache invalidation in `build.sh` (detects when source XSA is newer than cached package and forces a package rebuild)
- Added p25-httpd / maia-httpd source change detection for auto-rebuild
- Removed phantom AXI UART Lite from `fishball-p25.dtsi` (was at 0x42C00000, inherited from the Maia DTSI but not present in the P25 FPGA design -- caused a kernel panic in `uartlite_probe`)

### Register Map Fix

- SVD register offsets were wrong: FPGA uses bank select bits [4:3] of word address giving byte offsets 0x00/0x20/0x40/0x60, but SVD had 0x00/0x08/0x20/0x30
- Fixed `RegisterMap` in `p25_top.py`, regenerated SVD and PAC

### Register Clock-Domain-Crossing Fix

- `demod_registers` and `traffic_registers` were instantiated in the `s_axi_lite` clock domain (100 MHz) but their Wpulse outputs fed modules running in the `sync` domain (62.5 MHz)
- The unsynchronized Wpulse edges were missed ~38% of the time due to clock skew, causing the DMA start signal to be silently dropped
- Wrapped both with `RegisterCDC`, matching the existing `sdr_registers_cdc` pattern already used for the DDC bank

### DDC FIR Coefficient Initialization

- Added 3-stage FIR filter coefficient loading to p25-httpd (`fpga.rs`, `configure_ddc()`)
- Previously only NCO frequency and enable bits were programmed
- P25 channel filter: 128x decimation (16x4x2), 8 MSPS -> 62.5 kSPS (13 samples/symbol)
  - Stage 1: 48 taps, 200 kHz cutoff, /16
  - Stage 2: 32 taps, 50 kHz cutoff, /4
  - Stage 3: 64 taps, 8 kHz cutoff, /2
- Coefficients: Kaiser window (designed via `scipy.signal.firwin`), 18-bit quantized, >137 dB stopband

### Ring-Buffer DMA Rework

- Replaced one-shot `DmaStreamWrite` with a new `DmaStreamRingWrite` module (ported from the never-merged upstream IQ stream branch)
- 8 sub-buffers x 4 KB = 32 KB ring per DMA channel
- Sub-buffer completion raises an interrupt on the AXI B-channel write response
- New `demod_status.last_buffer` field (3 bits) tracks which sub-buffer was just written
- Continuous operation -- no PS-side restart per buffer
- `demod_control` / `traffic_demod_control` simplified: start/stop Wpulses removed, only `demod_enable` (RW level) remains
- `demod_status` / `traffic_demod_status` grow the `last_buffer` field; all other bank offsets unchanged (`ddc_frequency`=0x2C, `ddc_control`=0x30, `demod_status`=0x40, etc.)
- SVD and PAC regenerated

### DMA Address Layout

- FPGA hardcodes physical buffer addresses `0x17000000` (dibit) and `0x18000000` (traffic)
- Reduced from 1 MB each to 32 KB each to match the new ring size
- Device tree updated: `reg = <0x17000000 0x8000>` and `<0x18000000 0x8000>`

### Hardware Test Results

- Board booted with P25 bitstream, FPGA registers accessible
- Product ID: 0x70323566 ("p25f")
- AD9361 configured: 858.1 MHz center / 8 MSPS
- Dibit counter incrementing (DSP chain active)
- Web UI served on port 8080
- SDRTrunk confirmed P25 signal at 860.9625 MHz (NAC:2209, WACN:781824, System:2208)

### Unified C4FM/LSM Differential Demod

- Local control channel turned out to be LSM (CQPSK), not C4FM. The original FM-only cross-product discriminator could not produce LSM dibits 1 and 3
- Refactored `C4FMDemod` to compute the full complex differential product `z[n] * conj(z[n-1])` (4 DSP48E1 multiplies instead of 2). Outputs `diff_re_out` and `diff_im_out`; `disc_out` retained as a legacy alias for `diff_im_out`
- Refactored `SymbolTimingRecovery` to take both `diff_re_in` and `diff_im_in` and use a sign-bit slicer that maps the four quadrants of the differential plane to the four P25 dibit values, matching SDRTrunk's `P25P1DemodulatorLSM.toDibit` exactly. Works for both C4FM and LSM with no path divergence
- `p25_top.py` wires the new IQ pair through both control and traffic chains
- Updated `test_c4fm_demod.py` and `test_symbol_timing.py` to drive and verify the new I/Q differential interface; all 16 P25 HDL tests pass

### p25-httpd Diagnostic Logging

Added structured tracing and a `/api/dibit_dump` endpoint to make hardware bring-up debuggable from the web UI without devmem on the target.

- `target=p25_irq` -- IRQ arrival counter (first 10, then every 64th)
- `target=p25_reader` -- per-wakeup buffer count, byte total, dibit histogram
- `target=p25_stats` -- FPGA register state (`dibit_count`, `overflow`, `last_buffer`, `next_addr`) every 2 s
- `target=p25_decoder` -- periodic dibit histogram + sync correlator stats, near-sync events (Hamming distance ≤ 12), state transitions, NID Golay decode results
- `GET /api/dibit_dump` -- JSON: `total_dibits`, per-value histogram with percentages, sync correlator stats (`hits`, `near_misses`, `best_distance`), and the last 2048 dibits packed as hex
- Dashboard: two new cards ("Dibit Histogram", "Sync Correlator") that poll `/api/dibit_dump` and surface the histogram + best Hamming distance live

---

## [2026-04-08] P25 Migration into maia-sdr

**Branch:** fishball-p25

Migrated the P25 trunking radio from the standalone `fishball-p25` repo into
the maia-sdr tree. The standalone approach failed due to relative path breakage,
IIO DMA not routed, and DTS/bitstream mismatches.

### P25 FPGA Gateware (Phases 0-1)

- `maia-hdl/p25_hdl/` -- Amaranth HDL modules: C4FM demod, Gardner symbol timing, dibit packer
- `maia-hdl/ip/p25-core/` -- Vivado IP packaging (TCL + constraints)
- `maia-hdl/projects/fishball7020_p25/` -- Vivado project (block design, constraints, top-level wrapper)
- `maia-hdl/test/` -- 14 Amaranth simulation tests (C4FM, symbol timing, dibit packer)
- `maia-hdl/generate_p25_svd.py` -- SVD generation for P25 register PAC
- Dual DDC + demod chains: control channel + traffic channel with independent NCO
- Spectrometer + recorder removed (unused in P25), freeing ~16 DSP48, ~5K LUT, ~12 BRAM
- Resource usage: 36 DSP48 (16%), 15.6K LUT (29%), 16 BRAM (11%) on Z7020

### P25 Firmware (Phases 2-3)

- `p25-httpd/` -- Rust workspace with `p25-json` and `p25-pac` sub-crates
- Control channel decoder: frame sync, NID extraction, TSDU de-interleave, TSBK parser
- FEC: Golay(23,12), trellis Viterbi, CRC-16-CCITT
- 6 TSBK opcodes: GRP_V_CH_GRANT, GRP_V_CH_GRANT_UPDT, IDEN_UP, NET_STS_BCST, RFSS_STS_BCST, ADJ_STS_BCST
- Traffic manager: grant lifecycle, NCO word calculation, timeout management
- Web dashboard: REST API (6 endpoints), WebSocket events, embedded SPA
- 16 Rust unit tests passing

### Build Infrastructure

- `build_fpga.bat --p25` builds P25 FPGA (fishball7020_p25)
- P25 Tezuka config: `fishball_p25_7020_defconfig`

See `doc/changes/003_p25_scaffolding.md` through `doc/changes/007_p25_traffic_channel.md` for details.

---

## [2026-04-07] Upstream Sync

**Branch:** fishball-dev

Rebased `fishball-dev` onto `upstream/main`, picking up 10 new commits from
F5OEO: Vivado 2023 env, WASM broadcast channel, interpolation coefficients,
overclock simplification, FIFO redesign, clock simplification, x32 interpolator,
RX2 fix, DAC FIR support, RX with FIFO.

Also analyzed the upstream `refactor` branch (53 commits, 18K lines, not yet
merged to main) -- includes board-based project layout, common TCL library,
DVB-S2 receiver, IQ burst capture, frequency sweeper, multi-board sync.

See `doc/changes/002_upstream_sync.md` for full analysis.

---

## [2026-03-08] Project Setup

**Branch:** fishball-dev

### Fork & Repository

- Forked `F5OEO/maia-sdr` -> `andylee77/maia-sdr`
- Created `fishball-dev` branch for Fishball Z7020 development
- Set up upstream tracking: `upstream` -> `F5OEO/maia-sdr`, `origin` -> `andylee77/maia-sdr`

### Build Infrastructure (Initial)

- In-repo build scripts (see `doc/changes/001_build_scripts.md`):
  - `build_hdl.bat/.sh` -- Amaranth -> Verilog + SVD via Docker
  - `build_fpga.bat` -- Full Vivado FPGA synthesis (Windows native)
  - `sim_hdl.bat/.sh` -- HDL simulation via Docker (Amaranth + cocotb)
  - `clean.bat` -- Clean all build artifacts
- Docker volume `maia-hdl-build` for persistent Python venv cache
- Tezuka firmware pointed to this fork's `fishball-dev` branch

---

## Pending / Future

- [ ] Phase 5 (remaining): Live control channel decode, traffic following test, SD card clean boot
- [ ] Voice frame extraction (LDU1/LDU2 -> IMBE frames)
- [ ] Audio codec (mbelib, codec2, or DVSI)
- [ ] Document IQ streaming patches to maia-httpd
- [ ] FPGA ring buffer implementation for high-bandwidth IQ
