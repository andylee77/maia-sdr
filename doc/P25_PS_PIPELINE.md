# P25 PS Pipeline — End-to-End Review

**Scope:** everything the Zynq ARM (PS) does with P25 data **after** it leaves the FPGA (PL). Starts at the DMA ring buffers and ends at WAV files on disk / WebSocket frames to the browser.

**Target commit:** `fishball-p25` @ `e5d20ca` (2026-04-19). All line references are against that tree.

**Sibling docs:**

- [P25_API.md](P25_API.md) — REST / WebSocket surface reference
- [P25_ADDRESS_MAP.md](P25_ADDRESS_MAP.md) — PL register map
- [HDL_LAYOUT_AND_ROADMAP.md](HDL_LAYOUT_AND_ROADMAP.md) — PL side

---

## 0. Ten-Thousand-Foot View

Two almost-identical decoder chains, running in parallel on the same `p25-httpd` process:

```text
 ┌─────────────────────────── CONTROL CHAIN ────────────────────────────┐
 │  PL lsm_dibit_dma  →  ControlChannelDecoder (primary, LSM)           │
 │  PL dibit_dma      →  ControlChannelDecoder (legacy, C4FM, retiring) │
 │     ↓                                                                │
 │     TSBK opcodes → grants / IDEN_UP / system identity                │
 │     → TrafficManager (decides when to retune the traffic chain)      │
 └──────────────────────────────────────────────────────────────────────┘

 ┌─────────────────────────── TRAFFIC CHAIN ────────────────────────────┐
 │  PL traffic_lsm_dibit_dma → ControlChannelDecoder (voice DUIDs only) │
 │     ↓                                                                │
 │     HDU / LDU1 / LDU2 / TDU / TDU_LC → ImbeForwarder (VoiceHandler)  │
 │     ↓                                                                │
 │     9 × IMBE frames per LDU → mpsc → JMBE vocoder → 160 i16/frame    │
 │     ↓                                                                │
 │     AudioChunk broadcast → /ws/audio  +  recorder → WAV              │
 └──────────────────────────────────────────────────────────────────────┘
```

Both chains share one piece of code — [control_channel.rs](../p25-httpd/src/p25/control_channel.rs) — parameterised by what kind of `VoiceHandler` it has wired in and which DMA ring it reads from. The control instance emits TSBK grants; the traffic instance emits voice.

Key file sizes (signal of where the complexity lives):

| File | Lines | Role |
|------|------:|------|
| [main.rs](../p25-httpd/src/main.rs) | 3768 | Tokio task orchestration, ImbeForwarder, DMA pump loops |
| [p25/control_channel.rs](../p25-httpd/src/p25/control_channel.rs) | 2893 | Frame sync → NID → DUID → body dispatch |
| [p25/tsbk.rs](../p25-httpd/src/p25/tsbk.rs) | 1535 | 40+ TSBK opcode parsers |
| [p25/voice_frame.rs](../p25-httpd/src/p25/voice_frame.rs) | 1444 | IMBE extract + HDU / LDU / TDULC FEC + LCW parsers |
| [p25/traffic_manager.rs](../p25-httpd/src/p25/traffic_manager.rs) | 753 | Grant follower, DDC retune, state machine |
| [recorder.rs](../p25-httpd/src/recorder.rs) | 647 | Per-call WAV writer |
| [p25/fec.rs](../p25-httpd/src/p25/fec.rs) | 662 | 1/2-rate Trellis Viterbi + TSDU de-interleave |
| [p25/rs_p25.rs](../p25-httpd/src/p25/rs_p25.rs) | 412 | Shared Berlekamp-Massey RS over GF(2^6) |

---

## 1. Entry Point: PL → PS DMA

### 1.1 DMA rings

The PL writes into six (sometimes eight, depending on branch) kernel-mmap'd ring buffers. Each is 8 × 4 KB (dibit rings) or 8 × 32 KB (IQ rings). The PS side reads via [rxbuffer.rs](../p25-httpd/src/rxbuffer.rs), which wraps a UIO device (`/dev/uioN`) and a kernel-exported buffer memory region.

| Ring                        | Device name               | Rate              | Purpose                                  |
|-----------------------------|---------------------------|-------------------|------------------------------------------|
| `dibit_dma`                 | `p25-dibit`               | 4800 sym/s dibits | C4FM control channel (legacy)            |
| `lsm_dibit_dma`             | `p25-lsm-dibit`           | 4800 sym/s dibits | **LSM control channel (primary)**        |
| `traffic_dma`               | `p25-traffic`             | 4800 sym/s dibits | C4FM traffic (retired 2026-04-16)        |
| `traffic_lsm_dibit_dma`     | `p25-traffic-lsm-dibit`   | 4800 sym/s dibits | **LSM traffic**                          |
| `iq_dma`                    | `p25-iq`                  | 62.5 kSPS `i16×2` | Control post-DDC IQ (constellation/FFT)  |
| `traffic_iq_dma`            | `p25-traffic-iq`          | 62.5 kSPS `i16×2` | Traffic post-DDC IQ                      |
| `lsm_iq_dma` (10.6)         | `p25-lsm-iq`              | 31.25 kSPS `f32×2`| Post-RRC matched-filter IQ               |
| `traffic_lsm_iq_dma` (10.6) | `p25-traffic-lsm-iq`      | 31.25 kSPS `f32×2`| Post-RRC matched-filter IQ, traffic      |

