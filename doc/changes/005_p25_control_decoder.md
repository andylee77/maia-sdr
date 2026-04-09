# 005 -- P25 Phase 2A: Control Channel Decoder (Rust)

**Date:** 2026-04-08
**Phase:** 2A (PS firmware -- TSBK parser + control channel state machine)
**Branch:** fishball-p25

---

## Summary

Implemented the complete P25 control channel software decoder in Rust: TSBK
parser with 6 opcode decoders, Golay(23,12) + Viterbi trellis FEC, TSDU
de-interleaving, and the control channel state machine with system identity /
frequency band / grant tracking. 13 Rust unit tests passing.

## Architecture

```text
DMA buffer (64-bit words)
  -> Unpack 32 dibits per word
  -> Frame sync correlator (48-dibit pattern, Hamming <= 4)
  -> NID extraction (32 dibits, Golay FEC -> NAC + DUID)
  -> Data Unit framing (TSDU = 336 dibits)
  -> TSDU de-interleave (remove status symbols)
  -> Trellis decode (4-state Viterbi, 196 dibits -> 12 bytes per TSBK)
  -> CRC-16 check
  -> TSBK opcode decode
  -> State machine update (system ID, bands, grants)
```

## Files Changed

### New: `p25-httpd/src/p25/fec.rs`

- **GolayDecoder**: Golay(23,12) syndrome computation using generator polynomial 0xC75, up to 3-bit error correction, NID decode helper
- **TrellisDecoder**: 4-state Viterbi decoder, processes 98 dibit pairs (196 dibits) into 12 decoded bytes per TSBK
- **TsduDeinterleaver**: Removes status symbols (every 35th dibit), extracts TSBK blocks (196 dibits each)

### Modified: `p25-httpd/src/p25/types.rs`

- Added `DataUnit::from_duid()` with all 7 DUID values and `length_dibits()` for frame sizing
- Added `Channel::identifier()` / `Channel::number()` for frequency band lookups
- Added `Nac::new()`, `Display` impls for NAC, Talkgroup, RadioId, Channel
- Added frame sync constant

### Modified: `p25-httpd/src/p25/tsbk.rs`

- Full `TsbkBlock::parse()` -> `TsbkBlock::decode()` pipeline
- 6 opcode decoders:
  - `GRP_V_CH_GRANT` (0x00): channel + talkgroup + source radio
  - `GRP_V_CH_GRANT_UPDT` (0x02): two active grants
  - `IDEN_UP` (0x34): frequency band parameters (base, spacing, offset)
  - `NET_STS_BCST` (0x3B): WACN + system ID
  - `RFSS_STS_BCST` (0x3A): RFSS + site ID
  - `ADJ_STS_BCST` (0x3C): neighbor site info
- `FrequencyBand` struct: `channel_frequency()` and `channel_uplink_frequency()`
- CRC-16-CCITT (polynomial 0x1021, init 0xFFFF, final XOR 0xFFFF)

### Modified: `p25-httpd/src/p25/control_channel.rs`

- Full decoder state machine: `Hunting` -> `ReadingNid` -> `ReadingDataUnit`
- `process_tsdu()`: de-interleave -> trellis decode -> CRC -> parse -> state update
- `handle_tsbk()`: updates system identity, frequency bands, active grants
- `channel_to_frequency()`: resolves Channel -> Hz via band table
- `expire_grants()`: garbage-collects stale voice grants
- `SystemIdentity` struct: NAC, WACN, system_id, RFSS, site, LRA, control_channel

### Modified: `p25-httpd/Cargo.toml`

- `pm-remez` made optional (OpenBLAS doesn't build on Windows)

## Test Results

13 Rust tests passing:

- FEC: Golay syndrome, zero-error decode, single-error correction, NID decode, TSDU de-interleave
- TSBK: CRC-16, opcode parsing, GRP_V_CH_GRANT decode, NET_STS_BCST decode, frequency band calc
- Control channel: system identity tracking, frequency band table, grant tracking

## Validation Against Clay County System

- Band 0: base=851006250, spacing=6250, offset=-45000000
- Channel 1593 (control) resolves to 860962500 Hz (860.9625 MHz)
- Channel 1117 (traffic) resolves to 857987500 Hz (857.9875 MHz)
- WACN 0xBEE00 / system 0x8A0 / NAC 0x8A1 correctly parsed

## What's Left for End-to-End

The trellis decoder uses a simplified 4-state model. The exact P25 trellis
constellation mapping (TIA-102.BAAA Table 7-3) will need validation against
real captured data. The Golay decoder currently extracts NAC/DUID from NID bit
positions without full two-codeword Golay decode -- sufficient for initial
testing but should be hardened.
