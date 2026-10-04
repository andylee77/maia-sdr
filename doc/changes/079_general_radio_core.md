# 079 — A general radio core: a channelizer in the PL, every demodulator in software

**Date:** 2026-10-03. **Branch:** fishball-p25. **Bake required:** yes, at step 3 (a new core
and a new Vivado project). Steps 1 and 2 are software and host only.

**Status:** design approved by Andy on 2026-10-03. Steps 1, 3a and 4 are done and on unit A's
image (status log). The gateware for every mode is studied in "Every mode's needs", with bakes
proposed for Andy's decision. The 0.3.0 build is backed up in
`MAIA_SDR/_archive/build_2026-10-03_p25-core-0.3.0/` and tagged `build/p25-core-0.3.0` (the
gateware) and `build/2026-10-01-scanner-image3` (the image, in maia-sdr and tezuka_fw).

## Why

- **The PL holds one demodulator, for one modulation.** P25 core 0.3.0 has three chains, each a
  DDC and an LSM demodulator. Everything matched to SDRTrunk (C4FM, DMR, framing, FEC, vocoders)
  already runs on the PS from IQ.
- **The lanes are unequal.** Chain 1 has an IQ ring and carries P25 and DMR. Chain 2 has none and
  carries P25 LSM only. P25 voice goes only through the gateware LSM, which decodes C4FM sites
  poorly (TSBK CRC on recordings: FPL 42 %, SLERS about 64 %, against 99.4 % and 95.6 % for the
  software C4FM demodulator; CHANGELOG_FORK, 071b).
- **The fabric is full of copies.** Slices are at 91.6 % and DSP48s at 172 of 220. Each chain
  costs 45 DSPs: 11 in the DDC, 4 in the LPF and RRC, 30 in the demodulator. The demodulator gets
  one input sample every ~2,500 clocks and one symbol every ~13,000, so its DSPs are almost always
  idle. A fourth copy does not fit; the limit is the architecture, not the device.
- **Gateware fixes are slow.** A change to a demodulator needs a gateware build and an SD image.
  That cost is what drove the workarounds in the PS code (seeds, PLL clamps and watchdogs, retune
  heuristics).
- **Andy wants a general bitstream** that serves many radio functions, and more channels later.
  SDRTrunk's polyphase channelizer opened 10+ traffic channels on a PC; the PL can do the same work.

## Decision: split each receiver after the channel filter

| Stage | Rate | Where | Why |
|-------|------|-------|-----|
| Channelize, mix, decimate | 6.4-12.8 MSPS | **PL** | Per sample, protocol-agnostic; impossible on the A9s at N channels |
| Channel and matched filters (half-band, LPF, RRC) | 50 → 25 kSPS | PS now, **PL later** (step 5) | About 2 % of a core per software receiver since the FIR speed-up; only needed in the PL when lanes outgrow the CPU |
| LSM: AGC, Gardner timing, Costas loop, differential slicer | 4800 baud | **PS** | Protocol-specific and branchy; SDRTrunk's f32 code is the reference |
| C4FM and DMR: sync-driven timing, equalizer, slicer | 4800 baud | **PS** | Same; already ported and tested against SDRTrunk |
| Framing, FEC, messages, vocoders, audio | — | **PS** | Unchanged |

**Rejected: every demodulator in the PL.** The LSM alone is 34 DSPs per lane; C4FM and DMR symbol
processors for three lanes would pass 220 DSPs. A 4FSK symbol processor is months of work, the
result no longer matches SDRTrunk's f32 code, and every fix needs a bake. The CPU it would save is
small: the symbol stage of a software receiver costs about 5-7 % of a core.

## The core

```text
AD9361 at 6.4 or 12.8 MSPS
 ├─ spectrometer (4096-point)      → spectrum ring       (as today)
 ├─ raw IQ capture                 → capture ring        (validation captures)
 └─ polyphase bank: 25 kHz bins, 2x oversampled, 9 taps per bin (SDRTrunk's M2 design)
      ├─ per-bin power integrator  → channel activity    (scan, survey, conventional channels)
      └─ N lane synthesizers (two bins joined, fine NCO; time-shared)
                                   → 50 kSPS IQ per lane → lane ring
```

- **The polyphase bank** follows SDRTrunk's `ComplexPolyphaseChannelizerM2`: a non-maximally
  decimated filter bank whose bins are 25 kHz wide and oversampled 2x (50 kSPS each), with
  `POLYPHASE_CHANNELIZER_TAPS_PER_CHANNEL = 9`. The FFT is Maia's, sized M = rate / 25 kHz.
- **AD9361 rates.** A power-of-two M with 25 kHz bins means 6.4 MSPS (M = 256) or 12.8 MSPS
  (M = 512); 25.6 MSPS (M = 1024) is possible if the window needs it. These replace the 8/12/16
  MHz presets for live sites; the window planner keeps its rule (±0.45 x rate usable). Maia's
  radix-2² FFT takes even orders only (M = 256, 1024); M = 512 needs its radix-2 form, nine
  stages with a twiddle multiplier each, which step 2 has to cost.
- **A lane** joins the two bins nearest its channel and moves it to DC, as SDRTrunk's
  `TwoChannelSynthesizerM2` and `PolyphaseChannelSource` do. It outputs 50 kSPS, the rate the
  software receivers take today. Lanes share one synthesizer in time, so N is a build parameter
  and a lane costs a slot, not a chain. Lane 0 is the control channel; all lanes are identical.
- **Channel activity** integrates each bin's power for the scan, the survey and conventional
  channels. It does not replace the spectrometer, whose 4096 bins the display and the crystal
  tracker use.
- **The register bridge answers every address** (zeros for a vacant one), so a stray access never
  stalls the CPU. A capabilities register gives N, M and the features present, so the PS reads
  what the core has instead of gating on its version.
- **One lane ring** replaces the ring-per-stream layout. Packets have a fixed size; each carries a
  header (proposed: lane, flags such as overflow, a tune generation, the sample count, and the
  ADC-rate sample index of its first sample) and then IQ. The tune generation increments on every
  lane retune, so the PS drops stale samples exactly instead of waiting out a time. Sixteen lanes at
  50 kSPS are about 3.2 MB/s on HP1. The PS keeps polling; the interrupt is not needed.

**Not the 2026-05 channelizer.** The core retired on 2026-05-03 was critically sampled (64 bins of
125 kHz) with a DDC per target feeding the gateware LSM. Its problems were elsewhere, but its design
also differed: no 2x oversampling, no two-bin synthesis. Its commutator, circular BRAM buffer and
shared multiplier (`scanner-hdl/radio_core/polyphase_channelizer.py`) can be reused.

### Budget (estimates, to confirm by synthesis)

| | Core 0.3.0 | This core |
|---|---|---|
| DSP48E1 | 172 (78 %) | ~60-70 (~30 %) |
| LUTs | 55 % | ~35-40 % |
| Lanes | 3, unequal | 8-16, identical |
| DMA masters on HP1 | 8 | 3 (lanes, spectrum, capture) |

Removed from the PL: three DDCs (33 DSPs), three LSM chains (~102 DSPs, ~14.5k LUTs, ~21k FFs),
the dibit rings, the pre-diff taps, the seed and NID registers. Added: the polyphase bank (~20
DSPs), the lane synthesizer (~4-6 DSPs), the activity integrator and the lane ring. The
spectrometer, the raw IQ capture, `axi_ad9361` and the IIO DMAs stay.

### CPU

Since the FIR speed-up (status log) a software receiver costs 4-9 % of a core on the A9 (LSM
4.1 %, C4FM 4.3 %, DMR 9.3 %, `dsp::cost_tests`), of which the filters are about 2 %: step 5 (the
filters in the PL) would save about that much per receiver. On unit A the scanner with three LSM
receivers runs at 15 % of one core (status log, the CPU work).

## The PS side

- **A `demod/` layer** between `dsp/` and `protocol/`: `lsm`, `c4fm`, `dmr`, each taking 50 kSPS IQ
  and emitting symbols. The C4FM and DMR demodulators move out of `protocol::p25::c4fm` and
  `protocol::dmr::demod`; `protocol/` keeps framing and up. The half-band, LPF and RRC stay a
  separate front-end stage, so step 5 can move it to the PL without touching the demodulators.
- **One input shape.** The traits of DESIGN §5 stay; `RxInput` loses its `Dibits` arm at the
  cutover, and the lane-capability table goes away.