Ownership is in [fpga.rs](../p25-httpd/src/fpga.rs), `IpCore` struct. It just holds a `RxBuffer` per ring and surfaces `read_*_dma()` futures.

### 1.2 Pump loops

Spawned from `main.rs` during startup. One tokio task per dibit ring. The pattern is always:

```rust
loop {
    let buf: Bytes = ip_core.lock().await.read_X_dma().await?;
    for raw in buf.iter() {
        // Each byte holds 1 dibit in the low 2 bits (the PL packs 1/byte, not 4/byte)
        decoder.lock().await.process_dibit(*raw & 0x3);
    }
}
```

No timestamps travel with the samples; each task tags buffers with `Instant::now()` on receipt for latency stats.

The traffic pump invokes a **different** `ControlChannelDecoder` instance than the control pump — same type, different `VoiceHandler` wiring, different counters, different sync-threshold tuning.

---

## 2. Frame Synchronization

[control_channel.rs — `process_dibit`](../p25-httpd/src/p25/control_channel.rs) is the hot loop. A state machine walks four states: `Hunting → ReadingNid → ReadingDataUnit → (back to Hunting)`.

### 2.1 Sync detection

Every P25 frame opens with a 48-bit sync pattern (24 dibits). We hold a rolling 48-bit window in a `u64` shift register and compare against the canonical `0x5575F5FF77FF` (frame sync word) each symbol.

The comparison is a **soft Hamming distance** on the 48 bits, not a strict equality. If `popcount(window ^ SYNC) ≤ threshold` the decoder declares a sync hit and jumps to `ReadingNid`.

Runtime-tunable threshold:

