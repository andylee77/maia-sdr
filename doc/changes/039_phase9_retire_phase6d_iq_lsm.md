# Phase 9 — Retire the Phase 6D software LSM pipeline

Date: 2026-04-15

## Summary

Remove the PS-side code that duplicates functionality the PL (FPGA
gateware) is already providing, and refocus the dashboard on the PL
LSM chain as the single source of truth. The primary target is the
Phase 6D `iq_lsm_decoder` + `LsmStats` + iq_dma-fed software LSM
pipeline, which has been dead weight since Phase 6E.x ported the
full LSM demod into Amaranth. `iq_dma` HDL ring is left in the
bitstream as dormant dead code for future re-use, but disabled at
boot so no data flows into the retired PS code.

## Motivation

At the start of Phase 9 the p25-httpd daemon was running THREE
overlapping control-channel decoder pipelines on the ARM:

1. `decoder` — PS C4FM framer, fed by HDL `c4fm_dibit_dma`. Still
   present, still used on C4FM sites. Kept.
2. `lsm_decoder` — PS LSM framer, fed by HDL `lsm_dibit_dma`.
   Production control-channel decoder on LSM sites. Kept.
3. `iq_lsm_decoder` — PS "Phase 6D" pipeline: reads raw IQ from the
   `iq_dma` ring, runs a complete pure-Rust LSM demod (decimate /2 →
   LPF → RRC → AGC+PLL+Gardner+slicer → hard+soft sync detect →
   BCH(63,16,11) FEC), dispatches soft-decision TSDUs via
   `process_directed_tsdu`. **Retired.**

The third pipeline was built in Phase 6D as the *algorithmic
development and validation reference*, BEFORE any HDL existed. The
user iterated the algorithm offline against SDRTrunk captures until
the Rust reference matched ground truth, and then Phase 6E ported
each submodule into Amaranth one at a time, with the `golden_dump`
fixture emitter tapping the Rust pipeline to produce per-stage
fixtures for the HDL test bench.

Since Phase 6E.9 (HDL LSM chain on the control DDC) went green in
production, the PL path has been carrying 100 % of the
control-channel traffic and the Phase 6D software pipeline has been
pure CPU overhead: it runs the full software demod on every
iq_dma sub-buffer interrupt, locks a shared `ControlChannelDecoder`
mutex every dispatch, and its only visible output is a column in
`/api/decoder_compare` that exactly mirrors what `lsm_decoder` (the
HDL-fed framer) already reports.

The tipping point for formal retirement was the Phase 8 on-target
verification session (2026-04-15): while debugging the Active
Grants panel stuck-age bug I noticed `iq_lsm_decoder` has NO
`expire_grants` loop, so grants from the Phase 6D path were
accumulating forever in the union that feeds `/api/grants`. The
symptom was Active Grants entries showing 186 s / 368 s ages even
though the control channel was refreshing them every 2-3 s and the
`lsm_decoder` side had long since pruned them via its 5 s expiry
task. The fix options were "add an expire loop to iq_lsm_decoder"
or "retire iq_lsm_decoder entirely". Given Phase 6D has been
architecturally obsolete for months, retirement was the right call.

## What was removed

### `p25-httpd/src/main.rs`

1. **`iq_lsm_decoder` Arc construction**
   (was at ~line 472; now a retirement comment referencing this
   change doc).
2. **`lsm_stats` Arc construction** (`LsmStats` shared mutex for
   the Phase 6D runtime stats).
3. **The entire Phase 6D LSM IQ reader tokio task**
   (was 200 lines spanning `spawn(async move {` through the
   terminal `});`, plus the cross-batch carry-over / pending-event
   queue, the `iq_waiter.wait().await` loop, the overflow warning
   branch, and all `lsm_stats_task.lock().await.record_batch(...)`
   / `dec.process_directed_tsdu(...)` dispatches). Replaced with a
   retirement breadcrumb comment.
4. **`iq_waiter = interrupt_handler.waiter_iq_dma()` acquisition**
   — nothing consumes the iq_dma IRQ any more.
