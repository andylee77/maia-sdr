# 030 -- Phase 6F.11: PS at 100% + PL port roadmap

**Date:** 2026-04-11
**Phase:** Phase 6F.11 (PS LSM TSBK pipeline -- top opcode parsers + API-level merge)
**Branch:** fishball-p25
**Status:** SHIPPED + verified on target. PS side is feature-complete
for the Fishball P25 LSM control channel decoder.
**Next:** PL design discussion (Phase 6G).

---

## TL;DR

Phase 6F.11 added the 5 most-common unparsed TSBK opcodes from the
6F.10 verification (UU_ANS_REQ 0x05, TELE_INT_VCH_GRNT_UPDT 0x09,
SNDCP_DCH_ANN_EX 0x16, TDMA_SYNC_BCST 0x30, SEC_CCH_BROADCST 0x39),
extended `SystemIdentity` with the new state fields, and unioned
both decoders' state in the `/api/system`, `/api/grants`, and
`/api/bands` HTTP handlers. The result: **the top 9 opcodes are
all `parsed: yes`** in `/api/tsbk_opcodes`, **87.6 % of CRC-OK
blocks** turn into structured `TsbkMessage` events, **3
simultaneous active grants** were decoded in the verification
window, and the dashboard shows the **union** of both pipelines'
state without losing the per-pipeline diagnostic split.

**The PS side is done.** Anything from here is either incremental
opcode additions for vendor messages (Motorola, Harris) that don't
move the needle on radio functionality, or **moving signal-processing
work into the PL** to free CPU and improve PLL acquisition time.

This doc captures the 6F.11 changes + the **PL port roadmap** for
the next session, which will be the architectural design discussion
for Phase 6G (PL HDL improvements).

---

## Phase 6F.11 changes

### Five new opcode parsers in `p25-httpd/src/p25/tsbk.rs`

All using SDRTrunk-style absolute-bit-position layouts via the
existing `TsbkBlock::bits()` helper, doc'd inline with the SDRTrunk
class names + bit field tables for cross-validation. Each one is
wired through:

- `TsbkOpcode` enum + `From<u8>` map
- `TsbkMessage` variant
- `TsbkBlock::decode()` dispatcher
- `ControlChannelDecoder::handle_tsbk()` state update
- `ControlChannelDecoder::tsbk_to_event()` WebSocket event
- `httpd::get_recent_tsbks::summarize()` activity feed string
- `/api/tsbk_opcodes` `parsed=true` table

| Opcode | Name | What it brings to the radio |
|---|---|---|
| **0x05** | `UU_ANS_REQ` | Private call paging (target + source radio IDs) |
| **0x09** | `TELE_INT_VCH_GRNT_UPDT` | Telephone interconnect grant update (channel, call timer in seconds, unit ID) |
| **0x16** | `SNDCP_DCH_ANN_EX` | SNDCP packet-data channels (DL + UL channel, autonomous/requested flags) |
| **0x30** | `TDMA_SYNC_BCST` | System date/time + microslot rollover ("system clock") |
| **0x39** | `SEC_CCH_BROADCST` | Backup primary control channels A/B for trunking failover |

Each parser's bit-field layout matches its SDRTrunk Java reference
class one-for-one (`UnitToUnitAnswerRequest.java`,
`TelephoneInterconnectVoiceChannelGrantUpdate.java`,
`SNDCPDataChannelAnnouncementExplicit.java`,
`SynchronizationBroadcast.java`,
`SecondaryControlChannelBroadcast.java`).

### Extended `SystemIdentity`

New optional fields populated by the new parsers:

```rust
pub struct SystemIdentity {
    // ... existing fields ...
    pub secondary_cch_a: Option<Channel>,        // 6F.11
    pub secondary_cch_b: Option<Channel>,        // 6F.11
    pub sndcp_downlink_channel: Option<Channel>, // 6F.11
    pub sndcp_uplink_channel: Option<Channel>,   // 6F.11
    pub last_sync_clock: Option<(u16, u8, u8, u8, u8, bool)>, // 6F.11
}
```

`p25-json::SystemInfo` grows matching `Option<String>` fields with
`#[serde(default, skip_serializing_if = "Option::is_none")]` so
old clients still parse the response.

### API-level merge of both decoders

