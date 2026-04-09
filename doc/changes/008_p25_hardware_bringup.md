# 008 -- P25 Hardware Bring-up + Build Pipeline Hardening

**Date:** 2026-04-09
**Phase:** 5 (Tezuka firmware integration + first hardware boot)
**Branch:** fishball-p25 / fishball-dev (Tezuka)

---

## Summary

First successful boot of the P25 firmware on real Fishball Z7020 hardware.
Found and fixed a long chain of issues from build pipeline through FPGA
hardware to software register layout. Switched the dibit DMA from a one-shot
`DmaStreamWrite` to a continuous `DmaStreamRingWrite` so interrupts fire on
sub-buffer completion instead of every 13 minutes.

## Build Pipeline Fixes

### `build_fpga.bat`

- Fixed CMD batch parenthesis escaping in `echo` lines inside `if` blocks.
  Em dashes and unescaped `(...)` were causing CMD to mis-parse the
  closing paren as the end of the if block ("... was unexpected at this
  time" failures).
- Added missing ADI HDL libraries to Step 4: `util_clkdiv` (in `xilinx/`
  subdir), `util_rfifo`, `util_wfifo`. Without these, the pluto base
  design fails to instantiate `util_ad9361_divclk`.
- Added skip-if-built logic for all 9 ADI library packages — incremental
  builds now skip already-built libraries (10x faster repeat builds).
- P25 Verilog generation now delegates to `build_hdl.bat --verilog-only
  --p25` (Docker) instead of running Amaranth directly on Windows. The
  original direct invocation hit NTFS symlink issues that the build_hdl
  Docker setup was specifically designed to avoid.

### `build_hdl.sh` / `build_hdl.bat`

- Added `--p25` and `--p25-config` flags to support P25 Verilog/SVD/PAC
  generation alongside the Maia build.
- Generates `p25.svd` automatically when `--p25` is set (via Python in
  the same Docker container).
- Downloads `svd2rust` binary on first run and regenerates
  `p25-httpd/p25-pac/src/lib.rs` from the new SVD. Cached in the Docker
  volume for subsequent builds.
- Source path validation now checks `p25_hdl/` modules when --p25 is set.

### `.gitattributes`

- Added `*.sh text eol=lf` to enforce Unix line endings on shell scripts.
  Without this, Git was committing scripts with CRLF and Docker bash
  failed with `set: pipefail: invalid option name`.

### Tezuka `build.sh`

- Auto-invalidate `fishball_fpga_p25` package when source XSA is newer
  than the cached package (compares timestamps against `.stamp_rsynced`).
  Same logic for `fishball_fpga_7020`. Eliminates the "I flashed it and
  the FPGA still has the old behavior" confusion.
- Auto-invalidate `p25-httpd` and `maia-httpd` packages when their Rust
  source changes. Walks the read-only mount with `find -newer`.
- Use `find` instead of `ls | head` to avoid `set -e pipefail` aborts
  when no files match.

## Tezuka Firmware Fixes

### Phantom AXI UART Lite Removal (`fishball-p25.dtsi`)

The P25 device tree was inherited from `fishball.dtsi` which has an AXI
UART Lite at `0x42C00000`. The Maia FPGA has this peripheral but the
P25 FPGA does not. At boot the kernel `uartlite_probe` tried to read
the (nonexistent) device and crashed with an imprecise external abort:

```
Unhandled fault: imprecise external abort (0x406)
PC is at uartlite_inbe32+0xc/0x10
```

Fix: removed the `serial@42C00000` node from `fishball-p25.dtsi`.

### P25 DMA Reserved Memory Alignment

The P25 FPGA hardcodes DMA addresses in `p25_hdl/config.py`:

- `dibit_dma_address = 0x17000000`
- `traffic_dma_address = 0x18000000`

But the device tree had `0x16000000` and `0x16008000` (32 KB each) —
inherited from a different design. The DMA was writing to unreserved
DDR, which the kernel happily used for its own purposes, corrupting
both the dibit data and random kernel state.

Fix: device tree now matches FPGA addresses with the new ring buffer
sizes (`reg = <0x17000000 0x8000>` and `<0x18000000 0x8000>`).

## FPGA Hardware Fixes

### Register Map Mismatch (RegisterMap offsets)

The FPGA's register crossbar uses bits [4:3] of the word address to
select which bank a register belongs to:

| Bank bits | FPGA byte offset | Bank        |
|-----------|------------------|-------------|
| 00        | 0x00             | control     |
| 01        | 0x20             | sdr/ddc     |
| 10        | 0x40             | demod       |
| 11        | 0x60             | traffic     |

But the SVD was generated with byte offsets `{0x00, 0x08, 0x20, 0x30}`
because the `RegisterMap` constructor in `p25_top.py` was passed those
values directly. The hardware always banked at the [4:3] positions
regardless of what the SVD said.

Result: `p25-httpd` was reading/writing the wrong addresses. DDC
frequency, DDC enable, and demod enable all targeted registers that
existed in the SVD but didn't exist in the hardware. The DDC was
permanently in its reset state and the dibit pipeline was running on
unfiltered raw 8 MSPS noise.

Fix: updated `RegisterMap` offsets to `{0x00, 0x20, 0x40, 0x60}` to
match the hardware bank decoding. Verified by `devmem` dumping the
full 128-byte register space and comparing register values to
expected positions.

### Clock Domain Crossing for demod_registers and traffic_registers

The `sdr_registers` (DDC config) had a `RegisterCDC` to cross from the
`s_axi_lite` clock domain (100 MHz) to the `sync` domain (62.5 MHz)
where the DDC actually runs. But the `demod_registers` and
`traffic_registers` did not — they used a plain `s_axi_lite_renamer`
and their outputs connected combinationally to the sync-domain DMA.

The DMA `start` Wpulse field lasts exactly one s_axi_lite clock cycle
(10 ns). The sync domain samples at 16 ns intervals. The pulse has
about a 62% chance of being captured on any given attempt.

Confirmed empirically: firing `devmem 0x7C460044 32 0x00000005` once
left the DMA stuck. Firing it 10 times in a row got the DMA running
on most attempts.

Fix: added `demod_registers_cdc` and `traffic_registers_cdc` matching
the existing `sdr_registers_cdc` pattern. All register reads/writes
now properly handshake across the clock domain boundary. The CDC fix
is permanent and removes the need for any retry workaround.

### Continuous Ring DMA (DmaStreamRingWrite)

The original P25 design used `DmaStreamWrite` for dibit/traffic DMA.
This is a one-shot DMA: it writes from the start address to the end
address and then fires `finished` exactly once. At dibit rate
(1280 bytes/sec) and a 1 MB buffer, that's about 13 minutes between
interrupts. `p25-httpd` was stuck in `dibit_waiter.wait().await`
forever, never reading any data.

Fix: ported `DmaStreamRingWrite` from the (never-merged) IQ stream
ring buffer branch in `Downloads/maia-sdr-main/maia-sdr-main/maia-hdl/
maia_hdl/dma.py`. This module:

- Maintains a continuous ring of `2^num_buffers_log2` sub-buffers
- Wraps the AXI write address counter at the end of the ring
- Tracks completion via the B-channel response counter (separate from
  AW issue counter, so the `last_buffer` register only updates when
  DDR writes have actually completed)
- Fires `interrupt` for one sync clock cycle on each sub-buffer
  completion
- Exposes `last_buffer` (init=-1, wraps via natural overflow) so
  software can detect new sub-buffers without polling

P25 config: 8 sub-buffers x 4 KB = 32 KB total ring per channel.
Sub-buffer interrupts every ~3.2 sec.

The `DibitPacker` is already in the `sync` domain so no IqDma-style
wrapper is needed — `DmaStreamRingWrite` connects directly to the
packer's stream output.

### Register Layout Updates for Ring DMA

`demod_control` register simplified from `{start: Wpulse, stop:
Wpulse, demod_enable: RW}` to just `{demod_enable: RW}`. The level
bit drives the ring DMA's `enable` input directly. No more pulse race
conditions.

`demod_status` register gained a `last_buffer` field (3 bits for 8
sub-buffers, init=-1 from hardware). `traffic_demod_status` gained
the same field.

Both `dibit_dma.interrupt` and `traffic_dma.interrupt` now connect to
the existing `interrupts_reg` Rsticky bits.

## Software Fixes (p25-httpd)

### DDC FIR Coefficient Initialization

`p25-httpd` was only programming `set_ddc_frequency` and
`set_ddc_enable` at startup. It never loaded FIR coefficients,
decimation parameters, or operations counts. With operations=0 the
FIR stages effectively pass through raw samples — the C4FM demod
saw 8 MSPS of noise instead of a filtered 62.5 kSPS P25 channel,
the dibit packer overflowed, and decoding never worked.

New `configure_ddc()` method loads:

- **Stage 1 (FIR4DSP):** 48 taps, 200 kHz cutoff, decimate 16x
- **Stage 2 (FIR2DSP):** 32 taps, 50 kHz cutoff, decimate 4x
- **Stage 3 (FIR4DSP):** 64 taps, 8 kHz cutoff, decimate 2x

Total 128x decimation: 8 MSPS -> 62.5 kSPS = 13 samples/symbol at
4800 baud. Coefficients designed with `scipy.signal.firwin` (Kaiser
window), quantized to 18-bit signed, polyphase-reordered to match
the Maia DDC's FIR4DSP folded layout (mirrors the
`impl_set_ddc_fir!` macro in `maia-httpd/src/fpga.rs`).