5. **`ip_core.set_iq_dma_enable(true)`** → **`false`**. The HDL
   `iq_dma` master still exists in the bitstream but is held
   disabled at boot so it doesn't stream data into DDR for no
   consumer.
6. **`AppState { iq_lsm_decoder, lsm_stats, ... }`** fields dropped.
7. **Build tag bumped** →
   `2026-04-15-phase9-retire-phase6d-iq-lsm`.

### `p25-httpd/src/httpd/mod.rs`

1. **`AppState::iq_lsm_decoder` + `AppState::lsm_stats`** fields
   removed; replaced with a retirement breadcrumb comment.
2. **`/api/lsm` route + `get_lsm` handler** removed. The dashboard
   card that used to read it is also gone (see below).
3. **`get_system_info`** simplified from a `pick()` over
   `lsm_decoder + iq_lsm_decoder` to a single-decoder read on
   `lsm_decoder`.
4. **`get_grants`** simplified from a two-pass union to a single
   decoder pass. This is the change that fixed the Active Grants
   stale-age bug.
5. **`get_bands`** simplified from a union to a single decoder
   pass.
6. **`get_decoder_reset` / `post_decoder_reset`** — the
   `iq_lsm_decoder.reset_diagnostics()` call is gone.
7. **`get_decoder_compare`** — dropped the `ps_iq_lsm` and
   `ps_phase6d` JSON sections. Response is now a **3-column
   matrix**: `ps_c4fm` + `ps_lsm` + `pl_hdl`.

### Dashboard HTML + JS (still in `httpd/mod.rs`)

1. **"LSM Decoder (Phase 6D)" card** (7 rows of wakeups / IQ samples
   / dibits / hard+soft syncs / overflow / last sync) + the
   adjacent **"Top NACs (LSM)" card** (histogram of top 10 NACs
   from Phase 6D soft+hard sync events) — both removed. The PL
   HDL LSM chain detail card above already has the equivalent
   data (`/api/hdl_lsm`) — single source of truth, no drift.
2. **"Dibit Stream Diagnostics (PS C4FM vs PS LSM, side by side)"**
   header relabeled to **"Dibit Stream Diagnostics (PS C4FM
   fallback vs PL HDL LSM)"** — same two cards, more honest
   labels.
3. **Decoder Comparison Matrix** JS — the row array simplified
   from 5 columns to 3 (dropped `ps_phase6d.*` and `ps_iq_lsm.*`).
   PL-HDL-derivable values added for:
   - **Sync hits** = `pl_hdl.total_nids` (every HDL hard-sync hit
     triggers a NID BCH sweep 1:1).
   - **NID attempts** = `pl_hdl.total_nids` (same).
   - **NID BCH decode failures** = `total - valid` (BCH
     distance > 11 bit errors, outside the correction sphere).
   - **NID decoded OK (any DUID)** = `pl_hdl.valid_nids`.
   - **Total dibits processed** = `ps_lsm.total_dibits` (the PS
     framer is a pure pass-through counter on PL-emitted dibits,
     so the number IS the PL HDL dibit output count — labeled
     "(= PS)" in the PL column).
   Rows that are legitimately PS-only (Active grants, Frequency
   bands, Messages decoded, TSDU/TSBK-level counters) are
   labeled "(PS only)" or "(PS framer)" in the PL column so
   the blank cells tell a story instead of looking broken.
4. **`/api/lsm` fetch + populate-`lsm_status`-card JS block**
   removed.
5. **Table header** simplified from 5 columns to 4
   (Metric + PS C4FM + PS LSM + PL HDL). Colspan of the loading
   placeholder adjusted from 5 to 4.

### `doc/P25_API.md`

1. **Endpoint catalogue**: `/api/lsm` row removed; numbering
   renumbered. A "Phase 9 retirement" callout added to the top
   of the catalogue pointing at this change doc.
