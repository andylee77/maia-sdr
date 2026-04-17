# Fishball P25 Fork — Code Review (2026-04-16)

## Document metadata

| Field | Value |
|-------|-------|
| Review date | 2026-04-16 |
| Branch | `fishball-p25` |
| HEAD at review | `70206ec` (Phase 10 — AGC noise-floor gate, traffic API parity, dashboard overhaul) |
| Scope | Full fork vs `main` (207 files, ~78k insertions) |
| Method | Five parallel sub-reviews: HDL, protocol layer, platform/web, tools/docs, SDRTrunk basis-of-design |
| SDRTrunk reference revision | working tree at `C:/Users/Andy/Projects/SDRTrunk/sdrtrunk` |
| **Stage 1 status (2026-04-17)** | **1.1, 1.2, 1.3, 1.4, 1.9 resolved; 1.5 verified as non-issue. 1.6, 1.7, 1.8, 1.10-1.14 pending. §2 and §3 pending.** |
| **Stage 2 status (2026-04-17)** | **API-first refactor (commit `db59a01`): 9-module handler split, `/api/sys_health`, WS Lagged handling, `doc/P25_API.md` + `doc/API_CONSUMERS.md`. Section 2.3 Lagged + reconnect-backoff items resolved.** |
| **Stage 3 status (2026-04-17)** | **Dead-code sweep (`6fc2554`, `31a7e80`) -868 lines + docstring pass (`cad25b8`) + Linux hotfix (`d403375`). Linux build verified via Tezuka. Section 2.3 recording-fingerprint + recording DOM items resolved; §2.2 `DATA_DEINTERLEAVE` assert verified type-safe; §3.4 tool nits partially resolved (nid_analyze, p25_check regex). ~24 remaining Linux dead-code warnings deferred to future pass (see `project_2026_04_17_session_close` memory).** |
| **Stage 4 status (2026-04-17)** | **Code-review close-out before HDL roadmap: §1.7 `imbe_ring` mutex poison → `unwrap_or_else(\|p\| p.into_inner())` at both callers; §1.8 `TrafficManager` single-owner invariant documented at `impl` block; §1.13 ImbeForwarder `Vec<>` → `VecDeque<>` with `pop_front`/`push_back`; §4.1 vocoder/`TrafficManager` decoupling re-verified (atomic-flag gating only); §4.4 BCH-t runtime override verified as post-decode rejection filter, never downgrades ML capability. `BUILD_TAG=2026-04-17-stage4-code-review-verify`.** |

## Executive summary

**Overall posture:** the fork is mature and SDRTrunk-faithful on every algorithm that matters for decode quality. Most of the risk is concentrated in *boundary code* (the dashboard, diagnostic tools, doc references) rather than the radio pipeline itself. Five items should land before the next bake; about a dozen should-fix items can batch.

**Top five priorities (original list, revised after verification):**

1. ~~**Dashboard XSS**~~ **DONE** — three injection points wrapped with `escHtml()`.
2. ~~**Undefined variables in `tools/p25_check.py`**~~ **DONE** — `lsm_running`, `hdl_pct` defined; phase-tag regex updated.
3. ~~**Missing referenced docs**~~ **NOT ACTUALLY MISSING** — docs live at repo root; `CLAUDE.md` and `README.md` paths updated.
4. ~~**Stale target IP in build script**~~ **RECLASSIFIED** — 120.50 is Ethernet, 2.1 is RNDIS-USB. Both work; build_fpga.bat updated to list primary + note.
5. **Status-dibit counter divergence vs SDRTrunk** — a fixed positional formula instead of a running state machine. Works inside a data unit; lacks implicit recovery across sync slips. Watch the field, port if you see clustered TSBK garbling on noisy channels. **UNCHANGED** — monitor only.

**Stage 2 + 3 additional resolutions (2026-04-17):**

- §1.7 `imbe_ring` mutex poison: **Stage 4 DONE** — both call sites in `httpd/api/traffic.rs` (`/api/imbe_dump`, `/api/audio_test`) now recover with `unwrap_or_else(|p| p.into_inner())`. The ring is a diagnostic buffer; losing its invariant on poison is acceptable.
- §2.3 WebSocket Lagged handling: DONE in Stage 2 for both `/ws/events` and `/ws/audio`.
- §2.3 recording-list fingerprint: DONE in Stage 3 (`(id, started_unix_ms)` instead of `(id, size_bytes)`).
- §2.3 `/ws/events` reconnect backoff: DONE in Stage 2 (exponential 1s→15s).
- §3.2 Dead Golay routines: DONE in Stage 3 (`GolayDecoder` deleted entirely; tests rewritten to call `lsm::nid_fec::decode_nid` directly).
- §2.2 `DATA_DEINTERLEAVE` length-assert: VERIFIED type-system already enforces `[usize; 196]`; no runtime assert needed.
- §3.4 `p25_nid_analyze.py` bitwise op: DONE in Stage 3.

**Counts by severity:**

| Area | Blocking | Should-fix | Nits |
|------|----------|------------|------|
| HDL gateware | 3 | 4 | 8 |
| Protocol layer (Rust) | 4 | 7 | 8 |
| Platform / web (Rust + JS) | 3 | 5 | 9 |
| Tools / docs | 4 | 5 | 6 |
| SDRTrunk fidelity | 0 | 2 | 2 |
| **Totals** | **14** | **23** | **33** |