- **Gone at the cutover:** the dibit ring readers, NID polling, the warm-start seeds, the PLL clamp
  and watchdog, and the time-based retune discards. `p25-httpd/src/lsm` and `sw_demod` leave the
  repo once the scanner's LSM replaces them as the reference.

## Steps

Each step keeps the units running on core 0.3.0 until the cutover.

1. **Software LSM in the scanner (PS only).** Port SDRTrunk's `P25P1DecoderLSM` and
   `P25P1DemodulatorLSM`, with names following the Java as in the C4FM and DMR ports. The
   reference is SDRTrunk itself, run headless on Andy's SDRTrunk recordings
   (`tools/sdrtrunk_lsm_reference.py`); then run the port beside the gateware LSM on the control
   channel's IQ and on chain 1's IQ.
   *Gate:* SDRTrunk's frames on the recordings, TSBKs and voice frames at least the gateware's on
   Clay County, the P25 tests and the replay corpus unchanged, CPU measured. Chain 1 also gains
   C4FM voice.
2. **A fixed-point model of the polyphase bank and the lane synthesizer (host only).** The model is
   what the HDL must match sample for sample. Feed it real wideband captures from unit A, and the
   existing 50 kSPS captures moved to worst-case offsets (halfway between bins).
   *Gate:* the DMR reference keeps 24,984+ of 24,996 lines and the P25 tests keep their counts.
3. **The core (bake), in two parts.** A fresh Amaranth package and a fresh Vivado project;
   `p25_hdl` and `projects/fishball7020_p25` stay as they are until the cutover, as `p25-httpd`
   did for the scanner.
   - **3a, the lane ring core:** today's three DDCs as lanes 0-2 into one tagged lane ring; the
     LSM chains go. It needs no model. Spec below.
   - **3b, the channelizer:** the polyphase bank and the lane synthesizer add narrow lanes beside
     the DDC lanes, behind the same lane ring. *Gate:* bit-exact against the step 2 model.
     **The DDC lanes stay** (recommended, "Every mode's needs", item 1): data mode's 250 kSPS and
     1 MSPS lanes, and every mode at a rate the bank does not serve, are DDC lanes. **3b waits
     for a mode that needs more lanes** than DDC lanes give, about eight (item 2): Clay County
     never needed more than three in a day on unit A.

   *Gate for both:* timing met with no waiver and a hierarchical utilization report in the build.
4. **Cutover (PS), with 3a.** The scanner's hardware layer for the new core, every lane on
   software demodulators, the removals above. The SD image carries the new bitstream; a 3a
   bitstream and the scanner that reads it ship together.
5. **Optional: the filters in the PL**, when lanes outgrow the CPU. A fixed-point model first;
   *gate:* the same parity as step 2.

## Step 3a: the lane ring core

Ordered by Andy on 2026-10-03, who approved the header (with whatever else belongs in it now) and
the build in the existing package and project.

### What changes in the gateware

| | Core 0.3.0 | 3a |
|---|---|---|
| Lanes | control DDC, two traffic DDCs, an LSM chain on each | the same three DDCs, lanes 0 (control), 1 and 2, IQ only |
| To the PS | rings for control IQ, traffic IQ, three dibit streams, two pre-diff taps | one lane ring of tagged packets, the spectrum, the raw IQ capture |
| Registers | a vacant address, a bank in reset or a write without byte strobes hangs the bus | every access is answered |
| DMA | `DmaStreamRingWrite` on every ring | the same for the lane ring, let to address only a packet already whole in block RAM; its enable acts between packets |
| Removed | — | the LSM chains (~102 DSP, ~14.6k LUT, ~21k FF), dibit rings, pre-diff taps, seeds, NID registers, five HP1 masters |

The spectrometer's inputs are registered in `sync` (064's suggested fix for the worst timing path).

**Why the lane ring's DMA is gated.** `DmaStreamRingWrite` raises AWVALID whenever it is enabled
and has fewer than two bursts open, whether or not data is coming (`maia_hdl/dma.py`). On a
Zynq-7000 the ADI scripts build HP1's interconnect as `axi_interconnect`
(`adi_project_xilinx.tcl`), which passes write data in the order of the addresses. A lane ring
waiting up to 20 ms for its next packet would hold an address open that long, ahead of the capture
ring's writes. The lane ring therefore enables the DMA for exactly the 32 bursts of a packet it has
started. `RingWriterV2` (hwval) was the other candidate; it inserts its own marker words into the
stream, which would break the packets' 4 KB alignment.

### The lane packet

A lane fills a packet as its DDC delivers samples (50 kSPS) and hands it to the lane ring whole.
Packets are 512 words of 64 bits (4 KB, a multiple of the 128 B burst, four to a sub-buffer): an
8-word header and 504 words of IQ (1008 samples, 20.16 ms). Unused header bits are zero.

| Word | Bits | Field |
|---|---|---|
| 0 | 15:0 | magic `0x5243` ("RC") |
| 0 | 19:16 | format version (1) |
| 0 | 23:20 | lane (0-15) |
| 0 | 31:24 | flags: bit 0 `lost` (samples were dropped before this packet), bit 1 `retuned` (the first packet after the lane's enable, or with a tag other than the packet before), bit 2 `last` (the lane's disable closed this packet; a packet already full when the disable came is not marked) |
| 0 | 47:32 | count: valid samples (at most 1008; fewer when a tag change or a disable closes the packet) |
| 0 | 63:48 | tag: the lane's tag when its first sample was made |
| 1 | 63:0 | sample index: the AD9361 sample count (since `sdr_reset` was released) when the DDC made the first sample |
| 2 | 47:0 | power: the sum of I² + Q² over the valid samples |
| 2 | 63:48 | peak: the largest \|I\| or \|Q\| among them |
| 3 | 27:0 | the lane's NCO word for the first sample |
| 3 | 47:32 | sequence: the lane's packet count (wraps) |
| 4 | 31:0 | ADC clips: the running count of AD9361 samples at full scale (I or Q at +2047 or −2048), shared by the lanes; the difference between packets is the overload in between |
| 5-6 | | reserved (3b's channel fields) |
| 7 | 31:0 | check: the XOR of every 32-bit half of the packet's other 1023 halves, so a stale cache line is caught (`maia-kmod/maia-sdr.c` invalidates L1 before L2) |
| 8-511 | | IQ, two samples a word as today (`[15:0]` I, `[31:16]` Q, then the next sample); unused words are zero |

- **Tag.** The PS writes it with the NCO in one register bank, so it lands after the NCO. A tag
  change closes the current packet, so a packet never mixes two tunings. The first samples after a
  change still carry the old channel through the DDC's filters; the PS skips the DDC's settling
  (from the preset) after a new tag.
- **Sample index.** With the sample rate, it gives every sample's air time, for every lane, on one
  clock with the capture ring. The PS reads the running count (below) to tie it to its own clock.
- **Lost.** A lane's packet buffer is double: one fills while the other waits for the ring. If
  both are full the lane drops samples and the next packet says so; the sample index says how many.

### Registers

Byte offsets in the 1 KB window at 0x7C46_0000 (the address bits are decoded in full; nothing
aliases). Every bank keeps today's per-access crossing (`RegisterCDC`, in order, so coefficient
loads are safe). The bridge (`p25_hdl/axil_bridge.py`) answers an address no bank claims (reads
0) and a write with no byte strobes itself; the sync-domain banks are not claimed while
`sdr_reset` holds their domain, nor for 16 AXI-Lite cycles after; a bank that still does not
answer within 4,096 cycles is answered with 0 and its late answer ignored.

| Offset | Bank | Registers |
|---|---|---|
| 0x000 | control (AXI-Lite domain) | `product_id` 0x72616431 ("rad1"); `version` (1.0.0); `control.sdr_reset`; `interrupts` (read to clear: lane ring, spectrum, capture); `capabilities` (lanes 3, packet words 2^9, header words 8, spectrum and capture present) |
| 0x020, 0x040, 0x060 | lane 0, 1, 2 | `laneN_ddc_*`: the DDC registers at today's offsets (coefficient address and data, decimation, frequency, stage control); `laneN_control` (enable `[0]`, tag `[31:16]`); `laneN_status` (`lost`, read to clear, alone in its word) |
| 0x080 | lane ring | `lanes_ring_control` (enable, acting between packets); `lanes_ring_status` (last completed sub-buffer); `lanes_ring_next_address`; `sample_count_lo` / `_hi` (reading the low word latches the high); `adc_clips` |
| 0x0A0 | spectrum | as today's spectrometer bank (its `spec_overflow` has never been driven and reads 0) |
| 0x0C0 | capture | as today's raw IQ bank |

