# 082 — Bake A: the radio core 2.0.0, every mode's plumbing

**Date:** 2026-10-04. **Branch:** fishball-p25. **Bake required:** YES (core 2.0.0, product
"rad1"; the scanner of the same commit reads 2.x only, so core and scanner ship together).

**Status:** in progress (status log below).

## Why

079's study of every mode's needs ("Every mode's needs", "The bakes") proposed bake A: the
plumbing every mode uses, with no new DSP path, and the timing fix that makes room for bakes
B-D. Andy approved 079's recommendations on 2026-10-04: bake A is this change, its SD image on
unit A included; item 5 a mode per frame; item 6 a header in the spectrum's edge bins.

## What changes in the gateware (`scanner-hdl/radio_core`)

### The common edges

Every 3x multiplier's input multiplexers read a common-edge pulse (the clk3x cycle after each
sync edge). 3a had one generator for the core: 368 loads across the die, and synthesis merged the
consumers' equal copies of it and of the registers they derive from it (the integrator's
`common_edge_q`/`_qq` drove the FFT's twiddles). Measured on 3a's routed checkpoint, the pulse's
loads were each lane's mixer 56 (+4 in its input buffer), each of the FFT's five twiddle
multipliers 17, the integrator's power stage 76 and its exponent stage 18.

`common_edge.py` is a generator per consumer: the FFT, the integrator, each lane's DDC (and the
FFT window's 2x edge), with `keep` on its registers so that synthesis leaves the copies apart and
each is placed beside its consumer.

### The register map

The shared banks no longer move with the number of lanes:

| Offset | Bank | Registers |
|---|---|---|
| 0x000 | control (AXI-Lite domain) | `product_id` "rad1", `version` 2.0.0, `control`, `interrupts`, `capabilities` (as 1.0.0) |
| 0x020 | lane ring | `lanes_ring_control`, `lanes_ring_status` (`last_buffer`, 9 bits), `lanes_ring_next_address`, `sample_count_lo` / `_hi`, `adc_clips` |
| 0x040 | spectrum | `spec_control` (`spec_enable` [0], `spec_peak_detect` [1], `spec_abort` [2] pulse, `spec_num_integrations` [12:3], `spec_alternate` [13]); `spec_status` (`spec_last_buffer`, 4 bits); `spec_next_address` |
| 0x060 | capture | `capture_status` (`overflow` [0] read-to-clear, `last_buffer` [4:1], `stopped` [5]); `capture_control` (`enable` [0], `trigger` [1] pulse, `stop_after` [5:2]); `capture_next_address`; `capture_start_lo` / `_hi` (reading the low word latches the high); `capture_buffers` |
| 0x080-0x0FF | kept for later blocks (the 8-VSB, a burst detector); answered with 0 | |
| 0x100 + 0x20 × i | lane i (up to 15 in the 1 KB window) | as 1.0.0's lane banks |

- `spec_overflow` goes: it was never driven.
- The capture's registers are `capture_*` (1.0.0's `wideband_iq_*`); its status keeps
  `overflow` and `last_buffer` in the bits the bench reads.

### The rings (items 3 and 4)

| Device | Base | Size | Sub-buffers |
|---|---|---|---|
| `p25-lanes` | 0x1900_0000 | 8 MB | 512 × 16 KB (13.7 s at three 50 kSPS lanes, 0.67 s at three 1 MSPS lanes) |
| `p25-wideband-spec` | 0x2100_0000 | 512 KB | 16 frames of 32 KB |
| `p25-wideband-iq` | 0x2200_0000 | 16 MB | 16 × 1 MB (as 1.0.0) |

### The spectrum's frames (items 5 and 6)

`spectrum.py` (`SpectrumFrames`) rebuilds Maia's `Spectrometer` from its blocks: the FFT, the
integrator and the DMA that writes each frame to the next sub-buffer. `maia_hdl` is unchanged.

