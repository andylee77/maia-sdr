# Fishball P25: FPGA P25 Trunking Radio Dev Plan

## Context

Build a P25 Phase 1 trunking radio on the Fishball Z7020 (Zynq-7020 + AD9361).
FPGA handles DSP (channelization, C4FM demod, symbol timing). ARM PS handles
control channel protocol (TSBK parsing, talkgroup tracking, voice grant
management). Uses SDRTrunk as the reference architecture. Reuses Maia SDR's
AD9361 core, DDC, DMA, and register infrastructure from `maia-hdl/maia_hdl/`.

**Phased approach**: Phase 1 = control channel metadata/logging. Phase 2 = voice channel following + audio.
**Channel target**: 1 control + 1 traffic DDC (expandable).

---

## Phase 0: Project Scaffolding

### In-Tree Layout

P25 code lives alongside Maia in the `maia-sdr` repo on the `fishball-p25` branch:

```
maia-sdr/
  maia-hdl/
    maia_hdl/                           # Maia HDL (DDC, DMA, registers -- reused by P25)
    p25_hdl/                            # P25 HDL source
      p25_top.py                        # Top-level IP (like maia_sdr.py)
      c4fm_demod.py                     # C4FM discriminator
      symbol_timing.py                  # Gardner TED + loop filter
      dibit_packer.py                   # Pack dibits for DMA
    ip/p25-core/                        # Vivado IP packaging
    projects/fishball7020_p25/          # Vivado project (TCL, constraints)
    test/                               # Amaranth simulation tests (Maia + P25)
    generate_p25_svd.py                 # SVD generation for register PAC
  p25-httpd/                            # Rust PS application
    p25-pac/                            # SVD-generated register PAC
    p25-json/                           # JSON API types
    src/
      fpga.rs                           # FPGA register driver
      iio.rs                            # AD9361 IIO driver
      p25/
        control_channel.rs              # Control channel state machine
        tsbk.rs                         # TSBK message parser
        traffic_manager.rs              # Voice grant manager
        types.rs                        # NAC, talkgroup, channel types
      httpd/                            # Web UI + REST API
```

### Reuse Strategy

- `maia_hdl.ddc.DDC` -- same 3-stage FIR architecture, different coefficients for P25
- `maia_hdl.dma.DmaStreamWrite` -- DMA to DDR via AXI HP
- `maia_hdl.register` -- register framework for AXI-Lite control
- Rust patterns adapted (not submoduled) from `maia-httpd`: UIO driver, IIO AD9361, SVD workflow

### Build Pipeline

1. Amaranth -> Verilog: `python3 -m p25_hdl.p25_top`
2. Vivado IP packaging: `cd maia-hdl/ip/p25-core && make`
3. Bitstream: `build_fpga.bat --p25`
4. SVD -> Rust PAC: `svd2rust`
5. Rust cross-compile: `cross build --target armv7-unknown-linux-gnueabihf`
6. Tezuka Buildroot integration: XSA + bitstream + Rust binary

---

## Phase 1: FPGA Gateware

### DSP Sample Rate Chain

P25 Phase 1 C4FM: 4800 sym/sec, 12.5 kHz channel, 4FSK +/-1800/+/-600 Hz deviation.

- **AD9361**: 8 MSPS (Tezuka BBPLL: 1024 MHz / 4 = 256 MHz ADC / 32 HB/FIR chain)
- **DDC decimation**: 128x total -> 62.5 kSPS output (~13 samples/symbol)
  - Stage 1: /16 (500 kSPS), Stage 2: /4 (125 kSPS), Stage 3: /2 (62.5 kSPS)
  - Passband: 6.25 kHz, matches P25 channel
  - 13 samp/sym with fractional correction via Gardner loop filter
- **Analog LPF**: 4 MHz (= 8 MSPS / 2), usable RF BW ~8 MHz
- **Reuse Maia DDC directly** (`maia_hdl.ddc.DDC`): same 3-stage FIR architecture, just different coefficients

