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
- `maia_hdl.dma.DmaStreamRingWrite` -- ring-buffer DMA to DDR via AXI HP (8 sub-buffers, interrupt on sub-buffer completion). Ported from the never-merged upstream IQ stream branch.
- `maia_hdl.register` -- register framework for AXI-Lite control (with `RegisterCDC` for crossing from s_axi_lite clock into the sync/DSP clock)
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
- Uses `maia_hdl.dma.DmaStreamRingWrite` (8 x 4 KB sub-buffer ring, interrupt per sub-buffer)

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
8. **Phase 5**: Tezuka firmware integration. P25 board config, SD card boot, hardware test. -- COMPLETE (with redirect to Phase 6 for the demodulator architecture)
   - Done: Build pipeline fixes (Docker Verilog gen, CMD escaping, ADI libs, stale paths, .gitattributes LF, incremental builds)
   - Done: Tezuka firmware fixes (XSA cache invalidation, source change detection, phantom UART removal from DTS)
   - Done: Register map fix (SVD offsets corrected: bank select bits [4:3] -> byte offsets 0x00/0x20/0x40/0x60)
   - Done: DDC FIR coefficient loading in p25-httpd (3-stage, 48+32+64 taps, Kaiser window, 18-bit, >137 dB stopband)
   - Done: First hardware boot -- FPGA registers accessible (product_id 0x70323566), AD9361 at 858.1 MHz / 8 MSPS, dibit counter incrementing, web UI on port 8080
   - Done: SDRTrunk confirmed P25 signal at 860.9625 MHz (NAC:2209, WACN:781824, System:2208)
   - Done: Register CDC fix -- `demod_registers` and `traffic_registers` now cross from s_axi_lite (100 MHz) into sync (62.5 MHz) via `RegisterCDC`, matching the existing `sdr_registers_cdc` pattern. Previously Wpulse start signals had ~38% miss rate due to clock skew.
   - Done: Ring-buffer DMA -- replaced `DmaStreamWrite` with a new `DmaStreamRingWrite` (ported from the unmerged upstream IQ stream branch). 8 x 4 KB sub-buffers = 32 KB ring per channel. Sub-buffer completion fires an interrupt on the AXI B-channel response. `demod_status.last_buffer` (3 bits) tracks which sub-buffer was just written. Runs continuously, no PS restart required.
   - Done: Register layout simplification -- `demod_control` and `traffic_demod_control` collapse to a single `demod_enable` level bit (start/stop Wpulses removed). `demod_status` and `traffic_demod_status` expose the new `last_buffer` field. Other bank offsets unchanged. SVD + PAC regenerated.
   - Done: DMA physical address layout -- FPGA hardcoded reservations updated from 1 MB each to 32 KB ring: `0x17000000` (dibit) and `0x18000000` (traffic). Device tree `reg = <0x17000000 0x8000>` / `<0x18000000 0x8000>` updated to match.
   - Done: Build script automation -- `build_hdl.sh` now auto-generates `p25.svd` and regenerates `p25-pac/src/lib.rs` via a downloaded `svd2rust` binary when `--p25` is passed. `build.sh` (Tezuka) auto-invalidates the FPGA package on XSA timestamp change and the p25-httpd/maia-httpd packages on source change.
   - Done: Build_fpga.bat staleness detection -- `build_fpga.bat --p25` auto-runs Verilog regen via Docker when any `p25_hdl/*.py` or `maia_hdl/*.py` is newer than the cached `p25_core.v`. See change 009.
   - Done: Decoder observability -- p25-httpd stdout now redirected to `/var/log/p25-httpd.log` (was going to /dev/null via `start-stop-daemon -b`); `/api/stats` exposes AGC gain + RSSI; `/api/dibit_dump` exposes inner/outer dibit pct + raw_duid histogram; periodic grant expiry. See change 010.
   - **Redirect to Phase 6:** Live control channel decode verification done -- with the existing C4FM-only gateware AND with `SYNC_THRESHOLD` widened to 10, the on-target decoder produces ~3 sync hits/sec on a known-good control channel but the NIDs decode to random NACs and a near-uniform DUID histogram. Root cause discovered to be that the target (and all in-range) P25 systems are LSM Simulcast, not C4FM. The existing slicer architecture cannot decode LSM. Effort redirected to Phase 6.