**A mode per frame.** A write of `spec_peak_detect`, `spec_alternate` or `spec_num_integrations`
applies from the next frame; with `spec_alternate` each frame takes the other mode from the frame
before. Upstream's integrator reads `peak_detect` live at two stages, the power stage (CpwrPeak,
whose DSP48 sequence differs between the modes) and the write-back three samples later. A
mode that changes between them spoils the samples in flight: from average to peak, the DSP48's
average sequence leaves `is_greater` low (its comparison step only runs in peak mode) and the
peak write-back then skips those bins, which keep a value from two frames before. No single
switching moment avoids it, so `frame_integrator.py` (`FrameIntegrator`, a subclass of
upstream's integrator) carries each frame's mode with its samples: the power stage and the
write-back each apply the mode of the sample they hold. The power stage takes the earlier of the
two samples it works on in one cycle; they differ only across a frame's start, where the
entering sample is its first transform's and the accumulator it meets reads 0, so both modes
give it the same value.

**The header.** The DMA's read side puts these words in place of bins 0-7, and a check word in
place of bin 4095. Those bins lie at -fs/2, outside the ±0.45 × rate every user of the spectrum
reads.

| Word | Bits | Field |
|---|---|---|
| 0 | 15:0 | magic `0x5346` ("SF") |
| 0 | 19:16 | format (1) |
| 0 | 31:24 | flags: bit 0 `peak` (else average power), bit 1 `aborted` (an abort ended the frame before its integrations were done) |
| 0 | 41:32 | transforms integrated |
| 1 | 63:0 | the AD9361 sample index of the frame's first transform's first input sample |
| 2 | 31:0 | sequence: frames since the core's reset (wraps) |
| 2 | 63:32 | the running count of AD9361 samples at full scale at that first sample |
| 3-7 | | zero |
| bin 4095 | 31:0 | check: the XOR of the frame's other 32-bit halves (the frame's halves XOR to 0) |

The sample index is the lanes' and the capture's clock. The FFT takes input slot 0 of a
transform every 4096th sample since its reset and puts out output slot 0 `fft.delay` samples
later; the core keeps the indices taken at the last input slots 0 and gives a frame the one of
its first transform.

### The capture (item 7)

- **Each enable** (0 to 1) starts the ring afresh once the writes of the capture before have
  finished (microseconds): at its first sub-buffer, `last_buffer` at -1, with
  `capture_start_index` set to the index of the first sample written. `capture_buffers` (the
  sub-buffers completed since the enable) and `stopped` read 0 from the enable on.
- **While `overflow` stays clear,** the k-th sub-buffer since the enable (slot k mod 16) begins
  with sample `start + k × 262,144`.
- **A trigger** (`trigger`, a write pulse, with `stop_after` in the same write) lets the capture
  finish the sub-buffer it is writing and `stop_after` more, then stop at that sub-buffer's end:
  no address past it is issued, so the older sub-buffers keep what they hold. `stopped` says so.
- **AWVALID** stays up until its address is taken, whatever the enable does (an enable that fell
  while an address waited used to drop it, F6).
- Between captures the packer holds nothing, so words no capture will take are not counted as
  lost.

### F2 (`doc/HW_VALIDATION_SUITE.md`)

`IQPacker` pulsed `overflow` whenever the second sample of a pair arrived while a word waited,
even when the DMA took that word in the same cycle. It now pulses only when the waiting word is
lost. The hwval replica's tests that recorded the false pulses now check that the pulses equal
the words lost.

### The Vivado project (`maia-hdl/projects/fishball7020_p25`)

