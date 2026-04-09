# Fishball P25 -- Changelog

Tracking log for the `main` branch of `andylee77/fishball-p25`.

---

## [2026-04-08] Project Setup

### Repository

- Created `andylee77/fishball-p25` repo for FPGA P25 Phase 1 trunking radio
- Target hardware: Fishball Z7020 (Zynq-7020 + AD9361)
- Submodule: `ext/maia-sdr/` -> `andylee77/maia-sdr` (`fishball-dev` branch)
- Working copy: `C:\Users\Andy\Projects\fishball-p25`

### Project Scaffolding (Phase 0)

- `p25_hdl/` -- Amaranth HDL modules: top-level core, C4FM demod, symbol timing, dibit packer
- `ip/p25-core/` -- Vivado IP packaging (TCL + constraints)
- `projects/fishball7020_p25/` -- Vivado project (block design, constraints, top-level wrapper)
- `p25-httpd/` -- Rust workspace: main crate, `p25-json`, `p25-pac`
- `test/` -- Amaranth simulation tests for C4FM demod, symbol timing, dibit packer
- `generate_p25_svd.py` -- SVD generation script for register PAC
- Build infrastructure: `Makefile`, `pyproject.toml`

### Documentation

- `CLAUDE.md` -- AI workspace rules
- `README.md` -- Project overview and build instructions
- `CHANGELOG.md` -- This file
- `DEVLOG.md` -- Developer reference
- `doc/changes/` -- Detailed change documentation

---

## [2026-04-08] Phase 1A: FPGA DSP Modules

### C4FM Demodulator (`p25_hdl/c4fm_demod.py`)

- Cross-product FM discriminator: `disc[n] = re[n-1]*im[n] - im[n-1]*re[n]`
- 2-stage pipeline: IQ latch -> multiply -> difference -> 18-bit output
- 2 DSP48E1 cost (two 16x16 multiplies)
- 5 simulation tests: DC zero, positive/negative freq, 4-level ordering, strobe gating

### Symbol Timing Recovery (`p25_hdl/symbol_timing.py`)

- Gardner TED with decimating counter at 13 samp/sym (8 MSPS / 128x DDC = 62.5 kSPS)
- PI loop filter (Kp=185, Ki=1 in Q0.16 fixed-point, BW ~48 Hz, damping 0.707)
- Sign-of-midpoint error approximation (avoids full multiply)
- 4-level slicer: P25 dibit mapping (01=+3, 00=+1, 10=-1, 11=-3)
- Timing adjustment: loop filter output drives ±1 counter reload
- 4 simulation tests: known sequence, symbol rate, slicer levels, strobe gating

### Dibit Packer (`p25_hdl/dibit_packer.py`)

- Shift register packs 32 dibits into 64-bit DMA words
- AXI4-Stream handshaking: data_valid / stream_ready backpressure
- Overflow detection flag for stall conditions
- 5 simulation tests: 32-dibit pack, multi-word, no-strobe, backpressure, overflow

### Sample Rate Update

- AD9361 runs at 8 MSPS (Tezuka BBPLL: 1024 MHz / 4 = 256 MHz ADC / 32x HB chain)
- DDC: 128x decimation -> 62.5 kSPS -> ~13 samples/symbol (was 10 at 6.144 MSPS)
- Updated DEVPLAN.md with corrected sample rate chain

---

## [2026-04-08] Phase 1B: Integration + Vivado Prep

### Top-level Integration (`p25_hdl/p25_top.py`)

- Full demod chain wired: DDC -> C4FMDemod -> SymbolTimingRecovery -> DibitPacker -> DmaStreamWrite
- DmaStreamWrite instance for dibit DMA via AXI HP3 to DDR3
- Demod register bank at 0x40: start/stop DMA, dibit_count, demod_overflow, next_address
- Register address space expanded to 6 bits (64 registers) for 4 banks
- AXI port list updated with m_axi_dibit

### Vivado Project Updates

- `system_bd.tcl` -- Replaces maia_sdr IP with p25_core, adds HP3 for dibit DMA
- `system_top.v` -- Board-level wrapper (identical pin mapping to Maia fishball_iio)
- `package_ip.tcl` -- Added m_axi_dibit bus interface + clock association

### SVD + Rust PAC

- SVD generated at `p25-httpd/p25-pac/p25.svd` (13 KB, all register banks)
- Rust PAC generated via `svd2rust --target none` -- compiles clean
- `p25-pac/Cargo.toml` updated with vcell dependency
- Makefile targets: `make svd`, `make pac`

---

## [2026-04-08] Phase 2A: PS Firmware — Control Channel Decoder

### P25 Types and Constants (`p25-httpd/src/p25/types.rs`)

- Dibit, NAC, DataUnit (DUID decode + frame lengths), Channel (identifier/number), Talkgroup, RadioId
- Frame sync constant (48 dibits, 0x5575F5FF77FF)
- Display trait implementations for NAC, Talkgroup, RadioId, Channel

### TSBK Parser (`p25-httpd/src/p25/tsbk.rs`)

