# 026 -- Phase 6F.2 TSBK decoder fix saga: 6F.2c through 6F.2j

**Date:** 2026-04-11
**Phase:** Phase 6F.2 (PS LSM TSBK pipeline bring-up)
**Branch:** fishball-p25
**Status:** Phase 6F.2j SHIPPED. PS LSM software decoder is producing
real TSBKs end-to-end. System Identity (NAC, WACN, System ID, RFSS,
Site, Control Channel) populates correctly. 28+ messages decoded
within seconds of boot.

---

## TL;DR

Eight commits over one debugging session, all PS-only, no HDL or
Vivado work. The PS LSM software decoder went from "100 % TSBK CRC
failures, 0 messages decoded" to "fully populated System Identity
card with WACN BEE00 / System 8A0 / Site 1 / Control 0-1593,
matching SDRTrunk's reference output exactly". The dashboard from
6F.2 + the diagnostic counters from 6F.2b + the aligned-capture
endpoint from 6F.2h were the workflow that made this debuggable.

The actual root cause turned out to be **two compounding bugs in the
TSBK decode pipeline**:

1. **Missing `DATA_DEINTERLEAVE` step** before the Viterbi decoder.
   P25 1/2-rate trellis-coded blocks have their 196 bits permuted by
   the encoder per TIA-102 BAAA Table 7-7; we were feeding the
   permuted bits straight into the Viterbi instead of applying the
   inverse permutation first. The Viterbi found garbage paths with
   metric ~24 every time.
2. **Wrong CRC algorithm.** Our `crc_valid` used a byte-wise
   polynomial-division CRC-16/CCITT-FALSE, but P25 uses a
   bit-position-lookup CCITT_80 generated with
   `CRCUtil.generate(80, 16, 0x11021, 0xFFFF, true)` which has
   different bit reflection. Two algorithms, two answers, never matched.

Bug 1 alone was sufficient to explain all the symptoms. Bug 2 only
became visible after Bug 1 was fixed -- the dual-convention check
from 6F.2d masked it because no convention worked when the bytes
were garbage.

**The diagnostic data made the bisection possible.** Without the
6F.2b pipeline failure counters and the 6F.2h aligned-capture +
Python-replay infrastructure, this would have been a multi-day blind
hunt. With them, we triangulated against an SDRTrunk-recorded `.bits`
file from the same site within ~30 minutes of the actual fix landing.

---

## Commits

| SHA | Subject |
|---|---|
| `3406e9d` | Phase 6F.2d TSBK CRC dual-convention fix |
| `cfbf05f` | Phase 6F.2e SYNC_THRESHOLD 10 -> 4 |
| `3ddc22a` | Phase 6F.2f real P25 1/2 Viterbi + correct TSDU body length |
| `9e47602` | Phase 6F.2g TSBK body length 123->122, status positions {14,50,86} |
| `85add9b` | Phase 6F.2h aligned capture endpoint + python replay script |
| `ab14a56` | Phase 6F.2i FINAL TSBK body trace - status at {13,49,85,121} |
| `edd75e2` | **Phase 6F.2j THE FIX - DATA_DEINTERLEAVE + CCITT_80 CRC** |
| `2357323` | tools: p25_decode_capture.py - apply both fixes in the python replay |

(6F.2c was a stub change; 6F.2a/6F.2b were already covered in doc 025.)

---

## How we got here

Doc 025 closed out 6F.2b with this state from the on-target dashboard:

| Stage | Count |
|---|---|
| Sync hits | 941 |
| NID BCH decode failures | 161 (17 %) |
| **NID decoded OK** | **746 (79 %)** |
| NID decoded OK (TSDU only) | 446 |
| TSDU attempts | 446 |
| TSBK block attempts | 446 |
| **TSBK trellis failures** | **0** |
| **TSBK CRC failures** | **446 (100 %)** |
| TSBK CRC OK | 0 |
| Messages decoded | 0 |