2. **`/api/grants` dedup rules**: removed the "union with
   `iq_lsm_decoder.grants`" paragraph; added a one-liner
   acknowledging the single-decoder read and the stale-age bug
   fix.
3. **`/api/decoder_compare` response** section: JSON example
   trimmed to the 3 surviving pipelines. A Phase 9 retirement
   callout explains what was dropped, and a new "PL-side aliases"
   bullet list documents the `sync_hits` / `nid_attempts` /
   `nid_decoded_ok` / `nid_bch_failures` mappings.
4. **`/api/decoder_reset` response** shape updated to the new
   single-decoder message.
5. **Dashboard panels table**: `LSM Decoder (Phase 6D)` +
   `Top NACs (LSM)` rows removed. `Decoder Comparison Matrix`
   row updated to note the 3-column view. C4FM dibit stream
   entry labelled "dormant on LSM sites".

### `tools/p25_check.py`

1. **Module docstring**: endpoint list updated and a Phase 9
   retirement note added.
2. **`/api/decoder_compare → ps_iq_lsm` section** replaced with
   a **`/api/decoder_compare → pl_hdl` section** that prints the
   winner NAC, total NIDs, valid NIDs (with valid %), BCH
   failures, drop count, live PLL / sample_point / sync_distance,
   and overflow ticks. Turns the tool from "is the software
   cross-check healthy?" into "is the PL chain healthy?".
3. **`/api/lsm` section** removed; replaced with a breadcrumb
   comment pointing at the `/api/hdl_lsm` section.

### `tools/p25_status_and_next_step.py`

1. **Endpoint list**: `/api/lsm` removed.
2. **`render_decoder_compare`** dropped the `ps_iq_lsm` row and
   widened the `pl_hdl` footer to also show live PLL / sample
   point / sync distance alongside the existing valid-NID ratio.

### `p25-httpd/src/lsm/` (kept in-tree, dead-coded)

- Module-level `#![allow(dead_code)]` added to `lsm/mod.rs` with
  an extended docstring explaining the retirement and the three
  reasons the module stays in-tree:
  1. `nid_fec::T_MAX_ERRORS` + `nid_fec::encode_nid` are still
     referenced by `/api/bch_t` and the HDL test bench as a
     software reference encoder.
  2. The full `LsmPipeline` serves as a human-readable algorithmic
     reference for the HDL port and can be revived with a single
     `set_iq_dma_enable(true)` + reinstated tokio task.
  3. The `golden_dump` fixture emitter still drives off this
     pipeline and feeds `maia-hdl/test/golden_vectors/`.

## What was NOT changed (deliberate non-goals)

- **HDL `iq_dma` ring + `iq_packer` + `iq_registers`**: still in
  the bitstream. Costs a few hundred LUT and one M_AXI_HP channel
  that are now unused; Phase 9 keeps them as dormant dead code
  for cheap future re-enablement. Retirement of the HDL block
  itself is a separate decision for a future Vivado bake.
- **PS C4FM `decoder`** (`state.decoder`): kept. Still consumes
  `c4fm_dibit_dma`, still appears in `/api/decoder_compare`,
  still labelled "dormant on LSM sites". Not deleted because the
  eventual goal is to support BOTH LSM and C4FM sites, and the
  C4FM framer is the only way to decode TSBKs from a C4FM
  control channel. Retirement would be a larger architectural
  decision than Phase 9's scope.
- **C4FM traffic dibit reader task** in `main.rs` (~line 1762):
  kept. Still runs a dibit histogram + `mgr.note_activity()`
  liveness pet on the traffic C4FM chain. Retirement candidate,
  but the `note_activity` signal is currently how the
  TrafficManager's 3 s inactivity timer is petted — re-routing
  that to an LSM-side signal is Phase 10 work.
- **libiio / ADI IIO DMA chain**: untouched. Orthogonal to
  iq_dma; kept on the bitstream per the
  `feedback_keep_libiio_path` memory from 2026-04-09 so the
  same boot does both Fishball-native decode AND baseband
  capture to disk.