- Default: 6 (`SYNC_THRESHOLD` in [control_channel.rs:592](../p25-httpd/src/p25/control_channel.rs#L592))
- Global override: `PUT /api/sync_tune?threshold=N` (0..=24)
- Per-chain override: `PUT /api/sync_tune?threshold=N&side=traffic|control`
- Reset a per-chain override: `PUT /api/sync_tune?threshold=reset&side=...`
- Histogram of sync distances at hit time is recorded for health tracking (Phase 6F.6)

### 2.2 Status dibit removal

P25 interleaves a **status dibit** every 36 dibits in the body (first one 13 dibits after the NID for LDU/TSBK, 14 for TDULC). These are scheduling/busy bits, not payload, and must be stripped before FEC. The helper is `is_body_status_dibit(raw_pos)` in [types.rs](../p25-httpd/src/p25/types.rs). TDULC overrides it because its boundary is one dibit later.

### 2.3 Bit / dibit ordering

Dibits are MSB-first: dibit value `0bAB` unpacks to bits `[A, B]`. This matters because the BCH codebook, Golay/RS decoders, and Trellis decoder all consume flat bit arrays derived from the dibit stream. A reversed dibit convention silently produces Hamming-distance-random garbage at every downstream stage — one of the classic early-debug footguns.

---

## 3. NID Decoding — BCH(63,16,23)

After a sync hit the decoder reads 33 raw dibits (66 bits), strips the in-NID status dibit (position 11), and presents 64 bits to a BCH decoder:

- **Code:** shortened BCH(63,16) with minimum distance d=23, so t=11 errors correctable.
- **Payload:** 12-bit NAC + 4-bit DUID = 16 data bits. Remaining 48 bits are parity.
- **Implementation:** [lsm/nid_fec.rs](../p25-httpd/src/lsm/nid_fec.rs). A one-time `codebook()` call expands all `2^16 = 65536` valid codewords into a `[u64; 65536]` lookup table. Decode is a linear scan:

```rust
for (idx, &cw) in codebook.iter().enumerate() {
    let d = (cw ^ rx).count_ones();
    if d < best { best = d; winner = idx; }
}
if best > T_MAX_ERRORS { return None; }   // runtime-tunable via /api/bch_t
Some(DecodedNid { nac, duid, n_errors })
```

About 1 ms per frame on the Zynq Cortex-A9 — fine at 4800 sym/s (one NID every ~35 ms minimum).

### 3.1 DUID dispatch

`duid = (16-bit recovered payload) & 0xF`. [types.rs](../p25-httpd/src/p25/types.rs) has the enum:

| DUID | Name     | Meaning                            |
|-----:|----------|------------------------------------|
| 0x0  | HDU      | Header Data Unit (call start)      |
| 0x3  | TDU      | Termination Data Unit (call end)   |
| 0x5  | LDU1     | Voice + Link Control               |
| 0x7  | TSBK     | Control channel signaling block    |
| 0xA  | LDU2     | Voice + Encryption Sync Signature  |
| 0xF  | TDU_LC   | TDU with Link Control              |

On a valid DUID the decoder transitions to `ReadingDataUnit` with the correct expected body length. On an unknown DUID it bumps `nid_invalid_duid` and goes back to hunting — this is the most common "decoder is just noise" symptom.

---

## 4. Per-DUID Body Handling

### 4.1 TSBK (control channel) — the decoder's main job on the control chain

Pipeline: **strip status dibits → de-interleave → 1/2-rate Viterbi → CRC-16 → opcode dispatch**.

Multi-block is supported (Phase 6F.3). After stripping status dibits, the body is either 98, 196, or 294 trellis-coded dibits, yielding 12, 24, or 36 bytes:

| Blocks | Raw dibits | Status dibits                     | Null pad | Trellis payload  |
|-------:|-----------:|-----------------------------------|---------:|------------------|
| 1      | 123        | 4 @ {13,49,85,121}                | 21       |  98 →  12 bytes  |
| 2      | 231        | 7 @ {13,49,…,229}                 | 28       | 196 →  24 bytes  |
| 3      | 303        | 9 @ {13,49,…,301}                 |  0       | 294 →  36 bytes  |

**Trellis Viterbi** lives in [p25/fec.rs](../p25-httpd/src/p25/fec.rs). A 4-state, 4-input/4-output 1/2-rate code. The critical correctness detail: the trellis input must be de-interleaved against TIA-102 BAAA Table 7-7 **before** the Viterbi pass — without that, the decoder finds Hamming-random paths. Constant table is `DATA_DEINTERLEAVE`; the bug that drove this fix was in Phase 6F.2j (2026-04-11).

Step by step:

1. 98 dibits → 196 bits (MSB-first within each dibit).
2. Permute via `DATA_DEINTERLEAVE[i]` into de-interleaved order.
3. Repack into 49 × 4-bit symbols.
4. Viterbi over 4-state trellis, start state 0, Hamming distance on 4-bit symbols as branch metric, using `TRANSITION_MATRIX[prev][curr]` for expected outputs.
5. Traceback from state 0 (the encoder's flush-input guarantees this is the MLE endpoint).
6. Keep the 48 two-bit inputs (drop the 49th flush bit), pack into 12 bytes.

**CRC-16** (TIA-102 BAAA): [tsbk.rs](../p25-httpd/src/p25/tsbk.rs) checks both plain and XOR'd (`^ 0xFFFF`) conventions — both appear in the wild.

**Opcode dispatch:** TSBK has ~40 defined opcodes. The ones that drive the system:

| Opcode | Name              | Effect                                                      |
|-------:|-------------------|-------------------------------------------------------------|
| 0x00   | GRP_VCH_GRANT     | New voice grant — `TrafficManager::handle_grant`            |
| 0x02   | GRP_VCH_GRANT_UPD | Grant refresh / duplicate                                   |
| 0x04   | UU_VCH_GRANT      | Unit-to-unit grant                                          |
| 0x3A   | IDEN_UP           | Frequency band definition (needed to turn channel→Hz)       |
| 0x3B   | SYS_SRV_BCT       | System service info                                         |
| 0x3D   | ADJ_SITE_BCT      | Adjacent site broadcast                                     |
| 0x3F   | NET_STS_BCT       | WACN + system ID (feeds `/api/system`)                      |

All opcodes are parsed — even ones we don't act on — so [/api/recent_tsbks](P25_API.md) shows the full picture for debugging.

### 4.2 HDU — Header Data Unit

**Body:** 648 bits of FEC'd payload, becoming 120 data bits (20 hexbits) after two FEC layers.

**FEC stack:**

- **Inner:** Golay(18,6,8) per hexbit. 36 codewords of 18 bits each. Implementation: [voice_frame.rs:228 golay18_correct](../p25-httpd/src/p25/voice_frame.rs#L228). Corrects up to t=3 per hexbit.
- **Outer:** **Reed-Solomon(63,47,17) shortened to (36,20,17)** over GF(2^6) with primitive poly `x^6 + x + 1`. Corrects up to t=8 hexbit errors. Implementation: [rs_63_47_17.rs](../p25-httpd/src/p25/rs_63_47_17.rs), delegating to the shared decoder in [rs_p25.rs](../p25-httpd/src/p25/rs_p25.rs).

**Payload (120 bits):**

- Message Indicator (72 bits)
- Manufacturer ID (8 bits)
- Algorithm ID (8 bits) — `0x80` = unencrypted
- Key ID (16 bits)
- Talkgroup (16 bits)

**Handler:** `ImbeForwarder::on_hdu` in [main.rs](../p25-httpd/src/main.rs) calls [voice_frame::parse_hdu_body](../p25-httpd/src/p25/voice_frame.rs#L1074). If `is_encrypted()` is true it latches `call_encrypted = true` (sticky until TDU). It also emits a `TRF_HDU_INFO` event on `/ws/events` with the full metadata and fires a `CallBoundary::HduStart` so the recorder can finalise the previous call.

### 4.3 LDU1 — Voice + Link Control

**Body:** 1568 bits (after status-dibit stripping), containing:

- **9 IMBE frames** at fixed bit positions `[0, 144, 328, 512, 696, 880, 1064, 1248, 1424]`. Each is 144 raw bits = 18 bytes, passed straight to the vocoder with no FEC applied by the PS (the vocoder's Golay/Hamming/derand layers handle it).
- **Link Control Word (LCW) = 72 data bits**, FEC'd as Hamming(10,6,3) × 12 codewords (120 bits) + Reed-Solomon(24,12,13) over GF(2^6) (144 bits total).

**FEC implementations:**

- **Hamming(10,6,3)** — [voice_frame.rs:100](../p25-httpd/src/p25/voice_frame.rs#L100) (`hamming10_correct`; syndrome helper at [:76](../p25-httpd/src/p25/voice_frame.rs#L76)). Single-bit correction via syndrome table.
- **RS(24,12,13)** — [rs_24_12_13.rs](../p25-httpd/src/p25/rs_24_12_13.rs) → shared BM in [rs_p25.rs](../p25-httpd/src/p25/rs_p25.rs). Corrects t=6 hexbit errors.

**Handler:** `ImbeForwarder::on_ldu1`:

1. Bump counters (`ldu1_count`, `touch_imbe(9)`).
2. `forward_frames(frames)` — try-send into an mpsc queue to the vocoder task.
3. `parse_ldu1_source(body_raw)` — recover the source radio ID and emit a `CallBoundary::TdulcComplete` event the recorder consumes for per-speaker filename stamping.

### 4.4 LDU2 — Voice + Encryption Sync Signature (ESS)

Same bit layout as LDU1 (9 IMBE frames at the same offsets). The non-voice payload differs:

- **ESS = 96 bits**, FEC'd as Hamming(10,6,3) × 16 + **Reed-Solomon(24,16,9)** (corrects t=4 hexbit errors).
- Content: Algorithm ID (8), Key ID (16), Message Indicator (72).

**RS(24,16,9):** [rs_24_16_9.rs](../p25-httpd/src/p25/rs_24_16_9.rs) → shared BM in rs_p25.rs.

**Handler:** `ImbeForwarder::on_ldu2` forwards the frames and calls [parse_ldu2_ess](../p25-httpd/src/p25/voice_frame.rs#L1202). If the ESS's algorithm ID says encrypted and we didn't already know it from the HDU or the TSBK grant, it still latches `call_encrypted = true`. Redundant with the HDU in theory; real signals occasionally miss an HDU and LDU2 is the backstop.

### 4.5 TDU — Termination Data Unit

Trivial body (15 dibits, no FEC, no payload). Handler just bumps a counter and feeds `traffic_manager.tdu_received(now)`, which starts a 2 s post-TDU hold window (so a back-to-back speaker on the same TG doesn't bounce the slot).

### 4.6 TDU_LC — TDU with Link Control

Same LC payload shape as LDU1 (72 bits), FEC'd as **Golay(24,12,7) × 12** + **RS(24,12,13)**. Note this is Golay24, not Hamming10 — TDULC protects its LC with a stronger code than LDU1 does.

**Golay(24,12,7):** [voice_frame.rs:172](../p25-httpd/src/p25/voice_frame.rs#L172) (`golay24_correct`; syndrome helper at [:152](../p25-httpd/src/p25/voice_frame.rs#L152)). Syndrome-based, corrects t=3 bits per 24-bit codeword. (Golay18 is a shortened / zero-padded special case of the same decoder.)

TDULC is not one message — it's a family keyed by LCW opcode + MFID. [parse_tdulc_lcw](../p25-httpd/src/p25/voice_frame.rs#L374) dispatches to a variant:

| Variant                              | Opcode | MFID  | Payload                                             |
|--------------------------------------|-------:|------:|-----------------------------------------------------|
| `GroupVoiceChannelUser`              |   0x00 | 0x00  | TG + source radio ID                                |
| `GroupVoiceChannelUpdate`            |   0x02 | 0x00  | Dual TG on channel A/B                              |
| `CallTermination`                    |   0x0F | 0x00  | "System says call is over", may carry BY radio ID   |
| `MotorolaTalkComplete` (vendor)      |   0x0F | 0x90  | **Last-speaker radio ID** — the only way to get the |
|                                      |        |       | final speaker when multiple people keyed the grant  |

**Why MotorolaTalkComplete matters:** on a single TSBK grant, the radios can hand off the mic several times. SDRTrunk breaks a grant into per-speaker recordings using this LCW. We do the same — the handler in main.rs fires `CallBoundary::SpeakerEnd { source: Some(by_radio_id) }` to tell the recorder to split the WAV.

---

## 5. FEC Primitive Inventory

All primitives needed for P25 Phase 1 voice are **implemented in this tree**. There are no remaining stubs — a claim that was true two weeks ago but stopped being true once `rs_p25.rs` landed.

| Code                       | Where                                                                    | Corrects   | Used by       |
|----------------------------|--------------------------------------------------------------------------|-----------:|---------------|
| BCH(63,16,23)              | [lsm/nid_fec.rs](../p25-httpd/src/lsm/nid_fec.rs)                        | t=11 bits  | NID           |
| Hamming(10,6,3)            | [voice_frame.rs:100](../p25-httpd/src/p25/voice_frame.rs#L100)           | t=1 bit    | LDU1 LC, LDU2 ESS |
| Golay(24,12,7)             | [voice_frame.rs:172](../p25-httpd/src/p25/voice_frame.rs#L172)           | t=3 bits   | TDULC LC      |
| Golay(18,6,8)              | [voice_frame.rs:228](../p25-httpd/src/p25/voice_frame.rs#L228)           | t=3 bits   | HDU inner     |
| RS(24,12,13)               | [rs_24_12_13.rs](../p25-httpd/src/p25/rs_24_12_13.rs) + rs_p25.rs        | t=6 hexbits| LDU1 LC, TDULC LC |
| RS(24,16,9)                | [rs_24_16_9.rs](../p25-httpd/src/p25/rs_24_16_9.rs) + rs_p25.rs          | t=4 hexbits| LDU2 ESS      |
| RS(63,47,17) → (36,20,17)  | [rs_63_47_17.rs](../p25-httpd/src/p25/rs_63_47_17.rs) + rs_p25.rs        | t=8 hexbits| HDU outer     |
| 1/2-rate Trellis + BAAA de-interleave | [p25/fec.rs](../p25-httpd/src/p25/fec.rs)                     | Viterbi    | TSBK          |
| CRC-16 (plain + xor 0xFFFF)| [p25/tsbk.rs](../p25-httpd/src/p25/tsbk.rs)                              | —          | TSBK          |

All three Reed-Solomon variants are thin shims — each crate module sets `(NN, KK, PRIM_POLY)` consts and calls the shared Berlekamp-Massey in [rs_p25.rs](../p25-httpd/src/p25/rs_p25.rs). Tests for each live in `rs_p25.rs::tests` (`rs_24_16_9_three_errors`, `rs_63_47_17_six_errors`, etc.).

---

## 6. IMBE → PCM → Audio

### 6.1 IMBE frames

An LDU hands the PS nine 144-bit frames. The wire format is the literal IMBE bit layout used by mbelib/JMBE — Golay(23,12,7) on the most-significant bits, Hamming(15,11) on the mid-bits, derandomizer on the tail. We don't touch any of that in the PS P25 code — the vocoder eats 18-byte frames whole.

### 6.2 JMBE vocoder

[vocoder/mod.rs](../p25-httpd/src/vocoder/mod.rs) wraps [jmbe](../p25-httpd/src/jmbe/mod.rs) — a pure-Rust port of mbelib with spectral enhancement. mbelib FFI bindings were removed on 2026-04-17; `mbelib-sys` is still a workspace member but only for the `SAMPLES_PER_FRAME = 160` constant.

Per-frame: 18 input bytes → `[f32; 160]` → scaled to `[i16; 160]`. At 20 ms per frame, 9 frames per LDU = 180 ms of 8 kHz mono PCM per LDU.

### 6.3 Forwarding, gating, silence

`ImbeForwarder` in [main.rs](../p25-httpd/src/main.rs) owns the policy on whether to vocode a given frame group:

1. **TG = 0 gate** — follower is idle → drop (`imbe_frames_dropped_idle++`).
2. **Channel full** — mpsc `try_send` back-pressure → drop (`imbe_frames_dropped++`).
3. **Encryption gate** — `call_encrypted.load()` true → skip vocoder entirely (`vocoder_frames_encrypted++`).
4. **Silent frame suppression (Phase 10.6)** — after JMBE, if `max(|pcm|) < 500` treat as uncorrectable and don't broadcast (`vocoder_frames_silent_suppressed++`).

Anything that survives those four gates gets wrapped in an `AudioChunk { pcm, talkgroup, source, timestamp }` and pushed to a `tokio::sync::broadcast::Sender<AudioChunk>`. Two consumers subscribe:

- The WebSocket handler for `/ws/audio`.
- The recorder task.

### 6.4 PCM AGC

2026-04-19: a simple peak-hold AGC runs on the broadcast path before it reaches the WebSocket, so live-player volume tracks varying radio loudness. Recorder gets pre-AGC samples. This is the silent-pass-through + PCM-AGC change from commit `e700351`.

---

## 7. Call / Grant Handling — TrafficManager

[p25/traffic_manager.rs](../p25-httpd/src/p25/traffic_manager.rs). Three states:

```rust
enum TrafficState {
    Idle,
    Acquiring { channel, talkgroup, frequency_hz, started },
    Active    { channel, talkgroup, frequency_hz, started },
}
```

### 7.1 Grant arrival path

The control decoder stuffs every fresh `GRP_VCH_GRANT` into a `HashMap<Channel, GrantInfo>` and fires it on `grant_event_tx`. A 50 ms polling task in main.rs snapshots the map, picks the newest grant, and calls `handle_grant`.

`handle_grant` gates on:

1. **Monitor list** — if a TG filter is active and this TG isn't on it, skip.
2. **Encrypted TG list** — preemptively skip known encrypted TGs.
3. **Dedup window** — 2 s same-(TG, freq) tuple → count as update, don't retune.
4. **Retune** — compute NCO for `(grant_freq - rx_lo) / sample_rate`, write to FPGA, enable traffic demod.
5. **State transition** → `Acquiring`.

### 7.2 Traffic-chain DUID dispatch

Meanwhile, the traffic chain is independently running its own decoder on `traffic_lsm_dibit_dma`. When it decodes DUIDs, a heartbeat task in main.rs feeds them back to the traffic manager:

- HDU → `hdu_received(now)` — clears post-TDU hold, bumps counters.
- LDU1 / LDU2 → `note_activity()` — renews liveness watchdog.
- TDU / TDU_LC → `tdu_received(now)` — arms 2 s post-TDU hold window.

### 7.3 Post-TDU hold

Matches SDRTrunk PR #2010 semantics: TDU doesn't immediately release the slot. The decoder stays locked to the same TG for 2 s; if another HDU (new speaker, same TG) arrives in that window, the hold cancels and we continue. Otherwise the hold expires and the state goes back to Idle.

### 7.4 Timeouts

The polling loop also checks an activity timeout: if `TrafficState::Active` and no LDU/HDU in 2 s, force back to `Idle`. This is the watchdog that handles radio-side drop-outs.

---

## 8. Recording Pipeline

[recorder.rs](../p25-httpd/src/recorder.rs) is a single tokio task owning two broadcast receivers: `AudioChunk` and `CallBoundary`. It maintains `Option<ActiveCall>` and drives a deterministic state machine:

| Event                                   | Idle       | Active (same TG)                           | Active (different TG)              |
|-----------------------------------------|------------|--------------------------------------------|------------------------------------|
| `AudioChunk { tg }` (tg≠0)              | start new  | append                                     | finalise old, start new            |
| `CallBoundary::HduStart`                | no-op      | finalise, go idle                          | finalise, go idle                  |
| `CallBoundary::SpeakerEnd { source }`   | drop (log) | stamp `active.source`, **then finalise**   | drop (TG-mismatch guard)           |
| `CallBoundary::TdulcComplete { source }`| drop (log) | stamp `active.source` (no finalise)        | stamp `active.source`              |
| Grace timeout (1.5 s no chunks)         | no-op      | finalise, go idle                          | —                                  |

On finalise the task writes a canonical PCM-16 mono 8 kHz WAV to `/tmp/p25_recordings/`:

- With source: `rec_<start_unix_ms>_<id>_tg<TG>_from<source>.wav`
- Without: `rec_<start_unix_ms>_<id>_tg<TG>.wav`

And indexes it in a `VecDeque<RecordingEntry>` exposed through the REST API.

**Per-recording event log** (2026-04-19, commit 9d9895b): each recording gets its own filtered slice of the structured event ring, reachable at `/api/recordings/{id}/events`. The log captures every DUID arrival, every `CallBoundary`, every encryption-state change — the timeline you need to reconstruct whether a recording should have split or not.

**Split triggers in place as of 2026-04-19 (commit 9d9895b):**

- `HduStart` finalises the active call and goes idle (next PCM chunk opens a new `ActiveCall`).
- `SpeakerEnd` (Motorola `TALK_COMPLETE` via TDU_LC) stamps source then finalises, so per-speaker filenames land even when the new speaker keys up inside the 1.5 s grace window.
- `TdulcComplete` is a mid-call source stamp only — it does **not** finalise; the source field on the current recording is updated so the eventual filename reflects the last-known speaker.

**Residual gap** ([project_recordings_span_two_sources.md](../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/project_recordings_span_two_sources.md)): on sites that do **not** emit Motorola `TALK_COMPLETE` (vendor MFID 0x90), a single grant that carries A→B turn-taking still produces one WAV. There is no standards-mandated mid-call signal for "speaker changed but the TG didn't" — both we and SDRTrunk rely on the Motorola vendor LCW for this.

---

## 9. HTTP / WebSocket Surface

Full reference: [P25_API.md](P25_API.md). This section is just the map.

Router construction: [httpd/mod.rs](../p25-httpd/src/httpd/mod.rs).
Endpoint catalogue (self-describing, drives dashboard): [httpd/api/system.rs](../p25-httpd/src/httpd/api/system.rs).

### 9.1 REST

| Module (src/httpd/api/) | Endpoints                                                                 |
|-------------------------|---------------------------------------------------------------------------|
| `chain.rs`              | `/api/control_dibit_capture*`, `/api/traffic_dibit_capture*`, `/api/*_iq_dump`, `/api/nid_capture` |
| `radio.rs`              | `/api/system`, `/api/grants`, `/api/bands`, `/api/stats`, `/api/hdl_lsm`, `/api/irq_stats` |
| `traffic.rs`            | `/api/traffic`, `/api/imbe_dump`, `/api/audio_test`                       |
| `tuning.rs`             | `/api/sync_tune`, `/api/bch_t`, `/api/decoder_reset`, `/api/reinit`, `/api/rx_gain`, `/api/modulation` |
| `talkgroups.rs`         | `/api/monitor`, `/api/aliases`, `/api/encrypted_tgs`, `/api/grant_map`    |
| `history.rs`            | `/api/log`, `/api/recordings`, `/api/recordings/{id}.wav`, `/api/recordings/{id}/events`, `/api/recent_tsbks`, `/api/tsbk_opcodes` |
| `debug.rs`              | `/api/spectrum`, `/api/constellation`                                     |
| `system.rs`             | `/api/sys_health`, `/api/endpoints`, `/api/set_time`                      |

### 9.2 WebSockets

| URL               | Payload                                      | Source                              |
|-------------------|----------------------------------------------|-------------------------------------|
| `/ws/events`      | JSON text (one event per message)            | `app.event_tx` broadcast            |
| `/ws/audio`       | Binary `[TG(2) \| SRC(4) \| LEN(4) \| PCM16]`| `app.audio_tx` broadcast            |
| `/ws/iq`          | Binary complex IQ                            | post-DDC IQ DMA                     |
| `/ws/constellation` | Binary complex samples, post-symbol-timing | traffic IQ, decimated to symbol rate|

Event kinds carried on `/ws/events` (partial list):

```text
GRANT              — new grant decoded (opcode, TG, channel, freq_hz)
TRF_HDU_INFO       — HDU encryption/algorithm/key_id/MI
TRF_LDU1_LC        — LDU1 mid-call source + TG
TRF_LDU2_ESS       — LDU2 encryption refresh
TRF_TDULC_MOT      — Motorola TALK_COMPLETE with BY radio ID
TRF_TDULC          — GVCU tail burst
TRF_TDULC_GVU      — dual-channel update
```

---

## 10. Module Map

```text
p25-httpd/src/
├── main.rs                  — 3768 lines; tokio task orchestration,
│                              ImbeForwarder (the VoiceHandler impl),
│                              DMA pump loops, process startup
│
├── p25/                     — protocol core
│   ├── control_channel.rs   — state machine: Hunting → NID → DataUnit
│   ├── types.rs             — DataUnit enum, GrantInfo, FrequencyBand
│   ├── fec.rs               — 1/2-rate Trellis Viterbi + BAAA de-interleave
│   ├── tsbk.rs              — TSBK opcode parsers (~40 variants)
│   ├── voice_frame.rs       — IMBE extraction, HDU / LDU1 / LDU2 / TDULC
│   │                          FEC + LCW parsing, Golay/Hamming primitives
│   ├── rs_p25.rs            — shared Berlekamp-Massey RS over GF(2^6)
│   ├── rs_24_12_13.rs       — thin shim (LDU1 LC, TDULC LC)
│   ├── rs_24_16_9.rs        — thin shim (LDU2 ESS)
│   ├── rs_63_47_17.rs       — thin shim (HDU outer)
│   ├── traffic_manager.rs   — grant follower state machine
│   ├── events.rs            — P25Event enum for /ws/events
│   └── sdrtrunk_bits_test.rs — golden-vector tests against SDRTrunk dumps
│
├── lsm/                     — LSM demod reference + NID FEC
│   ├── nid_fec.rs           — LIVE, BCH(63,16) decoder for all chains
│   ├── mod.rs, filters.rs,  — retired reference implementations kept
│   │ demod.rs, sync.rs,       for bit-exact comparison against HDL
│   │ ring.rs                   (dead_code allowed)
│
├── jmbe/                    — pure-Rust IMBE decoder
├── vocoder/mod.rs           — JMBE wrapper, emits PCM
│
├── audio.rs                 — AudioChunk + CallBoundary broadcast types
├── recorder.rs              — per-call WAV writer
├── event_log.rs             — structured event ring
├── monitor.rs               — TG monitor list
├── ntp.rs                   — boot-time NTP sync
├── spectrum.rs              — FFT on post-DDC IQ
│
├── fpga.rs                  — IpCore: 8 DMA ring wrappers + register access
├── uio.rs                   — UIO device mapper
├── rxbuffer.rs              — kernel ring buffer adapter
├── iio.rs                   — AD9361 IIO sysfs wrapper
│
└── httpd/                   — HTTP/WS server
    ├── mod.rs               — router, AppState
    └── api/
        ├── chain.rs, radio.rs, traffic.rs, tuning.rs,
        ├── talkgroups.rs, history.rs, debug.rs, system.rs, ws.rs
```

### Sub-crates

- [p25-httpd/p25-json](../p25-httpd/p25-json/) — serde types on the API boundary (`SystemInfo`, `ChannelGrant`, `BandInfo`, `DecoderStats`).
- [p25-httpd/p25-pac](../p25-httpd/p25-pac/) — SVD-generated register PAC for the PL.
- [p25-httpd/mbelib-sys](../p25-httpd/mbelib-sys/) — vestigial; only `SAMPLES_PER_FRAME`.

---

## 11. End-to-End Walkthrough on One Call

Illustrative — the timings are approximate.

```text
t=0 ms    control-chain: TSBK GRP_VCH_GRANT TG=1234 on CH=0x1003 → 860.9875 MHz
          → TrafficManager::handle_grant
          → NCO write, traffic DDC retuned, state=Acquiring
          → /ws/events: { "type": "GRANT", "tg": 1234, "freq_hz": 860987500 }

t=80 ms   traffic-chain sync lock, NID decoded: NAC=0x8A1 DUID=0x0 (HDU)
          → voice_frame::parse_hdu_body → HduHeader { tg: 1234, algo: 0x80, ... }
          → ImbeForwarder::on_hdu:
             - call_encrypted = false
             - CallBoundary::HduStart  → recorder finalises any prior call
             - /ws/events: { "type": "TRF_HDU_INFO", ... }
          → TrafficManager::hdu_received
          → state=Active

t=180 ms  DUID=0x5 (LDU1) arrives
          → 9 IMBE frames extracted, pushed to mpsc
          → LC parsed: source_radio_id=1070003
          → /ws/events: { "type": "TRF_LDU1_LC", "source": 1070003 }
          → Vocoder task:
             - 9× JMBE decode → 9×160 = 1440 PCM samples
             - AudioChunk { pcm, tg: 1234, source: 1070003 } broadcast
          → /ws/audio subscribers get 180 ms of audio
          → Recorder starts ActiveCall { tg: 1234, source: 1070003 }

t=360 ms  DUID=0xA (LDU2), same shape. ESS says unencrypted, no change.

t=540 ms  DUID=0x5 (LDU1) …
  …       pattern repeats every 180 ms for ~3 seconds

t=3.5 s   DUID=0xF (TDU_LC), MFID=0x90, opcode=0x00 (MotorolaTalkComplete)
          → LC parsed: by_radio_id=1070003
          → CallBoundary::SpeakerEnd { source: Some(1070003) }
          → Recorder: finalises ActiveCall, writes
             /tmp/p25_recordings/rec_1745107200000_42_tg1234_from1070003.wav

t=3.5 s + ε   TrafficManager::tdu_received → post-TDU hold armed (2 s)

t=5.5 s   No new HDU → hold expires → state=Idle
          → Control chain continues emitting TSBKs; waiting for next grant
```

---

## 12. Known Gaps & Open Follow-Ups

- **Recordings span two speakers** — HDU-driven split wiring exists but isn't complete. See memory note.
- **C4FM PS decoder retirement** — 2026-04-16 FP&L test showed the HDL LSM chain decodes C4FM fine. The PS `c4fm_demod` path, the `dibit_dma` ring, and the dashboard's C4FM column are all candidates for removal once LSM-decodes-C4FM is confirmed on more sites.
- **HDU payload parsing** — formally present, but the dashboard only uses the encryption bit. TG, MI, algorithm aren't shown outside `/ws/events`.
- **LCW validity filter** — cosmetic log-cleanup task; TDULC idempotency already handles the functional impact.
- **Clean eye plot** — the IQ tap is pre-PLL / pre-timing-recovery, so the browser eye diagram smears. Fix is either an HDL post-rotate tap or client-side timing recovery.

---

## Appendix A — Quick-reference byte/bit budgets

| Frame   | Sync (bits) | NID (bits, post-strip) | Body (raw dibits) | Body (post-strip bits) | Payload (bits) |
|---------|------------:|-----------------------:|------------------:|-----------------------:|---------------:|
| HDU     | 48          | 64 → 16 data           | 330               | 648                    | 120            |
| LDU1    | 48          | 64 → 16 data           | 807 (incl. status)| 1568                   | 9×144 + 72 LC  |
| LDU2    | 48          | 64 → 16 data           | 807 (incl. status)| 1568                   | 9×144 + 96 ESS |
| TDU     | 48          | 64 → 16 data           | 15                | 28                     | 0              |
| TDU_LC  | 48          | 64 → 16 data           | 159 (incl. status)| 288                    | 72 LC          |
| TSBK×1  | 48          | 64 → 16 data           | 123               | 196                    | 96 (12 bytes)  |
| TSBK×3  | 48          | 64 → 16 data           | 303               | 588                    | 288 (36 bytes) |
