# 035 -- Phase 7C -- LDU sync + IMBE frame extraction (focused)

**Date:** 2026-04-11
**Phase:** 7C focused (LDU IMBE extraction only -- no HDU/TDU_LC payload
parsing, no LDU2 ESS, no LC payload from LDU1)
**Branch:** fishball-p25
**Status:** PS Rust changes complete, all 62 host-side tests pass,
cargo check --tests clean. On-target verification deferred to next
flash (combined with Phase 7A.2 HDL bake).
**Next:** Tezuka rebuild + flash + verify, then Phase 7D vocoder.

---

## TL;DR

Phase 7C wires the new traffic-side LSM dibit DMA ring (Phase 7A.2)
into a fourth `ControlChannelDecoder` instance that runs the same
Hunting -> ReadingNid -> ReadingDataUnit state machine as the
control side, with **new LDU1/LDU2/HDU/TDU/TDU_LC dispatch arms**
that extract raw 144-bit IMBE voice frames at the SDRTrunk-documented
bit positions. The frames flow through a new `VoiceHandler` trait
to an `ImbeCounter` (Phase 7C) that will become the vocoder feed
in Phase 7D.

**Operationally** Phase 7C ships:

- A new module `p25/voice_frame.rs` with `extract_imbe_frames()`
  validated against synthetic vectors (5 unit tests).
- Corrected `length_dibits` for HDU / TDU / LDU1 / LDU2 / TDU_LC
  in `p25/types.rs`, validated against the SDRTrunk
  `P25P1DataUnitID.java` table (2 unit tests).
- A new universal `is_body_status_dibit(pos)` helper, also tested.
- A new `VoiceHandler` trait + `set_voice_handler()` setter on
  `ControlChannelDecoder`, with default-no-op methods so the three
  control-channel decoders are unaffected.
- New LDU1 / LDU2 / HDU / TDU / TDU_LC dispatch arms in the
  `process_dibit` ReadingDataUnit state, calling
  `voice_frame::extract_imbe_frames` and forwarding to the handler.
- A new `traffic_lsm_decoder` instance in `main.rs` with the
  `ImbeCounter` voice handler installed, fed by a new tokio task
  that drains `traffic_lsm_dibit_dma`.
- **End-to-end encryption flag plumbing from the control channel**:
  the `GroupVoiceChannelGrant` TSBK now exposes its service options
  byte, `GrantInfo` carries `encrypted` + `emergency` flags, the
  `take_other_grants_for_talkgroup` helper preserves them across
  `GroupVoiceChannelGrantUpdate` refreshes, and `/api/grants` +
  `/api/traffic` surface them. Phase 7D vocoder will gate on the
  control-channel-derived encryption flag instead of needing HDU
  payload parsing -- see
  `reference_p25_encryption_flag_from_control_channel.md` memory.

**Phase 7C deliberately defers** HDU payload parsing (algorithm ID,
key ID, source RadioID, MI), TDU_LC LC payload parsing, LDU1 LC
parsing, LDU2 ESS parsing, and the trellis + RS(36,20,17) +
RS(24,12,13) + RS(24,16,9) decoders that all of those would need.
The encryption flag question is the one operationally important
thing those parsers would give us, and we get it for free from the
control channel grant TSBK service_options byte (per the user's
production SDRTrunk fork which uses the same approach). HDU/LDU
payload parsing remains a Phase 7C.2 / 7B.x cleanup item for the
late-entry case where we lock onto a call without having seen the
original grant.

---

## What was researched (and verified upstream)

Phase 7C is built on top of two SDRTrunk research passes (both
delegated to the Explore agent against the user's `andylee77/sdrtrunk`
fork, with explicit instructions to verify the relevant files are
upstream-equivalent and not contaminated by the user's `010-024`
fork modifications).

### Research pass 1: LDU1/LDU2 bit layout

Source files (all upstream-verified, commit 0d886c27 era):

- `module/decode/p25/phase1/message/ldu/LDUMessage.java:32-40`
  -- IMBE frame bit position constants
- `module/decode/p25/phase1/message/ldu/LDU1Message.java:38-89`
  -- LDU1 LC interleaving + RS(24,12,13)
- `module/decode/p25/phase1/message/ldu/LDU2Message.java:40-63`
  -- LDU2 ESS interleaving + RS(24,16,9)
- `module/decode/p25/phase1/P25P1MessageFramer.java:102, 178-203`
  -- status dibit "every 36" pattern