## Verification

### Build

- `cargo check --workspace`: 0 errors. Warning count went from
  132 (pre-retirement) to 75 (post-retirement) — the
  `#![allow(dead_code)]` on `lsm/mod.rs` silenced the expected
  dead-symbol warnings for the retired `LsmPipeline`, `LsmStats`,
  `LsmBatch`, `LastSync`, `Complex32`, and the Phase 6D
  `demod`/`filters` constants.

### Runtime smoke test (pending re-flash)

On-target acceptance criteria for the next firmware cycle:

1. **Build tag**: `/api/system.build` reads
   `2026-04-15-phase9-retire-phase6d-iq-lsm`.
2. **`/api/lsm` returns 404** (route is gone). The dashboard's
   "LSM Pipeline" card should not render at all.
3. **`/api/decoder_compare` has no `ps_iq_lsm` or `ps_phase6d`
   keys**. The response has exactly three pipeline sections:
   `ps_c4fm`, `ps_lsm`, `pl_hdl`.
4. **Active Grants panel ages rebalance**: grants that are being
   refreshed on the control channel show ages in the 0-5 s range
   (not 186 s / 368 s like before the retirement). Grants that
   stop refreshing disappear within 35 s (5 s expiry tick + 30 s
   max age). Single-decoder read = single expire-loop = no stuck
   entries.
5. **Decoder Comparison Matrix on the dashboard** is 3 columns
   wide (was 5). "Sync hits", "NID attempts", "NID BCH decode
   failures", "NID decoded OK (any DUID)", and "Total dibits
   processed" all have numbers in the PL HDL column (via the
   JS-side aliases). Rows labelled "(PS only)" / "(PS framer)"
   / "(= PS)" / "(HDL: hit-only)" / "(HDL: always valid)" have
   those tags in the PL column instead of a bare `--`.
6. **CPU load drops**: `top` on the ARM should show the daemon
   using noticeably less ARM CPU during steady-state TSBK
   activity, since the full software LSM demod (atan2, IIR
   filters, CORDIC, sync correlator) is no longer running on
   every iq_dma wake.
7. **`/api/hdl_lsm` unchanged**: the PL heartbeat endpoint is
   the one that the dashboard now reads for "is the LSM chain
   alive". Its JSON shape was not touched.
