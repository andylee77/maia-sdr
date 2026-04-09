# Fishball P25

FPGA-based P25 Phase 1 trunking radio for the Fishball Z7020 (Zynq-7020 + AD9361).

Decodes P25 control channels, follows voice grants to traffic channels, and serves a real-time web dashboard showing talkgroup activity. All DSP runs in FPGA fabric; protocol decoding and UI run on the ARM PS cores.

## Block Diagram

```text
┌─────────────────────────────────────────────────────────────────────────┐
│                        Fishball Z7020 (FPGA)                           │
│                                                                        │
│  AD9361 IQ ──► RxIQ CDC ──┬──► Control DDC ──► C4FM Demod ──►         │
│  (12-bit,      (sampling   │   (NCO tune to    (cross-product   Symbol │
│   8 MSPS)       → sync)   │    control CH,     discriminator)   Timing │
│                            │    128x → 62.5k)                   Recovery│
│                            │                                    (Gardner│
│                            │                                     TED)  │
│                            │                         ┌──────────┐      │
│                            │                    ┌────┤  Dibit   ├──┐   │
│                            │                    │    │  Packer  │  │   │
│                            │                    │    └──────────┘  │   │
│                            │                    │                  ▼   │
│                            │                    │            ┌────────┐│
│                            │                    │            │ DMA    ││
│                            │                    │            │ Write  ├┼──► HP1
│                            │                    │            └────────┘│    (DDR3)
│                            │                    │                      │
│                            └──► Traffic DDC ──► C4FM Demod ──►        │
│                                 (NCO tune to    Symbol Timing          │
│                                  traffic CH,    Recovery               │
│                                  PS-retuned)         │                 │
│                                                 ┌────┤  Dibit  ├──┐   │
│                                                 │    │  Packer │  │   │
│                                                 │    └─────────┘  │   │
│                                                 │                 ▼   │
│                                                 │           ┌────────┐│
│  AXI-Lite ◄──────────────────────────────────── │           │ DMA    ├┼──► HP1
│  Registers                                      │           │ Write  ││    (DDR3)
│  (control, DDC, demod, traffic)                 │           └────────┘│
│                                                                        │
└───────────────────────────────────┬─────────────────────────────────────┘
                                    │ AXI-Lite (0x7C460000)
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                        Zynq ARM PS (Rust)                              │
│                                                                        │
│  ┌──────────────┐  ┌──────────────────┐  ┌──────────────────────────┐ │
│  │ AD9361 IIO   │  │ FPGA Register    │  │ P25 Control Channel     │ │
│  │ Driver       │  │ Driver (UIO)     │  │ Decoder                 │ │
│  │ (LO, gain,  │  │ (DDC config,     │  │ • Frame sync + NID      │ │
│  │  sample rate)│  │  demod start,    │  │ • TSDU de-interleave    │ │
│  └──────────────┘  │  traffic retune) │  │ • Trellis + Golay FEC   │ │
│                    └──────────────────┘  │ • TSBK parse (6 opcodes)│ │
│                                          │ • System identity track  │ │
│  ┌──────────────────────────────────┐   │ • Freq band table        │ │
│  │ Traffic Manager                  │   │ • Grant tracking         │ │
│  │ • Grant → NCO word calculation   │   └──────────────────────────┘ │
│  │ • DDC retune (~1 µs)            │                                 │
│  │ • Call lifecycle (acquire/active)│   ┌──────────────────────────┐ │
│  │ • Timeout management            │   │ Web Dashboard (HTTP)     │ │
│  └──────────────────────────────────┘   │ • REST API              │ │
│                                          │ • WebSocket events      │ │
│                                          │ • Talkgroup aliases     │ │
│                                          │ • Frequency map         │ │
│                                          │ • Dark/light theme      │ │
│                                          └──────────────────────────┘ │
└─────────────────────────────────────────────────────────────────────────┘
```

## Components

| Directory | Language | Purpose |
|-----------|----------|---------|
| `p25_hdl/` | Python (Amaranth) | FPGA gateware (DDC, C4FM demod, symbol timing, dibit packer) |
| `ip/p25-core/` | TCL | Vivado IP packaging |
| `projects/fishball7020_p25/` | TCL/Verilog | Vivado project (block design, bitstream) |
| `p25-httpd/` | Rust | PS application (P25 decoder, traffic manager, web UI) |
| `ext/maia-sdr/` | submodule | Maia SDR (provides DDC, DMA, register infrastructure) |
| `test/` | Python | Amaranth simulation tests |

## FPGA Resource Usage

| Component | DSP48 | LUT | BRAM |
|-----------|-------|-----|------|
| AD9361 interface | 0 | ~2K | 2 |
| DDC x2 | 26 | ~6K | 12 |
| C4FM Demod x2 | 4 | ~1K | 0 |
| Symbol Timing x2 | 6 | ~2K | 0 |
| Dibit Packer x2 + DMA | 0 | ~1.6K | 2 |
| AXI + misc | 0 | ~3K | 0 |
| **Total** | **36 (16%)** | **15.6K (29%)** | **16 (11%)** |

## Building

### FPGA Bitstream

```bat
build_fpga.bat
```

Runs on Windows with Vivado 2023.2. Output: `output/system_top.xsa`.
See [BUILD_FPGA.md](BUILD_FPGA.md) for the full build guide.

### PS Firmware (Tezuka Buildroot)

Built via [Tezuka firmware](https://github.com/andylee77/tezuka_fw) on the `fishball-dev` branch.

```bash
# In Tezuka Docker build environment:
make fishball_p25_7020_defconfig && make
```

This cross-compiles p25-httpd for ARM and packages it with the P25 bitstream, kernel, and root filesystem. Flash the output to SD card.

### Running Tests

```bash
# Amaranth simulation tests (14 tests)
python -m pytest test/ -v

# Rust unit tests (16 tests)
cd p25-httpd && cargo test
```

## Target System

Clay County FL P25 (860.9625 MHz control channel):

- 11 LCNs spanning 855.24 - 860.96 MHz (5.725 MHz)
- All channels fit within 8 MHz ADC bandwidth — single LO, no retuning
- WACN: BEE00, System: 8A0, NAC: 8A1

## Status

Phases 0-5 complete. Bitstream builds. p25-httpd has full hardware drivers.
30 tests passing (14 Python + 16 Rust).

Next: device tree changes in Tezuka, then first hardware boot and live P25 test.

See [DEVPLAN.md](DEVPLAN.md) for the full roadmap.