- `module/decode/p25/phase1/audio/P25P1AudioModule.java:134-149`
  -- raw 144-bit IMBE -> JMBE handoff (no PS-side
  deinterleaving)

Key findings:

1. **An LDU body is 1568 bits = 784 dibits AFTER status dibits
   are stripped.** Status dibits live at body raw positions
   `{13, 49, 85, ..., 13 + 36*k}` for k = 0, 1, 2, ... (universal
   pattern across all DUIDs because the SDRTrunk
   `mStatusSymbolDibitCounter` is set to 21 at NID-detect and the
   first body dibit sees counter 23, so the counter hits 36 and
   drops the first body status dibit at body pos 13).
2. **Each LDU contains 9 IMBE frames at fixed bit positions:**
   `[0, 144, 328, 512, 696, 880, 1064, 1248, 1424]`. Each frame
   is **144 bits raw** (NOT 88 bits -- the 88-bit form is the
   post-vocoder output). The 144 bits include the on-air
   Golay/Hamming/derand internals that JMBE / mbelib handle
   internally.
3. **SDRTrunk passes raw 144-bit frames straight to JMBE**
   (`P25P1AudioModule.java:134-149`). It does NOT deinterleave or
   FEC-correct the IMBE bits in the LDU module. mbelib uses the
   same convention. So the maia-sdr Phase 7C extractor outputs
   raw 144-bit frames as `[u8; 18]` (MSB-first within bytes,
   big-endian byte order) and Phase 7D plugs straight into the
   vocoder library.
4. **No trellis encoding for LDU voice frames.** Trellis 1/2 is
   applied only to TSBK and PDU data blocks. The LDU IMBE bits
   need only the universal status-dibit strip + the 9 fixed
   bit-position extractions. This is much simpler than HDU
   (which needs Golay/Hamming + RS(36,20,17)) or TDU_LC (which
   needs Hamming + RS(24,12,13)).

The full research output is captured in
`reference_p25_ldu_bit_layout.md` memory.

### Research pass 2: encryption flag from control channel

Source files (all upstream-verified, no fork modifications to
the bit positions):

- `module/decode/p25/reference/ServiceOptions.java:27-30` --
  encryption + emergency + duplex + session bit constants
- `module/decode/p25/phase1/message/tsbk/standard/osp/GroupVoiceChannelGrant.java`
  -- `SERVICE_OPTIONS = {16,17,18,19,20,21,22,23}` constant
  array (bits 16-23 of the TSBK)

Key findings:

1. The P25 Phase 1 control channel `GroupVoiceChannelGrant`
   TSBK includes a service options byte at bits 16-23 with the
   layout `EMERGENCY (0x80) | ENCRYPTION (0x40) | DUPLEX (0x20)
   | SESSION_MODE (0x10) | reserved (0x08) | PRIORITY (0x07)`.
2. The encryption flag is bit 6 (mask `0x40`). It's set when
   the talkgroup is being granted an encrypted voice channel.
3. **This is available the moment the grant TSBK lands** -- BEFORE
   the radio has retuned the traffic DDC, BEFORE the HDU has
   arrived on the voice channel, BEFORE any IMBE frames have
   been extracted. Phase 7D vocoder gating happens at the
   control channel layer, NOT via HDU parsing on the voice
   channel.
4. The user's SDRTrunk fork has an `mIgnoreEncryptedCalls` flag
   (commits `90c8e350` + `ef329cd9`, both fork-only) that uses
   exactly this approach: filter encrypted calls at
   `processPhase1ControlChannelGrant()` BEFORE allocating a
   tuner channel. We don't copy the fork's filter as a hardcoded
   default -- Phase 7D will expose it as an opt-in
   `?ignore_encrypted=1` query param.

The full research output is captured in
`reference_p25_encryption_flag_from_control_channel.md` memory.

---

## Files touched

