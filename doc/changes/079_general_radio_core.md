# 079 — A general radio core: a channelizer in the PL, every demodulator in software

**Date:** 2026-10-03. **Branch:** fishball-p25. **Bake required:** yes, at step 3 (a new core
and a new Vivado project). Steps 1 and 2 are software and host only.

**Status:** design approved by Andy on 2026-10-03. Step 1 is next. The current build is backed up
in `MAIA_SDR/_archive/build_2026-10-03_p25-core-0.3.0/` and tagged `build/p25-core-0.3.0` (the
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
| Channel and matched filters (half-band, LPF, RRC) | 50 → 25 kSPS | PS now, **PL later** (step 5) | About 60 % of a software receiver's CPU; only needed in the PL when lanes outgrow the CPU |
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
  MHz presets for live sites; the window planner keeps its rule (±0.45 x rate usable).
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
shared multiplier (`maia-hdl/p25_hdl/polyphase_channelizer.py`) can be reused.

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

Three software lanes fit as today. A software receiver costs 12-17 % of a core, about 60 % of it
filtering, so around ten lanes need step 5 (the filters in the PL), which brings each to about
5-7 %. The software LSM's cost is not yet measured (step 1).

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
   - **3b, the channelizer:** the polyphase bank and the lane synthesizer replace the three DDCs,
     behind the same lane ring. *Gate:* bit-exact against the step 2 model.

   *Gate for both:* timing met with no waiver and a hierarchical utilization report in the build.
4. **Cutover (PS), with 3a.** The scanner's hardware layer for the new core, every lane on
   software demodulators, the removals above. The SD image carries the new bitstream; a 3a
   bitstream and the scanner that reads it ship together.
5. **Optional: the filters in the PL**, when lanes outgrow the CPU. A fixed-point model first;
   *gate:* the same parity as step 2.

## Step 3a: the lane ring core

Ordered by Andy on 2026-10-03. The packet and register details are for his review before the HDL
is written.

### What changes in the gateware

| | Core 0.3.0 | 3a |
|---|---|---|
| Lanes | control DDC, two traffic DDCs, an LSM chain on each | the same three DDCs, lanes 0 (control), 1 and 2, IQ only |
| To the PS | rings for control IQ, traffic IQ, three dibit streams, two pre-diff taps | one lane ring of tagged packets, the spectrum, the raw IQ capture |
| Registers | a vacant address, a bank in reset or a write without byte strobes hangs the bus | every access is answered |
| DMA | `DmaStreamRingWrite` (AW ahead of data, F3/F5/F6) | `RingWriterV2` (hwval: store and forward, drain on disable, burst counters) for the lane ring |
| Removed | — | the LSM chains (~102 DSP, ~14.6k LUT, ~21k FF), dibit rings, pre-diff taps, seeds, NID registers, five HP1 masters |

The spectrometer's inputs are registered in `sync` (064's suggested fix for the worst timing path).

### The lane packet

A lane fills a packet as its DDC delivers samples (50 kSPS) and hands it to the lane ring whole.
Packets are 512 words of 64 bits (4 KB, a multiple of the 128 B burst, four to a sub-buffer): an
8-word header and 504 words of IQ (1008 samples, 20.16 ms). Unused header bits are zero.