The new big architectural decision in 6F.11. We had two parallel
`ControlChannelDecoder` instances since 6F.9 (`lsm_decoder` for the
HDL slicer + dibit hard sync, `iq_lsm_decoder` for the Phase 6D
soft sync correlator on raw IQ). Each one tracked its own
`SystemIdentity`, `bands` table, and `grants` map. The dashboard's
`/api/system`, `/api/grants`, `/api/bands` handlers were reading
only `lsm_decoder` and missing the state visible to `iq_lsm_decoder`
-- which the 6F.10 on-target verification flagged when `ps_iq_lsm`
saw 3 active grants while `ps_lsm` saw 0.

**Two ways to fix this** were on the table from doc 029's open
follow-ups:

| Option | What changes | Pros | Cons |
|---|---|---|---|
| **(a) Architectural merge** -- feed both sync sources into one `ControlChannelDecoder` instance | Both `process_dibit` and `process_directed_tsdu` write to the same decoder | Matches SDRTrunk; single source of truth; simpler dashboard | Lose A/B comparison; can't tell which sync detector caught a TSBK; lose regression insurance against HDL slicer breakage |
| **(b) API-level merge** -- keep both decoder instances but union their state in the dashboard handlers | `/api/system` reads both decoders and picks the most-recent value; `/api/bands` unions; `/api/grants` unions deduped by channel | Keeps diagnostic A/B; keeps regression insurance; cheap to implement | Two truth sources internally; counters never combine |

**6F.11 went with (b).** The full design discussion lives at the
top of this session's chat log. The short version: SDRTrunk doesn't
have parallel decoders because it only has one sync source. We have
two sync sources because of how the project evolved (the HDL hard
correlator existed before the Phase 6D soft correlator did), and
the parallel-decoder architecture has real diagnostic value. The
6F.4-6F.10 saga depended on per-pipeline counters to find bugs.
Killing that visibility for "cleaner architecture" would have made
the next bug harder to find -- and we always have option (a)
available later if we ever need to ship as a non-research radio.

The implementation:

```rust
async fn get_system(State(state): State<Arc<AppState>>) -> Json<SystemInfo> {
    let dec_a = state.lsm_decoder.read().await;
    let dec_b = state.iq_lsm_decoder.read().await;
    let sa = &dec_a.system;
    let sb = &dec_b.system;
    fn pick<T: Clone>(a: Option<T>, b: Option<T>) -> Option<T> { a.or(b) }
    Json(SystemInfo {
        nac:               pick(sa.nac, sb.nac).map(|n| format!("{}", n)),
        wacn:              pick(sa.wacn, sb.wacn).map(|w| format!("{:05X}", w)),
        // ... etc
    })
}
```

`/api/grants` unions by channel, picking the YOUNGER (smaller
`age_secs`) of any duplicates. `/api/bands` unions by `identifier`
with first-write-wins (both decoders see the same IDEN_UPDATE
broadcasts so the values are identical anyway).

`/api/decoder_compare` is **intentionally unchanged** -- it still
reports per-pipeline counters so the diagnostic A/B comparison
that the 6F.4-6F.10 saga depended on stays intact.

---

## On-target verification (92 s post-PLL-lock baseline)

| Pipeline | TSDU/s | Block attempts/s | CRC OK/s | Pass% |
|---|---:|---:|---:|---:|
| `ps_lsm` | 10.17 | 30.50 | **25.04** | 82.1% |
| `ps_iq_lsm` | 10.23 | 30.70 | **16.55** | 53.9% |
| **COMBINED** | — | **61.20** | **41.59** | — |

### Per-block-position CRC pass rates (cross-batch defer healthy)

| Block | TSBK1 | TSBK2 | TSBK3 |
|---|---:|---:|---:|
| Attempts | 937 | 937 | 937 |
| CRC OK | 810 | 769 | 729 |
| Pass% | 86.4 % | 82.1 % | 77.8 % |

The mild gradient (86 → 78) is real and expected -- TSBK3 is the
furthest from the sync hit, so any per-symbol clock drift between
the PLL update and the end of the TSBK accumulates the most error
budget on TSBK3. Not worth chasing further; we're already past the
acceptance targets.

### Top opcodes after 6F.11 (all 9 most common are `parsed: yes`)