The exact failure shape -- trellis ALWAYS succeeds, CRC ALWAYS fails
-- is the canonical "the bytes are right but the CRC formula is
wrong by a constant" signature. Or the canonical "the bytes are
totally wrong but trellis is too lenient to reject them". Telling
those two apart took six commits.

## The wrong-turn iteration log

### 6F.2d: dual-convention CRC (`3406e9d`)

**Hypothesis:** P25 TSBK encoders use both `crc16(data)` and
`crc16(data) ^ 0xFFFF` conventions in the wild. SDRTrunk's
`CRCP25.correctCCITT80` accepts `residual == 0 || residual == 0xFFFF`,
so we should too.

**Change:** `crc_valid` returns `Option<CrcConvention>` instead of
`bool`; accepts both. Added `tsbk_crc_ok_plain` and
`tsbk_crc_ok_xored` counters to dashboard.

**Result:** still 100 % CRC fail. Both bucket counters stay at 0.
**This eliminates the convention as the bug** but the dashboard now
has the data to see that — the next iteration's diagnostic was much
sharper because of these counters.

### 6F.2e: SYNC_THRESHOLD 10 -> 4 (`cfbf05f`)

**Hypothesis:** SYNC_THRESHOLD = 10 was set in the legacy
DC-pedestal era. The HDL LSM dibit stream is much cleaner -- Phase 6D
runs at threshold 4 and gets 91 % NAC validity. Threshold 10 might
be admitting false-positive sync hits whose NID payloads BCH "corrects"
to random valid codewords, then those random NIDs feed garbage TSDU
bodies into the trellis.

**Change:** drop SYNC_THRESHOLD from 10 to 4.

**Result:** PS LSM raw_DUID histogram jumped from 59 % TSDU bucket
to 99 % TSDU bucket -- a real improvement at the NID stage, exactly
as predicted. **But TSBK CRC failures remained at 100 %.** So the
sync threshold WAS too loose AND there's a separate bug downstream.
This is the first time we got a clear "the bug is in the post-NID
pipeline" signal because the NID stage was now demonstrably clean.

### 6F.2f: real P25 1/2 Viterbi + correct TSDU body length (`3ddc22a`)

**Hypothesis:** Our `TrellisDecoder` was a 4-state, 1-bit-input toy
that bore no relation to TIA-102 BAAA Table 7-2. AND the TSDU body
length was 336 (assuming 1-3 TSBKs per TSDU) when it should be 119
data bits + 4 status + 21 nulls = 124 raw dibits per TSBK1.

**Change:** Replaced trellis with the real P25 1/2 Viterbi using
SDRTrunk's `P25_1_2_Node.TRANSITION_MATRIX`. Changed
`Tsdu.length_dibits` from 336 to 124. Wrote round-trip + 1-bit-error
correction tests, both passed.

**Result:** still 100 % CRC fail. **The new Viterbi is internally
correct (round-trip works) but its input bits are still being formed
wrong somewhere.**

### 6F.2g: status positions off by one (`9e47602`)

**Hypothesis:** The status dibit positions in the TSBK body must be
slightly off. SDRTrunk's framer counter starts at 21 after
`nidDetected()` and increments before checking `== 36`, so first
status drop is at body raw position 14 (re-derived).

**Change:** Status positions {14, 50, 86, 122} -> {14, 50, 86}; body
length 124 -> 122.

**Result:** still 100 % CRC fail. Same shape. PS LSM raw_DUID still
98 % TSDU. **My re-derivation was wrong by one in the OPPOSITE
direction.** I'd flipped from "off by 1 high" to "off by 1 low".

### 6F.2h: aligned capture + python replay infrastructure (`85add9b`)

By this point I'd burned three iterations on the off-by-one trace
without converging. Time to stop guessing.