| Word | Bits | Field |
|---|---|---|
| 0 | 15:0 | magic `0x5243` ("RC") |
| 0 | 19:16 | format version (1) |
| 0 | 23:20 | lane (0-15) |
| 0 | 31:24 | flags: bit 0 `lost` (samples were dropped before this packet), bit 1 `retuned` (first packet with a new tag), bit 2 `last` (the lane was disabled after this packet) |
| 0 | 47:32 | count: valid samples (at most 1008; fewer when a tag change or a disable closes the packet) |
| 0 | 63:48 | tag: the lane's tag when its first sample was made |
| 1 | 63:0 | sample index: the AD9361 sample count (since `sdr_reset` was released) when the DDC made the first sample |
| 2 | 47:0 | power: the sum of I² + Q² over the valid samples |
| 2 | 63:48 | peak: the largest \|I\| or \|Q\| among them |
| 3 | 27:0 | the lane's NCO word for the first sample |
| 3 | 47:32 | sequence: the lane's packet count (wraps) |
| 4 | 31:0 | ADC clips: the running count of AD9361 samples at full scale (I or Q at ±2047), shared by the lanes; the difference between packets is the overload in between |
| 5-6 | | reserved (3b's channel fields) |
| 7 | 31:0 | check: the XOR of every 32-bit half of the packet's other 1023 halves, so a stale cache line (the driver invalidates L1 before L2, F8) is caught |
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
loads are safe). The bridge answers an address no bank claims (reads 0), times out a bank that
does not answer (a domain in reset), and completes a write with no byte strobes.

| Offset | Bank | Registers |
|---|---|---|
| 0x000 | control (AXI-Lite domain) | `product_id` 0x72616431 ("rad1"); `version` (1.0.0); `capabilities` (lanes 3, packet words 2^9, header words 8, spectrum and capture present); `control.sdr_reset`; `interrupts` (read to clear: lane ring, spectrum, capture) |
| 0x020, 0x040, 0x060 | lane 0, 1, 2 | the DDC registers at today's offsets (coefficient address and data, decimation, frequency, stage control); `lane_control` (enable, tag); `lane_status` (lost, sticky, alone in its word) |
| 0x080 | lane ring | enable; last completed sub-buffer; committed bursts (32 bits, for lap checks); next address; overflow (sticky, alone); sample count (low word latches the high) |
| 0x0A0 | spectrum | as today's spectrometer bank |
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

### The PS side

- A `hardware::radiocore` backend and a PAC generated from the core's SVD in `scanner/core-pac`
  (inside what the image's scanner package already copies). `p25-httpd/p25-pac` stays the 0.3.0
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

### Build

- In place, in the existing package and project: `p25_hdl/p25_top.py` becomes the lane ring core
  (`P25Core` 1.0.0, product "rad1"), with the IP packaging, `projects/fishball7020_p25`
  (three HP1 masters: lanes, spectrum, capture) and `build_fpga.bat --p25` updated. A timing
  failure becomes an error (as `--hwval`), and a route-design hook writes the hierarchical
  utilization report.
- Vivado runs only in the main checkout (the ADI submodule is there); simulation runs anywhere.

### Tests and gates

- **Simulation** (`maia-hdl/test/test_p25_top.py`, rewritten; the timeout bus model): no access hangs (a
  vacant address, a bank in reset, a write without byte strobes); every register reads back; IQ
  through the DDCs into packets (the simulation switch for the input crossing, as hwval's); a tag
  change closes a packet; a disable flushes; `lost`, the sample index and the power are exact
  against a Python model of the packetiser.
- **Bake:** timing met, utilization report.
- **On unit A:** the control channel and both lanes decode at least as well as now; lane 2 carries
  DMR and C4FM; the starts of transmissions after a retune are no longer lost; CPU measured.

## What this supersedes

- **077 (chain-2 IQ tap on core 0.3.0) is not built.** Every lane gets IQ in step 3.
- DESIGN §13's verdicts on a channelizer, the LsmFir area recovery and the HDL front end are
  replaced by this plan.

## Open questions

- **N.** The bank's cost does not depend on N; the synthesizer's time slots and the ring do. 8
  lanes is the proposed first build.
- **The 16 MHz scan preset.** The scan can run at 12.8 MSPS, or the spectrometer alone can serve it
  at 16 MSPS with the lanes idle.
- **The gateware LSM's DC blocker** is not in the software LSM. Its AGC idle gate and no-signal
  hold are (step 1 showed SDRTrunk's loop trapping on traffic-channel gaps); nothing so far points
  at the DC blocker.
- **Wideband captures.** The scanner has no raw IQ capture route; step 2 needs one, or the IIO path.

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