### Ring DMA Buffer Reader

Rewrote `read_dma_buffers` to use the new `last_buffer` field
instead of polling `next_address`. The reader:

1. Reads `demod_status.last_buffer` (or `traffic_demod_status.last_buffer`)
2. Compares against the previously seen index
3. For each new sub-buffer index between previous+1 and current,
   invalidates the cache line range and pushes the slice
4. Wraps around the ring naturally (modulo `num_buffers`)

First-call behavior: snapshot the current `last_buffer` and return
empty. The hardware initializes `last_buffer` to all-1s, which wraps
to 0 on the first sub-buffer completion, so this avoids spurious
reads of uninitialized buffers.

### Removed Methods

- `demod_start()` / `demod_stop()` — no longer needed, the ring DMA
  starts/stops via the `demod_enable` level bit
- `traffic_demod_start()` / `traffic_demod_stop()` — same

The `set_demod_enable(true)` call in `main.rs` is now sufficient to
start the entire dibit pipeline.

## Files Changed

### maia-sdr (fishball-p25 branch)

| File | Change |
|------|--------|
| `maia-hdl/maia_hdl/dma.py` | Added `DmaStreamRingWrite` class |
| `maia-hdl/p25_hdl/config.py` | Ring buffer params + alignment validation |
| `maia-hdl/p25_hdl/p25_top.py` | Use ring DMA, RegisterCDC for demod/traffic, fixed RegisterMap offsets |
| `p25-httpd/p25-pac/p25.svd` | Regenerated for new register layout |
| `p25-httpd/p25-pac/src/lib.rs` | Regenerated by svd2rust |
| `p25-httpd/src/fpga.rs` | DDC FIR loading, ring buffer reader, removed start/stop |
| `p25-httpd/src/main.rs` | Drop demod_start, single set_demod_enable call |
| `build_fpga.bat` | CMD escaping, missing ADI libs, skip-if-built, --p25 wiring |
| `build_hdl.bat` | --p25, --p25-config flags with multi-arg parsing |
| `build_hdl.sh` | P25 Verilog/SVD generation, svd2rust install |
| `sim_hdl.sh` | Fixed CRLF line endings |
| `.gitattributes` | LF for *.sh files |