- TsbkBlock parser: 12-byte block -> opcode + manufacturer + payload + CRC
- 6 opcode decoders: GRP_V_CH_GRANT (0x00), GRP_V_CH_GRANT_UPDT (0x02), IDEN_UP (0x34), NET_STS_BCST (0x3B), RFSS_STS_BCST (0x3A), ADJ_STS_BCST (0x3C)
- CRC-16-CCITT validation
- FrequencyBand: channel-to-frequency calculation from IDEN_UP parameters
- Validated against Clay County system: band 0 ch 1593 -> 860.9625 MHz

### FEC Decoders (`p25-httpd/src/p25/fec.rs`)

- Golay(23,12) decoder: syndrome computation, up to 3-bit error correction, NID decode
- Trellis decoder: 4-state Viterbi algorithm, 196 dibits -> 12 bytes per TSBK
- TSDU de-interleaver: status symbol removal, TSBK block extraction

### Control Channel State Machine (`p25-httpd/src/p25/control_channel.rs`)

- Decoder states: Hunting -> ReadingNid -> ReadingDataUnit
- Frame sync correlator with Hamming distance threshold (≤4 errors)
- NID extraction with Golay FEC
- Full TSDU pipeline: de-interleave -> trellis decode -> CRC check -> TSBK parse -> state update
- System identity tracking: NAC, WACN, system ID, RFSS, site, LRA
- Frequency band table management (from IDEN_UP)
- Voice grant tracking with frequency resolution and expiry
- DMA word unpacking (64-bit -> 32 dibits)

### Build Fix

- `pm-remez` made optional (OpenBLAS doesn't build on Windows, only needed for FIR design)
- All 13 Rust tests + 14 Python tests passing

---

## [2026-04-08] Phase 2B: Web Dashboard

### REST API (`p25-httpd/src/httpd/mod.rs`)

- `GET /api/system` -- System identity (NAC, WACN, system, RFSS, site)
- `GET /api/grants` -- Active voice grants with talkgroup aliases
- `GET /api/bands` -- Frequency band table from IDEN_UP
- `GET /api/stats` -- Decoder stats (messages, grants, bands, dibit count, overflow)
- `GET /api/aliases` -- Talkgroup alias map (ID -> name)
- `PUT /api/aliases` -- Upload/update alias JSON

### WebSocket (`/ws/events`)

- Real-time TSBK event push via tokio broadcast channel
- Events serialized as TsbkEvent JSON (timestamp, type, summary, talkgroup, frequency)
- Auto-reconnect on disconnect (3 second retry)

### Embedded Dashboard (`/`)

- Live activity feed: WebSocket-driven, newest-first, color-coded by event type
- System identity + decode stats cards (two-column grid)
- Frequency map: 11 LCNs positioned proportionally, active grants highlighted, CC marked
- Active grants table with talkgroup aliases
- Frequency bands table
- Dark/light theme toggle (CSS custom properties, localStorage persistence)
- Talkgroup aliases modal (edit JSON, PUT to server)

### Event Broadcasting (`control_channel.rs`)

- `set_event_tx()` connects decoder to WebSocket broadcast channel
- `tsbk_to_event()` converts each TSBK message type to serializable event
- All 6 opcode types produce formatted WebSocket events

### Shared Types (`p25-json/src/lib.rs`)

- `TsbkEvent`: timestamp + event_type + summary + talkgroup/alias/channel/frequency/source
- `AliasMap`: HashMap<u16, String> for talkgroup aliases
- `DecoderStats`: message count, grant count, bands, dibit count, overflow, DMA address
- Updated `SystemInfo`, `ChannelGrant`, `BandInfo` with alias support

### Wiring (`main.rs`)

- `AppState` with `RwLock<ControlChannelDecoder>` + broadcast channel
- Default RX LO: 858.1 MHz, default control freq: 860.9625 MHz
- Server binds to `0.0.0.0:8080`, verified endpoints respond correctly

---

## [2026-04-08] Phase 3: Traffic Channel + Cleanup

### Second DDC + Demod Chain (`p25_hdl/p25_top.py`)

- Second DDC with independent NCO frequency register for PS-controlled retuning
- Full traffic demod pipeline: C4FM -> symbol timing -> dibit packer -> DMA
- Traffic register bank at 0x30 with DDC + demod controls
- Both DMA channels share HP1 via AXI interconnect

### Removed: Spectrometer + Recorder

- Removed Maia debug tools (unused in P25)
- Freed: HP1/HP2 ports, clk2x domain, ~16 DSP48, ~5K LUT, ~12 BRAM
- Simplified register map: 4 banks (control, sdr, demod, traffic)

### Traffic Manager (`p25-httpd/src/p25/traffic_manager.rs`)

- Grant lifecycle: Idle -> Acquiring -> Active with timeouts
- NCO word calculation: `(target - rx_lo) / sample_rate * 2^28`
- 200ms acquire timeout, 3s call inactivity timeout
- 3 unit tests: NCO calculation, negative offset, grant lifecycle

### Updated Resource Budget

- Total: ~36 DSP48 (16%), ~15.6K LUT (29%), ~16 BRAM (11%)
- Z7020 has ample headroom

---

## Pending / Future

- [ ] Hardware integration test with live Clay County P25 system
- [ ] Voice frame extraction (LDU1/LDU2 -> IMBE frames)
- [ ] Audio codec (mbelib, codec2, or DVSI)