## How this document is organised

Section 1 lists every blocking item with a verification flag (`[VERIFIED]` if I personally read the cited line; `[REPORTED]` if it comes from the sub-reviewer's reading). Section 2 lists should-fix items grouped by area. Section 3 is nits, batched. Section 4 is the SDRTrunk basis-of-design comparison (10 invariants + incidentals). Section 5 is strengths worth preserving. Section 6 lists what was *not* reviewed and why. Section 7 is the recommended action plan.

---

## 1. Blocking — fix before next flash

### 1.1 Dashboard XSS — three injection points  `[VERIFIED]`

The dashboard interpolates user-controlled strings directly into `innerHTML` template literals with no escaping. All three vectors land on a LAN-exposed daemon.

- **Grants table** at [p25-httpd/src/httpd/mod.rs:5172](../p25-httpd/src/httpd/mod.rs#L5172) — `${g.talkgroup_alias}` raw inside an HTML row.
- **Activity log alias** at [p25-httpd/src/httpd/mod.rs:5246](../p25-httpd/src/httpd/mod.rs#L5246) — `evt.talkgroup_alias` raw.
- **Activity log summary** at [p25-httpd/src/httpd/mod.rs:5250](../p25-httpd/src/httpd/mod.rs#L5250) — `${evt.summary}` raw. The reviewer flagged the alias only; the `summary` field is a second injection vector on the same line.

Aliases originate from `/api/aliases` (PUT). The dashboard then serialises them back into both panels on every WS event. Even if the alias endpoint is intended to be operator-only, there is no auth gate.

**Fix shape:** replace the template-literal interpolations with `textContent` writes on cloned nodes, or escape via a small helper (`s => s.replace(/[&<>"']/g, …)`). The TG Monitor panel already uses the right pattern (`renderTgMonitor`, around line 6313) — copy that style.

### 1.2 `tools/p25_check.py` acceptance summary crashes with `NameError`  `[VERIFIED]`

[tools/p25_check.py:452-453](../tools/p25_check.py#L452-L453) reference `lsm_running` and `hdl_pct`, neither of which are defined in `main()`. The script reaches this block on every successful run, so the *primary* diagnostic tool currently fails before printing its acceptance verdict.

**Fix shape:** compute `lsm_running` from the `/api/decoder_compare` response (probably `ps_lsm.get("messages", 0) > 0`) and `hdl_pct` from `pl_hdl` block (likely `pl_pct` already exists at line 174 — confirm and rename or reuse).

### 1.3 Documentation files referenced but missing  `[VERIFIED]`

`doc/DEVPLAN.md` and `doc/BUILD_FPGA.md` are referenced from [CLAUDE.md](../CLAUDE.md) (Key Directories section) and the README, but `doc/` only contains `ADI_HDL_INTEGRATION.md`, `P25_ADDRESS_MAP.md`, `P25_API.md`, `PANEL_ADDON_BOARD.md`. Either the docs need to be restored from history (last present in commit `95bcb1f` per the sub-reviewer) or the references need updating.

### 1.4 Stale target IP in `build_fpga.bat`  `[RESOLVED 2026-04-17]`

**Clarification from user:** `192.168.120.50` is **not stale** — it is the board's Ethernet interface IP. `192.168.2.1` is the RNDIS-over-USB interface; both work depending on how the board is connected. Current session uses RNDIS.

[build_fpga.bat:509](../build_fpga.bat#L509) updated 2026-04-17 to list `192.168.2.1` as primary (matches current usage + `reference_fishball_target` memory) and note `192.168.120.50` as the Ethernet alternative. No blocking issue; the "stale" label was wrong.

### 1.5 Array-index panics on malformed protocol input  `[VERIFIED — NOT A PANIC RISK]`

Verified 2026-04-17: every cited site is guarded by a type-level or state-machine invariant. Leaving them as raw indexing is consistent with the project rule "don't add validation for scenarios that can't happen."

- [p25-httpd/src/p25/tsbk.rs:499](../p25-httpd/src/p25/tsbk.rs#L499) — `self.payload` is `[u8; 8]` (line 304). Accesses `payload[0..=7]` are type-safe.
- [p25-httpd/src/p25/voice_frame.rs:172](../p25-httpd/src/p25/voice_frame.rs#L172) — `bits[abs_bit]` is bounded: `abs_bit = start + byte_idx*8 + bit_in_byte`, with `start ≤ 1424`, `byte_idx < 18`, `bit_in_byte < 8`, max = 1567 = `LDU_DATA_BITS - 1`. Already guarded by `debug_assert_eq!(bits.len(), LDU_DATA_BITS)` at line 165.
- [p25-httpd/src/p25/control_channel.rs:1545, 1578, 1588](../p25-httpd/src/p25/control_channel.rs) — indexing `tsbk_block_attempts_by_pos: [u64; 3]` by `block_idx`. State-machine enforces `tsdu_blocks_decoded < MAX_BLOCKS` before entry (gate at line 1620). The function is unreachable with a bad index.
- [p25-httpd/src/p25/control_channel.rs:783](../p25-httpd/src/p25/control_channel.rs#L783) — `.expect("body_dibits_for_blocks returns Some for 1..=3")`. The enclosing loop is `for block_idx in 0..MAX_BLOCKS` so `num_blocks ∈ 1..=3`; `body_dibits_for_blocks` provably returns `Some` in that range (tested at `fec.rs:791-796`). The `.expect` is load-bearing documentation, not a bug.

**Conclusion:** all 5 claims were static-analysis false positives from a sub-reviewer who did not read the surrounding invariants. No code change needed.

The sub-reviewer also mis-cited paths as `src/control_channel.rs` — the actual module is at `src/p25/control_channel.rs`. Worth fixing in any future review pass.

### 1.6 HDL CDC closure on Phase-10-prep IRQ path  `[REPORTED]`

[maia-hdl/p25_hdl/p25_top.py:889-933](../maia-hdl/p25_hdl/p25_top.py#L889-L933) — the `PulseSynchronizer` fix is in place, but the in-source comment notes that earlier closure was "by placement luck." The v2 fork exposed a CDC path through `Rsticky`'s direct OR with no synchroniser. Re-run full Vivado timing on the next bake before assuming the synchroniser fully resolved this; verify worst negative slack on the affected nets.

### 1.7 Vocoder `imbe_ring` mutex `.unwrap()` will kill the HTTP task on poison  `[REPORTED]`

[p25-httpd/src/httpd/mod.rs:3369](../p25-httpd/src/httpd/mod.rs#L3369) — `get_audio_test` does `.lock().unwrap().clone()`. If any other holder panics while holding the lock, the next request kills the HTTP task. Use `.lock().unwrap_or_else(|p| p.into_inner())` or recover explicitly.

### 1.8 Concurrency contract on `TrafficManager::handle_grant()`  `[REPORTED]`

[p25-httpd/src/p25/traffic_manager.rs:316-346](../p25-httpd/src/p25/traffic_manager.rs#L316-L346) — `handle_grant()`, `hdu_received()`, `tdu_received()` all take `&mut self`, but three event sources (control-channel poll loop, traffic-LSM events, HDU/TDU callbacks) feed in. There is no documented serialisation. If multiple event sources fire on the same call boundary, state-based matching can race. Either document the single-task-owner invariant or wrap shared state in an explicit lock.

### 1.9 `traffic_lsm_control` readback hides the AGC bit  `[REPORTED]`

[p25-httpd/src/httpd/mod.rs:1995-2000](../p25-httpd/src/httpd/mod.rs#L1995-L2000) — Phase 10 added bit 3 (AGC) to the traffic LSM control register. The readback only returns three values (en, dma_en, dc_block). Callers cannot read back the AGC state they just wrote, breaking idempotent dashboard behaviour and leaving the AGC bit invisible in `/api/system`. Add to both readback and JSON.

### 1.10 HDL AGC can park at `GAIN_MIN=1` after impulse + silence  `[REPORTED]`

[maia-hdl/p25_hdl/lsm_agc.py:189-190](../maia-hdl/p25_hdl/lsm_agc.py#L189-L190) — when a saturating impulse drives gain to `GAIN_MIN`, then the channel goes silent, the `mag_update_threshold=1024` gate blocks update and gain stays at 1 forever. There is an absolute floor but no slow-creep recovery. Field-likelihood is low (real signals don't behave that way) but worth a documented bound, or a slow creep-up path on extended silence.

### 1.11 Diagnostic tools crash on JSON schema drift  `[REPORTED]`

Several tools use raw dict indexing (`d['imbe'][...]`, `t["state"]`) instead of `.get()`. Any field rename or removal in `/api/traffic` causes immediate `KeyError`:

- [tools/p25_call_monitor.py:60-71](../tools/p25_call_monitor.py#L60-L71)
- [tools/p25_sticky_lock_test.py:46](../tools/p25_sticky_lock_test.py#L46), [54-64](../tools/p25_sticky_lock_test.py#L54-L64), [74-85](../tools/p25_sticky_lock_test.py#L74-L85)
- [tools/monitor_p25_decoder.py:41](../tools/monitor_p25_decoder.py#L41)
- [tools/voice_capture.py:114-126](../tools/voice_capture.py#L114-L126)

**Fix shape:** centralise into a single helper (`safe_path(d, "imbe", "ldu1_count", default=0)`), or at minimum migrate every direct index to `.get()` with a typed default.

### 1.12 `lsm_timing_interp` `sample_point` warmup init not asserted in code  `[REPORTED]`

[maia-hdl/p25_hdl/lsm_timing_interp.py:138-141](../maia-hdl/p25_hdl/lsm_timing_interp.py#L138-L141) — the docstring at line 208 asserts the cold-start init value, but the actual numeric constant isn't shown at the cited line range. The reviewer flagged this as "blocking" because if `sample_point` doesn't match the Rust reference (`-ONE_Q12` cold-start), warm-up symbols decode incorrectly. Worth confirming directly against `demod_lsm_with_state` Rust code.

### 1.13 ImbeForwarder evicts with `Vec::remove(0)` per voice frame  `[REPORTED]`

[p25-httpd/src/main.rs:306-313](../p25-httpd/src/main.rs#L306-L313) — `O(n)` shift on every IMBE frame across multiple decoder paths. Not visible at single-call rates, but with 10 concurrent traffic chains (the project goal per memory) it adds up. Swap to `VecDeque::pop_front()`.

### 1.14 `voice_frame.rs` IMBE extraction bounds  `[REPORTED]`

Same shape as 1.5 — [p25-httpd/src/p25/voice_frame.rs:172](../p25-httpd/src/p25/voice_frame.rs#L172). Math is proven, but `.get(abs_bit)` would convert silent panic into a debuggable error.

---

## 2. Should-fix — batch when convenient

### 2.1 HDL

- **8C / 8C.1 revert lacks regression test** at [maia-hdl/p25_hdl/p25_top.py:730-743](../maia-hdl/p25_hdl/p25_top.py#L730-L743). The `91.7% → 24.8%` CRC cliff from the DomainRenamer-based reset isn't captured anywhere; a future refactor that re-introduces it will reproduce silently. Add a `test_lsm_demod_loop` case that exercises both clean and transient signals.
- **Traffic LSM reset ordering vs DDC retune** at [maia-hdl/p25_hdl/p25_top.py:554-570](../maia-hdl/p25_hdl/p25_top.py#L554-L570). Strict order is `freq write → reset pulse → enable`. PS-side enforcement isn't documented. Add a comment in the Rust retune flow citing the contract.
- **NID drop counter resets on Phase-8A runtime reset** at [maia-hdl/p25_hdl/lsm_nid_pipeline.py:153-162](../maia-hdl/p25_hdl/lsm_nid_pipeline.py#L153-L162). Latch a shadow value before clearing, or have PS read before issuing the reset, so a count of 0 isn't ambiguous between "fine" and "just-reset".
- **DibitPacker / IQPacker overflow pulse multi-cycle bound** at [maia-hdl/p25_hdl/dibit_packer.py:80-102](../maia-hdl/p25_hdl/dibit_packer.py#L80-L102) and [iq_packer.py:110-141](../maia-hdl/p25_hdl/iq_packer.py#L110-L141). The Rsticky read-clear is sensitive to multi-cycle pulses; add a sim assertion or single-line comment confirming the one-cycle bound under backpressure.

### 2.2 Protocol layer (Rust)

- **`SystemTime` fallback hides clock skew** at [p25-httpd/src/p25/traffic_manager.rs:220-221](../p25-httpd/src/p25/traffic_manager.rs#L220-L221). `.unwrap_or(0)` makes timestamps silently 1970 if the clock is broken. Combine with the known NTP-on-boot TODO — log a warning and use `Instant::now()` fallback.
- **TSDU `tsdu_blocks_decoded` counter has no overflow guard** at [p25-httpd/src/control_channel.rs:1615](../p25-httpd/src/control_channel.rs#L1615). Other counters use saturating adds; this one wraps at `u64::MAX` (very long, but inconsistent).
- **GVCG payload-length not validated** at [p25-httpd/src/p25/tsbk.rs:498-512](../p25-httpd/src/p25/tsbk.rs#L498-L512). Code assumes `payload[0]` exists; a malformed TSBK with fewer than 8 payload bytes panics. Same shape as 1.5.
- **`DATA_DEINTERLEAVE` size assumed compile-time** at [p25-httpd/src/p25/fec.rs:242-254](../p25-httpd/src/p25/fec.rs#L242-L254). Add `const _: () = assert!(DATA_DEINTERLEAVE.len() == 196);` so a future edit can't silently shorten it.
- **`frequency_mhz: 0.0` is ambiguous** at [p25-httpd/src/control_channel.rs:1958](../p25-httpd/src/control_channel.rs#L1958), [2004](../p25-httpd/src/control_channel.rs#L2004). When `channel_to_frequency()` returns `None`, the field should be `Option<f64>` with `skip_serializing_if`, not silently 0.
- **`p25-json::TsbkEvent` has no `block_idx` field** but the dashboard parses it from the summary string. Add the field to the JSON contract instead of relying on string parsing.
- **Capture finalisation duplicated** at [p25-httpd/src/control_channel.rs:1623](../p25-httpd/src/control_channel.rs#L1623) and [1640](../p25-httpd/src/control_channel.rs#L1640). Consolidate into a single call before the loop.

### 2.3 Platform / web

- **WS broadcast `Lagged` continues without backoff** at [p25-httpd/src/httpd/mod.rs:3919-3928](../p25-httpd/src/httpd/mod.rs#L3919-L3928). A subscriber that lags during silence will hear the next burst start mid-frame. No reconnect hint.
- **Recording-list fingerprint includes `size_bytes`** at [p25-httpd/src/httpd/mod.rs:6517-6525](../p25-httpd/src/httpd/mod.rs#L6517-L6525). During an active recording the size changes every poll, defeating the fingerprint and triggering full list re-render. Fingerprint by `(id, started_unix_ms)` instead, or skip in-progress rows.
- **Activity log creates 300 DOM nodes per refresh** at [p25-httpd/src/httpd/mod.rs:5243-5254](../p25-httpd/src/httpd/mod.rs#L5243-L5254). Same fix as the XSS in 1.1 will likely also let you switch to the diff-list pattern used by TG Monitor.
- **WS `/events` reconnect is fixed 3 s** at [p25-httpd/src/httpd/mod.rs:5258](../p25-httpd/src/httpd/mod.rs#L5258). No exponential backoff; hammers the server when unreachable.
- **UIO `Send`/`Sync` invariant comment lacks detail** at [p25-httpd/src/uio.rs:30-33](../p25-httpd/src/uio.rs#L30-L33). The mapped region is shared with FPGA hardware — say so explicitly: "(a) PAC uses volatile reads, (b) kernel guarantees process-exclusive mmap."

### 2.4 Tools / docs

- **`p25_check.py` build-tag regex `phase[67]`** at [tools/p25_check.py:105](../tools/p25_check.py#L105) — silently fails for Phase 10+. Use `phase\d+`.
- **`p25_lsm_demod.py` hardcodes `C:\Users\Andy\…`** at [tools/p25_lsm_demod.py:25](../tools/p25_lsm_demod.py#L25). Use a generic reference ("upstream SDRTrunk") for portability.
- **`monitor_p25_decoder.py` lambda accesses `d["sync"]["best_distance"]`** at [tools/monitor_p25_decoder.py:41](../tools/monitor_p25_decoder.py#L41) — same as 1.11 but worth calling out separately because it's in a hot path.
- **`p25_nid_analyze.py` bitwise op intent unclear** at [tools/p25_nid_analyze.py:100](../tools/p25_nid_analyze.py#L100) — `(1 << 63) - 1 | (1 << 63)` produces `0xFFFFFFFFFFFFFFFF`. Replace with the explicit constant.
- **`CHANGELOG_FORK.md:87`** links into a private memory file under `.claude/projects/.../memory/` — that path shouldn't be in a committed doc.

---

## 3. Nits — opportunistic cleanup

### 3.1 HDL

- `P25DDC.macc_trunc` profile hardcoded in subclass; tuning requires editing two files. Add a `P25Config.ddc_macc_trunc` field.
- `LsmDemodLoop` docstring missing AGC latency (~50 cycles) — readers may assume only PLL latency in feedback loop.
- `lsm_demod_loop.py` Inputs/Outputs sections don't mention `agc_enable` and `agc_mag_update_threshold` added in Phase 10-prep.
- `MAG_UPDATE_THRESHOLD_DEFAULT = 1024` lacks one-line dB derivation. Add `margin_dB = 20*log10(23170/1024) ≈ 27 dB`.
- `symbol_timing` first symbol after reset references `sym_{re,im}_prev = 0` (implicit). PLL recovers immediately; document explicitly.
- `LsmSyncNidExtract` popcount uses `sum()` over Signals — declare `Signal(7)` and assert width.
- `iq_dma_address` alignment assertion at `config.py:221-223` lacks a "why" comment about `DmaStreamRingWrite` mask-based wrap.
- `symbol_timing` counter reload is in-range under the design's clamping — add a short bounds-comment for future readers.

### 3.2 Protocol layer

- `bits()` helper in `tsbk.rs` re-walks bit positions per call. Optimise hot opcode bytes if profiling later flags it.
- `.next().unwrap()` in test helpers should be `.expect("…")` for clearer failures.
- `as u64` on `as_millis()` discards sub-ms precision — document or switch to `Instant`.
- `events.rs` `GrantEvent` duplicates fields from `GrantInfo`. Consider unifying.
- `types.rs` Phase 7C correction note lists old vs new without citing the validation artefact.
- Dead helpers: `parity_of_bit()` and old Golay routines at [p25-httpd/src/p25/fec.rs:80-82](../p25-httpd/src/p25/fec.rs#L80-L82) — flagged "kept for backwards compat" but nothing calls them.
- `raw_duid_hist` shim at [p25-httpd/src/control_channel.rs:70-74](../p25-httpd/src/control_channel.rs#L70-L74) was for the pre-6D "always TSDU" hardcode; retire.

### 3.3 Platform / web

- `BUILD_TAG` is date-only — two builds on the same date are indistinguishable via `/api/system`. Add commit hash or counter.
- C4FM "retained for diagnostics" comment at [p25-httpd/src/httpd/mod.rs:31-35](../p25-httpd/src/httpd/mod.rs#L31-L35) — clarify LSM-first preference now that `/api/decoder_compare` shows it dormant.
- `/api/log` polling (2-second loop) has no `?limit=` cap — a first connect with `?since=0` serialises the whole 1024-entry ring.
- `/api/decoder_compare` 3-column matrix isn't documented in the endpoint catalogue.
- Spectrum `chain` parameter validation defends correctly but the catalogue isn't updated when a new chain is added.
- WAV writer test code in `vocoder/mod.rs` uses `.unwrap()` on file ops — fine for tests but worth `.expect()` with a useful message.
- `cache_invalidate()` per-buffer call coordination in `rxbuffer.rs` could benefit from a "batch invalidate before reading N buffers" helper to avoid CPU/FPGA races on burst reads.

### 3.4 Tools / docs

- `p25_check.py:26` docstring conflates the Phase 9 retirement of `/api/lsm` with the Phase 10 rename of `/api/control_lsm_control`.
- `build_hdl.bat` doesn't pin the `python:3.11-slim` Docker tag — fine in practice, worth noting for reproducibility.
- `p25_status_and_next_step.py:191` label "PS C4FM" still implies an active decoder — clarify "dormant on LSM sites".
- A handful of tools overlap (`p25_call_monitor` / `p25_sticky_lock_test` / `monitor_p25_decoder`). Worth a consolidation pass.

---

## 4. SDRTrunk basis-of-design comparison

**Method:** for ten algorithmic invariants traceable to specific SDRTrunk Java files (per the project mandate in `doc/changes/011`), the Rust/HDL implementation was compared and classified as MATCHES, DIVERGES, SIMPLIFIED, or MISSING. Each finding cites SDRTrunk file + Rust/HDL file. SDRTrunk reference is the working tree at `C:/Users/Andy/Projects/SDRTrunk/sdrtrunk`.

**Headline:** the fork is **algorithmically aligned with SDRTrunk on every critical decode path.** Two simplifications are accepted as design trade-offs (status-dibit counter, encryption late-entry); one TSBK opcode-coverage gap is worth tracking. No correctness divergences were found.

### 4.1 Audio data-path decoupling — MATCHES

SDRTrunk's `P25P1AudioModule` decodes IMBE as soon as LDU1/LDU2 arrives, with no dependency on `P25TrafficChannelManager` state. Encryption flag and TG identity are read from HDU directly.

The Fishball vocoder task at [p25-httpd/src/main.rs:246](../p25-httpd/src/main.rs#L246) consumes IMBE frames from a channel fed by the voice-frame extractor; it gates on the per-call `call_encrypted` flag set at the grant moment, not on `TrafficManager` state. The decoupling intent is preserved.

**Note:** Fishball reads encryption *earlier* than SDRTrunk (control-channel grant moment via TSBK service options) instead of from HDU. This is functionally equivalent for the common case but does not catch *late-entry* encrypted calls where the grant was missed. See 4.10.

**Recommendation:** accept as-is.

### 4.2 Status-dibit counter — DIVERGES (acceptable simplification)

SDRTrunk's `P25P1MessageFramer.nidDetected()` resets a running `mStatusSymbolDibitCounter` to 21 at sync detection. The counter then fires status processing at every increment of 36 across the full receive lifetime, providing implicit recovery across data-unit boundaries.

Fishball strips status dibits with a fixed positional formula `(pos - 13) % 36 == 0` for `pos >= 13`, applied per data unit (inside `voice_frame.rs` for LDUs, inside the TSDU deinterleaver for control frames). This is mathematically equivalent *within* a data unit but does not provide cross-unit recovery: if frame sync slips between data units the formula re-aligns from the next NID, while SDRTrunk's running counter would have re-synced implicitly at the same point.

**Impact:** subtle. On clean signals: identical. On noisy channels with frequent sync slips: SDRTrunk recovers status alignment one unit earlier. The likely visible artefact would be clustered TSBK garbling on the unit immediately after a sync slip. Watch the field; port the running counter from SDRTrunk if observed.

**Recommendation:** accept as-is, monitor.

### 4.3 PLL bound (π/3) — MATCHES

SDRTrunk uses ±π/3 phase clamp (≈±800 Hz at 4800 baud) with loop gain 0.1. Pyradio uses ±π — do not match pyradio.

HDL `lsm_pll_update.py` / `lsm_pll_rotate.py` use the same ±π/3 bound with the same loop gain. Comments cite "±π/3 ≈ ±1.047 rad". Bit-exact alignment.

**Recommendation:** accept as-is.

### 4.4 NID FEC — MATCHES (BCH(63,16,23) maximum-likelihood)

SDRTrunk's `BCH_63_16_23_P25.java` uses ML decode over the 65,536-codeword space, capable of correcting up to t=11 errors.

Fishball's [p25-httpd/src/p25/fec.rs:84-91](../p25-httpd/src/p25/fec.rs#L84-L91) delegates to a port that uses the same lazy 512 KB codebook, same ML algorithm. HDL `lsm_nid_bch_fec.py` mirrors the same correction strength.

**Note for the project memory note that says "BCH t=4 / sync t=3 are volatile":** that refers to the *runtime threshold* exposed via `/api/bch_t`, not the underlying decoder capability. The decoder is ML; the threshold is a tunable diagnostic gate. Worth verifying directly that the runtime threshold doesn't override the ML decode.

**Recommendation:** accept as-is. Confirm the runtime BCH-t gate is purely diagnostic, not a downgrade of the decoder.

### 4.5 Trellis 1/2-rate — MATCHES

SDRTrunk's `ViterbiDecoder_1_2_P25` is a 49-step Viterbi over the TIA-102 BAAA Table 7-2 trellis.

Fishball's [p25-httpd/src/p25/fec.rs:226-343](../p25-httpd/src/p25/fec.rs#L226-L343) implements the same trellis. Notable structural difference: Fishball performs the bit-level deinterleave (Table 7-7) explicitly inside the decoder (lines 240-254) rather than upstream in the framer as SDRTrunk does. Both are correct; Fishball's is pedagogically clearer.

**Recommendation:** accept as-is.

### 4.6 CRC-CCITT P25 — MATCHES (with feature)

SDRTrunk's `CRCP25.java` uses polynomial 0x11021, init 0xFFFF, with XOR-out.

Fishball's TSBK block handler at [p25-httpd/src/control_channel.rs:819-832](../p25-httpd/src/control_channel.rs#L819-L832) checks both conventions (plain and XORed) and tallies each via `tsbk_crc_ok_plain` / `tsbk_crc_ok_xored`. This is a *feature* over SDRTrunk for diagnosing mixed-vendor systems.

**Recommendation:** accept as-is.

### 4.7 LDU bit layout — MATCHES

SDRTrunk's `LDU1Message`/`LDU2Message` define IMBE frame start positions at `[0, 144, 328, 512, 696, 880, 1064, 1248, 1424]` bits in the 1568-bit body.

Fishball's [p25-httpd/src/p25/voice_frame.rs:91-93](../p25-httpd/src/p25/voice_frame.rs#L91-L93) defines the identical array. Bit-exact.

**Recommendation:** accept as-is.

### 4.8 TSBK opcode dispatch — SIMPLIFIED (gap worth tracking)

SDRTrunk's `TSBKMessageFactory` + per-opcode classes under `standard/osp/` cover ~40 opcodes.

Fishball's [p25-httpd/src/p25/tsbk.rs](../p25-httpd/src/p25/tsbk.rs) covers the high-frequency opcodes for current target sites:

- Group Voice Channel Grant (0x00, 0x02, 0x03)
- Unit-to-Unit Voice / Answer Request (0x04, 0x05)
- Telephone Interconnect (0x08, 0x09)
- SNDCP Data (0x16)
- TDMA Sync (0x30)
- Identifier Update TDMA (0x33)
- Identifier Update VHF/UHF (0x34, 0x3D)

**Notable gaps:** Patch Group Voice (0x01, 0x2E), Extended Function (most of 0x20-0x2F), Emergency Alarm (0x3F), Group Affiliation (0x19).

**Impact:** moderate on full-featured systems; the missing opcodes are silently dropped. The current set covers ~90% of typical site traffic.

**Recommendation:** add stub handlers for missing opcodes that increment a histogram bucket so the dashboard can surface "unhandled opcode X seen N times" — operators can then request implementation in priority order.

### 4.9 Gardner TED — MATCHES

SDRTrunk uses Gardner TED on 2-D demodulated symbols.

HDL `lsm_gardner_ted.py:7-20` implements the identical formula in fixed point (Q3.15 inputs, Q8.30 accumulator, Q4.12 output, `MAX_TIMING_ADJ ≈ 0.260`, `TED_GAIN ≈ 1.628`). Bit-exact.

**Recommendation:** accept as-is.

### 4.10 Encryption handling — SIMPLIFIED (known trade-off)

SDRTrunk reads the encryption flag from HDU on first arrival (primary), with TSBK service options as secondary.

Fishball reads only from TSBK service options at the grant moment (`control_channel.rs:1752`), preserved across grant updates via `PreservedGrantFields`. HDU parsing is deferred. There is also a per-TG `encrypted_tg_history` cache (`main.rs:233`) that suppresses subsequent calls on a TG ever seen encrypted.

**Impact:** Fishball will not detect late-entry encrypted calls when the control-channel grant was missed *and* the TG has no prior encrypted history. On a busy site this is rare; on a quiet site with intermittent control-channel reception it can occur.

**Recommendation:** accept as-is for current phase; track Phase 7C.2 (HDU encryption parse). The `encrypted_tg_history` mechanism partially compensates by sticky-marking TGs once seen encrypted.

### 4.11 Other divergences noticed

- **Diagnostic histograms** — Fishball has extensive per-opcode / per-MFID / per-position TSBK histograms not in SDRTrunk. Pure feature addition.
- **Multi-block TSBK explicit handling** — Fishball's `tsdu_blocks_decoded` counter and explicit 1/2/3-block TSDU lengths are clearer than SDRTrunk's implicit `mDibitCounter` state. No correctness difference.
- **Hard-sync vs soft-sync** — Fishball uses hard-sync correlation (Hamming on `sync_register`) only; SDRTrunk supports both via `P25P1SoftSyncDetector`. Acceptable for a fixed-gain receiver; revisit if low-SNR sites become a target.
- **Runtime BCH-t / sync-t tuning** — Fishball exposes runtime tuning via `/api/bch_t` not present in SDRTrunk. Pure feature. (Confirm this is a *threshold* not a *capability* downgrade — see 4.4.)

### 4.12 Verification status

The audio-decoupling, status-dibit, encryption, and BCH t=11 claims should be re-verified against current code before any of them inform a code change — the SDRTrunk reviewer worked from referenced file:line pairs but I did not personally re-read each cited Rust line. Spot-check before acting.

---

## 5. Strengths worth preserving

- **HDL resource-budget docstrings** in every major module make headroom planning trivial.
- **Q-format derivations** in `lsm_agc.py` and `lsm_gardner_ted.py` are textbook — keep this style.
- **Parallel C4FM + LSM decoder chains** with separate DMAs are a powerful on-air differential test harness. Don't lose it during the C4FM retirement.
- **TG Monitor DOM-reuse pattern** (`renderTgMonitor` around line 6313) is exactly the right shape for the dashboard. Apply the same pattern to the grants table and activity log (which incidentally fixes 1.1).
- **Recording-list fingerprinting** preserves playback state across rebuilds — good UX engineering.
- **Hand-rolled Cooley-Tukey FFT in spectrum.rs** avoids dragging in a dep that won't compile on Tezuka's older Rust. Pragmatic.
- **`build_fpga.bat` staleness detection** for `p25_core.v` regen is well thought out; `check_verilog_stale.ps1` is a nice touch.
- **Dual CRC-convention TSBK validation** (4.6) actively helps debug mixed-vendor systems.

---

## 6. What was NOT reviewed (and why)

- **Upstream Maia HDL** outside `p25_hdl/`, `ip/p25-core/`, `projects/fishball7020_p25/` — out of scope; the brief is the P25 fork.
- **`p25-pac/` generated code** — register PAC produced from SVD; reviewing the generator (not the output) would be the right approach if the PAC ever produces incorrect code.
- **`maia-wasm/`** — upstream Maia waterfall; the P25 dashboard is a separate Rust-side static asset.
- **Submodules `adi-hdl` and `XilinxUnisimLibrary`** — third-party.
- **Vivado-generated Verilog** under any `gen/` directory.
- **Per-opcode SDRTrunk OSP message classes** beyond the dispatch comparison — would only matter if Fishball expands TSBK coverage; the basis-of-design comparison covered the dispatch *layer*, not every opcode body.

---

## 7. Recommended action plan

### Immediate (before next bake/flash) — DONE 2026-04-17

1. ~~Patch the three XSS sinks in [p25-httpd/src/httpd/mod.rs](../p25-httpd/src/httpd/mod.rs) (1.1).~~ Added `escHtml()` helper; wrapped grants-table alias, event alias, event timestamp/type/summary.
2. ~~Fix `lsm_running` / `hdl_pct` in [tools/p25_check.py](../tools/p25_check.py) (1.2).~~ Added `lsm_running = nid_attempts > 0` and `hdl_pct = pl_pct` (or 0.0 fallback). Also relaxed the phase-tag regex from `phase[67]` to `phase\d+|p\d+prep` so Phase 10+ is recognised.
3. ~~Restore or remove the references to `doc/DEVPLAN.md` and `doc/BUILD_FPGA.md` (1.3).~~ Files already exist at repo root (moved in commit 113ff73, 2026-04-08); updated `CLAUDE.md` + `README.md` paths. No restore needed.
4. ~~Update the IP in [build_fpga.bat:509](../build_fpga.bat#L509) (1.4)~~ — primary switched to `192.168.2.1` with `192.168.120.50` noted as Ethernet alternate (see revised 1.4 above).
5. ~~Add the AGC bit to the `traffic_lsm_control` readback (1.9)~~ — `traffic_lsm_control_readback()` now returns `(en, dma_en, dc_block, agc)`; all callers updated; dashboard `/api/traffic_lsm_control` and `/api/traffic_lsm` both include the AGC bit. Control chain readback updated symmetrically.

### Next session — Stage 4 outcome (2026-04-17)

1. ~~Convert raw `[idx]` to `.get(idx)?` at the five sites in 1.5 + 1.14.~~ **§1.5 reclassified NOT-a-panic-risk in initial review; §1.14 same invariants apply (type-level `[u8; 18]` / state-machine bounds). No change needed.**
2. ~~Document or enforce the `TrafficManager` single-task-owner invariant (1.8).~~ **DONE** — doc comment above `impl TrafficManager` cites the `Arc<tokio::sync::Mutex<>>` wrapper in `main.rs` and enumerates the four call-site classes.
3. Re-run full Vivado timing and confirm the Phase 10-prep CDC closure isn't placement-luck (1.6). — **Deferred to next bake.**
4. ~~Verify item 4.4 — confirm runtime BCH-t gate is diagnostic only.~~ **DONE** — `bch_t_override` is applied at `control_channel.rs:1053-1056` strictly as a post-ML rejection threshold: the ML codebook search runs at full `T_MAX_ERRORS=11` capability, then the override rejects results whose `n_errors` exceed the live setting. No downgrade.
5. ~~Verify item 4.1 — re-read the vocoder gating to confirm no `TrafficManager` state coupling crept in.~~ **DONE** — vocoder task consumes from `imbe_tx` mpsc + gates on `ImbeForwarder`'s atomic `call_encrypted` / `current_talkgroup` / `vocoder_reset_pending` flags only. No `TrafficManager` reference in the vocoder path.
6. ~~§1.13 `Vec::remove(0)` → `VecDeque::pop_front()` swap in `ImbeForwarder.imbe_ring`.~~ **DONE** — field type migrated, both push/evict sites updated, the two `/api/imbe_dump`/`/api/audio_test` readers work unchanged (iteration + `.clone()` are identical on `VecDeque`).

### Backlog

11. Status-dibit running counter port (4.2) — only if field traffic shows clustered post-slip TSBK garbling.
12. HDU encryption parse (Phase 7C.2, 4.10).
13. TSBK opcode coverage stub-and-histogram (4.8).
14. Should-fix items in §2 — batch into a "code hygiene" commit.
15. Nits in §3 — opportunistic.

---

## 8. Reviewer's confidence and limitations

The five sub-reviews were conducted by separate explore agents working in parallel. I personally verified the four highest-impact findings (1.1, 1.2, 1.3, 1.4) by reading the cited lines. The remaining findings cite specific file:line pairs but rely on each sub-reviewer's read; spot-check before acting on them, especially the SDRTrunk basis-of-design comparison in §4 where line numbers were drawn from a pair of references rather than re-read for this review.

No on-target validation, no test-suite execution, no Vivado synthesis, no live RF capture were performed as part of this review. Findings are static-analysis only.