8. **`tools/p25_check.py`** runs clean end-to-end with no
   "pre-6F.9 build?" warnings (it no longer looks for
   `ps_iq_lsm` and bails gracefully if it's missing).
9. **`tools/p25_status_and_next_step.py`** renders its summary
   without trying to read `/api/lsm`.

## Phase 9.1 addendum (same session) — vocoder duration fix + grant-log policy

After the initial Phase 9 commit went in, on-target testing surfaced
two follow-up items that got folded into the same session (still
BUILD_TAG `2026-04-15-phase9.1-retire-iq-lsm-plus-duration-fix`):

### Vocoder `duration_ms` was measuring retune-to-retune wall clock

The vocoder task at `main.rs` line ~2495 computed call_end's
`duration_ms` as `started.elapsed()`, where `started` was latched
once per `vocoder_reset_pending` consumption and only moved on the
NEXT retune. There was no silence-based flush, so if the follower
stayed locked on a TG for 97 seconds (many grant refreshes hitting
the `same_tg_same_freq` short-circuit → no retune → no reset) and
only produced 900 ms of actual voice in that window, the call_end
summary reported `frames=45 pcm=7200 (97244 ms)` — a 97× mismatch
between "real audio" and "claimed duration".

**Fix**: added a `call_last_frame_at: Option<Instant>` state in the
vocoder task. Updated on every decoded IMBE frame. `duration_ms` is
now `last_frame_at.duration_since(started)` — the true voice-arrival
span, not the retune gap. Resets to `None` on every
`vocoder_reset_pending` flush and TG-change flush so each new call
starts with a clean measurement.

Expected behaviour after the fix: a 900 ms burst of voice followed
by 96 seconds of silence-plus-noise-TDU_LCs now reports
`duration_ms ≈ 900` on call_end (since the last frame arrived ~900
ms after the first). The 97-second gap is still implicit in the
retune timestamps but no longer contaminates the duration field.

### Grant log policy: no dedupe, rely on downstream double-retune protection

Initial plan was to dedupe grant log entries on
`(tg, channel, freq)` with a sliding window, to collapse the 2-4
identical log lines that appear when the trunking system packs a
`GroupVoiceChannelGrant` and a `GroupVoiceChannelGrantUpdate` into
the same 3-TSBK TSDU burst.

**User feedback rejected the dedupe**: they want every real TSBK
arrival visible in the activity feed, even when multiple land in
the same millisecond. The concern was only whether the follower
handles them correctly without double-retuning or bouncing the
vocoder.

**Verification**: traced through `handle_grant` →
`TrafficManager::handle_grant` → the `same_tg_same_freq` branch at
`p25/traffic_manager.rs:258-294`. Sequence for two simultaneous
grants on the same `(tg, channel, freq)`:

1. First grant arrives, state is `Idle` → `same_tg_same_freq =
   false` → falls through to the retune path → state becomes
   `Acquiring`, `retunes += 1`, returns `true` (retune fires).
2. Second grant arrives microseconds later, state is now
   `Acquiring` with the same (tg, freq) → `same_tg_same_freq =
   true` → auto-promote to `Active` → returns `false` (no second
   retune).
3. Back in `handle_grant_event`, `retune=false` means
   `vocoder_reset_pending` is NOT set (no vocoder bounce).
4. In the caller at `main.rs:2049`, `pre_state != post_state` check
   only pushes a state-transition log entry on an actual change, so
   we don't get a spurious "Active → Active" line.

Net effect: the dashboard sees BOTH grant log entries (as intended),
the follower fires EXACTLY ONE retune for the pair, and the
`Idle → Acquiring → Active` state chain is logged correctly.

**Comment added** at `main.rs:1780` documenting that the activity
log is deliberately NOT deduped and that the correct-handling
invariant lives in `traffic_manager.rs:258`.

## Phase 10 follow-ups (out of scope for this commit)

- **End-of-call TDU_LC burst fix**: on-target testing showed the
  LSM chain produces a ~1-2 s stream of noise-derived TDU_LC
  NIDs after the real voice stops, because the traffic LSM chain
  is still enabled until the 3 s inactivity timer fires and
  `pause_traffic_chain` is called. Two candidate fixes:
  (a) **TDU_LC-as-terminator**: treat the first TDU_LC after any
  LDU1/LDU2 in the current call as "call ended" and immediately
  pause the chain. SDRTrunk does this. Gates off the noise burst
  within ~35 ms of the last real voice frame.
  (b) **Shorter inactivity timeout**: drop `call_timeout_ms` from
  3000 to ~500 ms so the chain pauses faster after real voice
  stops. Simpler but less responsive to brief voice gaps within
  a single transmission.
- **Retire the HDL `iq_dma` block + `iq_packer` + `iq_registers`**
  in a future Vivado bake to free ~few hundred LUT + one
  M_AXI_HP channel.
- **Retire the PS C4FM `decoder` + traffic C4FM dibit reader
  task** if we commit to LSM-only permanently, or leave them
  as scaffolding for future C4FM site support.

## Files touched

- `p25-httpd/src/main.rs`
- `p25-httpd/src/httpd/mod.rs` (AppState + routes + handlers +
  embedded dashboard HTML/JS)
- `p25-httpd/src/lsm/mod.rs` (module-level `#![allow(dead_code)]`
  + extended docstring)
- `doc/P25_API.md`
- `tools/p25_check.py`
- `tools/p25_status_and_next_step.py`
- `doc/changes/039_phase9_retire_phase6d_iq_lsm.md` (this file)
- `DEVPLAN.md` (Phase 9 entry)
- `CHANGELOG_FORK.md` (Phase 9 entry)