### Tezuka (fishball-dev branch)

| File | Change |
|------|--------|
| `board/tezuka/fishball7020/dts/fishball-p25.dtsi` | Remove phantom UART, fix DMA reserved memory |
| `build.sh` | XSA cache invalidation, source change detection for httpds |

## Validation

### First Boot

Booted successfully on real Fishball Z7020 hardware:

- FPGA registers accessible: `devmem 0x7C460000` returns `0x70323566`
  ("p25f" magic)
- AD9361 configured at 858.1 MHz LO, 8 MSPS, AGC slow_attack
- Dibit counter incrementing at ~5000/sec (close to 4800 baud + noise)
- Web UI served on port 8080
- p25-httpd running stably

### SDRTrunk Cross-Validation

Verified the actual P25 signal at 860.9625 MHz exists and decodes
cleanly via PlutoSDR + SDRTrunk:

- NAC: 2209 / 0x8A1
- WACN: 781824 / 0xBEE00
- System: 2208 / 0x8A0
- RFSS: 1, Site: 1
- LCN 11 = control channel (860.9625 MHz)
- TSBKs decoding clean: IDEN_UPDATE, NET_STATUS_BCST,
  RFSS_STATUS_BCST, TDMA_SYNC_BCST, MOTOROLA_SYSTEM_LOADING,
  SEC_CCH_BROADCAST, SNDCP_DCH_ANN_EXP

## Pending

- Verify ring DMA fires interrupts at expected rate after this
  bitstream is flashed
- Verify p25-httpd dibit reader picks up sub-buffers and feeds the
  control channel decoder
- First successful TSBK decode on the Fishball
- Then: traffic channel following test, voice extraction
