# 082 — Bake A: the radio core 2.0.0, every mode's plumbing

**Date:** 2026-10-04. **Branch:** fishball-p25. **Bake required:** YES (core 2.0.0, product
"rad1"; the scanner of the same commit reads 2.x only, so core and scanner ship together).

**Status:** on unit A (image `2026-10-04-bake-a1`). Every gate run is met but the TV scan, which
named 5 stations on what looks like another antenna than 081's (status log); the burst-timing
gate's tooling is built and its run is left for Andy.

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

`common_edge.py` is a generator per consumer, with `keep` on its registers so that synthesis
leaves the copies apart and each is placed beside its consumer: each of the FFT's five twiddle
multipliers, the integrator, each lane's DDC (and the FFT window's 2x edge). The twiddles have one
each because the first build gave the FFT one: synthesis merged the five multipliers' equal
derived registers (`common_edge_q`/`_qq`) into twiddle4's, which then drove all five across the
FFT and held clk3x at +0.307 ns. `TwiddleEdgeFFT` (`spectrum.py`) is upstream's FFT with an edge
input per twiddle multiplier.

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
  (not core 0.3.0's LSM banks and a `commit <TBD>`); the TX rate counter's and `sdr_reset`'s
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

## The burst timing check

The gate "one burst at the same time in a frame's header, a lane's packets and a capture" needs a
transmitter keyed by hand, so it is a tool Andy runs: `tools/burst_timing.py --freq 433.92`
(`POST /api/v1/diag/burst` in the scanner, `services::burst`).

- **The radio:** taken as a scan takes it (the live site pauses); the 2 MSPS preset with the burst
  300 kHz above the LO, the spectrometer at one transform a frame (a frame every 2 ms, which the
  spectrum's reader keeps up with), every frame's band (±150 kHz) seen by the reader's tap.
- **A first press** shows where the transmitter really is: a fob may sit tens of kHz from its
  nominal frequency, outside a lane's channel filter. The control lane moves there.
- **The capture** then runs; the floors are learned for a second; the next press, seen in the
  frames, triggers it, and it stops two sub-buffers later, so the ring keeps 1.7 s of lead-in.
- **Each start** is the AD9361 sample index where the power first rises 15 dB above its floor:
  the capture's in 64-sample blocks, the frame's header index, the lane's in blocks of four lane
  samples less the DDC's delay (from its preset).
- **Pass:** the capture's start within one transform (4096 samples) of the lane's, and in the
  frame's transform or the one before (where the window hides a burst's start). The tool also
  reports frames the reader missed and whether the capture lost words.
- The lane blocks the streams hand out carry the sample index of their first sample (`Block::index`).

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

## The bitstream

Vivado 2023.2, `build_fpga_p25_pretty.sh`; XSA sha256 `843aae06…`.

**Timing** (worst setup slack; worst hold +0.018 ns, 3a +0.010):

| Paths | 3a (1.0.0) | First 2.0.0 build | 2.0.0 |
|-------|-----------:|------------------:|------:|
| clk3x (187.5 MHz) | +0.091 | +0.307 | **+0.605** |
| sync to clk3x | +0.503 | +0.270 | **+0.480** (the design's worst) |
| sync (62.5 MHz) | +3.571 | +0.924 | +1.991 |
| AXI-Lite (100 MHz) | +0.509 | +1.040 | +0.929 |

- **clk3x** meets the +0.4 ns target. In the first build the FFT had one generator for its five
  twiddle multipliers, and synthesis merged their derived registers (above): twiddle4's
  `common_edge_q` drove every twiddle's DSP, +0.307. With a generator each, the worst clk3x paths
  are the lanes' and twiddles' own pulses into their DSPs.
- **sync to clk3x** is a lane mixer's NCO register into its DSP's D port through the 3x
  multiplexer: Maia's `Mixer`, unchanged. Its slack follows placement: lane 0's +0.527 in 3a,
  lane 1's +0.270 in the first build (3.5 ns of routing from the register to the DSP), +0.480
  here.
- **sync's** worst paths are the domain's reset fanning out to the lane banks (14 ns of routing in
  the first build); 2.0.0 has more registers on that reset (the capture's and the spectrum's).

**Utilization** (the route hook's `utilization_hier.rpt`; the core's blocks):

| Block | LUTs | FFs | RAMB36 | RAMB18 | DSP | 3a |
|-------|-----:|----:|-------:|-------:|----:|----|
| A lane's DDC | 390-398 | 575-580 | 0 | 10 | 11 | 396 / 575 |
| A lane's packetizer | 702-703 | 1,089 | 2 | 0 | 2 | 703 / 1,089 |
| A lane's registers | 70-72 | 154 | | | | |
| The spectrometer | 2,901 | 2,960 | 20 | 4 | 7 | 2,749 / 2,304 |
| - its FFT | 2,374 | 1,865 | 8 | 4 | 6 | 2,292 |
| - its integrator (`FrameIntegrator`) | 430 | 434 | 12 | 0 | 1 | 432 |
| - the header, the index history and the check | 78 | 613 | | | | |
| The capture (packer, DMA, start index, trigger) | 110 | 304 | | | | |
| The lane ring and its DMA | 159 | 77 | | | | |
| The common-edge generators (11) | 2-4 each | 5 each | | | | 8 / 9 (two) |
| **The core** | **7,124** | **10,136** | **28** | **35** | **46** | 6,836 / 9,165, 28, 35, 46 |
| **The design** | 15,251 (28.7 %) | 22,945 (21.6 %) | 38 | 40 | 68 (30.9 %) | 14,958 / 21,974 |

Slices 59.5 % (3a 57.7 %). Bake A added 288 LUTs and 971 FFs to the core and no block RAM or DSP.

## Gates

079's "Every bake's gates", with bake A's own; the numbers are in the status log.

| Gate | Result |
|------|--------|
| Spectrum frames bit-exact against the integrator's model (sim) | **Met** (256 points: modes, alternate, integrations, abort; 4096 points: alternating) |
| Timing with no waiver, clk3x slack reported | **Met:** worst +0.480 ns, clk3x +0.605 (target +0.4) |
| Hierarchical utilization down to the core's blocks, against 3a | **Met** (the core +288 LUTs, +971 FFs, no block RAM or DSP) |
| CLAUDE.md checks | **Met:** scanner host tests 488, ARM check, DMR reference 24,984 of 24,996 with the follower test; scanner-hdl 217; agent 63 + 11, bench host 269 (the P25 recordings not needed: no receiver changed) |
| An hour on Clay County: no packet fault, lost or missed packet; control ≥ 40 messages a second at ≥ 99.8 % | **Met:** 40.3 or more at 100 %, none |
| Clay Electric DMR decoding | **Met** |
| A TV scan naming as many stations as 081's (16) | **Not met: 5** named, at 10-25 dB less input than 081's scan; the decoder's MER edge unchanged. Likely the antenna (above); for Andy |
| fbench's tests that read the core, on the new map | **Met:** `sys.identity`, `xport.p25_ring_prbs`, `xport.p25_ring_lap`, `iface.fpga_loopback` |
| Every spectrum frame read for 10 minutes with no gap | **Met:** 27,467 frames in the hour, none missed |
| One burst at the same time in a frame's header, a lane's packets and a capture | **Tooling built** (`tools/burst_timing.py`); the run needs a key fob or the ESP32-DIV: left for Andy |

## Status log

- **2026-10-04.** Gateware, tests, PS side and bench written. The first bitstream met timing at
  +0.270 ns with clk3x at +0.307: the FFT's twiddles still shared one edge. With an edge each, the
  second met it at +0.480, clk3x +0.605.
- **2026-10-04, the image on unit A** (`2026-10-04-bake-a1`: fishball-p25 b4747b6, tezuka_fw
  f24e01b). Checked before installing: the ramdisk's scanner carries the tag and reads 2.x, the
  DTB has the new carve-outs, BOOT.bin holds the XSA's bitstream. The boot files it replaced are
  in `/mnt/sd/boot_backup_radiocore_atsc1`, the new ones also in `/mnt/sd/boot_new_bakea1`; the
  card's copies matched the build's sha256 before the reboot. A answered 37 s after it.
  - **The core:** version 2.0.0; `/dev/p25-lanes` 512 × 16 KB, `/dev/p25-wideband-spec` 16 ×
    32 KB, `/dev/p25-wideband-iq` 16 × 1 MB; System RAM now skips 0x1900_0000-0x197F_FFFF,
    0x2100_0000-0x2107_FFFF and 0x2200_0000-0x22FF_FFFF.
  - **Frame headers:** consecutive frames' sample indices step by exactly 1,048,576 (256
    transforms of 4096).
  - **fbench, on the 2.0.0 map** (the agent rebuilt at e5565d5 and set up on A with the map):
    `sys.identity` pass; `xport.p25_ring_prbs` pass, 1,920 MB through the capture ring in 60 s
    with no anomaly; `xport.p25_ring_lap` pass, loss from a 450-500 ms reader stall against the
    model's 492 ms; `iface.fpga_loopback` pass, 0 PN errors in 262,144 samples.
  - **An hour on Clay County** (`tools/scanner_live_check.py`, 61 samples): the control channel
    at 40.3 messages a second or more, 100 % passing; lanes 0, 1, 2 178,579 / 177,087 / 178,582
    packets with none lost or missed and no packet fault (60 and 13 packets of an older tag after
    retunes, dropped as designed); 27,467 spectrum frames read with no gap in the sequence and no
    fault; no grant or control block dropped; 365 calls (90 followed, 275 encrypted), 68
    recordings; crystal −0.689 ppm; the scanner 5.5 % of a core (top), 16-21 MB; nothing logged
    as an error.
  - **Clay Electric DMR** (`clay_elec_site_2`, 10 minutes): the control channel at 42.0 messages
    a second or more, 100 % passing; 5 calls, all followed, 26.6 s of voice, 5 recordings; lanes
    0 and 1 29,790 and 25,070 packets with none lost or missed and no packet fault; 4,582
    spectrum frames with no gap.
  - **A TV scan** (`tools/atsc_check.py`, the AGC, `runs/atsc/20261004_164849`): 33 channels in
    57.3 s, 20 8-VSB; naming tried 11 and named **5 RF channels** (14, 19, 20, 21, 30: 22 virtual
    channels, every number and name as the HDHomeRun's), short of 081's 16. RF 11 read as vacant
    (the check's one miss). The five named at 17.9-26.8 dB of MER; the six that failed Reed-Solomon
    on all or nearly all packets at 15.8-17.2 dB (RF 23 at 15.9 dB of carrier to noise found no
    sync).
    - **What differs is the input,** by 10-25 dB on most channels: the measured power of RF 9 was
      −23.6 dBm in 081's scan on A (2026-10-04 02:14, the image before this one) and −48.3 dBm
      now, RF 14 −25.9 and −43.3, RF 28 −35.5 and −51.0, RF 31 −40.4 and −61.6; RF 18-21 stayed
      within a few dB. The capture's MER fell with the spectrum's carrier to noise (RF 28: 39.6 /
      35.6 dB then, 21.1 / 17.0 now).
    - **The decoder's edge did not move:** 081 named its stations at 17.9-37.4 dB of MER and
      failed RF 10 and 11 at 17.2 and 17.0 dB; here it names from 17.9 dB and fails at 17.2 and
      below. The captures it decoded were contiguous (RF 19 and 21: no packet beyond
      Reed-Solomon).
    - **081's 16 were named with A on the VHF/UHF directional antenna aimed at Jacksonville,
      moved over from the HDHomeRun;** on the omni, 081's captures named RF 19, 20 and 21. Today's
      levels look like A is no longer on that antenna. An A/B on today's antenna (the previous
      image is in `/mnt/sd/boot_backup_radiocore_atsc1`) was not run: the rollback was refused by
      this session's permissions. **For Andy:** the TV gate wants the scan again with A on the
      directional antenna (or the A/B).
