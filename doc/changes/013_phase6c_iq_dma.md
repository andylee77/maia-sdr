# 013 -- Phase 6C: Post-DDC IQ DMA Path in P25 Gateware

**Date:** 2026-04-09
**Phase:** 6C (FPGA gateware: IQ ring DMA)
**Branch:** fishball-p25

---

## TL;DR

1. **Phase 6A/6B's Python LSM reference is file-based only** — it runs against
   SDRTrunk-captured `.wav` recordings, not live RF off the Fishball antenna.
   To exercise the validated demod against live signal (Phase 6D, Rust port to
   PS) the gateware needs a way to deliver post-DDC IQ samples to DRAM.
2. **Added a third ring DMA inside the P25 IP core** that streams the
   control DDC's post-decimation IQ output (62.5 kSPS, 16-bit signed I/Q) to a
   reserved DDR carve-out, parallel to the existing dibit + traffic dibit DMAs
   and not disturbing them.
3. **New tiny `IQPacker` Amaranth module** mirrors `DibitPacker`'s handshake +
   sticky-overflow conventions and packs two consecutive sample pairs per
   64-bit DMA word.
4. **New 5th AXI-Lite register bank** at byte offset `0x80` exposes
   `iq_dma_status`, `iq_dma_control`, and `iq_next_address`. The bank decoder
   in `p25_top.elaborate()` was widened from `address[3:5]` (2-bit bank field,
   4 banks) to `address[3:6]` (3-bit bank field, 8 banks max). No
   `axi4_awidth` change was required — the existing 7-bit word address has
   plenty of headroom.
5. **HP1 SmartConnect picks up the new master** via one extra
   `ad_mem_hp1_interconnect` line in `system_bd.tcl`. HP1 budget at ~1.7 GB/s
   absorbs the new ~250 KB/s consumer with ~0.015% utilisation.