`product_id` and `version` stay at 0x00 and 0x04, so a scanner can tell the cores apart before it
touches anything else.

### Rings and the device tree

| Device | Base | Size | Sub-buffers |
|---|---|---|---|
| `p25-lanes` | 0x1900_0000 | 2 MB | 128 × 16 KB (3.4 s at three lanes) |
| `p25-wideband-spec` | 0x2100_0000 | 64 KB | 2 × 32 KB (as today) |
| `p25-wideband-iq` | 0x2200_0000 | 16 MB | 16 × 1 MB (as today) |

- The UIO node stays `p25-core@7c460000` and the spectrum and capture keep their names, so the
  bench agent's image detection and capture ring carry over; the other six old ring nodes and
  their carve-outs go.
- **The DMA driver goes into the image.** The P25 defconfig never selected maia-kmod: the units
  load a module left over from an old Maia build in the persistent Buildroot target (053), and a
  clean image would have no ring devices. 3a selects it with its init script.
- The new bitstream replaces `bitstream/p25/system_top.xsa` (Andy: build in the existing package
  and project; 0.3.0 is kept in `_archive/build_2026-10-03_p25-core-0.3.0` and the
  `build/p25-core-0.3.0` tags).

### What the PS needs from 3a

- A `hardware::radiocore` backend and a PAC generated from the core's SVD in `scanner/core-pac`
  (`core_pac::radio_core`; inside what the image's scanner package already copies). `p25-httpd/p25-pac` stays the 0.3.0
  map, so p25-httpd still builds until it leaves the repo. The scanner of this change needs the
  radio core; unit B keeps the 0.3.0 image until A has run the new one.
- One lane reader splits the ring into lanes: lane 0 is the control channel's IQ, lanes 1 and 2
  the traffic lanes. Every lane decodes in software: P25 with the site's modulation (LSM or C4FM),
  DMR on either lane.
- After a retune a lane drops the packets of older tags and the DDC's settling; air time comes
  from the sample index. `IQ_SETTLE` and the per-symbol stamping go.
- The crystal tracker reads the software loops (the LSM's carrier loop, the C4FM and DMR
  equalisers' offsets) instead of the gateware's; the scan's P25 probe runs the LSM on IQ.
- Gone: the dibit ring readers and clock, the NID poller, the gateware loop readbacks, the
  `lsm_gateware` counters, the core-version clamps.
- The bench agent and fbench read the new map (UIO name, banks, ring names) after the core works.

### Step 4 in the scanner

- **`hardware::radiocore`** replaces `hardware::p25core` and `core_version`: the registers
  through `core_pac::radio_core` (product "rad1", version 1.x, the lane count from
  `capabilities`), the lanes by number (0 the control channel, then the traffic lanes), the lane
  ring with the packet parser (check word verified), the spectrum, the sample count.
- **One ring reader** (`radio::streams`) polls the lane ring every 20 ms and hands each lane's
  packets to that lane's subscribers (the control receivers or the scan on lane 0, the trunk on
  the traffic lanes) as blocks of IQ with an air time, the lane's tag, and whether samples were
  lost before them.
- **Tags in place of settle timers.** Every retune, control move or preset load gives the lane a
  new tag after its NCO; the reader drops packets of older tags and the DDC's settling (from the
  preset's filter lengths), and marks the first block of a tag. `IQ_SETTLE`, the scan's
  `RETUNE_SETTLE` and its drain go.
- **Air time** is the packet's sample index on a clock anchored by reading the sample count
  beside the monotonic clock, less the DDC's group delay.
- **Every lane in software:** P25 lanes on the control channel's modulation (LSM or C4FM; the
  control receivers tell the trunk when their choice switches, and each lane takes it at its next
  tuning). DMR stays on lane one's receiver, both timeslots of its carrier, as before; a second
  DMR receiver on lane two is a change to the follower, for later. The coast decision reads the
  lane's LSM loop; the dibit rings, the NID poller, the gateware readbacks and `lsm_gateware` go.
- **The crystal tracker** reads the control decoder's own carrier offset, signal minus NCO, for
  P25 (the LSM's loop, while a signal is on the channel) as for DMR (the equaliser); the scan's
  P25 probe runs the LSM on IQ.
- The API's loop fields come from the software loops; the radio readback shows each lane's
  NCO, packets on or off and tag, the ring, the sample and clip counts and each lane's packet
  counters.

### Build

- In place, in the existing package and project: `p25_hdl/p25_top.py` becomes the lane ring core
  (`P25Core` 1.0.0, product "rad1"), with the IP packaging, `projects/fishball7020_p25`
  (three HP1 masters: lanes, spectrum, capture) and `build_fpga.bat --p25` updated. A timing
  failure becomes an error (as `--hwval`), and a route-design hook writes the hierarchical
  utilization report.
- Vivado runs only in the main checkout (the ADI submodule is there); simulation runs anywhere.

### Tests and gates

- **Simulation:**
  - `test_lane_ring.py`: three packet builders, the lane ring and the production DMA into the AXI
    write model. Every packet against the Python reference (`header_words`): full packets, tag
    changes at odd counts, disables, re-enables, `lost` with the sample index's gap equal to the
    samples dropped, a stalling interconnect, the ring switched off and on with packets in flight;
    every AW has its data within a burst or two.
  - `test_axil_bridge.py`: claimed, unclaimed, strobe-less, silent and late-answering banks.
  - `test_p25_top.py` (rewritten): the map against the table above, the build files' version and
    masters, identity, a bus that answers in reset, at vacant addresses and without strobes,
    readback without aliasing, the sample and clip counters, and one lane's packet through the
    DDC, ring and DMA (`P25Core(sim=True)` feeds samples in `sync` in place of the input FIFO).
- **Bake:** timing met, utilization report.
- **On unit A:** the control channel and both lanes decode at least as well as now; lane 2 carries
  DMR and C4FM; the starts of transmissions after a retune are no longer lost; CPU measured.

## What this supersedes

- **077 (chain-2 IQ tap on core 0.3.0) is not built.** Every lane gets IQ in step 3.
- DESIGN §13's verdicts on a channelizer, the LsmFir area recovery and the HDL front end are
  replaced by this plan.

## Every mode's needs

Andy, 2026-10-04: the gateware serves every mode (scanner, ATSC TV, data), so that a new mode
is a software change. This is the list to take up with the next HDL work. The modes' designs:

- this change, for the scanner;
- `doc/changes/080_atsc_tv_mode.md` and `081_atsc_station_names.md`, for ATSC TV;
- `scanner/doc/DATA_MODE.md`, for the data scanner.

**The principle.** The core's blocks carry no protocol. Every rate, filter and mode is a runtime
register, so modes differ only in what the PS writes. Most of this is true already:

- each lane's decimations, filter taps and `bypass2` / `bypass3` are registers (the PS clears
  the bypass bits today);
- the spectrometer's integrations (1-1023) and its peak-hold are registers;
- the capture ring takes the raw AD9361 stream at any preset rate.

Data mode's wide lanes (250 kSPS and 1 MSPS) are new coefficient sets, not a bake
(`scanner/doc/DATA_MODE.md` §3).

| # | Change | Modes | Today |
|---|--------|-------|-------|
| 1 | Keep wide lanes beside the channelizer | data, survey; ADS-B later | 3b would replace the three DDCs with 50 kSPS lanes |
| 2 | More lanes | scanner, data | 3 (`config.lanes` allows 15; the PS has 3 register banks) |
| 3 | A larger lane ring | data | 2 MB: 3.4 s at three 50 kSPS lanes, 0.17 s at three 1 MSPS lanes |
| 4 | A deeper spectrum ring | data, diagnostics, survey | 2 frames of 32 KB; each can be read once |
| 5 | Average and peak | data, ATSC, survey | `spec_peak_detect` picks one, live, mid-frame too |
| 6 | Time on spectrum frames | data, survey | None; the PS places a frame by when it reads it |
| 7 | Time on the capture ring, and stop after a trigger | data, validation | Enable, overflow, last buffer and next address only |
| 8 | Windows wider than 16 MSPS | data, survey | Presets 2-16 MSPS |
| 9 | A burst detector in the PL | data, survey | None |
| 10 | A classifier tap | survey, data | None |
| 11 | An 8-VSB demodulator | ATSC | Names from a 0.5 s capture decoded on the PS (081) |