```
0x16  SNDCP_DCH_ANN_EX           374    yes  ← NEW in 6F.11
0x3D  IDEN_UPDATE                275    yes
0x33  IDEN_UPDATE_TDMA           269    yes
0x3B  NET_STATUS_BCAST           188    yes
0x3A  RFSS_STATUS_BCST           187    yes
0x05  UU_ANS_REQ                 185    yes  ← NEW in 6F.11
0x39  SEC_CCH_BROADCST           182    yes  ← NEW in 6F.11
0x09  TELE_INT_VCH_GRNT_UPDT     181    yes  ← NEW in 6F.11
0x30  TDMA_SYNC_BCST             180    yes  ← NEW in 6F.11
0x0B  Motorola CCH_BASE_STAT_ID   98    no   (vendor mfid=0x90)
0x02  GRP_V_CH_GRANT_UPDT         97    yes
0x15  SNDCP_DCH_PAG_RQ            27    no   (status only)
0x14  SNDCP_DCH_GRANT             21    no   (status only)
0x20  ACK_RESPONSE_FNE            20    no   (acknowledgment)
0x00  GRP_V_CH_GRANT              11    yes
... long tail of acks and vendor opcodes ...
```

**2021 of 2308 CRC-OK blocks parsed = 87.6 % opcode coverage.**
The remaining 12.4 % is split across vendor opcodes (Motorola
0x0B), pure-status SNDCP messages, and a long tail of
acknowledgments / responses with no useful state.

### Live grants -- the radio works

```text
ch=0-1117  TG=300  freq=857.9875 MHz  age=15s
ch=0-1189  TG=433  freq=858.4375 MHz  age=38s
ch=0-1193  TG=402  freq=858.4625 MHz  age=172s
```

Three simultaneous active voice grants tracked in real time. The
TDMA_SYNC_BCST decoder is correct (`2026-04-11 16:05`). SNDCP DL/UL
channels and SCCB backup channels are populated.

---

## What's left on the PS side (none of it matters for radio function)

These are all in the "incremental polish on a working baseline"
category. None of them affect the radio's ability to track grants,
system identity, talkgroups, or band tables.

### Vendor opcode parsers (Motorola)

The biggest remaining unparsed bucket is `0x0B` Motorola
`CCH_BASE_STAT_ID` (~98/win). Adding it requires a new "vendor
manufacturer" routing layer in `TsbkBlock::decode()` that picks
between standard and vendor-specific parsers based on the
`manufacturer` byte. Same deal for any of the other Motorola
opcodes (`0x05` `MOTOROLA_TRAFFIC_CHANNEL_ID`, `0x16`
`MOTOROLA_TDMA_DATA_CHANNEL`, etc).

**Effort:** medium (requires the vendor routing layer).
**Value:** zero for radio functionality. These are mostly
diagnostic/loading info that doesn't change how grants or bands
work. Defer indefinitely.

### Pure-status acknowledgment opcodes

`SNDCP_DCH_PAG_RQ`, `SNDCP_DCH_GRANT`, `ACK_RESPONSE_FNE`,
`DE_REGIST_ACK`, `GRP_AFFIL_RESP`, etc -- each is ~20-30 lines but
they don't add state. They'd just clear out the
`tsbk_unknown_opcode` counter. Defer.

### Heartbeat observability fix (~30 lines main.rs, queued from doc 025)

The heartbeat task uses `lsm_status.nid_event` Rsticky which has a
known CDC issue after ~200 s uptime. Replace with reading the
LSM-side `ControlChannelDecoder`'s NID counter. Cleanup, not a real
bug. Defer.

### Dashboard polish

The dashboard's existing `/ws/events` already broadcasts every
parsed TSBK with the new opcodes' summaries; the dashboard HTML/JS
just needs to render them as a live feed. Already partially done by
`/api/recent_tsbks`. Defer to "after PL is sorted".

---

## PL port roadmap (Phase 6G)

**This is the agenda for the next session.** The big architectural
question is "what should move from PS Rust to PL HDL gateware to
make the radio more reliable / lower CPU load / better acquisition
time", and "what should stay in PS forever because the cost of
porting it to HDL exceeds the value".

### What stays in PS Rust forever (the right call)