### New Modules

**C4FM Demodulator** (`maia-hdl/p25_hdl/c4fm_demod.py`):

- Cross-product FM discriminator: `disc[n] = re[n-1]*im[n] - im[n-1]*re[n]`
- Proportional to instantaneous frequency (P25 modulation index is small enough for linear approx)
- Cost: 2 DSP48E1

**Symbol Timing Recovery** (`maia-hdl/p25_hdl/symbol_timing.py`):

- Gardner timing error detector at 13 samp/sym
- PI loop filter (BW ~48 Hz, damping 0.707)
- Linear interpolator + fractional NCO
- Symbol slicer: 2-bit dibit output (4 levels)
- Cost: 2-3 DSP48E1

**Dibit Packer** (`maia-hdl/p25_hdl/dibit_packer.py`):

- Pack 32 dibits into 64-bit words for DMA
- 4800 sym/sec = 150 DMA words/sec (negligible bandwidth)
- Include timestamp words for cross-channel alignment
- Uses `maia_hdl.dma.DmaStreamWrite`

**Frame sync**: Deferred to PS software -- at 4800 sym/sec, ARM can easily do NID correlation

### Top Level (`maia-hdl/p25_hdl/p25_top.py`)

Data flow:

```
AD9361 IQ (12-bit) -> RxIQCDC -> DDC (tune + decimate to 62.5 kSPS)
  -> C4FMDemod (discriminator) -> SymbolTimingRecovery (4800 sym/sec dibits)
  -> DibitPacker -> DmaStreamWrite -> DDR3 -> PS reads via UIO
```

### FPGA Resource Budget

| Component | DSP48 | LUT (est.) | BRAM |
|-----------|-------|------------|------|
| AD9361 interface | 0 | ~2000 | 2 |
| DDC (1x) | 13 | ~3000 | 6 |
| C4FM Demod | 2 | ~500 | 0 |
| Symbol Timing | 3 | ~1000 | 0 |
| Dibit Packer + DMA | 0 | ~800 | 1 |
| AXI + misc | 0 | ~3000 | 0 |
| **Total (1 ch)** | **~18** | **~10300** | **~9** |
| **Total (2 ch)** | **~36** | **~15600** | **~16** |
| **Z7020 capacity** | **220** | **53200** | **140** |
| **Utilization (2 ch)** | **16%** | **29%** | **11%** |

Plenty of headroom for additional channels.

### Vivado Project

Based on `maia-hdl/projects/fishball7020_iio/system_bd.tcl` -- same PS7, AD9361 LVDS,
clocking wizard (62.5/125/187.5 MHz). Replace `maia_sdr` IP with `p25_core` IP.
Same AXI HP port connections for DMA.

---

## Phase 2: PS Firmware (Rust)

### Control Channel Decoder

At 9600 bps, the ARM A9 has trivial CPU load for all protocol processing.

**Dibit stream processing**:

1. Read DMA buffer, unpack dibits
2. NID sync word correlation (48-dibit pattern match)
3. Frame Data Units: HDU, LDU1, LDU2, TDU, TSDU, PDU

**FEC decoding** (all in software):

- Golay(23,12) for NID
- 1/2 rate Trellis Coded Modulation -> Viterbi decoder
- Reed-Solomon RS(36,20) over GF(2^6) for TSBK
- Use Rust crates where available

**TSBK parser** -- critical opcodes:

| Opcode | Name | Purpose |
|--------|------|---------|
| 0x34 | IDEN_UP | Frequency band parameters |
| 0x3B | NET_STS_BCST | WACN, System ID |
| 0x3A | RFSS_STS_BCST | Site ID |
| 0x00 | GRP_V_CH_GRANT | Voice grant (talkgroup + channel) |
| 0x02 | GRP_V_CH_GRANT_UPDT | Grant update |

**State machine**: Track system identity (NAC, WACN, system ID), frequency band table, talkgroup activity, active grants.

### AD9361 Configuration