### What 3a uses, and where its timing margin goes

Measured on 3a's routed checkpoint (2026-10-03 build; `report_utilization -cells` on the core,
which the route hook's depth-4 report does not reach):

| Block | LUTs | FFs | RAMB36 | RAMB18 | DSP |
|-------|-----:|----:|-------:|-------:|----:|
| A lane's DDC (mixer 1 DSP, three FIR stages 10) | 396 | 575 | 0 | 10 | 11 |
| A lane's packetizer (the power sum takes the 2 DSPs) | 703 | 1,089 | 2 | 0 | 2 |
| A lane's registers and their crossing | 150 | 310 | 0 | 0 | 0 |
| The spectrometer (the FFT 2,292 LUTs, 8 RAMB36, 6 DSPs; the integrator 432, 12, 1) | 2,749 | 2,304 | 20 | 4 | 7 |
| The lane ring, the capture packer and their DMAs | 190 | 200 | 0 | 0 | 0 |
| **The core** | **6,836** | **9,165** | **28** | **35** | **46** |
| **The design** (xc7z020) | 14,958 (28 %) | 21,974 (21 %) | 38 | 40 | 68 (31 %) |

- **A lane costs** about 1,250 LUTs, 1,970 FFs, 7 block RAM tiles and 13 DSPs.
- **Free:** about 38,000 LUTs, 82 of 140 block RAM tiles and 152 of 220 DSPs.

**Timing.** clk3x (187.5 MHz, 5.333 ns) holds the design's worst setup slack, +0.091 ns. The
worst paths there:

| Slack (ns) | From | To | Paths |
|-----------:|------|----|------:|
| +0.091 | the common-edge pulse (`ClkNxCommonEdge`, shared) | the spectrometer FFT's first twiddle multiplier | 20 |
| +0.487 to +0.602 | the same pulse | the lane mixers' multipliers | 44 |
| +0.526 to +0.842 | the integrator's copy of the pulse | the FFT's twiddle multipliers | 60 |
| +0.702 | `spec_control` (the peak-detect bit) | the integrator's power stage | 3 |

- **Each path is one LUT and 3.5-4.2 ns of routing** (85-88 % of the path), so the margin goes to
  fan-out, not logic depth.
- The pulse register drives 368 loads, the multiplexers of every 3x multiplier in the core. Vivado
  made three copies (130, 159 and 79 loads), two on the left of the die and one on the right.
- Synthesis also merged the integrator's and the FFT's equal copies of the pulse into one.
- **The fix:** one common-edge generator per consumer (the FFT, the integrator, each lane's
  DDC), kept apart from merging as 3a kept each lane's input register. It comes first in the next
  bake, before anything is added at clk3x.
- **The other domains:**
  - sync (62.5 MHz) has 3.6 ns;
  - the AXI-Lite clock's worst path (+0.509 ns) is in ADI's HP2 interconnect, outside the core;
  - worst hold is +0.010 ns, as the router left it.

**What each item touches at clk3x:**

- **Items 3, 4, 6 and 7** are counters, latches and DMA control in sync. They add nothing at clk3x.
- **Item 5** touches the integrator's power stage only through the peak-detect bit, which is
  already a +0.702 ns path. Latched per frame (below), it becomes a local register.
- **Items 2, 9 and 11** add 3x multipliers. They need the common-edge fix first.

### Rates by mode

| Mode | Use | AD9361 rate | Blocks |
|------|-----|-------------|--------|
| Scanner, live site | control and traffic channels | presets 2-16 MSPS (8, 12, 16 in use); 6.4 or 12.8 with the channelizer | lanes, spectrum (crystal tracker, display) |
| Scanner, systems scan | carriers across a window | 16 MSPS | spectrum |
| ATSC, TV scan | pilots, carrier to noise | 16 MSPS, two channels a window | spectrum |
| ATSC, naming | 0.5 s of IQ | 10 MSPS | capture |
| ATSC, live (item 11) | the transport stream | 10 MSPS | the 8-VSB block |
| Data, scan and park | bursts across a band | 16 MSPS; about 30 for 902-928 MHz (item 8) | spectrum, wide lanes (250 kSPS, 1 MSPS), capture |
| Data, later (ADS-B) | 1090 MHz | 2-4 MSPS | a stage-1-only lane |
| Validation, diagnostics | captures | any preset | capture |

**Only the scanner's live sites would move** to the channelizer's rates. Every other use keeps
its rate, because it uses the spectrum, the capture or DDC lanes, which work at any rate. The
channelizer's lanes exist only at 6.4, 12.8 or 25.6 MSPS.

### The items

**1. Wide lanes after 3b. Recommended: the DDC lanes stay.**

- **The two options:**
  - **A synthesizer joining J bins:**
    - its rates come in steps of 25 kHz;
    - it needs a J-point synthesis transform for each wide lane;
    - every joined edge has the bank's filter shape;
    - it exists only at the bank's rates, so not at data mode's 16 MSPS windows.
  - **A DDC lane:** any rate at any input rate, with its own filter. It exists today.
