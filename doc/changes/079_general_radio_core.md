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
3. **The core (bake).** A fresh Amaranth package and a fresh Vivado project; `p25_hdl` and
   `projects/fishball7020_p25` stay as they are until the cutover, as `p25-httpd` did for the
   scanner. Simulation against the step 2 model.
   *Gate:* bit-exact against the model, timing met with no waiver, a hierarchical utilization
   report in the build, and the bench corpus (B into A).
4. **Cutover (PS).** The scanner's hardware layer for the new core, every lane on software
   demodulators, the removals above. The SD image carries the new bitstream.
5. **Optional: the filters in the PL**, when lanes outgrow the CPU. A fixed-point model first;
   *gate:* the same parity as step 2.

## What this supersedes

- **077 (chain-2 IQ tap on core 0.3.0) is not built.** Every lane gets IQ in step 3.
- DESIGN §13's verdicts on a channelizer, the LsmFir area recovery and the HDL front end are
  replaced by this plan.

## Open questions

- **N.** The bank's cost does not depend on N; the synthesizer's time slots and the ring do. 8
  lanes is the proposed first build.
- **The 16 MHz scan preset.** The scan can run at 12.8 MSPS, or the spectrometer alone can serve it
  at 16 MSPS with the lanes idle.
- **The gateware LSM's own additions** (DC blocker, AGC idle gate, the no-signal hold) are not in
  SDRTrunk. The software LSM starts as SDRTrunk's; step 1's comparison decides whether any of them
  is needed.
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
