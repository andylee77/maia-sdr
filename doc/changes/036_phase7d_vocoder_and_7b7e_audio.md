# Phase 7D: IMBE Vocoder + Phase 7B/7E Audio Pipeline

Date: 2026-04-12

## Summary

Adds IMBE vocoder decoding (raw 144-bit frames → PCM audio), talkgroup
monitor list, live audio streaming endpoints, and the event-driven grant
follower. Two vocoder backends: mbelib (C FFI, fallback) and JMBE (pure
Rust port, primary — better audio quality).

## Phase 7D: Vocoder

### mbelib (C FFI, fallback)

- Vendored mbelib C source in `p25-httpd/mbelib-sys/` (ISC license)
- `cc` build script compiles all 6 `.c` files
- Safe Rust wrapper in `p25-httpd/src/vocoder/mod.rs`
- DSD `iW/iX/iY/iZ` de-interleave tables (from `p25p1_const.h`)

### JMBE (Pure Rust, primary)

- Port of DSheirer/jmbe (GPL-3.0) in `p25-httpd/src/jmbe/mod.rs`
- ~2500 LOC covering the complete IMBE decode pipeline:
  - De-interleave (JMBE's own 144-entry permutation table)
  - FEC: Golay(23,12) with 3-error correction, Hamming(15,11)
  - Derandomizer (PRNG seeded from c0 Golay word)
  - Voice parameter extraction (fundamental frequency, voicing
    decisions, spectral amplitudes via DCT decode)
  - Spectral enhancement (Algorithms #105-116)
  - Adaptive smoothing based on error rate
  - Voiced synthesis with phase tracking (Algorithms #127-141)
  - Unvoiced synthesis via 256-point DFT + band scaling (Alg #117-126)
  - Frame repeat / muting on high error rates
- All lookup tables for L=9 through L=56 (48 entries each for
  StepSizes, QuantizedValueIndexes, HarmonicAllocation, GainIndexes)
- No external dependencies (pure `std`)

### Vocoder task

- `ImbeForwarder` replaces `ImbeCounter`: same atomic counters +
  mpsc channel to vocoder task + encryption state
- Vocoder tokio task reads `[ImbeFrameRaw; 9]` batches, decodes
  via JMBE, pushes `AudioChunk` to broadcast channel
- Vocoder reset on call boundaries (prevents cross-call artifacts)
- Encryption gating: encrypted frames counted but not decoded

### Encryption improvements

- Sticky-true flag: once `call_encrypted` is set, only clears on
  Idle transition (not on grant refreshes)
- TG encryption history (`HashSet<u16>`): remembers which TGs were
  ever seen encrypted. Grant updates for those TGs default to
  encrypted even without service options.
- Dashboard `current_call_encrypted` reads from `ImbeForwarder`
  (single source of truth)

## Phase 7B: Event-Driven Grant Follower + Monitor List

- `p25/events.rs`: typed `P25Event::Grant(GrantEvent)` enum
- `monitor.rs`: `MonitorList` with priority-ordered TG set
- `ControlChannelDecoder` pushes `P25Event::Grant` via mpsc on
  every grant insert (3 sites)
- Grant follower rewritten from 50ms polling to `tokio::select!`
  on mpsc receiver + 200ms timeout tick
- `GET/PUT /api/monitor`: view/replace the monitor list
  (`?add=302`, `?remove=302` for quick curl)
- Grant store cleanup on Idle: removes ended TG's grant so
  Active Grants panel clears immediately

## Phase 7E: Audio Streaming

- `audio.rs`: `AudioChunk` broadcast channel (capacity 256 = ~5s)
- `GET /api/audio`: chunked HTTP stream (raw PCM or `?format=wav`)
- `WS /ws/audio`: binary WebSocket frames (320 bytes = 20ms each)
- WAV header with indeterminate length for VLC compatibility

## Dashboard Changes

- Traffic Channel section: Grant Follower + IMBE + Vocoder cards
- Live Activity moved to bottom of page
- Filter checkboxes for event types (default off for noisy broadcasts)
- Traffic channel DUIDs (HDU/LDU/TDU) in activity feed with channel info
- `GRP_GRANT` summary includes source RadioId
- `flex-direction: column-reverse` scroll fix

## Diagnostic Endpoints

- `GET /api/imbe_dump`: raw IMBE frame ring (last 128, hex + TG + enc)
- `GET /api/audio_test`: decode IMBE ring on-device, return WAV file

## Verification

- JMBE produces audio on every frame (no silence gaps like mbelib)
- Spectral enhancement gives notably better quality than mbelib
- 72 unit tests pass, 3 ignored (WAV generation tests)
- Hardware verified: vocoder_pcm_produced > 0 during clear calls,
  vocoder_frames_encrypted incrementing on TG 402/600

## Files Added

- `p25-httpd/mbelib-sys/` — vendored mbelib C source + Cargo crate
- `p25-httpd/src/jmbe/mod.rs` — JMBE Rust port
- `p25-httpd/src/vocoder/mod.rs` — vocoder wrappers (mbelib + JMBE)
- `p25-httpd/src/audio.rs` — audio broadcast channel
- `p25-httpd/src/monitor.rs` — talkgroup monitor list
- `p25-httpd/src/p25/events.rs` — typed grant events
- `tools/p25_check.py` — renamed from p25_check_phase6f4.py, all endpoints
- `tools/p25_imbe_test.py` — IMBE frame capture tool
- `tools/p25_decode_imbe_capture.py` — offline decode helper
