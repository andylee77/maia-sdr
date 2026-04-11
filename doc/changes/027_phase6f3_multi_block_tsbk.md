# 027 -- Phase 6F.3 multi-block TSBK2 / TSBK3 support

**Date:** 2026-04-11
**Phase:** Phase 6F.3 (PS LSM TSBK pipeline -- multi-block follow-up)
**Branch:** fishball-p25
**Status:** Implemented + unit tested. Pending on-target verification
on the Clay County Phase 1 control channel.

---

## TL;DR

The PS LSM software decoder shipped in Phase 6F.2j (doc 026) reads
exactly **one** TSBK1 block per TSDU and stops there. SDRTrunk's
reference recording from the Clay County test target shows almost
every TSDU on this site is actually a multi-block TSDU containing
**TSBK1 + TSBK2 + TSBK3** -- so we were dropping ~2/3 of the on-air
TSBK content. This phase adds full multi-block support per
SDRTrunk's `P25P1DataUnitID.TRUNKING_SIGNALING_BLOCK_{1,2,3}` table.

The fix is event-driven: we read TSBK1 (123 raw body dibits) and
inspect its `LB` (last block) header bit. If `LB == 0`, we extend
the data-unit length to 231 dibits, read 108 more, decode TSBK2.
If TSBK2's `LB == 0`, extend to 303 and read TSBK3. If `LB == 1`
or trellis/CRC fails, we stop and return to Hunting.

This should roughly **triple** the decoded message rate on Clay
County and is also expected to fix the existing follow-up items
where `bands_known` and `active_grants` stay at zero (because
`IDEN_UPDATE` and `GRP_VOICE_CHAN_GRANT` TSBKs are likely riding
in TSBK2/TSBK3 slots that we never read).

---

## Background: SDRTrunk's TSBK1/TSBK2/TSBK3 layout

The relevant constants live in
`io.github.dsheirer.module.decode.p25.phase1.P25P1DataUnitID`:

| Variant                          | messageLength | statusDibits | nullBits |
|----------------------------------|--------------:|-------------:|---------:|
| `TRUNKING_SIGNALING_BLOCK_1`     |   196         |       5      |    42    |
| `TRUNKING_SIGNALING_BLOCK_2`     |   196 * 2     |       8      |    56    |
| `TRUNKING_SIGNALING_BLOCK_3`     |   196 * 3     |      10      |     0    |

`messageLength` is the cumulative trellis-coded data **in bits**.
`statusDibits` is the total number of P25 status dibits embedded in
the on-air sequence (sync + NID + body) **including the one inside
the NID window**. `nullBits` is the number of trailing null padding
bits inside the body, also a multiple of 2.

Translating into raw on-air body dibits (post-NID, no sync, no
in-NID status), and into the trellis input dibits per block:

| Blocks | Raw body dibits | Body status dibits      | Trail nulls | Trellis dibits |
|--------|----------------:|-------------------------|------------:|---------------:|
|   1    |             123 | 4 — {13,49,85,121}      |         21  |             98 |
|   2    |             231 | 7 — {…,157,193,229}     |         28  |            196 |
|   3    |             303 | 9 — {…,265,301}         |          0  |            294 |

The status-dibit positions follow the same period-36 schedule
proven in 6F.2i: starting at body raw position 13 and bumping by 36
each. After dropping status dibits and the trailing nulls, the
trellis stream is 98 dibits per block, contiguous: block 0 in
`[0..98]`, block 1 in `[98..196]`, block 2 in `[196..294]`.

Each block is independently trellis-decoded, CRC-checked, and
parsed. The header byte (`bytes[0]`) carries the `LB` bit (bit 7)
that tells the receiver whether more blocks follow. SDRTrunk's
`P25P1MessageFramer.dispatchTSBK()` recursively reconfigures the
assembler when `LB == 0`:

```java
case TRUNKING_SIGNALING_BLOCK_1:
    if (tsbk1.isValid() && tsbk1.isLastBlock()) {
        adjustDibitCounterFromMessageAssembler();
        mMessageAssembler = null;
    } else {
        // Reconfigure the assembler to continue capturing TSBK2
        mMessageAssembler.reconfigure(P25P1DataUnitID.TRUNKING_SIGNALING_BLOCK_2);
    }
```

We mirror that decision logic, but inline in the existing state
machine instead of using a separate assembler object.

---

## Code changes

### `p25-httpd/src/p25/fec.rs`

`TsduDeinterleaver` was a single-block extractor with hard-coded
status positions for TSBK1. Reworked into a multi-block API:

- New constants: `TRELLIS_DATA_DIBITS = 98`, `MAX_BLOCKS = 3`,
  `BODY_DIBITS_PER_BLOCKS = [123, 231, 303]`,
  `NULL_DIBITS_PER_BLOCKS = [21, 28, 0]`,
  `STATUS_POSITIONS_ALL = [13, 49, 85, 121, 157, 193, 229, 265, 301]`,
  `STATUS_COUNT_PER_BLOCKS = [4, 7, 9]`.
- New public method
  `body_dibits_for_blocks(num_blocks: usize) -> Option<usize>` --
  used by the state machine to extend `du_expected_len` after each
  successful block decode.
- New public method
  `deinterleave_multi(tsdu_dibits: &[u8], num_blocks: usize) -> Vec<u8>`
  -- strips the relevant status dibits and trailing nulls, returns
  `num_blocks * 98` trellis dibits laid out contiguously.

The old single-block `deinterleave` and `extract_tsbk_blocks`
methods were removed. They had no other callers besides
`control_channel.rs::process_tsdu` and the deinterleaver test, both
of which were updated.