- **XDC:** the CDC comments describe the radio core's banks, interrupts and reset synchronisers
  (not core 0.3.0's LSM banks and a "commit <TBD>"); the TX rate counter's and `sdr_reset`'s
  comments are current. The `manual_decim` false path went: no cell matched it (3a's checkpoint;
  the build raised a critical warning for it), as the P25 block design has no `manual_decim`.
- **Block design:** the clocks' comment says what clk_out2 and clk_out3 feed, without a phase
  number; the HP1 comment has the rings' new sizes.
- **Utilization:** the route hook appends a report on the core's cell to depth 4 under the
  design's, so `utilization_hier.rpt` reaches each lane's DDC stages and packetizer, the
  spectrometer's FFT, integrator and DMA, the rings and the register banks.

## What changes on the PS

- **`hardware::radiocore`** reads 2.x only (`MAJOR_VERSION`): the PAC regenerated from the
  core's SVD; `frame` parses and checks a spectrum frame and blanks its header bins (bins 0-7 take
  bin 8's value, bin 4095 bin 4094's); `read_frames` reads every frame completed since the last
  call, in order; `set_spectrum_mode(peak, alternate)`; the capture's `start_capture`,
  `trigger_capture(stop_after)`, `stop_capture`, `capture_status` (the overflow kept across
  readings) and `read_capture`, which hands each sub-buffer over with the sample index of its
  first sample and counts those the ring overwrote (the newest 14 completed are whole: the next
  sub-buffer may have begun while the last one's write responses are on their way).
- **`radio::spectrum`:** one task reads every frame every 20 ms, checks it and counts frames,
  sequence gaps (`missed`) and faults; it keeps the newest, blanked, for whoever asks next
  (`RadioHw::spectrum` returns it with its header), and a tap sees every frame.
- **`RadioHw::capture`** returns the sample index of its first sample and whether words were
  lost; the capture's first sub-buffer is no longer skipped (it holds only the capture's
  samples).
- **The readback** (`/api/v1/radio`): the spectrum reader's counts with the newest frame's header,
  and the capture's status.

## The bench

- `bench/share/p25_regs.json` is the 2.0.0 map (`radio_core.bench_map`); scanner-hdl's test
  checks it against the gateware.
- The agent's capture-ring commands (`ring check`/`capture` on `p25-wideband`, `txlink`'s FPGA
  loopback) use the `capture_*` names and wait 1 ms after an enable, by which time the ring has
  started again at its first sub-buffer. The agent's and fbench's tests and the agent's smoke run
  name the new offsets.

## tezuka_fw (fishball-dev)

The new XSA, and `fishball-p25.dtsi`'s carve-outs: the lane ring 8 MB (0x1900_0000-0x197F_FFFF),
the spectrum 512 KB (0x2100_0000-0x2107_FFFF), the capture 16 MB as before
(0x2200_0000-0x22FF_FFFF), clear of each other inside the 1 GB of DDR.

## Tests

- **scanner-hdl:**
  - `test_spectrum.py`: random samples through `SpectrumFrames`; the FFT's output checked
    against its model on the input transforms (which also fixes which input sample began each
    transform); every frame the DMA wrote equal, word for word, to the integrator's model on its
    transforms with the header and the check. At 256 points: average frames of 3, one ended
    early by an abort, peak from the frame after a mid-frame write, alternate, then 2
    integrations; at the core's 4096 points (`MAIA_HDL_SLOW_TESTS=1`): alternating frames of one
    transform.
  - `test_capture.py`: the start index and every sub-buffer's samples since an enable, the ring
    wrapping; a re-enable while the last bursts drain (the count 0 at once, the ring at its first
    sub-buffer); a trigger stopping at a sub-buffer's end with nothing addressed after it
    (`stop_after` 0, 1, 2); AWVALID held through disables; the overflow pulses equal to the words
    lost.
  - `test_iq_packer.py`: F2's case both ways; the clear.
  - `test_p25_top.py`: the map, five lanes leaving the shared banks in place, the fields, the
    version and build files, the reset values, the capture's start index through the core.

## Gates

(results in the status log)

## Status log

- **2026-10-04.** Gateware, tests, PS side and bench written; the bitstream build started.