- **The change:**
  - HDL: none (3a's lanes).
  - PS: wide-lane presets (data mode step 1).
  - 3b, when it comes, adds narrow lanes beside the DDC lanes; it does not replace them.
- **Cost:** 13 DSPs, 1,250 LUTs and 7 block RAM tiles per DDC lane kept.
- **Modes:** data, survey, ADS-B later, and every mode at a rate the bank does not serve.
- **Evidence still needed:** data mode step 1's check of the 250 kSPS and 1 MSPS presets: flat
  ±100 / ±350 kHz, alias rejection, no lost packets with three 1 MSPS lanes for an hour.

**2. More lanes.**

- **Demand on Clay County,** from unit A's history (24 h to 2026-10-04 14:49 UTC):
  - 6,557 grants: 1,912 followed, 4,010 encrypted, 632 held, 3 not followed because every lane
    was busy.
  - The clear grants overlapped as one call 10.1 % of the time, two 0.62 %, three 0.008 %, never
    four.
  - Lane 1 carried 1,800 calls, lane 2 112. Three lanes are enough here.
- **Two ways to more lanes:**
  - **More DDC lanes:**
    - `config.lanes`, up to 15. No model is needed, and every lane keeps any rate.
    - Eight lanes: about 133 DSPs (60 %), 21,000 LUTs (40 %) and 93 tiles (66 %).
    - Block RAM sets the ceiling, at about ten lanes, or eight beside the 8-VSB block (item 11).
  - **The channelizer (3b):**
    - sixteen or more narrow lanes for about the cost of two DDC lanes;
    - only at its rates;
    - it needs step 2's model first.
- **The change for more DDC lanes:**
  - HDL: `config.lanes`, and a register map whose shared banks do not move with N (bake A).
  - PS: `radiocore/regs.rs` (`LANE_BANKS`, the `lane_bank!` list), and the trunk's lane pool
    sized from `capabilities`.
- **Timing:** each DDC adds 3x multipliers. With a common edge of its own, its risk is placement
  only.
- **Modes:** scanner (two systems in one window, busier sites), data (more hot spots).
- **Evidence still needed:** a mode that wants more than three lanes. Candidates:
  - data mode's hot spots, measured in its step 3;
  - following two systems at once;
  - a busier site.
- **Recommended:** no change now. When a mode needs it, DDC lanes up to about eight, the
  channelizer past that.

**3. A larger lane ring.**

- **The change:** 2 MB to 8 MB (512 × 16 KB) at 0x1900_0000.
- **What it holds:** 13.7 s at three 50 kSPS lanes, 0.67 s at three 1 MSPS lanes (33 times the
  20 ms poll).
- **HDL:** `config.py` (`last_buffer` grows from 7 to 9 bits) and the device tree's carve-out.
- **PS:** nothing; the reader takes the ring's geometry from its device.
- **Cost:** 6 MB more DDR reserved from Linux. No timing risk.
- **Modes:** data (wide lanes), and any larger N.
- **Evidence:** none needed.

**4. A deeper spectrum ring.**

- **The change:** 2 to 16 frames of 32 KB (512 KB).
- **HDL:** `config.py` and the device tree.
- **PS:**
  - `read_spectrum` reads every completed frame in order (`completed_since`, as the lane ring);
  - `spec_last_buffer` grows from 1 to 4 bits. svd2rust turns it from `.bit()` into `.bits()`;
    the call is cfg(linux), so the ARM check catches it.
- **Cost:** nothing in the fabric. No timing risk.
- **Modes:** data, survey, diagnostics.
- **Evidence:** none needed.

**5. Average and peak. Recommended: a mode per frame, not both in one.**

- **Both in every frame** needs a second accumulator:
  - 12 RAMB36 (the integrator's ping-pong of 4096 × 50 bits);
  - a second power stage (1 DSP at clk3x, in the block with the least slack);
  - about 450 LUTs;
  - frames twice the size.
- **A mode per frame** costs a few flip-flops:
  - the integrator takes `peak_detect` and `nint` when a frame starts, not live (today a
    mid-frame write spoils that frame);
  - the frame's header (item 6) records its mode;
  - an `alternate` bit flips the mode every frame without the PS.
- **Data mode may need only peak frames.** Over noise, a bin's peak over 256 transforms sits about
  7.9 dB above its mean, with little spread, so the burst detector's floor can come from peak
  frames.
- **PS:** set the mode and `alternate`; read each frame's mode from its header.
- **Modes:** data and survey (peak); ATSC (average, as now).
- **Evidence still needed:** whether the detector misses bursts with alternating frames
  (data mode step 3, with the ESP32-DIV's bursts). Only then the second accumulator.

**6. Time on spectrum frames.**

- **A header in the frame's first bins.**
  - Bins 0-7 lie at −fs/2, outside the ±0.45 × rate that every user of the spectrum reads, so
    nothing is lost.
  - Bin 4095 takes a check word, as the lane packet's.
  - The DMA's read side puts these words in place of the bins.
- **The fields:**
  - magic and format;
  - the frame's sequence;
  - its mode and integrations;
  - whether it was aborted;
  - the AD9361 sample index of the frame's first transform's first input sample;
  - the ADC clip count;
  - the check: the XOR of the frame's other 32-bit halves.
- **HDL:** a spectrometer wrapper in `scanner-hdl/radio_core`, leaving `maia_hdl`'s blocks as they are. In sync:
  a 64-bit latch, the read-side multiplexer and the XOR, about 200 LUTs.
- **PS:**
  - parse and check the header;
  - for the display and the floor, blank the header's bins (copy their neighbours);
  - the survey and data mode's detector place frames by the sample index.
- **Timing:** sync only, low risk.
- **Modes:** data, survey. ATSC can count its frames exactly.
- **Evidence:** none; the simulation checks the index against the input.

**7. Time on the capture ring, and stop after a trigger.**

- **New registers in the capture bank:**
  - `capture_start_index` (two words, the low latching the high): the sample index of the first
    sample written after an enable;
  - `capture_buffers`: the sub-buffers completed since the enable;
  - `capture_stop_after` and `capture_trigger` (a write pulse): the capture writes that many
    more sub-buffers after the trigger and stops itself at a sub-buffer's end;
  - `capture_stopped`, in the status.
- **Each sub-buffer's first sample** is then `start + k × 262,144` (1 MB of 4-byte samples),
  exact while `overflow` stays clear.
- **HDL:** a latch, two counters and the stop logic in sync, about 120 LUTs.
- **PS:**
  - `RadioHw::capture` returns the sample index of its first sample;
  - a trigger call for data mode's burst captures.
- **Timing:** sync only, low risk.
- **Modes:**
  - data: a burst's lead-in stays in the ring (0.26 s deep at 16 MSPS) without the PS racing it;
  - validation captures;
  - ATSC naming: the capture gets an air time.
- **Evidence:** none needed.

**8. Windows wider than 16 MSPS. Probably no bake: a preset and a check.**

- **The core already takes such rates.**
  - Maia runs its spectrometer at up to 61.44 MSPS on these clocks. The sync domain (62.5 MHz)
    takes a sample a cycle.
  - `axi_ad9361`'s interface is constrained for the full rate (`rx_clk` at 4 ns).
- **What changes at about 30 MSPS:**
  - a DDC's FIR budget at clk3x falls to about 6 operations an input sample;
  - lane rates must divide the input rate (30 MSPS: 1 MSPS is /30, 50 kSPS /600);
  - a spectrum bin becomes 7.3 kHz;
  - the capture ring holds 0.13 s.
- **DMA bandwidth on HP1:**
  - **The port:** one 64-bit port at the sync clock (`system_bd.tcl`, `ad_mem_hp1_interconnect` on
    `clk_out1`), about 500 MB/s for the core's three masters (also `HW_VALIDATION_SUITE.md` F13).
  - **The load at 30 MSPS:**
    - the capture, 120 MB/s;
    - three 1 MSPS lanes, 12.2 MB/s;
    - the spectrum, under 1 MB/s at 30 frames a second;
    - 8-VSB codewords (item 11), 2.7 MB/s;
    - together about 136 MB/s, 27 % of the port.
  - **The risk is latency, not bandwidth.**
    - The capture packer holds one word (`iq_packer.py`), and the DMA keeps at most a few
      bursts open.
    - At 8 MSPS the hwval simulation lost samples from 16.6 µs of write latency (F5). At 30 MSPS
      that tolerance shrinks to about 4.4 µs.
    - The interconnect is built for performance (ADI's `STRATEGY 2`), with a data FIFO on each
      slave port (a RAMB36 and a RAMB18 each in 3a's report), which this estimate does not
      count.
- **The check, on unit A:**
  - a 30 MSPS preset;
  - 10 minutes with the capture on, three 1 MSPS lanes, the spectrum at 30 frames a second and
    both A9 cores loaded (a TV naming decode);
  - pass: no capture overflow and no lane `lost`.
  - fbench's `hw.contention` would measure the margin, but it needs the hwval image, which has
    never been baked.
- **If the check fails:** a FIFO in front of the capture DMA (two RAMB36 hold 0.5 ms at
  30 MSPS). Or move the capture to HP0 or HP3, which are unused, at a faster clock.
- **Modes:** data (902-928 MHz in one window), survey.

**9. A burst detector in the PL.**

- **What it is:**
  - a per-bin floor (4096 × 16 bits) tracked from each transform's power, before integration;
  - a threshold over it;
  - events (bins, first and last transform's sample index, peak) to a small ring.
  - It times bursts to a transform (0.26 ms at 16 MSPS) where a frame is 65 ms.
- **Cost:**
  - the transform's magnitude: 1-2 DSPs at clk3x, or none with an alpha-max-beta-min estimate
    in LUTs;
  - 2-3 RAMB36 and about 1,500 LUTs;
  - an event path: one more client of the lane ring, or a register FIFO.
- **Timing:** medium. It taps the FFT's output inside the block with the least slack. With the
  common-edge fix and a LUT magnitude it stays in sync.
- **Modes:** data, survey. The channel-activity integrator (3b) is its narrowband cousin.
- **Evidence still needed:** data mode's software detector (step 3) on peak frames with item 6's
  times. Build this only if 65 ms frames are too coarse for what it must do; the lanes already
  time a burst's pulses exactly.

**10. A classifier tap.**

- **What it is:** an AXI-Stream point where a generated block (hls4ml, FINN) reads spectrum frames
  or a lane's IQ and writes labels to a ring (`_shared/FPGA_ML_INFERENCE_GUIDE.md`).
- **Cost:**
  - the tap itself: a stream port and a ring client, a few hundred LUTs;
  - the classifier: unknown until a model is trained and sized on the PC.
- **Modes:** survey, data.
- **Evidence still needed:**
  - data mode's corpus, labelled by rtl_433 (its step 4);
  - a model measured on the PC.
- Last of the eleven.

**11. An 8-VSB demodulator. Recommended: the PL from the input to Reed-Solomon's syndromes; the
PS corrects, derandomizes and does everything above.**

**Where the time goes now.** 081 measured RF 19 (0.8 s of signal) at 5.3 s on both A9 cores:

| Part | Time | Share |
|------|-----:|------:|
| Front end: pilot, matched filter and symbols, timing, field syncs, equalizer | 3.68 s | 69 % |
| Viterbi | 1.1 s | 21 % |
| The rest of the FEC: bytes, deinterleaver, Reed-Solomon, derandomizer | 0.48 s | 9 % |

- **Real time needs 6.6 times that speed** on both cores.
- **Even the last row is too much in real time:** 0.48 s for 0.8 s on two cores is more than one
  core.
- **The A9 can only do the work above the bytes.** The Viterbi is the second-largest stage, so it
  goes to the PL with the front end.

**The split:**

| Stage | Where | Why |
|-------|-------|-----|
| Pilot and carrier loop | PL | Per sample, 10 M a second |
| Matched filter at any instant (081's table, 64 phases × 40 taps at 10 MSPS); timing loop on the segment syncs; field sync (PN511) | PL | 10.76 M outputs a second |
| Equalizer: 128 taps (32 ahead) applied | PL | 1.38 G multiply-accumulates a second |
| Equalizer taps solved | PS | 081's least-squares solver named 16-18 stations. It runs on snapshots of the equalizer's input, at about 10 % of a core for a solve every 0.25 s (estimate). An LMS in the PL would be a new algorithm with no model and convergence of its own to prove |
| Slicer and the 12 trellis decoders (4-state soft Viterbi, time-shared, one encoder a symbol) | PL | 21 % of the software's time. Logic only, no DSPs |
| Bytes back in order, the convolutional deinterleaver (52 branches, 5,304 bytes), Reed-Solomon syndromes | PL | Two block RAMs and XOR networks. A codeword with zero syndromes needs nothing more |
| Correcting the flagged codewords (Berlekamp-Massey, Chien, Forney), the derandomizer, the transport stream, PSIP, streaming | PS | 081's code. About 10 µs a flagged codeword: at 12,894 codewords a second, half of them flagged is about 6 % of a core. No Reed-Solomon decoder in the PL, so no LogiCORE licence |

The split asked for (bytes to a ring after the Viterbi, the deinterleaver and Reed-Solomon on the
PS) would leave the PS a deinterleave and syndromes over 2.7 MB/s: about a third of a core
(estimate). Putting them in the PL costs two block RAMs.

**The rate: 10 MSPS.**

- **081's receiver works there.** 16-18 stations were named from 10 MSPS captures.
- **The matched filter costs its span times the input rate:**
  - 40 taps at 10 MSPS;
  - 51 at 12.8;
  - 86 at 21.52 (twice the symbol rate).
- **A multiple of the symbol rate simplifies nothing.**
  - The filter computes any instant from its table.
  - The timing loop tracks the crystal's error anyway (A: −0.688 ppm).
- **12.8 MSPS would matter only beside the channelizer,** but in ATSC mode the unit has the radio
  to itself.
- 10 MSPS keeps the channel at the LO, as 081 tunes it.

**The output:**

- 207-byte codewords with their syndrome flag, in 4 KB packets, as one more client of the lane
  ring: a packet kind in the header, 2.7 MB/s;
- the header carries the field and segment counts, the sample index, the MER and the loops'
  states;
- no fourth DMA master and no new ring.

**Budget** (estimates; the model and synthesis confirm):

| Stage | DSP | LUTs | Block RAM tiles |
|-------|----:|-----:|----------------:|
| Pilot, carrier loop, mixer | 2-3 | 1,000 | 0-1 |
| Matched filter (861 M multiply-accumulates a second) | 5-6 | 800 | 2 |
| Timing loop, field sync | 1 | 800 | 1 |
| Equalizer (1.38 G a second) and its snapshot | 8 | 1,500 | 3-4 |
| Slicer, 12 trellis decoders | 0 | 2,000 | 1-2 |
| Deinterleaver, syndromes, packets | 0 | 1,000 | 3-4 |
| **Total** | **about 18** | **about 7,000** | **about 12** |

- **It fits** beside 3a, or beside eight DDC lanes: about 151 DSPs (69 %) and 105 tiles (75 %).
- **Timing risk: medium.**
  - New 3x DSP chains (the matched filter and the equalizer), fed from block RAM.
  - They are built as DSP cascades with registered RAM outputs and their own enables, not on
    the shared common edge.

**PS:**

- 081's solver split out to run on snapshots;
- its `fec` (correction only), `ts` and `psip` on the stream;
- a socket carrying a program's packets to the Viewer.

**Modes:** ATSC (live video, the Viewer's live stats, the full PSIP).

**Evidence still needed, in this order:**

1. **A fixed-point model of the PL half** in Rust, beside 081's f32 receiver. It must decode the
   17 captures in `runs/atsc/captures_20261003/` with the receiver's results. The HDL matches the
   model bit for bit.
2. **Captures of several seconds,** to see whether taps solved every 0.25 s hold the MER or an
   LMS is needed. The capture ring holds 0.4 s at 10 MSPS; longer needs the IIO path
   (`axi_ad9361_adc_dma` is in the design) or a larger ring.
3. **The browser's player** (MPEG-2 video and AC-3 in WASM), proven first on the HDHomeRun's own
   transport stream (`http://10.0.0.117:5004/auto/v<channel>`). Without it the PL receiver gives
   live statistics and the programme guide, not pictures.

### The bakes

| Bake | Items | Why together | Needs first |
|------|-------|--------------|-------------|
| **A: every mode's plumbing** (core 2.0.0) | the common-edge fix; a register map that does not move with N; 3, 4, 5 (a mode per frame), 6, 7; the core's block report | No new DSP path: counters, latches and DMA control in sync. Every mode uses them, and the timing fix makes room for B-D | nothing |
| **B: more lanes** | 2: DDC lanes to about eight, or 3b's channelizer past that | | a mode that needs more than three lanes (item 2) |
| **C: data mode in the PL** | 9; the capture FIFO if item 8's check fails | | data mode steps 3-4 |
| **D: 8-VSB** | 11 | The largest block, with its own model | the model, long captures and the browser's player (item 11) |
| no bake | 8: a preset and a check; 10 after data mode's corpus | | |

- **Bake A's register map:**
  - control at 0x000, the lane ring at 0x020, the spectrum at 0x040, the capture at 0x060;
  - 0x080-0x0FF kept for later blocks (the 8-VSB, a burst detector);
  - lane i at 0x100 + 0x20 × i, up to 15 lanes in the same 1 KB window.
  - The version goes to 2.0.0 and the product stays "rad1". The scanner of the same commit reads
    2.x only, so core and scanner ship together, as 3a did.
- **Bake A's PS side:**
  - `hardware::radiocore` for the 2.0.0 map;
  - the spectrum ring read frame by frame, with the header, its check and the blanked bins;
  - the frame mode and `alternate`;
  - the capture's start index, buffer count and trigger;
  - fbench and the bench agent on the new map;
  - tezuka_fw: the XSA and the device tree's carve-outs (lanes 8 MB, spectrum 512 KB).
- **Every bake's gates:**
  - **Models:** bit-exact against a model wherever there is DSP. Bake A has no new DSP; its
    spectrum frames are checked against the integrator's model.
    - B: step 2's model for the channelizer; Maia's DDC for DDC lanes.
    - C: a model of the detector.
    - D: the fixed-point 8-VSB model.
  - **Timing** met with no waiver, with the clk3x slack reported. Bake A targets +0.4 ns at
    clk3x, the reason for its common-edge fix.
  - **A hierarchical utilization report down to the core's blocks** (the route hook gains a
    report on the core's cell).
  - **The CLAUDE.md checks:** host tests, the ARM check, the DMR reference; the P25 replay
    corpus when Andy wires the bench link.
  - **On unit A**, at least:
    - an hour on Clay County with no packet fault, lost or missed packet, and the control
      channel at 40 or more messages a second at 99.8 % or better;
    - a TV scan naming as many stations as 081's;
    - fbench's tests that read the core, on the new map.
  - **Bake A on unit A also needs:**
    - every spectrum frame read for 10 minutes with no gap in the sequence;
    - one burst (a key fob or the ESP32-DIV) at the same time to within one transform in a
      frame's header, a lane's packets and a capture.
- **Numbering:** each bake takes the next free change number in `doc/changes` when it starts.

## Open questions

- **N.** The bank's cost does not depend on N; the synthesizer's time slots and the ring do. Clay
  County needed three lanes at most in a day on unit A (item 2), so N waits for a mode that needs
  more.
- **The 16 MHz scan preset.** The scan can run at 12.8 MSPS, or the spectrometer alone can serve it
  at 16 MSPS with the lanes idle.
- **The gateware LSM's DC blocker** is not in the software LSM. Its AGC idle gate and no-signal
  hold are (step 1 showed SDRTrunk's loop trapping on traffic-channel gaps); nothing so far points
  at the DC blocker.
- **Wideband captures.** The scanner reads the capture ring (`RadioHw::capture`, since 081's
  station naming). What is missing is an API route for raw IQ (data mode §6: `/ws/iq` for a lane,
  window snapshots on the Captures tab). Step 2's wideband captures can come from that route or
  from the IIO path.

## Status log

- **2026-10-03.** Review of core 0.3.0 and the PS (Andy's question: what runs on the PL and the PS,
  and where the demodulators should run). Design approved. Build backed up and tagged.
- **2026-10-03, step 1 (branch `079-lsm`).** `scanner::protocol::p25::lsm` ports SDRTrunk's LSM
  decoder with SDRTrunk's own taps. Measured on the 313 SDRTrunk recordings (50 kSPS
  `_baseband.wav`, Apr 15 - May 3; Clay County is CQPSK in Andy's SDRTrunk playlist), against
  SDRTrunk's decoder on the same files:
  - **Dibits:** 99.85 % agreement on 234 recordings. Most of the other 76 are 2-6 s traffic
    recordings whose silent tails lower the figure; their LDU counts equal SDRTrunk's.
  - **Frames, through the scanner's framer:** SDRTrunk 116,347 TSDUs and 3,647 / 3,390 LDU1 /
    LDU2; the port 112,721 and 3,635 / 3,374. The whole difference is in the 8 recordings whose
    carrier offset is above 440 Hz (the others are below 270 Hz). There the loop has two stable
    points, the true one and one π/2 away, both inside its ±π/3 limit, and either decoder can
    take the wrong one: the port lost 1 control and 3 traffic recordings that way, SDRTrunk a
    different 1 and 3. Live SDRTrunk avoids it by retuning from the loop's error; the scanner
    keeps offsets small by crystal tracking (A's control channel: about 5 Hz).
  - **SDRTrunk's LSM baseband low-pass does not converge** (67 taps: +31 dB at 12 kHz, +39 dB at
    12.5 kHz). The gateware's 121-tap filter has the same band edges and −65 dB there. On the
    recordings the two give the same frames apart from the trap recordings, so the port keeps
    SDRTrunk's. It matters again for step 5.
  - **Input at ±1.0 full scale, as SDRTrunk's:** the AGC's 500x limit is what keeps a quiet
    channel's noise small. At raw 16-bit scale the loop wandered onto its limit before the
    signal came and one control recording decoded nothing.
  - SDRTrunk's demodulator throws on a block shorter than the one before (live SDRTrunk sends
    fixed blocks); the harness leaves out each file's last partial block.
  - Next: CPU on an A9, then the port beside the gateware LSM on unit A.
- **2026-10-03, step 1 live on unit A** (Clay County simulcast, branch `079-lsm` 6cbf291 hand
  deployed; the software LSM is counted only). 600 s:
  - **Control channel:** TSBKs gateware LSM 23,965 (5 CRC failures), software LSM 24,032 (0),
    C4FM 24,025 (1). The gate is met here.
  - **Lane 1** (the software LSM on its IQ whenever the lane is tuned): LDU1 / LDU2 gateware
    321 / 309, software 316 / 303; HDUs 30 / 24; TDULCs 1,109 / 1,040. The shortfall is at the
    ends of the lane's tunings, where the two paths do not see the same samples: the IQ ring has
    no timestamps, so the software path drops 200 ms after each retune and stops when the lane
    pauses, while the gateware's dibits are cut by air time. Not yet shown frame by frame; the
    recordings (both decoders starting together on the same samples) show no demodulator
    deficit. The lane ring's tune generation and sample index (step 3) remove the cause.
  - **CPU:** a software LSM receiver costs 12-15 % of an A9 core: the control thread 20.3 % →
    35.8 %, lane 1 on the runtime workers 16.2 % → 27.8 %. NEON does not change it: the SD
    image's build turns it on (`tezuka_fw/package/scanner/scanner.mk` RUSTFLAGS), the hand builds
    measured here did not, and the same code built with the image's flags runs the control thread
    at 35.1 %. The shared FIR (`dsp::fsk4::Fir`) sums its taps in one dependent chain, which the
    compiler does not vectorise; about 60 % of a software receiver is its FIRs (DESIGN §13), and
    the C4FM and DMR receivers pay the same.
- **2026-10-03, the software LSM in use on unit A** (control channel and lane 1, the gateware's
  counted beside it; lane 2 on the gateware). Control: software 24,013 TSBKs, gateware 23,913,
  C4FM 23,835 in 600 s. Lane 1 trailed the gateware by 6-27 % of LDUs. A temporary logging build
  found two causes, over 22 gateware HDUs on lane 1:
  - **The carrier loop at its limit.** At 6 of them the software loop sat at ±π/3 and the HDU
    was lost, with the loop staying there over the next transmissions: change 059's trap. A
    traffic channel has gaps between transmissions where SDRTrunk's decision-directed loop walks
    on the noise; the control channel has none.
  - **The retune drop.** About one voice NID a retune fell inside the 200 ms the software path
    drops after a retune, because the IQ ring carries no sample times.
- **Fix for the first: 059's hold and 0.65 rad clamp in the software LSM** (a departure from
  SDRTrunk, with this evidence). On the 313 recordings, through the scanner's framer: NIDs
  142,279 (SDRTrunk 137,430), TSDUs 119,746 (116,347), LDU1 / LDU2 3,667 / 3,411 (3,647 /
  3,390), HDUs 448 (443). Two recordings lose one LDU each; 40 gain, the carrier-offset
  recordings that SDRTrunk's loop lost among them. The second cause needs sample times on the
  IQ ring: its write-address registers (unread today) can give them as the dibit ring's do, or
  the lane ring's tags (step 3).
- **2026-10-03, with the hold on unit A** (dc274c1, the image's NEON flags), over 22 gateware
  HDUs on lane 1 (7 min): LDU1 / LDU2 software 209 / 196, gateware 212 / 195; HDUs 19 / 22;
  NIDs 1,239 / 1,249. Control channel: software LSM 16,895 TSBKs, gateware 16,802, C4FM 16,620.
  What is left is mostly HDUs at the start of a tuning, the retune drop.
- **2026-10-03, step 3a gateware written** (branch `079-lsm`; Andy: add what belongs in the header
  now, build in the existing package). `p25_hdl/p25_top.py` is the lane ring core 1.0.0 with
  `lane_packetizer.py`, `lane_ring.py` and `axil_bridge.py`; `config.py` holds the three rings.
  The IP packaging and block design carry the three masters, `build_fpga.bat --p25` packages
  1.0.0 and fails on timing, a route hook writes `utilization_hier.rpt`, and the SVD and PAC go
  to `scanner/core-pac`. The simulations above pass. They found two faults in the first draft:
  - the write address of a packet's samples carried the slot bit one place too high, so both
    slots were written into the first;
  - a packet closed by its 1008th sample left that sample out of its power and peak.

  The burst gating (above) was added after reading the DMA and the ADI interconnect scripts.
- **2026-10-03, the bake.** The first build met timing at +0.024 ns: one input register shared
  by the three DDCs fed their mixers across the die (3.8 ns of routing into lane 1's mixer DSP on
  the next clk3x edge). Each lane now has its own copy, kept from merging. The second: worst
  setup +0.091 ns, in the spectrometer's clk3x path (Maia's block, unchanged; 0.3.0's worst
  too, at +0.208), the sync to clk3x crossing +0.503 ns, worst hold +0.010 ns. The core takes
  6,827 LUTs, 9,085 FFs, 28 + 35 block RAMs and 46 DSPs; the design 57.7 % of slices (0.3.0:
  91.6 %) and 68 DSPs (172). The XSA is on `fishball-p25`, not yet in tezuka_fw.
- **2026-10-03, step 4 written** (branch `079-lsm`, as the step 4 section above):
  - **The software LSM's loop reads NCO minus signal,** as the gateware's did: on signals 150 Hz
    above and below the NCO it reads −146 and +146 Hz before the sign. The receivers publish it
    as signal minus NCO, DMR's convention, so the crystal tracker takes both the same way.
  - **A DDC settles in 53-55 output samples** (about 1.1 ms) at every preset, with half of it
    the delay: the reader drops that much after a new tag, where the IQ path dropped 200 ms.
  - Host tests 432; the ARM check clean.
- **2026-10-03, the image on unit A** (`2026-10-03-radio-core-image1`; tezuka_fw 385f52f with the
  device tree's rings and maia-kmod selected, 96b16c3 so a changed device tree rebuilds the
  kernel; B had no USB link to the PC). The 0.3.0 boot files are on A's card in
  `boot_backup_core030`. Clay County, the first 5 minutes:
  - **The core:** `/dev/p25-lanes` 128 x 16 KB; each lane's packets continuous (no stale, lost
    or missed packet, no check failure); 54 samples of settling dropped per tuning; 0.035 % of
    the AD9361's samples at full scale.
  - **Control channel (LSM):** 808 TSBKs in 20 s (40.6 a second, 99.8 % passing; C4FM 710);
    carrier offset +4 Hz; 17,423 blocks, none dropped, no gap.
  - **Lanes:** lane 1 followed TG 850 (1,026 voice frames, each call ended on its talk
    complete) with an HDU for each of its 6 calls; lane 2 on the data channel, 9,778 NIDs.
  - **CPU:** the scanner 81 % of one core (the control decoder 33 %, the lanes on the runtime
    workers 48 %); the system three quarters idle.
  - Next: a longer run against the step 1 numbers, the bench agent and fbench on the new map,
    the FIR speed-up.
- **2026-10-03, two hours on the image:** no warning in the log, no packet fault, lost or missed
  packet on any lane (about 360,000 each), control 40.6 TSBKs a second at 100 %, 23,156 grants
  with none dropped, no block dropped and no gap; crystal −0.688 ppm; the scanner 85 % of a core.
- **2026-10-03, the FIR speed-up** (`2026-10-03-radio-core-fir1`, hand-deployed on A). The A9's
  VFP ran the filters at about 11 cycles a tap pair: its loads into the single-precision halves
  of the registers that held the sums made each tap wait for the one before. `dsp::fsk4::Fir`
  now folds equal mirrored taps in pairs and takes eight outputs at a time in NEON (inline asm);
  a decimating half-band runs on its two input phases. Two things had kept the receivers'
  filters off the folded path: SDRTrunk's half-band holds 1e-17 residues where its zero taps are
  (now counted as zero below 1e-12 of the centre; SDRTrunk's own scalar half-band skips them),
  and both RRCs have a lone tap beyond their symmetric run (the DMR one also an unequal inner
  pair, from SDRTrunk's centre formula). The taps are unchanged.
  - **Cost on A** (ms per second of a lane's IQ, one of I and Q): half-band 22.2 → 3.4, LSM
    low-pass 23.3 → 4.5, C4FM low-pass 13.8 → 3.0, RRC 16.0 → 3.5. Receivers: LSM 14.5 → 4.1 %,
    C4FM 13.6 → 4.3 %, DMR 19.4 → 9.3 % of a core; about 7 % of DMR's is outside the filters.
  - **Parity:** the 313 recordings decode to byte-identical dibits, and 23 of them on the A9
    too; DMR reference 24,984 of 24,996 with the 20:57 call followed; host tests 434.
  - **Unit A, Clay County,** 5 minutes before and after: the scanner 83.7 → 47.7 % of one core
    (the control thread 33.1 → 16.4 %, the runtime workers 50.5 → 31.1 %). Control 40.7
    messages a second at 100 %, no block dropped; lane 1 19 HDUs and 134 / 121 LDU1 / LDU2 in
    its first 6 minutes.
- **2026-10-03, the CPU work** (Andy: find the other costs; drop the dual LSM/C4FM decode, the
  site says what to decode and the scan validates both). `fbench-agent profile` (new: perf
  sampling by thread and function, the link register for a leaf's caller) on unit A found, past
  the filters:
  - **The NID's BCH decoder** searched its 512 KB codebook to the end for any NID with an error,
    and for every false sync on an idle lane (5 a second): 16.7 % of a core across the lanes and
    the control decoders. It is now SDRTrunk's algebraic decoder (syndromes, Berlekamp-Massey,
    the locator's roots), with the closest codeword's answers: 0.1 %.
  - **Both control decoders on every P25 site.** A site runs its modulation's alone; a scan sets
    it on the sites it adds (and on one set to auto); a site set to auto runs both until one is
    chosen. The probe and the receivers choose by one rule, LSM unless C4FM passes clearly more.
  - **The calls view** republished all 100 recent calls after every input, about 110 times a
    second (clones, drops, the allocator: about 3 %); now when a call closes. The survey's noise
    floor is a selection instead of a sort; spectrometer dB in f32.
  - **Gates:** the 313 recordings decode the same (counts and dibits); DMR reference 24,984 of
    24,996 with the 20:57 call; host tests 439.
  - **Unit A, Clay County,** 5 minutes each: the scanner 47.7 % (fir1) → 19.9 % (one decoder,
    the BCH early exit, the calls view) → 15.0 % of one core (the algebraic decoder); the control
    thread 16.4 → 4.0 %. Control 40.8 messages a second at 100 %. Left: the three LSM receivers
    (filters about 6.4 %), the kernel 1.7 %, the spectrometer's frames about 1 %.
  - **Not SDRTrunk's, measured and kept (Andy: SDRTrunk is the baseline, not the ceiling).**
    SDRTrunk decodes the NID's 63-bit BCH word without bit 63, and retries an uncorrectable NID
    with the site's NAC; ours accepts 11 bits or fewer over all 64 and does not retry. On the 313
    recordings both together recover 9 NIDs of 142,359 (+9 TSBKs, +1 LDU1, +1 LDU2, +3 TDU-LCs,
    every one passing its own check; the harness now counts those). On noise the retry passes the
    NAC check it would otherwise fail: a noise NID decodes with the site's NAC 3.3e-4 of the time
    with it, 5e-7 without (a random-word estimate), so an idle lane's 5 false syncs a second
    would give a false NID about every 10 minutes, a noise data unit read as voice. Not adopted.
- **2026-10-03, the CPU work, second round** (`2026-10-03-radio-core-cpu4`, hand-deployed on A;
  Andy: NID recovery and more CPU, then the ATSC work to share it):
  - **`dsp::run`,** the filters' multiply-accumulate runs in one module for every receiver and
    for 081's equalizer: `run::folded` (the symmetric pairs) and `run::plain` (any taps), eight
    outputs at a time in NEON. A folded run adds to outputs the filter's first single tap started
    instead of zeroing them: half-band 3.43 → 3.12, LSM low-pass 4.47 → 4.14, C4FM low-pass
    3.01 → 2.68, RRC 3.49 → 3.18 ms per second of IQ. A 128-tap plain run: 385 ns an output
    against 698 for one sum an output. Four outputs and two taps a step measured slower than
    eight and one.
  - **Spectrometer dB** in integer and f32 arithmetic (no software u64 conversion, no logarithm
    call; within 1e-4 dB): the survey's frames 1.2 → 0.7 % of a core.
  - **The kernel's share** (`profile` now names kernel functions): 1.1 % of a core, wakeups and
    context switches, 0.2 % the lane ring's cache maintenance; nothing to take out.
  - **Gates:** the 313 recordings decode the same; DMR reference 24,984 of 24,996; host tests
    441; the NEON runs' tests on A.
  - **Unit A:** 16.7 % of one core over 5 minutes, in a busier window than cpu2's 15.0 % (Clay
    on the directional antenna; lane 1 carried 2,817 voice frames against 1,215).
  - Left: the LSM demodulator's double-precision atan2, sin and cos (about 0.5 % for three
    receivers) would need the recordings to judge single precision.
- **2026-10-04, every mode's needs** (Andy: the gateware should serve every mode; record the HDL
  changes for the next gateware work). The data mode study (`scanner/doc/DATA_MODE.md`) found
  that the 3a core already gives wide lanes through the DDCs' registers. The section "Every
  mode's needs" lists what the next bakes should add; step 3b now has to keep wide lanes.
- **2026-10-04, the gateware study** ("Every mode's needs", rewritten with the items' changes,
  costs, timing and evidence). What it rests on:
  - per-block utilization and the worst clk3x paths from 3a's routed checkpoint (Vivado on the
    checkpoint, no build);
  - Clay County's lane demand from unit A's history (24 h, read through the API).

  Findings:
  - 3a's thin timing margin is the shared 3x common-edge pulse's fan-out (368 loads, one LUT and
    3.5-4.2 ns of routing on each worst path), not logic depth.
  - A DDC lane costs 13 DSPs, 1,250 LUTs and 7 block RAM tiles.
  - Clay County never needed more than three lanes for its clear calls; 3 of 6,557 grants found
    every lane busy.
  - Only the scanner's live sites would use the channelizer's rates; every other mode needs DDC
    lanes, the spectrum or the capture at its own rate.
  - 8-VSB in the PL: from the input to Reed-Solomon's syndromes, about 18 DSPs, 7,000 LUTs and 12
    tiles, at 10 MSPS.

  Proposed: bake A (the timing fix, a fixed register map, items 3-7), then B-D on evidence.
  Decisions are Andy's.