The `trellis_encode` test helper was lifted out of `mod tests` to
the module level (still gated with `#[cfg(test)]`) and renamed to
`trellis_encode_block`. A new `trellis_encode_bytes(bytes: &[u8; 12])`
wrapper was added so the e2e tests in `control_channel.rs` can build
real TSBK frames byte-first instead of having to pre-pack bits into
48 two-bit symbols. Both helpers are `pub(crate)` so cross-module
test code can call them.

### `p25-httpd/src/p25/control_channel.rs`

The decoder gained a single new field
`tsdu_blocks_decoded: usize` that tracks how many TSBK blocks have
been decoded from the in-flight TSDU. It is reset to 0 every time
the state machine transitions into `ReadingDataUnit { Tsdu }`.

`process_tsdu` was renamed to `process_tsdu_block` and rewritten
to:

1. Bump `tsdu_attempts` only when `tsdu_blocks_decoded == 0`.
2. Compute the slice for the current block:
   `block_dibits = data_dibits[block_idx*98 .. (block_idx+1)*98]`.
3. Run the Viterbi, parse the TSBK header, validate the CRC, and
   dispatch the parsed message exactly like before.
4. Increment `tsdu_blocks_decoded`.
5. If the decoded `LB` bit is set OR we've now finished block 3 OR
   trellis/CRC failed, finalize the aligned capture (if armed) and
   return `true`. The state machine reads `true` and transitions
   back to `Hunting`.
6. Otherwise, set `du_expected_len` to the next block boundary
   (231 for TSBK2, 303 for TSBK3) and return `false`. The state
   machine stays in `ReadingDataUnit` and continues collecting the
   additional dibits.

The `ReadingDataUnit` arm in `process_dibit` now matches on the
return value of `process_tsdu_block` instead of unconditionally
transitioning to `Hunting` on completion.

The aligned-capture finalization logic was lifted into a small
helper `finalize_capture(...)`. The capture remains a one-shot
TSBK1 snapshot for diagnostic continuity -- multi-block TSDUs
finalize the capture immediately after block 0 either way, so the
on-target replay tool keeps working unchanged.

### `tools/p25_decode_capture.py`

No changes. The captured frame is still a TSBK1 snapshot
(`raw_body_dibits` is 123 dibits, `trellis_dibits` is 98 dibits),
which is what the python replay tool already understands. If we
ever need to capture and replay continuation blocks, the easiest
extension is a new endpoint -- the existing one is the right thing
for diagnosing TSBK1 sync alignment issues, which is what it was
built for.

---

## Tests

Two new e2e tests in `p25/control_channel.rs::tests`:

- **`test_multi_block_tsbk_e2e`** -- builds a real 2-block TSDU
  body with `TSBK1=NET_STS_BCST (LB=0)` and
  `TSBK2=RFSS_STS_BCST (LB=1)`, computes valid CCITT_80 CRCs for
  both, trellis-encodes them, splices in the 7 status dibits at
  body raw positions {13,49,85,121,157,193,229} plus 28 trailing
  null padding dibits, drives the decoder end-to-end (sync + NID +
  body), and verifies that:
  1. `tsdu_attempts == 1`
  2. `tsbk_block_attempts == 2`
  3. `tsbk_crc_ok == 2`
  4. `system.wacn == Some(0xBEE00)` (set by TSBK1)
  5. `system.rfss_id == Some(0x01)` (set by TSBK2)

- **`test_single_block_tsbk_terminates_on_lb1`** -- regression
  guard that confirms a TSBK1 with `LB == 1` correctly stops
  reading after 123 dibits and returns to `Hunting`, instead of
  consuming 108 more dibits looking for a non-existent TSBK2.

Two new fec.rs tests for the multi-block deinterleaver:

- **`test_tsdu_deinterleave_two_blocks`** -- 231 raw → 196 trellis
  with the 7 status drops and 28 trailing nulls.
- **`test_tsdu_deinterleave_three_blocks`** -- 303 raw → 294
  trellis with the 9 status drops and 0 trailing nulls.
- **`test_body_dibits_for_blocks_table`** -- regression on the
  raw-body-dibit table.

`cargo test p25::` runs 28 unit tests, all green. The full crate
test suite (52 tests) is also green.

---

## Diagnostic counters

No new counters were added. The existing 6F.2b counters
(`tsdu_attempts`, `tsbk_block_attempts`, `tsbk_crc_ok`,
`tsbk_crc_failures`, `tsbk_unknown_opcode`) automatically pick up
the multi-block traffic:

- `tsbk_block_attempts / tsdu_attempts` will rise from ~1.0 to
  somewhere near 2-3 on a multi-block site -- a quick proxy for
  how multi-block-heavy the site is.
- `tsbk_crc_ok` should grow ~2-3× on a healthy multi-block site.
- `tsbk_unknown_opcode` should DROP because many of the
  "unknown" opcodes were in fact TSBK2/TSBK3 fragments mis-aligned
  by the old single-block reader (we'd successfully trellis-decode
  the wrong 98 dibits and get a junk byte 0).

---

## Open follow-ups still pending

These were enumerated in doc 026 / memory `project_phase6f_entry_point`
and remain on the queue after this phase:

- **Item 2:** `bands_known` and `active_grants` stay at 0. After
  6F.3 lands, re-check whether they populate. If not, the bug is
  in the parser bit-field offsets in `tsbk.rs::decode_iden_update`
  / `decode_grp_v_ch_grant`.
- **Item 3:** `rfss_id` reads as 160 (0xA0) in the on-target run.
  Likely a single-byte offset bug in `decode_rfss_sts_bcst`.
- **Item 5:** Heartbeat observability fix (~30 lines main.rs).
- **Item 6:** Dashboard polishing -- live activity feed of recent
  TSBKs.

None of these are blockers; the dashboard should now be a
substantially more useful working radio after 6F.3 ships to target.