| Component | Why it stays |
|---|---|
| BCH(63,16,11) NID FEC | 512 KB ML codebook, complex maximum-likelihood decoder. Already exists in HDL via `LsmBchNidExtract` for the in-line PL chain, but the PS port uses a different (faster) algorithm. PS implementation is fine -- ARM has the RAM. |
| Trellis Viterbi (P25 1/2 rate) | 49-step traceback, ~5 µs/decode on Cortex-A9. Already in HDL via the C4FM chain but the LSM HDL chain doesn't use it -- PS handles it for both. |
| TSBK CRC (CCITT_80 table-based) | Tiny 96-entry XOR table, 80 ops/CRC. Microscopic. |
| All TSBK opcode parsers | High-level state, not signal processing. Trivial in Rust, painful in HDL. |
| Dashboard / WebSocket / aliases / event log / API | This is what the radio EXISTS for. Nothing about it benefits from being in HDL. |

### What's a candidate for the PL port

The candidates are ordered by **reliability + acquisition-time
benefit**, which is what the user has flagged as the primary goal
for the PL port.

#### Candidate 1: HDL DC blocker on the slicer input (TOP PRIORITY)

**The biggest single win available.** Today, the LSM PLL takes
2-3 minutes to fully converge after a flash because the slicer is
running on a slightly DC-biased input. During the transient the
inner/outer dibit ratio runs ~60/40 instead of the ideal 50/50,
which puts ~5 bit errors into every 48-bit sync window and
collapses the sync hit rate from ~14/sec down to ~5/sec until the
PLL settles. This is the entire reason 6F.5/6F.6/6F.7 burned 3
flash cycles tuning the sync threshold (the threshold was fine,
the slicer was just temporarily noisy).

**The fix:** add an IIR or FIR DC blocker upstream of the slicer
in the LSM HDL chain. Standard one-pole IIR DC blocker is about
40 lines of Amaranth and adds maybe 6 LUTs.

**Expected impact:**
- PLL acquisition transient shrinks from 2-3 min to a few seconds
- 60/40 inner/outer ratio becomes 50/50 immediately
- Sync hit rate stays at the steady-state ~14/sec from boot
- "Reset and measure" experiments take 30 s instead of 3 min
- Combined CRC-OK rate goes from "82-92% post-lock" to "~92%
  consistently from boot"

**Effort:** small (one Amaranth module, one HDL test, one
integration in the LSM demod chain).
**Value:** HUGE for both reliability and developer experience.

#### Candidate 2: Soft sync correlator into PL HDL

Today the Phase 6D `LsmPipeline` runs in PS Rust and computes the
soft-decision sync correlation in software. This is what feeds
`iq_lsm_decoder` and gives us the second TSBK pipeline (~17
CRC-OK/sec) that runs in parallel with the HDL hard sync chain.

If we port the soft sync correlator into HDL, the PS Rust
`LsmPipeline` becomes redundant for sync detection (we'd still need
some of its other outputs like the soft phases for diagnostics).
The HDL would emit "sync hit at this dibit position" events at the
~9/sec rate the soft correlator currently delivers, and the
existing HDL LSM dibit chain would handle everything downstream.

**Trade-off:** the parallel `iq_lsm_decoder` pipeline goes away.
We lose the regression insurance against HDL slicer breakage that
6F.9-6F.11 gave us. We'd need to weigh that against the CPU
savings.

**Effort:** medium (the multiply-accumulate is straightforward but
the soft phase output from the LSM demod isn't currently exposed
on the HDL slicer side -- needs PL plumbing).
**Value:** moderate. Frees the LSM IQ task on PS and removes a
parallel codepath. Doesn't improve acquisition time or throughput.

#### Candidate 3: TSBK status-dibit deinterleave + multi-block staging

Today PS Rust handles the entire TSDU body extraction:
- `TsduDeinterleaver::deinterleave_multi(body, num_blocks)` strips
  status dibits and trailing nulls
- Slices into 98-dibit trellis blocks for each TSBK
- Calls `TrellisDecoder::decode` per block
- Walks the multi-block continuation logic with the LB bit

All of this is pure dibit shuffling -- trivial in HDL with a small
state machine + lookup table for status positions. The HDL could
present PS with a per-TSDU stream of pre-deinterleaved 98-dibit
trellis blocks, and PS would just run the Viterbi + CRC + parse.

**Trade-off:** PS CPU savings are small (current load is well
under 5 %). Mostly a "cleanliness" win.

**Effort:** medium-large (state machine in HDL, BRAM for the
status dibit position table, integration into the existing LSM
dibit DMA path).
**Value:** low. We aren't CPU-limited. Defer indefinitely.

#### Candidate 4: Multi-channel parallel TSBK decode (NEW idea)