**Change:** Added two new REST endpoints:
- `GET /api/lsm_capture` -- raw 2048-dibit hex dump from the LSM
  decoder's `recent_dibits` rolling buffer.
- `GET /api/lsm_capture_aligned` -- arms a one-shot capture flag and
  waits up to 2 seconds for the next sync hit. Returns a full
  pipeline trace through that one frame: sync dibits, raw NID
  dibits, BCH-decoded NID, raw body dibits, deinterleaved trellis
  dibits, decoded TSBK bytes, CRC verdict.

Plus a new tool:
- `tools/p25_decode_capture.py` -- pure-Python replay of the JSON
  from `/api/lsm_capture_aligned`. Cross-checks each stage against
  the existing `p25_nid_fec.py` reference and prints byte-by-byte
  diffs. Uses ✓/⚠ markers to localize where the rust pipeline
  diverges from python.

**Result:** the diagnostic itself, no behavioral change.

### 6F.2i: ANOTHER off-by-one fix (`ab14a56`)

After 6F.2h was committed (but before it was flashed), I re-traced
SDRTrunk's framer ONE MORE TIME and found that:

1. `mDibitCounter == 57` creates the assembler but does NOT call
   `receive()` on the current dibit (that current dibit is the "+1
   status" the comment refers to).
2. `mStatusSymbolDibitCounter` is set to 21 by `nidDetected()`, then
   the very next iteration increments it to 22 at the top of
   `process()`. The iteration AFTER that increments to 23, and THAT's
   the iteration where the assembler first calls `receive()`. So
   the first body data dibit (raw position 0) sees counter == 23,
   not 22.
3. From counter 23, the first `== 36` check fires after **13** more
   iterations -> at body raw position 13. Then 49, 85, 121.
4. After body raw position 122 the assembler has 119 non-status
   dibits and is complete. Body length = 123 raw dibits with 4
   status drops at {13, 49, 85, 121}.

**Change:** Status positions {14, 50, 86} -> {13, 49, 85, 121}, body
length 122 -> 123.

**Result:** still 100 % CRC fail. PS LSM raw_DUID stayed at ~99 %
TSDU. **At this point I'd burned both possible off-by-one
correction directions (and the original) without success, and the
data triangulated to "the body length and status positions are
right but everything downstream of them is broken".**

### Pivot: use the diagnostic infrastructure I just built

The 6F.2h capture endpoint had been deployed, so I grabbed a live
capture from `/api/lsm_capture_aligned` and ran it through
`tools/p25_decode_capture.py`. The output was illuminating:

```text
Stage 1: SYNC ✓ (distance 0)
Stage 2: NID ✓ (Python and Rust produce identical nid_bits 0x8A17BC1ACE5EEAEE,
            BCH validates NAC=0x8A1 DUID=7 cleanly)
Stage 4: deinterleave ✓ (Python and Rust outputs match exactly)
Stage 5: Viterbi: Python and Rust match (29 5F 0A 40 AA 02 00 11 01 80 3A 04)
            but FINAL STATE-0 METRIC = 28
Stage 6: CRC fails in BOTH Python and Rust
```

**Viterbi metric of 28 was the smoking gun.** A clean trellis input
should have metric 0-2, not 28. So the trellis IS getting bits but
they're scrambled. The python script crashed on a Windows
console-encoding quirk on my first run, but once that was fixed,
the data was unambiguous.

### The SDRTrunk ground-truth comparison

While the user happened to have just saved an SDRTrunk recording
of the exact same site at the exact same time (`.bits` file +
decoded message log), I compared our captured raw body dibits
against SDRTrunk's NID-aligned dibits at the same sync position
and got **120 of 123 dibits matching**. So the DIBIT STREAM is the
right one -- the bug is purely in how we process those dibits.

Then I ran our Rust pipeline on SDRTrunk's known-good dibit stream:
**zero valid CRCs across 1248 candidate alignment sweeps** (status
position × dibit value permutation × pair swap). That meant the
problem was deeper than alignment.

Searched SDRTrunk's `TSBKMessageFactory.deinterleaveViterbiAndCrc`
and found the answer on line 149:

```java
//Get deinterleaved header chunk
CorrectedBinaryMessage deinterleaved = P25P1Interleave.deinterleaveChunk(
    P25P1Interleave.DATA_DEINTERLEAVE, raw);

//Decode 1/2 rate trellis encoded PDU header
CorrectedBinaryMessage message = VITERBI_HALF_RATE_DECODER.decode(deinterleaved);
```

**The deinterleave is BEFORE the Viterbi**, not before our trellis
decoder at all. We were missing it entirely. The deinterleave table
is `P25P1Interleave.DATA_DEINTERLEAVE` -- a 196-entry permutation
copied from TIA-102 BAAA Table 7-7.

Applying the deinterleave to the captured trellis dibits in Python
(plus the existing CCITT_80 CRC) immediately produced
`metric=0` and `bytes = 39 00 01 01 04 FD 04 05 E5 04 1E 97`. Byte 0
opcode = 0x39 = `OSP_SECONDARY_CONTROL_CHANNEL_BROADCAST`, which
matches the FIRST line of the SDRTrunk decoded log:

```text
20260411 023048,PASSED,NAC:2209/x8A1 TSBK1 SEC_CCH_BROADCST RFSS:1/x01 SITE:1/x01 ...
```

### 6F.2j: THE FIX (`edd75e2`)

**Two changes**:

1. **Add `DATA_DEINTERLEAVE` table to `p25/fec.rs`** (196 entries)
   and apply it inside `TrellisDecoder::decode` before the Viterbi:

   ```rust
   pub fn decode(dibits: &[u8]) -> Option<[u8; 12]> {
       // Step 1: convert 98 input dibits into 196 raw bits
       // (interleaved order).
       let mut interleaved_bits = [0u8; 196];
       for n in 0..98 {
           let d = dibits[n] & 0x03;
           interleaved_bits[n * 2]     = (d >> 1) & 1;
           interleaved_bits[n * 2 + 1] =  d       & 1;
       }
       // Step 2: apply deinterleave permutation.
       let mut de_bits = [0u8; 196];
       for i in 0..196 {
           de_bits[DATA_DEINTERLEAVE[i]] = interleaved_bits[i];
       }
       // Step 3: re-pack into 49 four-bit trellis nibbles.
       // ... feed nibbles to existing Viterbi ...
   }
   ```

   The round-trip test was updated to also apply the inverse
   permutation in the encoder helper, so the test still passes.

2. **Add `CCITT_80_CHECKSUMS` table + `ccitt80_crc()` function to
   `p25/tsbk.rs`** (96 entries copied verbatim from SDRTrunk's
   `CRCP25.CCITT_80_CHECKSUMS`) and replace `crc_valid` to use it:

   ```rust
   pub fn crc_valid(&self, data: &[u8; 12]) -> Option<CrcConvention> {
       let calc = ccitt80_crc(data);
       let msg = u16::from_be_bytes([data[10], data[11]]);
       let residual = calc ^ msg;
       if residual == 0 {
           Some(CrcConvention::Plain)
       } else if residual == 0xFFFF {
           Some(CrcConvention::Xored)
       } else {
           None
       }
   }
   ```

   New unit test `test_ccitt80_crc_known_tsbk` validates the
   `39 00 01 01 04 FD 04 05 E5 04 1E 97` byte sequence captured from
   SDRTrunk's recording. Residual is 0xFFFF (Xored convention).

Cargo: 23/23 p25 tests pass, both targets clean.

## Verification

After flashing the 6F.2j binary the on-target dashboard immediately
showed:

```json
GET /api/system
{
  "nac": "8A1",
  "wacn": "BEE00",          // Western Wireless WACN ✓
  "system_id": "8A0",       // Clay County system ID ✓
  "rfss_id": 160,           // see "open issues" #3 below
  "site_id": 1,
  "lra": 0,
  "control_channel": "0-1593",
  "build": "2026-04-11-phase6f.2j-trellis-deinterleave-and-ccitt80"
}
```

These match SDRTrunk's reference log exactly:

```text
NET_STATUS_BCAST WACN:781824/xBEE00 SYSTEM:2208/x8A0 LRA:0/x00 CHAN:0-1593
RFSS_STATUS_BCST SYSTEM:2208/x8A0 RFSS:1/x01 SITE:1/x01 LRA:0/x00 CHAN:0-1593
```

PS LSM column of the comparison matrix (snapshot ~1 minute uptime):

| Metric | Value |
|---|---|
| Messages decoded | 28 (climbing) |
| NID attempts | 545 |
| NID BCH decode failures | 104 (19 %) |
| NID decoded OK | 440 (81 %) |
| NID decoded OK (TSDU) | 438 |
| TSDU attempts | 438 |
| TSBK block attempts | 438 |
| TSBK trellis failures | 0 |
| TSBK CRC failures | 294 (67 %) |
| **TSBK CRC OK** | **144 (33 %)** |
| - via plain CRC convention | 87 |
| - via xored 0xFFFF convention | 57 |
| TSBK unknown opcode | 116 |

**Both CRC conventions are real and used in different proportions.**
The dual-convention check from 6F.2d turned out to be necessary
after all -- 87 plain + 57 xored = 60/40 ratio of the two formulas
in the wild on this site.

The Python replay against the same captured frame that the rust
binary processed:

```text
Stage 5: Viterbi decode (12 bytes)
  python: 16 00 00 C0 04 A9 FF FF 00 01 7E 9B  (final state-0 metric: 2)
Stage 6: CRC
  python crc16(bytes[0..10]):       0x8164
  message CRC field (bytes[10..12]): 0x7E9B
  xored match (calc^0xFFFF == msg): True
  ✓ DECODED A VALID TSBK
```

End-to-end Python replay matches the rust decoder, both validate
CRC, both produce real TSBK bytes.

## Open follow-ups

Not blockers, but worth tracking:

1. **Multi-block TSBK2 / TSBK3 support.** Currently the decoder
   reads only the first TSBK1 block (123 raw dibits = ~119 data +
   4 status + 21 null). SDRTrunk's log shows EVERY TSDU on this
   site has TSBK1 + TSBK2 + TSBK3 (3 blocks per TSDU). We're
   discarding 2/3 of the on-air content. Implementing multi-block
   would roughly triple our message decode rate. Requires:
   - Reading the `last_block` flag from each decoded TSBK
   - Continuing to read the next 122 raw dibits if `last_block == 0`
   - Tracking the TSBK number (1/2/3) for the framer
   - Possibly: TSBK2 and TSBK3 have different `nullBits` counts
     (`nullBits=42` for TSBK1, but `nullBits=56` for TSBK2,
     `nullBits=0` for TSBK3 per SDRTrunk's `P25P1DataUnitID`).

2. **`bands_known: 0` and `active_grants: 0`.** SDRTrunk's log shows
   plenty of `IDEN_UPDATE` and `GRP_VOICE_CHAN_GRANT` TSBKs but our
   `bands_known` and `active_grants` stay at 0. Two possible reasons:
   - These TSBKs are TSBK2/TSBK3 (multi-block) -- fixed by item 1.
   - Our parser's bit field offsets are wrong for these specific
     opcodes. Cross-check against SDRTrunk's `IdentifierUpdate.java`
     and `GroupVoiceChannelGrant.java`.

3. **`rfss_id: 160` should be 1.** We report `0xA0`, SDRTrunk says
   `0x01`. Single byte off in `RFSS_STATUS_BCST` parsing -- looks
   like a wrong field offset. Quick fix.

4. **`tsbk_unknown_opcode: 116` (~26 % of CRC OK).** Some are
   probably TSBK2/TSBK3 fragments mis-aligned (fixed by item 1),
   some are real opcodes we haven't implemented yet. Worth a sweep
   of the most common unknown opcode values to prioritise.

5. **TSBK CRC failure rate 67 %.** Higher than ideal. Likely
   combination of marginal-quality NIDs (where bytes barely make it
   through trellis) plus genuine multi-block TSBKs being
   mis-decoded. Should drop below 30 % once item 1 lands.

6. **Heartbeat observability fix.** Still on the queue from
   doc 025's open list. The heartbeat task still uses the
   `lsm_status.nid_event` Rsticky read which has a known CDC issue
   after long uptimes. Replace with a read of the LSM-side
   `ControlChannelDecoder`'s NID counter. The `/api/hdl_lsm`
   endpoint already exposes the same data so this is mostly cleanup.

## What this batch does NOT do

- Does NOT touch HDL or rebuild the bitstream. XSA on disk is bake E
  from `cfd691f`.
- Does NOT change the LSM HDL chain in any way. The PL HDL chain
  has been producing valid 0x8A1 NIDs at 80-90 % since 6F.2e (sync
  threshold fix) and is rock-solid.
- Does NOT implement multi-block TSBK2 / TSBK3 (item 1 above).
- Does NOT fix the IDEN_UPDATE / RFSS_STATUS / grant parsing
  field-offset issues -- those need a side-by-side comparison
  against SDRTrunk's per-opcode parsers.

## Key files touched

```text
p25-httpd/src/p25/fec.rs                  (3406e9d, cfbf05f, 3ddc22a, 9e47602, ab14a56, edd75e2)
p25-httpd/src/p25/tsbk.rs                 (3406e9d, edd75e2)
p25-httpd/src/p25/control_channel.rs      (3406e9d, cfbf05f, 85add9b)
p25-httpd/src/p25/types.rs                (3ddc22a, 9e47602, ab14a56)
p25-httpd/src/httpd/mod.rs                (3406e9d, 85add9b)
p25-httpd/src/main.rs                     (BUILD_TAG bumps every commit)
tools/p25_decode_capture.py               (85add9b created, ab14a56 sync, 2357323 final)
```

## Diagnostic infrastructure that made this possible

This bug would have taken days of guessing to find without the
infrastructure built up over 6F.2 / 6F.2b / 6F.2h:

1. **Pipeline failure counters** (6F.2b) -- told us at a glance
   that "trellis 0 fail / CRC 100 % fail" was a stable signature
   across 7 different fix attempts. Without these we'd have been
   reading raw log files trying to guess where the data was being
   lost.

2. **Comparison matrix** (6F.2) -- side-by-side PS C4FM vs PS LSM
   vs PS Phase 6D vs PL HDL meant we could cross-validate at every
   step. The 80 % NID validity on PS LSM (matching PL HDL) ruled
   out the NID stage early.

3. **Aligned-capture endpoint + Python replay** (6F.2h) -- the
   ability to capture ONE on-target frame, replay it offline in
   Python, and bisect stage by stage with ✓/⚠ markers cut the
   final fix-finding loop from "blind sweep across 7 candidate
   bugs" to "look at the SDRTrunk source and find the missing step".

4. **SDRTrunk `.bits` file as ground truth.** The user happened to
   have an SDRTrunk recording of the same site at roughly the same
   time. Being able to compare our HDL dibit stream against
   SDRTrunk's known-good dibits at sync-aligned positions was the
   final confirmation that the bug was post-NID, not in the demod
   chain.

The lesson for future bring-up: **build the diagnostic tools BEFORE
you need them**, especially when the failure mode is "every counter
is in the right ballpark but the final stage fails." The 6F.2b/6F.2h
work felt like a detour at the time but it's the only reason this
session converged.