6. **Verification on Windows host:** seven Amaranth pysim tests cover packing
   (single pair, multiple pairs, signed-extreme two's complement), backpressure
   handshake, and the sticky overflow flag. All seven pass. Full P25Core
   elaboration succeeds; `build_hdl.sh --verilog-only --p25` (run inside the
   project's Docker container) regenerates `p25_core.v` (22046 lines), `p25.svd`
   (with the three new registers at `0x80`/`0x84`/`0x88`), and `p25-pac/src/lib.rs`
   via `svd2rust` cleanly.

This phase ends at "gateware logic verified, Verilog regenerates, register
PAC regenerates". The Vivado bitstream synth + on-hardware smoke test are
queued separately (the bitstream build takes 30-60 min on the host Vivado
install). Phase 6D — Rust port of the LSM demod to PS, fed by this new IQ
ring — is the next step on the ladder.

## Why Phase 6C exists

Phase 6A (commit `e1980aa`, change doc 011) ported SDRTrunk's full LSM chain
(decimator → baseband LPF → RRC matched filter → AGC → PLL → Gardner TED →
slicer → soft+hard sync detector → status-aware NID extractor) to Python in
[tools/p25_lsm_demod.py](tools/p25_lsm_demod.py). Phase 6B (commit `46630d6`,
change doc 012) added a bit-perfect BCH(63,16,11) NID FEC port and integrated
it. Together they hit 100% NAC accuracy (313/313 syncs) on the better-signal
SDRTrunk reference wav.

Both of those run **offline against `.wav` files**. The Python prototype reads
a captured 25 kSPS interleaved-IQ wav from SDRTrunk + Pluto and processes it
end-to-end. The Fishball gateware itself decodes nothing useful on the Clay
County simulcast site because the existing C4FM symbol-rate slicer cannot
demodulate LSM (see the long block comment at the top of
[maia-hdl/p25_hdl/p25_top.py](maia-hdl/p25_hdl/p25_top.py)).

The path forward in [DEVPLAN.md](DEVPLAN.md) is the five-phase ladder:

- **6A** — Python LSM reference ✅
- **6B** — NID BCH FEC ✅
- **6C** — IQ DMA in gateware ← **this change**
- **6D** — Rust port of demod + FEC to PS, validated end-to-end on live RF
- **6E** — HDL port of the demod blocks back to PL

Each phase locks in a fixed reference for the next. Phase 6C says "the PS can
stream raw post-DDC IQ from the FPGA at the right rate and format". Without
this bridge, Phase 6D has no input.

## What changed

### New module: `IQPacker`

[maia-hdl/p25_hdl/iq_packer.py](maia-hdl/p25_hdl/iq_packer.py) — ~120 lines.
Buffers two consecutive `(re_in, im_in)` pairs and emits one 64-bit
AXI4-Stream word per pair-of-strobes. Bit layout::

    bit 63                                                              bit 0
    +-----------------+-----------------+-----------------+-----------------+
    |    im[1] s16    |    re[1] s16    |    im[0] s16    |    re[0] s16    |
    +-----------------+-----------------+-----------------+-----------------+
           63..48            47..32            31..16            15..0
                   sample 1                          sample 0
                   (later)                           (earlier)

Sample 0 is in the low half. The PS-side reader interprets each 64-bit DMA
word as four little-endian `int16`s in the order `re0, im0, re1, im1` —
i.e. the natural byte order of an interleaved-IQ buffer.

Modeled directly on
[maia-hdl/p25_hdl/dibit_packer.py](maia-hdl/p25_hdl/dibit_packer.py) — same
`stream_ready`/`data_valid` handshake, same sticky overflow flag (latches if
`data_valid && !stream_ready`, never self-clears in gateware; the AXI-Lite
`Rsticky` field clears it on PS read).

### New `P25Config` fields

[maia-hdl/p25_hdl/config.py](maia-hdl/p25_hdl/config.py) — added
`iq_dma_address = 0x1900_0000`, `iq_dma_num_buffers_log2 = 3`,
`iq_dma_buffer_size = 0x8000`, the matching `iq_dma_num_buffers` /
`iq_dma_total_size` properties, and an alignment assert in `validate()`.
The block comment spells out the full bandwidth math (62.5 kSPS × 4 B = 250
KB/s, ~128 ms per sub-buffer interrupt, ~1 s of IQ in flight) and the design
rationale.

### `p25_top.py` wiring

[maia-hdl/p25_hdl/p25_top.py](maia-hdl/p25_hdl/p25_top.py):

- New import of `IQPacker`.
- New sticky `iq_dma` bit in `control.interrupts`.
- New `iq_packer = IQPacker()` and `iq_dma = DmaStreamRingWrite(...,
  name='m_axi_iq')` in the constructor, with a comment block explaining
  the third-tap topology.
- New `iq_registers` `Registers` block with three registers
  (`iq_dma_status`, `iq_dma_control`, `iq_next_address`) at offset `0x80` in
  the `RegisterMap`.
- `ports()` extended with `+ self.iq_dma.axi.ports()` (20 new AXI3 master
  signals).
- `elaborate()` body extended with submodule registration, comb wiring for the
  third DDC tap (`iq_packer.re_in/im_in/strobe_in <= ddc.re_out/im_out/strobe_out`),
  packer→DMA stream handshake, enable/interrupt/status register fan-out, a new
  `RegisterCDC` for the iq bank, and the matching sync-side CDC wiring at the
  bottom of the file.
- **Bank decoder widened from 2 bits to 3 bits.** The address-bank slice
  changed from `self.axi4lite.address[3:5]` to `self.axi4lite.address[3:6]`,
  and the per-bank `*_select` literals were promoted from `0b00..0b11` to
  `0b000..0b100`. The `rdata`/`rdone`/`wdone` OR-tree and the
  `i_ren`/`i_wstrobe`/`i_address`/`i_wdata` fan-out got new entries for
  `iq_registers_cdc`.

### Vivado packaging + block design

[maia-hdl/ip/p25-core/package_ip.tcl](maia-hdl/ip/p25-core/package_ip.tcl) —
one new `ipx::associate_bus_interfaces -busif m_axi_iq -clock clk` line
right after the existing `m_axi_traffic` association.

[maia-hdl/projects/fishball7020_p25/system_bd.tcl](maia-hdl/projects/fishball7020_p25/system_bd.tcl)
— one new `ad_mem_hp1_interconnect maia_sdr_clk/clk_out1
p25_core/m_axi_iq` line in the HP1 wiring block. `ad_mem_hp1_interconnect`
is idempotent — repeated calls extend the same SmartConnect rather than
creating a new one — so no other block-design change was needed.

The `ad_cpu_interconnect 0x7C460000 p25_core` line stays as-is —
`axi4_awidth` is unchanged, so the AXI-Lite BAR is still 512 bytes and
Vivado sizes the slave window automatically.

### Address-space master table

[doc/P25_ADDRESS_MAP.md](doc/P25_ADDRESS_MAP.md) — *new file*. Single source
of truth for DDR carve-outs, AXI-Lite register banks, and IRQ assignments
across the whole P25 IP. Per the doc-as-we-go discipline (memory note
`feedback_doc_as_we_go`), this was written *before* the wiring code so the
address-map decisions were committed in writing before they hardened.

The DDR carve-out table now shows:

| Name | Base | Total | Ring depth | Byte rate |
|------|------|-------|------------|-----------|
| `dibit_dma`   | `0x1700_0000` | 32 KB  | ~25 s | ~1.28 KB/s |
| `traffic_dma` | `0x1800_0000` | 32 KB  | ~25 s | ~1.28 KB/s |
| `iq_dma`      | `0x1900_0000` | 256 KB | ~1 s  | ~250 KB/s  |

The register-bank table now shows banks 0-4 occupied (`control`, `sdr`,
`demod`, `traffic`, `iq`) and banks 5-7 free for future expansion.

## Verification

### Pure-Python pysim (Windows host)

[maia-hdl/test/test_iq_packer.py](maia-hdl/test/test_iq_packer.py) — 7 tests
using `amaranth.sim.Simulator`, mirroring the structure of
`test_dibit_packer.py`:

| Test | Coverage |
|------|----------|
| `test_pack_one_pair` | Two strobed (re, im) pairs produce one packed 64-bit word |
| `test_pack_multiple_pairs` | Eight pairs produce four packed words with the right contents |
| `test_negative_values` | Two's complement extremes (-32768, +32767) pack correctly |
| `test_no_output_without_strobe` | No output when strobe stays low for 100 cycles |
| `test_backpressure` | data_valid stays high until stream_ready handshakes |
| `test_overflow_flag` | Overflow latches when a new word arrives while previous stalled |
| `test_overflow_sticky` | Overflow stays set after stream_ready re-asserts (Rsticky semantics) |

All seven pass: `python -m unittest test.test_iq_packer -v` → `Ran 7 tests
in 0.117s — OK`.

### cocotb scaffold (for Linux/Docker CI)

[maia-hdl/test_cocotb/iq_packer/](maia-hdl/test_cocotb/iq_packer/) — full
cocotb scaffold (`Makefile`, `verilog.py`, `tb.v`, `test_iq_packer.py`)
mirroring the existing `test_cocotb/dma_stream/` pattern. Not exercised on
the Windows host (no icarus/cocotb installed locally) but ready to run in
WSL Ubuntu or Docker as a CI step.

### Verilog + SVD + PAC regen

`build_hdl.sh --verilog-only --p25` run inside the project's Python 3.11
Docker container regenerates:

- `maia-hdl/ip/p25-core/default/p25_core.v` (22046 lines, all 20
  `m_axi_iq_*` ports declared)
- `p25-httpd/p25-pac/p25.svd` (17131 bytes, 21 registers, including
  `0x80: iq_dma_status`, `0x84: iq_dma_control`, `0x88: iq_next_address`)
- `p25-httpd/p25-pac/src/lib.rs` (auto-regenerated by `svd2rust v0.33.5`)

All three regenerate cleanly. The auto-regen of the Rust PAC means Phase
6D's Rust port can `use p25_pac::iq;` immediately.

### Pending verification (deferred)

- **Vivado bitstream build** (`build_fpga.bat --p25`) — 30-60 min on the
  host Vivado 2023.2. Will exercise IP packaging (`m_axi_iq` exposed),
  block design (HP1 SmartConnect arbitration across three masters), and
  timing closure (the new packer is trivial, should not affect critical
  path).
- **On-hardware smoke test** — load bitstream, `devmem` to enable
  `iq_dma_control`, mmap `0x1900_0000`, dump a few sub-buffers, feed to
  `tools/p25_lsm_demod.py`, expect NAC accuracy comparable to the
  SDRTrunk wav reference. This is the bridge between Phase 6C and Phase 6D
  — strictly Phase 6C ends at "bitstream loads, registers respond, samples
  land in DDR". Phase 6D will own the proper PS-side mmap + Rust reader
  path.

## Files touched

- **New:** [maia-hdl/p25_hdl/iq_packer.py](maia-hdl/p25_hdl/iq_packer.py)
- **New:** [maia-hdl/test/test_iq_packer.py](maia-hdl/test/test_iq_packer.py)
- **New:** [maia-hdl/test_cocotb/iq_packer/](maia-hdl/test_cocotb/iq_packer/)
  (Makefile, verilog.py, tb.v, test_iq_packer.py)
- **New:** [doc/P25_ADDRESS_MAP.md](doc/P25_ADDRESS_MAP.md)
- **New:** [doc/changes/013_phase6c_iq_dma.md](doc/changes/013_phase6c_iq_dma.md) (this file)
- **Modified:** [maia-hdl/p25_hdl/config.py](maia-hdl/p25_hdl/config.py)
- **Modified:** [maia-hdl/p25_hdl/p25_top.py](maia-hdl/p25_hdl/p25_top.py)
- **Modified:** [maia-hdl/ip/p25-core/package_ip.tcl](maia-hdl/ip/p25-core/package_ip.tcl)
- **Modified:** [maia-hdl/projects/fishball7020_p25/system_bd.tcl](maia-hdl/projects/fishball7020_p25/system_bd.tcl)
- **Regenerated:** [maia-hdl/ip/p25-core/default/p25_core.v](maia-hdl/ip/p25-core/default/p25_core.v)
- **Regenerated:** [p25-httpd/p25-pac/p25.svd](p25-httpd/p25-pac/p25.svd)
- **Regenerated:** [p25-httpd/p25-pac/src/lib.rs](p25-httpd/p25-pac/src/lib.rs)

## What's explicitly out of scope

- **Tezuka device tree** — DDR carve-out for the new ring at `0x1900_0000`
  and any UIO entry are Phase 6D's PS-side wiring. The kernel currently
  doesn't reserve that region; Phase 6D will pin it.
- **`p25-httpd` Rust consumer code** — no caller of the new IQ ring exists
  yet; that *is* Phase 6D.
- **Traffic-channel IQ DMA** / register-selectable mux — explicitly
  considered and rejected. The Phase 6A reference processes the LSM
  control channel, so the IQ tap follows.
- **HDL LSM port (Phase 6E)** — out of scope. The Python and (eventually)
  Rust reference are still the source of truth; the FPGA fabric port comes
  later.