The Fishball Z7020 has plenty of LUT/BRAM headroom and the AD9361
can be tuned to multiple channels via the existing DDC. **If we
add a second LSM decoder chain in HDL targeting the secondary
control channel** (which we now decode via SCCB and surface as
`secondary_cch_a/b` in `/api/system`), we get instant trunking
failover -- if the primary CCH drops, the radio is already locked
to the backup.

**Effort:** medium (instantiate a second LSM chain in HDL, route
its dibit stream to a second `lsm_dibit_dma` ring, run a second
`ControlChannelDecoder` instance in PS).
**Value:** moderate. Real radios do trunking failover; ours
currently doesn't. But it's only useful if the primary CCH
actually drops, which it doesn't in normal operation. **Defer
until after we have a real-world reason to need failover.**

### Recommended PL port order

1. **HDL DC blocker** -- biggest reliability + acquisition win,
   smallest code change. Should be the first thing in Phase 6G.
2. **(possibly) soft sync correlator into PL** -- moderate value,
   moderate effort. Skip if we decide the parallel-decoder
   diagnostic value outweighs the CPU savings.
3. **Multi-channel decode** -- only if we have a real reason for
   failover.
4. **TSBK status-dibit / Viterbi feed** -- defer indefinitely.

### What we should NOT do

- **Don't port BCH FEC to HDL.** It's already there in the LSM
  HDL chain, but the PS port is faster and we use the PS port for
  both pipelines anyway. No reason to touch it.
- **Don't port opcode parsers to HDL.** This was never on the
  table -- TSBK parsing is high-level state machine work that
  belongs in software.
- **Don't kill the parallel-decoder architecture as a "cleanup".**
  The 6F.4-6F.10 saga proved its diagnostic value. If we ever
  need to consolidate, we'll do it deliberately as a separate
  Phase 6H.

---

## Final commit chain on `fishball-p25` after 6F.11

```text
8270314  p25-httpd: Phase 6F.11 -- PS at 100% (5 new opcode parsers + API merge)
eb9a380  p25-httpd: Phase 6F.10 -- iq_lsm cross-batch defer + max_recent 1000
ea9bf60  doc: 027/028/029 + CHANGELOG_FORK -- Phase 6F.3-6F.9 throughput saga
f911b3a  tools: p25_check_phase6f4.py + p25_sync_sweep.py for 6F.4-6F.9 verification
cfeb7db  p25-httpd: Phase 6F.3-6F.9 throughput breakthrough -- 14.8 msg/sec @ 92% CRC
b861ddd  doc: 026 Phase 6F.2 TSBK decoder fix saga + capture file cleanup
2357323  tools: p25_decode_capture.py - apply DATA_DEINTERLEAVE + CCITT_80 CRC
edd75e2  p25-httpd: Phase 6F.2j THE FIX - DATA_DEINTERLEAVE + CCITT_80 CRC
```

25+ commits ahead of `origin/fishball-p25`.

---

## What "PS at 100%" means concretely

**Definition:** the PS Rust side decodes the Fishball P25 LSM
control channel well enough that:

1. ✓ System identity (NAC, WACN, system ID, RFSS, site, control
   channel, secondary CCH A/B, SNDCP channels, system clock) is
   tracked in real time
2. ✓ All 6 frequency bands (FDMA + TDMA) populate within seconds
   of boot
3. ✓ Active voice grants are tracked with accurate channel,
   talkgroup, source, frequency, and age
4. ✓ Top 9 opcodes parsed → 87.6 % of CRC-OK blocks dispatch as
   structured `TsbkMessage` events
5. ✓ Activity feed (`/api/recent_tsbks`) shows decoded TSBK1/2/3
   labels matching SDRTrunk's `decoded_messages.log` format
6. ✓ Per-pipeline diagnostic A/B available via
   `/api/decoder_compare`
7. ✓ Both stretch throughput targets met (30 TSBK/sec, 10 msg/sec
   -- we hit 60+ TSBK/sec combined and ~25 parsed msg/sec)

**Not defined as 100 %:**

- Vendor opcode parsing (Motorola, Harris) -- low-value
- Pure-status / acknowledgment parsing -- adds nothing
- HDL side reliability -- that's Phase 6G

The "100 %" is for the PS Rust software running against an
already-working HDL chain. The HDL side has known weaknesses
(2-3 min PLL acquisition transient, slicer DC bias) that the next
phase will address.