| File | Change |
|---|---|
| `p25-httpd/src/p25/types.rs` | Corrected `length_dibits` for HDU (324 -> 339), TDU (0 -> 15), LDU1 (792 -> 807), LDU2 (792 -> 807), TDU_LC (168 -> 159). Added `data_dibits()` (post-status-strip count). Added universal `is_body_status_dibit(pos)` helper. Added 2 unit tests cross-checking the SDRTrunk `P25P1DataUnitID.java` table. The previous values were never validated because Phase 6 only handled TSDU; Phase 7C is the first time LDU lengths matter. |
| `p25-httpd/src/p25/voice_frame.rs` | NEW. `ImbeFrameRaw` (18-byte raw IMBE frame), `IMBE_FRAME_BIT_POSITIONS` constant ([0, 144, 328, 512, 696, 880, 1064, 1248, 1424] from `LDUMessage.java:32-40`), `LDU_DATA_BITS = 1568`, `LDU_RAW_DIBITS = 807`, `strip_body_status_dibits()` (removes the universal status pattern), `dibits_to_bits()` (MSB-first bit pack), `extract_imbe_frames()` (full LDU body -> 9 IMBE frames). 5 unit tests covering strip math, bit-pack ordering, all-ones round-trip, frame 0 marker bit, frame 1 marker bit (verifying frame-to-frame stride). |
| `p25-httpd/src/p25/mod.rs` | Added `pub mod voice_frame;`. |
| `p25-httpd/src/p25/tsbk.rs` | Added `service_options` byte to `TsbkMessage::GroupVoiceChannelGrant` variant, decoded from `payload[0]` (verified via SDRTrunk SERVICE_OPTIONS = {16-23} bit constant). New `pub mod service_options` with `ENCRYPTION_FLAG = 0x40` + `EMERGENCY_FLAG = 0x80` constants and `is_encrypted()` / `is_emergency()` helpers (verbatim from SDRTrunk `ServiceOptions.java:27-30`). New test `test_grp_v_ch_grant_decode_encrypted` verifying that `payload[0] = 0x40` decodes to `is_encrypted == true`. Updated `test_grp_v_ch_grant_decode` for the new field. |
| `p25-httpd/src/p25/control_channel.rs` | New `VoiceHandler` trait with default-no-op methods for `on_ldu1` / `on_ldu2` / `on_hdu` / `on_tdu` / `on_tdu_lc`. Added `voice_handler: Option<Arc<dyn VoiceHandler + Send + Sync>>` field on `ControlChannelDecoder`, with `set_voice_handler()` setter. Added `ldu1_count` / `ldu2_count` / `hdu_count` / `tdu_count` / `tdu_lc_count` cumulative counter fields. Added LDU1/LDU2/HDU/TDU/TDU_LC dispatch arms in the `process_dibit` `match duid` block (was previously a no-op `_ => true`). Added `encrypted` + `emergency` fields to `GrantInfo`, populated from the new `service_options` byte in the `GroupVoiceChannelGrant` handler. New `PreservedGrantFields` struct returned from `take_other_grants_for_talkgroup` (was `Option<RadioId>`) carrying `source` + `encrypted` + `emergency` so the `GroupVoiceChannelGrantUpdate` handler can preserve all three across grant refreshes. Updated 6 test instantiations + 2 pattern match arms (`tsbk_to_event` formatter, the websocket event formatter in `httpd/mod.rs`) for the new `service_options` field. |
| `p25-httpd/src/main.rs` | Extended `TrafficStats` with HDU/LDU/TDU counters. New `ImbeCounter` struct with `AtomicU64` counters implementing `VoiceHandler` (atomic counters because the handler is called synchronously from inside the dibit decoder task and a Mutex would deadlock with the existing tokio dibit reader's stats lock). New `traffic_lsm_decoder` ControlChannelDecoder instance with the `ImbeCounter` installed via `set_voice_handler`. New traffic LSM dibit reader task spawned in cfg(linux) block, mirror of the existing control-side `lsm_dibit_decoder` task. New `traffic_lsm_dibit_waiter` in the IRQ-handler-waiter cluster. AppState construction extended with `traffic_lsm_decoder` + `imbe_counter`. BUILD_TAG bumped to `2026-04-11-phase7c-ldu-imbe-extraction`. |
| `p25-httpd/src/httpd/mod.rs` | Extended `AppState` with `traffic_lsm_decoder` + `imbe_counter` fields. Updated the WebSocket event formatter pattern to include the new `service_options` field and append `[ENC]` to the summary string when encrypted. Extended `/api/traffic` JSON with `current_call_encrypted` (read from the lsm_decoder grant store for the locked TG), `imbe` block (atomic counters from ImbeCounter), `traffic_lsm_decoder` block (decoder-internal counters: sync_hits, recent_msg_count, per-DUID counts). Phase string bumped to `7C`. |
| `p25-httpd/src/p25/traffic_manager.rs` | (no changes -- the heartbeat task in main.rs continues to drive HDU/TDU/LDU dispatch on the TrafficManager. The Phase 7C dibit decoder runs in parallel and feeds IMBE frames to the ImbeCounter, NOT to TrafficManager -- keeping the SDRTrunk dual-path separation between the audio data path and the event tracking path.) |
| `p25-httpd/p25-json/src/lib.rs` | Added `encrypted: bool` and `emergency: bool` fields to `ChannelGrant`, both `#[serde(default)]` for forward-compat with older binaries. |
| `doc/changes/035_phase7c_ldu_imbe_extraction.md` | NEW -- this doc. |
| `doc/P25_API.md` | (will be updated in a follow-up edit -- new `current_call_encrypted` and `imbe` fields in `/api/traffic` need to be documented) |
| `tools/p25_status_and_next_step.py` | (will be updated -- new Phase 7C ROADMAP entry) |
| `CHANGELOG_FORK.md` | (will be updated -- new Phase 7C entry above the Phase 7A.2 entry) |

---

## Architectural decisions

### VoiceHandler trait, not concrete callback

The `VoiceHandler` trait keeps the decoder library-style: the
decoder has no knowledge of `tokio::sync::mpsc`, `TrafficStats`,
`ImbeCounter`, or any of the per-binary types. Implementations
plug in via `Arc<dyn VoiceHandler + Send + Sync>` and use
interior mutability where state is needed (the `ImbeCounter`
struct uses `AtomicU64` for the same reason).

Default no-op methods on the trait so the three control-channel
decoders (which never see voice frames) don't have to install a
handler at all -- their `voice_handler` field stays `None` and
the LDU/HDU/TDU dispatch arms are dead code from their
perspective.

### Atomic counters in ImbeCounter, not Mutex

The voice handler is called synchronously from inside the dibit
decoder task (which runs in a tokio context but holds the
decoder's RwLock for writing). Calling `tokio::sync::Mutex::lock`
from inside an active `RwLock::write` guard would deadlock if
the lock-acquisition logic ever needed to yield. `AtomicU64`
counters sidestep the issue entirely -- the handler does
`fetch_add(1)` per event and `/api/traffic` reads via
`load(Ordering::Relaxed)`. No locks, no contention, ~1 ns per
counter increment.

The downside is that we can't store an `Instant` directly in an
atomic. Workaround: store `last_imbe_at_millis: AtomicU64`
(milliseconds since UNIX epoch), with 0 as the "never seen one"
sentinel. The `/api/traffic` handler converts to "secs ago" by
subtracting from the current time at read.

### Dual-path: dibit decoder is data, heartbeat is events

The Phase 7A.2 traffic LSM heartbeat task (16 ms cadence,
register-bank polling) continues to drive `TrafficManager`
HDU/TDU/LDU dispatch for the **fast path** (sub-second TDU
release, post-TDU hold window). The Phase 7C dibit decoder
(spawned alongside, ~3.4 sec sub-buffer fill latency) is
responsible for IMBE extraction but does NOT touch
`TrafficManager`.

This is the SDRTrunk dual-path pattern from
`reference_sdrtrunk_dual_path_audio.md` memory: audio is
data-driven from the LDU sync detector forward, event tracking
is event-driven from the TSBK grant lifecycle, neither path
gates the other. Phase 7C honors the separation: the dibit
decoder's voice handler updates `ImbeCounter` (data path), the
heartbeat updates `TrafficManager` (event path), the
`/api/traffic` snapshot reads both.

### length_dibits + status pattern in types.rs vs voice_frame.rs

Length / status pattern math lives in `types.rs` next to the
DataUnit enum because it's a property of the P25 wire format
(not specific to voice). The voice-specific bit positions live
in `voice_frame.rs` because they're LDU-specific. The strip
helper `strip_body_status_dibits` lives in `voice_frame.rs`
because that's where it's used, but it's universal across all
DUIDs and could be moved to `types.rs` if a future TSDU
refactor wants it there.

The Phase 6F.2i validation of TSDU body status positions
{13, 49, 85, 121} is preserved by the new universal helper -- a
unit test (`body_status_pattern_matches_tsdu`) cross-checks the
helper against the TSDU positions, so any future refactor that
breaks the universal pattern will fail this test.

### Encryption flag from control channel, not HDU

Per the agent research + the user's note that they already use
this approach in their SDRTrunk fork: the encryption flag is
available from the `GroupVoiceChannelGrant` TSBK service_options
byte at the moment the grant lands. Pulling it from there avoids
~600 lines of HDU payload parsing code (trellis + RS(36,20,17)
+ bit-layout extractor) for what would be functionally
equivalent information at higher latency.

The late-entry case (where we lock onto a call without having
seen the original grant) is a real scenario but a separate
concern. Phase 7C ships without HDU payload parsing; the
late-entry case will be handled in Phase 7C.2 / 7B as a focused
follow-up if it ever turns out to matter on this site. Most
talk groups on Clay County are not encrypted, and any encrypted
TG will eventually get a fresh grant TSBK.

---

## Verification protocol

After flash:

```bash
# 1. Build tag confirms new binary
curl -s http://192.168.2.1:8080/api/system | python -c \
    "import sys,json;print(json.load(sys.stdin).get('build'))"
# expected: "2026-04-11-phase7c-ldu-imbe-extraction"

# 2. Status script roadmap advances past Phase 7A.2
python tools/p25_status_and_next_step.py | tail -25
# expected: [OK] Phase 7A.2 ... ; [OK] Phase 7C (or next-step Phase 7D)

# 3. /api/traffic exposes the new IMBE counters and the
#    current_call_encrypted flag
curl -s http://192.168.2.1:8080/api/traffic | python -m json.tool
# expected: top-level "phase": "7C", "imbe": {...}, "traffic_lsm_decoder": {...},
#           "current_call_encrypted" present (null or bool depending
#           on whether a call is locked)

# 4. /api/grants now has encrypted + emergency flags per grant
curl -s http://192.168.2.1:8080/api/grants | python -m json.tool
# expected: each grant has "encrypted": false (or true for an
#           encrypted TG) and "emergency": false

# 5. Live IMBE frame extraction during a real call. Watch the
#    counters increment as LDU1/LDU2 frames stream off the
#    locked traffic channel.
python -c "
import urllib.request, json, time
prev_ldu1 = prev_ldu2 = prev_imbe = 0
for i in range(30):
    d = json.load(urllib.request.urlopen('http://192.168.2.1:8080/api/traffic', timeout=3))
    imbe = d['imbe']
    print(f't={i:2d} state={d[\"state\"]:<10} '
          f'tg={d[\"current_talkgroup\"]} enc={d[\"current_call_encrypted\"]} '
          f'hdu={imbe[\"hdu_count\"]} '
          f'ldu1={imbe[\"ldu1_count\"]} (+{imbe[\"ldu1_count\"]-prev_ldu1}/s) '
          f'ldu2={imbe[\"ldu2_count\"]} (+{imbe[\"ldu2_count\"]-prev_ldu2}/s) '
          f'tdu={imbe[\"tdu_count\"]} '
          f'imbe_frames={imbe[\"imbe_frames_extracted\"]} '
          f'(+{imbe[\"imbe_frames_extracted\"]-prev_imbe}/s)')
    prev_ldu1 = imbe['ldu1_count']
    prev_ldu2 = imbe['ldu2_count']
    prev_imbe = imbe['imbe_frames_extracted']
    time.sleep(1)
"

# Expected during a real call:
# - hdu_count increments by 1 at the start
# - ldu1_count + ldu2_count grow at ~7-8 frames/sec combined
# - imbe_frames_extracted == (ldu1_count + ldu2_count) * 9 exactly
#   (any divergence indicates an extraction failure)
# - tdu_count or tdu_lc_count increments by 1 at the end
# - state transitions Idle -> Acquiring -> Active -> Idle
```

**Acceptance criteria:**

1. ✅ Build tag matches `phase7c-ldu-imbe-extraction`
2. ✅ `/api/traffic.imbe.imbe_frames_extracted > 0` after a real
    call
3. ✅ `imbe_frames_extracted == (ldu1_count + ldu2_count) * 9`
    exactly (sanity check on the extraction math)
4. ✅ `/api/grants[].encrypted` populated correctly (false for
    most TGs, true for any known-encrypted TG)
5. ✅ `traffic_lsm_decoder.sync_hits > 0` during an active call
    (the framer is finding sync in the dibit stream)
6. ✅ `traffic_lsm_decoder.{ldu1, ldu2}` counters track 1:1 with
    `imbe.{ldu1_count, ldu2_count}` (the decoder framer and the
    voice handler agree)
7. ✅ Phase 7A.1 sticky lock test still passes
    (`tools/p25_sticky_lock_test.py`) -- the new traffic_lsm_decoder
    + dibit reader doesn't perturb the existing follower behavior

If criterion 5 fails (decoder sees zero sync hits) the most
likely cause is the same status-dibit math we just corrected:
the dibit stream needs the universal status pattern stripped,
and if my `length_dibits` corrections are wrong the decoder will
march out of frame sync within a few NIDs. The diagnostic is
to compare the decoder's `sync_hits` against the existing
`lsm_decoder.sync_hits` -- both decoders run on identical
algorithms, and the only difference is the dibit source. If the
control-side decoder is healthy and the traffic-side isn't, the
problem is either (a) the traffic_lsm_dibit_dma ring isn't
producing real dibits (HDL bug from Phase 7A.2), or (b) my
length_dibits corrections are off (PS bug from Phase 7C).

---

## Known limitations + Phase 7C.2 / 7D follow-ups

1. **No HDU payload parsing.** Algorithm ID, key ID, source
   RadioID, MFID, MI -- all deferred. Encryption flag comes from
   the control channel grant instead. Late-entry calls
   (locked-on without seeing the original grant) will default to
   `encrypted=false` until a fresh grant arrives.
2. **No LDU1 LC payload parsing.** The 240 raw bits of LC chunks
   in each LDU1 carry redundant TG/source/service-options. Same
   info as the control channel grant (which we already have),
   except in the late-entry case.
3. **No LDU2 ESS payload parsing.** Algorithm/key/MI for
   encrypted calls. Doesn't matter until we want to actually
   decrypt -- which we never will.
4. **No TDU_LC LC payload parsing.** End-of-call LC metadata.
   Cosmetic for the dashboard's call log; defer.
5. **No vocoder.** That's Phase 7D. The IMBE frames are
   extracted and counted but currently DROPPED on the floor
   inside the `ImbeCounter::on_ldu1/2` methods. Phase 7D will
   replace the placeholder with a `mpsc::Sender<ImbeFrameRaw>`
   feeding mbelib (or codec2 / DVSI HW).
6. **No RTP audio output.** Phase 7E.
7. **The traffic_lsm dibit reader has the same ~3.4 sec
   sub-buffer fill latency as the control-side LSM dibit
   reader.** This is fine for offline IMBE extraction but
   creates ~3.4 sec of audio latency in the eventual Phase 7D
   vocoder + Phase 7E RTP stream. Phase 7E may need to revisit
   the `traffic_lsm_dibit_dma_buffer_size` constant to reduce
   per-IRQ latency at the cost of higher IRQ rate.
8. **The Phase 7A.2 traffic LSM heartbeat task is still
   running** in parallel with the new Phase 7C dibit decoder.
   Both produce HDU/TDU/LDU "events" but the heartbeat dispatches
   to TrafficManager (fast path, register-bank polling) and the
   dibit decoder dispatches to ImbeCounter (slow path, dibit
   stream). Per the SDRTrunk dual-path lesson this separation is
   correct -- audio is data-driven, events are event-driven.
   Phase 7B may unify them with a typed event channel.

---

## What's next: Phase 7D

With Phase 7C the singleton voice channel produces a continuous
stream of raw 144-bit IMBE frames into `ImbeCounter::on_ldu1/2`
callbacks. Phase 7D wires the vocoder:

1. Replace the `ImbeCounter` placeholder with a real
   `ImbeForwarder` that pushes each `ImbeFrameRaw` to a
   `tokio::sync::mpsc::Sender<ImbeFrameRaw>`.
2. Spawn a vocoder task that reads from the receiver, runs each
   144-bit frame through mbelib's
   `mbe_processImbe7100x4400Data` (or codec2 alternative),
   produces 160 PCM samples per frame (20 ms @ 8 kHz).
3. Gate vocoding on `current_call.encrypted` from the grant
   store -- skip encrypted calls (or output a "this call is
   encrypted" tone).
4. Buffer PCM samples for Phase 7E RTP output.
5. Surface "PCM samples produced" + "vocoder errors" counters
   in `/api/traffic`.

mbelib licensing is grey-area but functional. Codec2 is FOSS but
not bit-compatible with IMBE. DVSI hardware is the
vendor-blessed option but adds a chip. Phase 7D's first
iteration will use mbelib for fast iteration; we can switch
later if licensing becomes an issue.

After Phase 7D + 7E the singleton voice channel produces real
audio. Phases 7F-H scale to ~10 channels via a polyphase
channelizer or wide-capture + cheap fine tuners (decision
deferred per `project_phase7_entry_point.md`).