9. **Phase 6**: LSM demodulator (Python -> Rust on PS -> HDL on PL). NEW PHASE.

   The Fishball P25 target site is LSM Simulcast (`P25 Phase 1 Simulcast (LSM)`
   per SDRTrunk). All P25 systems within RF range of the user's location are
   LSM. The existing Phase 1 C4FM-only gateware is the wrong architecture for
   the modulation actually being received and cannot be made to work without
   adding two missing blocks: an RRC matched filter and a decision-directed
   PLL for carrier recovery.

   Project mandate: SDRTrunk is the reference implementation. Port
   `P25P1DecoderLSM` and `P25P1DemodulatorLSM`, do not invent algorithms.
   Final destination is PL (FPGA fabric). PS Rust is acceptable as an
   intermediate validation step.

   Three-step ladder, each step validated against the previous one with a
   bit-exact reference vector (the captured `.wav` recording from
   `C:\Users\Andy\SDRTrunk\recordings\` and matching truth log from
   `C:\Users\Andy\SDRTrunk\event_logs\`):

   - **Phase 6A: Python reference port** -- DONE (change 011).
     `tools/p25_lsm_demod.py` is a self-contained ~900-line port of
     SDRTrunk's full LSM chain (decimator, baseband LPF, RRC matched
     filter, demod loop with AGC + PLL + Gardner TED + slicer, soft + hard
     sync detectors, status-aware NID extractor). Validated against
     SDRTrunk truth: 339 sync events vs 335 truth (101.2% recall), 91% at
     Hamming distance 0, NAC = 0x8A1 in 93.5% of detections, DUID = 0x7
     in 95.9% of detections. The remaining ~6% gap is uncorrected NID
     bit errors that BCH(64,16) FEC will close.

   - **Phase 6B: Add BCH(64,16) NID FEC + finish the Python reference.**
     Port `BCH_63_16_23_P25.java` and parent `BCH.java` (Berlekamp-Massey
     decoder over GF(2^6)) to Python. Should bring NAC accuracy to >99.9%.
     Note: the user has an existing pyradio port in their Downloads folder
     with a `decode_p25_nid` function in `fec/reed_solomon.py` that may be
     reusable -- explore before porting from scratch.

   - **Phase 6C: IQ DMA path in the FPGA gateware.** New ring DMA parallel
     to the existing dibit DMA. Streams raw post-DDC IQ to DRAM via the
     same `DmaStreamRingWrite` pattern as the dibit path. Same physical
     memory layout, new device tree entry, new UIO mapping. Lets PS read
     IQ samples directly without changing the existing dibit pipeline.

   - **Phase 6D: Rust LSM demod module in p25-httpd.** Mechanical port of
     the validated Python prototype to Rust, file-by-file with golden
     vector unit tests against frozen Python outputs. New `lsm/` module
     with one Rust file per Python stage. Replaces the dibit reader path
     with an IQ reader path that runs the new demod chain in software on
     the Cortex-A9. Validates end-to-end on the live Fishball using the
     same antenna SDRTrunk uses.

   - **Phase 6E: HDL LSM demod in PL.** Each block ported from Rust to
     Amaranth with cocotb tests against the Rust reference. The
     decimator and FIRs map to existing maia_hdl FIR infrastructure.
     The Gardner+PLL+slicer is the unique work and is fully specified
     by the Python reference. Final goal of the project: pure-PL DSP
     path producing dibits identical to the Rust reference, with the
     PS doing only the post-dibit decode and dashboard.

   Why this ordering: each phase locks in a fixed reference for the next
   one. Phase 6A says "the algorithm is right". Phase 6B says "the FEC is
   right". Phase 6C says "we can stream raw IQ from FPGA to PS". Phase 6D
   says "the Rust port is right". Phase 6E says "the HDL port is right".
   At each step there is one degree of freedom and a known-good target,
   not several entangled unknowns at once.

### Critical Maia Files Referenced

- `maia-hdl/maia_hdl/maia_sdr.py` -- top-level pattern followed
- `maia-hdl/maia_hdl/ddc.py` -- DDC module reused
- `maia-hdl/maia_hdl/dma.py` -- DMA infrastructure reused
- `maia-hdl/maia_hdl/register.py` -- register framework reused
- `maia-hdl/projects/fishball7020_iio/system_bd.tcl` -- Vivado project adapted
- `maia-httpd/src/fpga.rs` -- FPGA driver pattern
- `maia-httpd/src/iio.rs` -- AD9361 IIO driver pattern
- `maia-httpd/src/ddc.rs` -- DDC coefficient design