- RX LO: center of P25 system band (858.1 MHz for Clay County)
- Sample rate: 8 MSPS, BW: ~8 MHz
- Gain: slow AGC
- DDC NCO offset tunes to specific channel within band

### Web UI

- REST API: `/api/system`, `/api/talkgroups`, `/api/grants`
- WebSocket for real-time activity updates
- Simple dashboard: system info, talkgroup table, grant activity log

---

## Phase 3: Traffic Channel (Voice Following)

### Second DDC + Demod Chain

- Add second `DDC` + `C4FMDemod` + `SymbolTimingRecovery` + `DibitPacker` for traffic channel
- Independent register bank for PS to retune on grant detection
- Separate DMA channel to PS
- Additional cost: ~18 DSP48E1, stays well under 30% utilization

### Grant Following Latency

1. TSBK received: ~20ms
2. PS processes grant: ~1ms
3. DDC retune (register write): ~1us
4. FIR flush + sync acquisition: ~40ms
5. **Total: ~60ms** (P25 allows ~200ms -- well within budget)

### Voice Frame Extraction

- Traffic channel decoder: sync to LDU1/LDU2
- Extract 9 IMBE frames per LDU (88 bits each)
- Trellis + RS FEC on voice frames
- Audio codec: deferred (mbelib FFI, codec2, or hardware DVSI chip)

---

## Key Risks & Mitigations

| Risk | Mitigation |
|------|-----------|
| C4FM demod AGC (disc output scales with abs(x)^2) | Digital AGC before demod, or adaptive thresholds in PS |
| FEC correctness (Trellis + RS) | Test against SDRTrunk with same IQ recordings |
| Frequency band table parsing (system-specific) | Test against multiple real P25 system captures |
| Control + traffic channels outside AD9361 BW | Most P25 sites within 3-5 MHz; retune LO if needed (~1ms) |

---

## Verification Strategy

**Amaranth simulation**: Synthetic C4FM IQ -> DDC -> demod -> verify BER at various SNR

**Hardware test**: Record P25 IQ via Maia recorder -> play through P25 pipeline -> compare dibits vs SDRTrunk output

**Rust unit tests**: Known TSBK byte sequences, state machine grant scenarios

**Integration test**: Live P25 system -> control channel decode -> talkgroup display in web UI

---

## Implementation Order

1. **Phase 0**: Repo scaffolding, skeleton `p25_top.py` wrapping Maia DDC. Build bitstream, boot on Fishball. Done
2. **Phase 1A**: `C4FMDemod` + `SymbolTimingRecovery` in Amaranth. Simulation tests with synthetic C4FM. Done
3. **Phase 1B**: Integrate into `p25_top.py`, add dibit packer + DMA. Test on hardware with recorded P25 IQ. Done
4. **Phase 2A**: Rust TSBK parser + control channel state machine. Test with captured dibit streams. Done
5. **Phase 2B**: Web UI, integration test with live P25 system. Done
6. **Phase 3**: Second DDC/demod chain, traffic manager, voice extraction, audio. Done
7. **Phase 4**: Bitstream build with Vivado. Build script (`build_fpga.bat --p25`), XSA export. Done
8. **Phase 5**: Tezuka firmware integration. P25 board config, SD card boot, hardware test. -- NEXT

### Critical Maia Files Referenced

- `maia-hdl/maia_hdl/maia_sdr.py` -- top-level pattern followed
- `maia-hdl/maia_hdl/ddc.py` -- DDC module reused
- `maia-hdl/maia_hdl/dma.py` -- DMA infrastructure reused
- `maia-hdl/maia_hdl/register.py` -- register framework reused
- `maia-hdl/projects/fishball7020_iio/system_bd.tcl` -- Vivado project adapted
- `maia-httpd/src/fpga.rs` -- FPGA driver pattern
- `maia-httpd/src/iio.rs` -- AD9361 IIO driver pattern
- `maia-httpd/src/ddc.rs` -- DDC coefficient design
