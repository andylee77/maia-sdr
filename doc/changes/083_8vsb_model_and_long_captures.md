# 083 — Bake D's first evidence: a fixed-point model of the 8-VSB PL half, and long captures

**Date:** 2026-10-04. **Branch:** fishball-p25. **Bake required:** NO. A model and studies only;
no gateware, no SD image.

**Status:** done: the model (its gate met on all 17 of 081's captures), the widths, ten long
captures on unit A, and the answer to 079's question: snapshot solves every 0.25 s hold the MER
when each starts from its own field sync; no LMS. Bake D waits for its third piece of evidence,
the browser's player. Unit A is back in scanner mode on Clay County.

## Why

079's item 11 recommends an 8-VSB receiver in the PL from the input to Reed-Solomon's syndromes,
with the PS correcting, derandomizing and reading the stream. Bake D waited for three pieces of
evidence (079, item 11, "Evidence"). This change is the first two:

1. a fixed-point model of the PL half in Rust, beside 081's f32 receiver, that names the stations
   081 names on its 17 captures; each stage's width chosen by measurement, and each stage's input
   and output inspectable so that the HDL can match the model bit for bit;
2. captures of several seconds, to see whether taps solved on the PS every 0.25 s hold the MER
   or the equalizer must track (an LMS in the PL).

The browser's player (the third) is not in this change.

## The model (`scanner/src/protocol/atsc/pl`)

A test-only module beside 081's receiver. Every stage is a struct a later HDL test can drive
with recorded inputs; the front end can keep each symbol's stage outputs (`front::Symbol`: the
instant, the filter's I and Q, the gain word, the gained I and Q, θ, the turned real and
imaginary parts, the DC, y).

The stages form three groups with nothing fed back from one to an earlier one: the front end
(input to the equalizer's input), the equalizer, the decoder. The PS's tap solves read the
equalizer's input and write its taps. So the model runs the groups one after the other over a
capture, which is what the PL does at once; a field's taps are those the schedule has loaded
before its first symbol.

### Front end (`front`)

| Stage | What | Width (chosen) |
|---|---|---|
| Input | the AD9361's 12-bit I and Q | 12 |
| Timing NCO | each symbol's instant in input samples, 32.32; the step the nominal one (from the true sample rate, the crystal's error taken out, as 081) plus the timing loop's rate | 64 (32.32) |
| Matched filter at any instant | 081's table: 64 offsets between input samples × 40 taps (±10 symbols of RS/2 at 10 MSPS), the row the instant's fraction rounded to 1/64; real taps on I and Q; 80 multiply-accumulates a symbol | taps 12, output 13 (the input's scale) |
| Gain | I and Q times the gain word | word 12 (unity 2^8) |
| Turn | times e^{jθ} from a sine table; θ steps a quarter turn a symbol (the pilot, RS/4 below the centre, to DC) plus the carrier loop's frequency; the real part is the levels and the pilot's DC, the imaginary part the carrier loop's error | table: 11 address bits, values 10 |
| DC | the pilot's level, followed over 2^16 symbols and taken out | |
| y | the equalizer's input: a level is 2^7 | 12 |

The loops (errors taken at 128 a level):

- **Carrier:** θ less the turned imaginary part shifted up 9 bits each symbol, its frequency the
  running sum shifted down 5: a type-2 loop of about 900 rad/s natural frequency, damping 0.7
  (noise bandwidth about 500 Hz).
- **Timing:** at each segment sync, the sync's correlation half a symbol late less half a symbol
  early (five more matched-filter outputs a segment, 0.6 %), the instant moved by it shifted up
  10 bits and the rate by it shifted down 11. For the first 512 segments an acquisition gear
  (14, 3) catches a symbol clock some ppm off before its drift builds.
- **Gain:** the mean |y| held at 4 levels; the gain word is the running sum of the error.
- **Segment syncs:** the sync's correlation at each of a segment's 832 places summed over 64
  segments, after 2^16 symbols for the loops to settle; then counted.
- **Field syncs:** the best PN511 correlation of 313 segments, if it passes a quarter of a perfect
  one (six times the data's spread); then counted, a quarter of a perfect correlation where one is
  due counting as found, the middle PN63's sign giving the parity; four missed in a row start the
  search again.

### Equalizer (`equalizer`)

128 taps, 32 ahead, out[k] = Σ w[i] y[k + 32 − i]; 128 multiply-accumulates a symbol. Taps 16
bits (1.0 is 2^12), the output 12 bits (a level is 2^6). Until the first load it passes its
input. The PS loads a whole set; it takes effect at a field's first symbol.

### Decoder (`decoder`)

- **MER:** each field's data symbols against the nearest level; the error energy goes in the
  packet header.
- **Soft symbols:** 5 bits below a level, limited to ±12 levels (10 bits).
- **12 trellis decoders**, time-shared (symbol k of data segment s to decoder (k + 4 s) mod 12):
  4-state soft Viterbi with 081's branch metrics (the squared distance to the nearer level of each
  coset, its Z2 kept), survivors as registers 40 decisions deep; the decision leaves the best
  state's register; the precoder undone on it.
- **Bytes in field order** (081's byte map), the **deinterleaver** as 52 FIFOs (branch j delays
  by 4 (51 − j) of the commutator's turns), **Reed-Solomon's 20 syndromes** a codeword; the flag
  is their OR.

### Packets (`packets`)

The lane packet (079, 4 KB) in a format of its own (2): an 8-word header, then 19 codewords of 26
words each (207 bytes and the flag); 12,894 codewords a second fill 679 packets, 2.78 MB/s. The
header: the first codeword's field and place, the sample index at its field sync, whether the
timing loop tracks, the tap sets loaded, the field's error energy (MER), the carrier and timing
loops' states, the gain word, the sync's correlation, and the lane packet's check word. The
layout is in `packets.rs`. The model hands the PS its snapshots directly; their packets (a third
kind, below) are the HDL's to lay out.

### The PS half (`ps`)

- **Taps:** every 0.25 s from a snapshot of y (the segment before a field sync, the field sync
  and 10 data segments, 9,984 values), by 081's least squares: trained on the field sync's 728
  known symbols, then one round on the taps' own decisions over runs of 1,024 (eight in a
  snapshot); the taps load at the next field. The first solve waits for the timing loop's
  tracking gear (the header says which gear it is in). The schedule is a parameter (`Schedule`):
  how often, the snapshot, the rounds, from the field sync or from the last taps.
- **The stream:** each codeword whose flag is set is corrected by 081's Reed-Solomon decoder; all
  are derandomized and read by 081's code (`receiver::read`: PSIP, the programs' streams).

## The gate: the model against 081's receiver on 081's captures

`the_model_names_every_station_081_names`: each of 081's 17 captures (0.8 s) through 081's
receiver and through the model at the chosen widths with the default schedule. The model must
name every station 081 names (the same TSID and virtual channels, number and short name), and
nothing that neither 081 nor the HDHomeRun's lineup has on that TSID. **The gate passes.**

| Capture | 081: MER, corrected, failed (of 9,933) | Model: MER, corrected, failed (of 9,308) | Model: failed after the first taps | Names (both) |
|---|---|---|---:|---|
| RF 9 directional | 30.4, 1, 0 | 30.2, 1, 0 | 0 of 8684 | 597: 4.2 DABL +5 |
| RF 10 directional | 17.1, 0, 9933 | 17.0, 0, 9308 | 8684 of 8684 | — |
| RF 11 directional | 17.7, 9681, 220 | 17.6, 6439, 2867 | 2244 of 8684 | 0: 6.1 W11DV-D |
| RF 13 directional | 20.9, 553, 0 | 20.7, 571, 615 | 0 of 8684 | 599: 12.1 WTLV-HD +7 |
| RF 14 omni | 16.7, 0, 9933 | 14.5, 0, 572 | — | — |
| RF 14 directional | 36.8, 1, 0 | 36.1, 12, 0 | 0 of 8684 | 603: 30.1 WFOXHD +3 |
| RF 15 directional | 14.5, 0, 9933 | no field sync | — | — |
| RF 17 omni | 17.0, 0, 9933 | 16.6, 0, 9308 | 8684 of 8684 | — |
| RF 17 directional | 28.4, 1, 0 | 27.5, 6, 617 | 0 of 8684 | 10051: 23.1 WJVF-LD +6 |
| RF 18 omni | 5.4, 0, 9933 | no field sync | — | — |
| RF 19 omni | 27.9, 1, 0 | 26.6, 3, 309 | 0 of 8684 | 605: 47.1 WJAXHD +3 |
| RF 20 omni | 18.7, 1336, 0 | 18.7, 1036, 622 | 0 of 8684 | 601: 4.1 WJXT-HD +5 |
| RF 20 directional | 18.0, 8589, 0 | 18.2, 6184, 809 | 187 of 8684 | 601: 4.1 WJXT-HD +5 |
| RF 21 omni | 23.5, 0, 0 | 22.4, 16, 622 | 0 of 8684 | 607: 59.1 TBN HD +2 |
| RF 22 directional | 12.0, 0, 9933 | no field sync | — | — |
| RF 23 omni | 16.5, 0, 9933 | 16.3, 0, 9308 | 8684 of 8684 | — |
| RF 23 directional | 20.6, 1, 0 | 20.3, 6, 618 | 0 of 8684 | 9469: 20.1 WKBJ-LD +6 |
| RF 36 directional | 16.5, 0, 9933 | 16.0, 0, 8996 | 8684 of 8684 | — |

- **Names:** the same on every capture: RF 9, 13, 14, 17 and 23 directional, RF 11's W11DV-D, RF
  19, 20 and 21 omni (Andy's main stations 4, 7, 12, 17, 30, 47 and 59 among them). Neither names
  RF 10, 15, 22 or 36 directional, or RF 14, 17 or 23 omni.
- **The model starts later:** it counts its fields from the first field sync after its loops
  settle and its segment syncs are found (9,308 packets against 081's 9,933), and its first taps
  come once the timing loop tracks, about three fields in. Its failed packets before them (309-622
  where 081 fails none) are that start; after them it fails none wherever 081 fails none.
- **At the decoding edge it fails more:** RF 20 directional 187 packets after its first taps
  (081: 0) and RF 11 directional 2,244 (081: 220). 081 equalizes the whole capture with taps
  solved over all of it, which a receiver cannot. The names are the same.
- **MER:** within 0.1-1.3 dB of 081's (the front end's loops: "The widths", below), the same at
  the edge.
- RF 14 omni finds its field syncs only near the end (it fails in 081 too, at 16.7 dB); RF 15 and
  22 directional (14.5 and 12.0 dB) and RF 18 (ATSC 3.0) give no field sync, and none of them
  decodes in 081.

## The widths

Each stage was narrowed alone, the others wide (`Widths::WIDE`: 24 bits everywhere, a 16-bit
sine table address, soft symbols 8 bits below a level, survivors 64 deep), on ten of 081's
captures: Andy's main stations (RF 9, 13 and 14 directional; RF 19, 20 and 21 omni), RF 17 and
23 directional, and the two at the decoding edge (RF 20 and 11 directional, 17.6-17.8 dB). The
measure is the MER lost against the wide model's, the worst over the ten. That is always RF 14
directional (36 dB): a clean channel shows the arithmetic first; at 17-20 dB the same widths cost
nothing measurable. The decoder's stages do not change the MER; for them, the measure is the
codewords Reed-Solomon could not correct on the two edge captures (RF 20 / RF 11 directional, of
about 1,600 / 3,000 failed with the wide model). A front-end variant reruns the loops, which moves
the edge captures' failures by about ±50 between runs; a back-half variant reuses the wide front
end, so its counts are exact (`widths_stage_by_stage`).

| Stage | Tried: worst MER loss (dB) or failed codewords more | Chosen |
|---|---|---:|
| Matched filter's taps | 10: 0.22; 12: 0.02; 14: 0.00 | 12 |
| Matched filter's output | 10: 0.61; 12: 0.04; 13: 0.01; 14: 0.00 | 13 |
| Gain word | 10: 0.82; 12: 0.03; 14: 0.00 | 12 |
| Sine table's address | 6: 8.82; 8: 0.78; 10: 0.05; 11: 0.02; 12: 0.01 | 11 |
| Sine table's values | 8: 0.16; 10: 0.03; 12: 0.00 | 10 |
| y, the equalizer's input | 8: 3.73; 10: 0.23; 12: 0.03; 14: 0.01 | 12 |
| Equalizer's taps | 10: 6.72; 12: 1.60; 14: 0.14; 15: 0.06; 16: 0.02; 18: 0.00 | 16 |
| Equalizer's output | 8: 1.00; 9: 0.95; 10: 0.28; 11: 0.07; 12: 0.02; 14: 0.00 | 12 |
| Soft symbols, bits below a level | 1: +301 / +1,440; 2: +71 / +310; 3: +19 / +38; 4: +13 / +10; 5: +1 / −3; 6: −1 / +1 | 5 |
| Soft symbols' limit, levels | 4: every codeword; 6: +1,469 / +5,494; 8: +8 / −13; 12: 0 / 0 | 12 |
| Survivors, decisions deep | 8: +1,256 / +4,162; 16: +176 / +852; 24: +26 / +184; 28: +8 / +85; 32: +5 / +31; 40: −1 / +1; 48: +2 / 0 | 40 |

- **Together** (`Widths::CHOSEN`) they lose 0.10 dB at worst (RF 14 directional) and 0.02 dB on
  average; the edge captures' failures stay within the loops' spread (−47, +41).
- **Each rule:** the narrowest that, alone, loses at most 0.03 dB on the 36 dB capture, or no
  codeword on the edge ones. The equalizer's output also sets the soft symbols' resolution (a
  level is 2^6 at 12 bits), so it is not narrower than the soft symbols need.
- **The soft symbols are 10 bits** (5 below a level, ±12 levels); the branch metrics are their
  squared distances to the cosets' levels (up to 19 levels away: 19 bits).
- **The survivors need 40 decisions,** deeper than the rule of thumb of five constraint lengths;
  48 and 64 gain nothing more.
- In a DSP48 the multipliers' ports (25 × 18) take these widths at no cost; the widths matter for
  the tables, the windows and the sums in LUTs (the plan, below).

The loops' registers and the accumulators: the largest each reached on the ten captures at the
chosen widths (`register_widths`), in signed bits. The HDL adds margin to these.

| Register | Bits |
|---|---:|
| The carrier loop's frequency sum (the frequency is it shifted down 5) | 22 |
| The timing loop's rate sum (the rate is it shifted down 16) | 31 |
| The gain loop's sum | 27 |
| The DC sum | 25 |
| A segment sync's correlation; its early and late sums | 14; 14 |
| A segment's PN511 correlation | 20 |
| The segment search's sums (64 segments) | 19 |
| The equalizer's accumulator | 25 |
| The trellis decoders' path metrics (unsigned, less the smallest each step) | 17 |
| The matched filter's accumulator (bounded by its taps and 12-bit inputs) | 24 |

**Against 081's f32 receiver, stage by stage** (the gate's captures):

- **The equalizer, the decoder and the PS's solves** at wide widths lose nothing: on 081's own
  front end's symbols the model's back half gives 081's MER (RF 14 directional 37.0 dB against
  36.8), the snapshot solves every 0.25 s included.
- **The front end** loses 0.1-1.4 dB, all of it in the loops: 081 fits its timing to a line over
  the whole capture and its carrier to 0.2 ms blocks, where a receiver can only follow them. On RF
  14 directional the timing loop's instants wander 0.0042 symbols RMS about a straight line (0.012
  at most), and the carrier loop's frequency ±0.4 Hz. The loss is largest on the clean channels
  (RF 19 omni 26.6 against 27.9; RF 21 omni 22.4 against 23.5; RF 14 directional 36.1 against
  36.8) and nothing at the edge (RF 20 omni 18.7 and 18.7).
- **The widths** lose 0.10 dB at most.

## The antenna (Andy's question)

Unit A has two antennas on the roof: a UHF TV omni, and the VHF/UHF directional (a Yagi)
aimed at Jacksonville, which 081's directional captures and scans used. Andy: the omni sits about
a foot behind the Yagi on the 12-foot peak, so its line of sight to the transmitters passes
through the Yagi and its reflector. He asked whether that shapes what the omni receives; the
recordings on both antennas say it is likely.

The same stations, 0.8 s of each (`pl::study`: the equalizer solved on each field's own symbols,
which no receiver can beat; echoes as energy outside ±5 symbols of the main path; the spectrum
across the channel smoothed to 125 kHz):

| Capture | MER, one solve | MER, solved per field | Echoes against the main path | Spectrum across the channel |
|---|---:|---:|---:|---|
| RF 14, directional (Oct 3 23:04) | 37.1 | 37.6 | −26.2 dB | flat, 3.4 dB peak to peak |
| RF 14, omni (Oct 3 22:05) | 16.7 | 16.6 | −15.5 dB | 20 dB notches |
| RF 14, omni (Oct 4 21:33) | 18.0 | 18.0 | −9.4 dB | 20 dB notches |
| RF 17, directional | 27.9 | 29.2 | −26.2 dB | flat, 3.5 dB |
| RF 17, omni | 17.0 | 16.8 | −12.1 dB | an 18 dB notch at +0.5 MHz |
| RF 23, directional | 20.6 | 20.6 | −19.9 dB | |
| RF 23, omni | 16.5 | 16.4 | −9.8 dB | |
| RF 20, directional | 18.0 | 18.3 | −9.2 dB | a 15 dB notch at +1.75 MHz |
| RF 20, omni (Oct 3) | 18.7 | 19.0 | −11.1 dB | a 21 dB notch at the upper edge |

- **The omni's channel is notched, not noisy.** RF 14 on the omni has 28-31 dB of carrier to
  noise (the floor below the channel, which RF 14's neighbours leave clear) yet decodes at 17-18
  dB: a linear equalizer that fills a 20 dB notch raises the noise in it. On the directional the
  same station is flat to 3.4 dB and decodes at 37 dB.
- **The echoes are 0.4-2 µs late** (120-600 m of extra path): reflections from the
  surroundings, which the Yagi rejects and an omni takes from every side. The Yagi a foot away
  adds about 1 ns, too little to be an echo of its own, but standing in the direct path it would
  weaken the direct signal against those echoes, which deepens every notch.
- **The omni's notches moved between Oct 3 and Oct 4** (RF 14, RF 20): its multipath is not
  fixed.
- **RF 20 is notched on both antennas**, so that one is the station's own path.
- The data agree with Andy's reading but cannot separate "an omni" from "this omni behind the
  Yagi"; moving the omni clear of the Yagi and capturing again would.

The model's gate uses all 17 of 081's captures, both antennas. For the tap-update question Andy
moved A's feed to the directional, and the long captures were taken on both antennas.

## Long captures (`runs/atsc/captures_20261004/`)

Ten captures of 8.0 s, five channels on each antenna (Andy moved A's feed from the omni to the
directional between the sets), with 081's method: raw IQ from `cf-ad9361-lpc` by `iio_readdev`
with the scanner stopped, written to `/tmp` (A's rootfs is in RAM) and copied off with `scp -O`;
10 MSPS, RF bandwidth 8 MHz, the slow-attack AGC, no AD9361 FIR, the LO on the channel's centre
plus the crystal shift (+0.688 ppm of the LO). Each read took 8.34 s for 80 M samples, and the
ADC DMA status read 0 after each. The folder's README has each one's LO, start time, gains (the
AGC's gain was logged every 0.25 s), level, and the HDHomeRun's lineup fetched before.

| RF | Station (HDHomeRun, TSID) | Omni (21:30-21:34 UTC): gain, MER* | Directional (22:36-22:40 UTC): gain, MER* |
|---|---|---|---|
| 19 | 47.1 WJAXHD (605) | 53 dB, 25.7 | 43 dB, 33.8 |
| 20 | 4.1 WJXT-HD, 17.1 WCWJ-HD (601) | 54 dB, 23.3 | 50 dB, 17.8 |
| 21 | 59.1 TBN HD (607) | 55 dB, 23.3 | 56→57 dB, 18.3 |
| 30 | 39.1 W30EE-D (10619) | 73 dB, 18.0 | 60 dB, 31.6 |
| 14 | 30.1 WFOXHD (603) | 62→61 dB, 17.8 | 43 dB, 37.7 |

\* MER with the equalizer solved on each field's own symbols, averaged over the capture's 330
fields (`pl::study`).

- RF 19, 20 and 21 were A's strongest on the omni in 082's TV scan; RF 30 and 14 the marginal
  ones. Andy's main stations are 4, 7, 12, 17, 30, 47 and 59: RF 20, 9, 13, 20, 14, 19 and 21.
  The long captures hold the UHF ones; RF 9 and 13 are VHF, which the omni barely hears; 081's
  directional captures of both are in the gate.
- **RF 20 and 21 were poor on the directional this evening** (a −7.6 dB and a −11.9 dB echo at
  0.6 µs, 18 dB), where 081's scan on the directional at 02:14 had RF 21 at 31.7 dB: their paths
  vary with the time or the conditions. On the omni they did better today.
- A first set on the directional was taken at 8 MSPS by mistake (the scanner had left its
  Clay County preset; the rate was not set) and retaken at 10 MSPS over the same files.

## How often the taps must be solved

Ten 8 s captures (330 fields each), at the chosen widths. The front end runs once a capture; the
equalizer and decoder then run under each schedule (`how_often_the_taps_must_be_solved`). Solves
start 20 fields in (0.5 s, the loops long settled), and the measures cover fields 21-329: the MER
over them (their energies summed), and in parentheses, where there are any, the codewords
Reed-Solomon could not correct (of about 95,500). Every field's MER under every schedule is in
`runs/atsc/captures_20261004/tap_study/` (CSV, and the passes' summaries).

**How often, each solve from the last taps** (20 data segments, two rounds; 0.25 s is 079's
proposal):

| Capture | Once | Every 0.5 s | Every 0.25 s | Every 5 fields | Every 2 | Every field | Own field (bound) |
|---|---:|---:|---:|---:|---:|---:|---:|
| RF 14 dir | 33.32 | 35.78 | 35.95 | 36.10 | 36.37 | 36.52 | 37.04 |
| RF 19 dir | 28.77 | 32.90 | 33.02 | 33.29 | 33.40 | 33.50 | 33.76 |
| RF 30 dir | 30.17 | 31.18 | 31.21 | 31.23 | 31.24 | 31.26 | 31.41 |
| RF 19 omni | 19.49 | 25.43 | 25.75 | 25.91 | 26.00 | 26.03 | 26.11 |
| RF 20 omni | 17.64 (32.5 %) | 21.36 | 22.19 | 22.53 | 22.71 | 22.74 | 22.90 |
| RF 21 omni | 18.56 (14.1 %) | 21.80 | 22.79 | 23.14 | 23.28 | 23.32 | 23.40 |
| RF 30 omni | 17.76 (12.3 %) | 18.01 (0.2 %) | 18.05 (0.1 %) | 18.07 (0.1 %) | 18.08 (0.1 %) | 18.09 (0.1 %) | 18.10 (0.1 %) |
| RF 14 omni | 16.97 (88.4 %) | 17.25 (80.5 %) | 17.39 (57.4 %) | 17.59 (41.3 %) | 17.75 (23.6 %) | 17.78 (19.4 %) | 17.80 (18.6 %) |
| RF 20 dir | 14.39 (99.1 %) | 16.88 (99.1 %) | 17.17 (83.9 %) | 17.53 (62.3 %) | 17.78 (34.3 %) | 17.82 (29.9 %) | 17.84 (29.0 %) |
| RF 21 dir | 14.27 (97.0 %) | 17.10 (95.6 %) | 17.64 (54.0 %) | 18.05 (16.2 %) | 18.23 (2.4 %) | 18.26 (0.9 %) | 18.28 (0.7 %) |

- **One solve is not enough.** Aging over 7 s, it gives 0.3-6.3 dB less MER than solving every
  0.25 s, and RF 20, 21 and 30 omni fail codewords they otherwise would not.
- **0.25 s holds the strong channels** within 0.6 dB of solving every field, with no codeword
  lost, and within 0.2-1.1 dB of the bound (each field solved on its own snapshot).
- **On the marginal channels, solving from the last taps every 0.25 s loses most of them:** 54-84
  % of codewords fail on RF 14 omni and RF 20 and 21 directional, against 1-30 % solving every
  field.

**How each solve starts** (every 0.25 s):

| Capture | From the last taps (20 segments, 2 rounds) | From the last taps, 10 segments | From the last taps, 40 segments | From the last taps, 1 round | **From the field sync** (20, 2) | Every field (from the last taps) |
|---|---:|---:|---:|---:|---:|---:|
| RF 14 dir | 35.95 | 35.86 | 36.00 | 35.93 | 35.95 | 36.52 |
| RF 19 dir | 33.02 | 32.96 | 33.06 | 33.02 | 33.02 | 33.50 |
| RF 30 dir | 31.21 | 31.18 | 31.22 | 31.21 | 31.21 | 31.26 |
| RF 19 omni | 25.75 | 25.70 | 25.75 | 25.75 | 25.75 | 26.03 |
| RF 20 omni | 22.19 | 22.15 | 22.20 | 22.06 | 22.19 | 22.74 |
| RF 21 omni | 22.79 | 22.75 | 22.79 | 22.64 | 22.80 | 23.32 |
| RF 30 omni | 18.05 (0.1 %) | 18.03 (0.1 %) | 18.07 (0.1 %) | 18.03 (0.1 %) | 17.99 (0.3 %) | 18.09 (0.1 %) |
| RF 14 omni | 17.39 (57.4 %) | 17.50 (48.3 %) | 17.41 (56.3 %) | 17.24 (81.3 %) | 17.71 (27.7 %) | 17.78 (19.4 %) |
| RF 20 dir | 17.17 (83.9 %) | 17.42 (70.6 %) | 17.21 (82.9 %) | 16.90 (99.0 %) | 17.80 (30.1 %) | 17.82 (29.9 %) |
| RF 21 dir | 17.64 (54.0 %) | 17.82 (32.9 %) | 17.69 (49.0 %) | 17.12 (95.9 %) | 18.20 (1.7 %) | 18.26 (0.9 %) |

- **Starting from the field sync, every 0.25 s comes within 0.1 dB of solving every field** on
  the marginal channels, and fails about as many codewords (RF 21 directional 1.7 % against 0.9
  %; RF 20 directional 30 % against 30 %; RF 14 omni 28 % against 19 %). On the strong channels
  the start makes no difference.
- **Why:** from the last taps, the first round's decisions on a 17 dB channel are made with taps
  0.25 s old, and their errors bias the solve; a second round does not undo them. From the field
  sync, the 728 known symbols of the snapshot's own field give taps for the channel as it is.

**The snapshot's size, the rounds and the cadence, each solve from the field sync:**

| Capture | 20 segments, 2 rounds | 20, 1 round | 10, 2 rounds | **10, 1 round** | Every 0.5 s (20, 2) | Every 1 s (20, 2) |
|---|---:|---:|---:|---:|---:|---:|
| RF 14 dir | 35.95 | 35.93 | 35.86 | 35.87 | 35.78 | 35.18 |
| RF 19 dir | 33.02 | 33.02 | 32.96 | 32.96 | 32.90 | 32.31 |
| RF 30 dir | 31.21 | 31.21 | 31.18 | 31.17 | 31.18 | 30.98 |
| RF 19 omni | 25.75 | 25.75 | 25.70 | 25.70 | 25.43 | 24.45 |
| RF 20 omni | 22.19 | 22.19 | 22.16 | 22.16 | 21.40 | 20.35 |
| RF 21 omni | 22.80 | 22.79 | 22.76 | 22.76 | 21.87 | 20.22 (3 codewords) |
| RF 30 omni | 17.99 (0.3 %) | 17.95 (0.4 %) | 17.96 (0.4 %) | 17.94 (0.4 %) | 17.98 (0.2 %) | 17.94 (0.5 %) |
| RF 14 omni | 17.71 (27.7 %) | 17.70 (27.6 %) | 17.70 (28.8 %) | 17.69 (28.2 %) | 17.61 (40.8 %) | 17.39 (51.6 %) |
| RF 20 dir | 17.80 (30.1 %) | 17.79 (28.8 %) | 17.78 (30.5 %) | 17.78 (29.1 %) | 17.69 (39.1 %) | 17.45 (53.8 %) |
| RF 21 dir | 18.20 (1.7 %) | 18.21 (1.3 %) | 18.18 (1.8 %) | 18.19 (1.4 %) | 18.06 (7.6 %) | 17.73 (22.2 %) |

- **The snapshot and the rounds barely matter** once each solve starts from its field sync: 10
  data segments and one round do as well as 20 and two.
- **0.25 s is the cadence:** at the edge (RF 14 omni, RF 20 and 21 directional) every 0.5 s
  fails 1.3-5.6 times the codewords of 0.25 s, and every 1 s more; on the strong channels 0.5 s
  costs up to 0.9 dB of MER and 1 s up to 2.6 dB.
- **The bound** (each field solved on its own snapshot, which no receiver can do) is within 0.1 dB
  of solving every field on the marginal channels: the channel does not move enough within a
  field for tracking in the PL to win anything there.

**The answer to 079's question:** snapshot solves on the PS every 0.25 s hold the MER, provided
each starts from its own snapshot's field sync. The evidence does not ask for an LMS in the PL.
The model's default schedule is now that: every 10 fields, from the field sync, 10 data
segments, one round, the taps loaded at the next field (`Schedule::DEFAULT`).

- **The start** (the default schedule from the timing loop's tracking gear, `start:` in the
  summaries): the first taps come 1-2 fields after the field syncs are counted. While the timing
  loop settles, about 0.5 s more, taps age within their 0.25 s: on RF 14 directional the first 19
  fields run at 22-35 dB before the steady 36. No codeword failed in that start on the strong
  channels, and the marginal ones run at their steady MER from the first taps. On 081's 0.8 s
  captures no codeword fails after the first taps wherever 081 fails none.
- **One exception:** on RF 30 omni (18 dB, notch-free) starting from the field sync fails 0.3-0.7
  % of codewords against 0.1 % from the last taps.
- **The limits of this evidence:** five channels on each of two antennas, on one evening (the
  omni 21:30-21:34 UTC, the directional 22:36-22:40 UTC). Faster fading (an aircraft's flutter,
  wind on the antenna) was not seen here and would ask for faster solves or tracking.

## The solver on the A9

`the_tap_solve_timed` on unit A (the test binary built with the image's flags: `cortex-a9`,
NEON; the scanner live on Clay County beside it, about 5 % of a core): 20 solves of each kind on a
snapshot with the PL's shape and echoes (a solve's work does not depend on its data: its rows,
rounds and Cholesky are fixed). The normal equations' sums run half on each core; over the run
the A9 was 1.7 cores busy for each second of solving, so a solve's CPU time is about 1.7 times its
wall time.

| Solve | Wall time | CPU time (about) | At 4 a second |
|---|---:|---:|---:|
| From the field sync, 20 segments, 2 rounds | 137 ms | 230 ms | 92 % of a core |
| From the field sync, 20 segments, 1 round | 76 ms | 130 ms | 52 % |
| From the field sync, 10 segments, 2 rounds | 83 ms | 140 ms | 56 % |
| **From the field sync, 10 segments, 1 round (the default)** | **47 ms** | **80 ms** | **32 %** |
| From the last taps, 20 segments, 2 rounds | 131 ms | 225 ms | 89 % |
| From the last taps, 20 segments, 1 round | 69 ms | 120 ms | 47 % |

- **079 estimated about 10 % of a core** for a solve every 0.25 s; the default costs about a
  third of one (a sixth of the A9). In ATSC mode the live site sleeps and both cores are free;
  the rest of the PS's work is light (correcting the flagged codewords, the derandomizer, the
  stream).
- The normal equations are in f64 on the VFP. 081 measured NEON f32 at about 0.7
  multiply-accumulates a cycle; whether f32 sums solve as well is not measured here.

## What changes in 079's plan for bake D

The split stands: the PL from the input to Reed-Solomon's syndromes, the PS solving the taps,
correcting, derandomizing and reading the stream. What the model changes or adds:

- **The turn comes after the matched filter, at the symbol rate.** 079 placed the pilot and
  mixer per input sample. As in 081, the filter is real and runs on the channel-centred input;
  the turn is applied to its output, a quarter turn a symbol plus the carrier loop's frequency:
  4 multiplies a symbol (the real part, and the imaginary part for the loop) from an 11-bit sine
  table (512 entries as a quarter wave).
- **A gain and a DC loop**, which 079 did not list: the gain on I and Q (2 multiplies a symbol)
  holds the equalizer's input at its scale through the AD9361's gain steps; the DC loop takes
  the pilot's level out. 081 does both once over a capture.
- **The timing loop** works on the segment syncs: five more matched-filter outputs a segment
  (the instants half a symbol either side of the sync, 0.6 % more filtering), an acquisition gear
  for the first 512 segments, then a narrow tracking loop. It reports its gear in the packet
  header, and the PS takes its first snapshot once it tracks (about 1.6 fields after the segment
  syncs are found).
- **The segment and field syncs:** the search sums 832 places over 64 segments (one RAMB18 of
  sums); a field sync is the best PN511 correlation of 313 segments above a quarter of a perfect
  one. A fixed half of a perfect correlation missed RF 21 on the omni, whose multipath holds the
  unequalized correlation at about 49 %.
- **The snapshot goes through the lane ring,** not block RAM: the segment before a field sync,
  the field sync and 10 data segments, 9,984 values of 12 bits (20 KB as 16-bit words, five
  packets of a third kind) every 0.25 s. 20 and 40 segments solve no better.
- **The widths** above. The block RAM they take: the matched filter's table, 64 × 40 × 12 bits
  (one RAMB36; half that by its symmetry); the equalizer's taps and window, 128 × 16 and
  128 × 12 bits (LUT RAM); the segment search's sums; the survivors, 12 × 4 × 40 × 2 bits
  (3,840 bits, LUT RAM); the deinterleaver, 5,304 bytes (a RAMB36 and a RAMB18); the packets'
  double buffer (two RAMB36, as a lane's).
- **The trellis decoders' branch metrics** are squares of 10-bit soft values: squarers in LUTs,
  or one DSP shared over the 17 clk3x cycles a symbol has.
- **The DSPs:** the matched filter's 861 M multiply-accumulates a second need 5 at clk3x
  (187.5 MHz); the equalizer's 1.38 G, 8; the turn and the gain (6 multiplies a symbol), 1; the
  branch metrics' squares, 0 or 1. 079's estimate of about 18 holds at 14-15.
- **The taps:** solved on the PS every 0.25 s, each from its own snapshot's field sync; no LMS in
  the PL. A snapshot is 12 segments (the one before the field sync, the field sync, 10 data
  segments): 9,984 values of 12 bits, five packets as 16-bit words, every 0.25 s. The solve is
  about a third of an A9 core (079: 10 %).
- **The PS's other work** stays as 079 has it (081's correction, derandomizer, stream, PSIP).
- **Bake D's remaining evidence:** the browser's player (079, item 11, third).

## Checks

- **Host tests:** 497 (16 ignored: the offline studies and gates). The model's own:
  requantizing; the matched filter against its definition; the turn's quarter turns; the Viterbi
  registers against 081's traceback (equal without noise, under 0.1 % apart with it); the
  deinterleaver's FIFOs against 081's index map; the packets (codewords, flags, the check); a
  station through 081's test modulator naming itself through the model, and off centre by 40 kHz
  with the clock 2 ppm off.
- **ARM check:** clean.
- **DMR reference:** 24,984 of 24,996 lines as SDRTrunk's; the follower test keeps the 20:57 call.
- **081's capture test:** its results as before, line for line.
- **The gate:** passes (`ATSC_MODEL_WIDTHS=chosen`).
- **Unit A:** the scanner stopped for the captures (about 5 and 8 minutes, peers told first) and
  back in scanner mode on Clay County after each; the solver timed beside the live scanner.

The offline runs (from `scanner/`, `--release`; the capture folders under `runs/atsc/`):

| Test | Environment | What |
|---|---|---|
| `the_model_names_every_station_081_names` | `ATSC_CAPTURE_DIR=captures_20261003`, `ATSC_MODEL_WIDTHS=chosen` | the gate |
| `widths_stage_by_stage` (ignored) | `ATSC_CAPTURE_DIR`, `ATSC_SWEEP=stage,…` or `chosen` | the widths |
| `register_widths` (ignored) | `ATSC_CAPTURE_DIR` | the registers |
| `how_often_the_taps_must_be_solved` (ignored) | `ATSC_LONG_CAPTURE_DIR=captures_20261004`, `ATSC_TAP_CSV`, `ATSC_TAP_ONLY` | the tap study |
| `echoes_and_their_movement` (ignored) | `ATSC_STUDY_FILES` (`;`-separated), `ATSC_STUDY_SECONDS` | the echoes |
| `diagnose` (ignored) | `ATSC_DIAG_FILE`, `ATSC_LOOPS` | one capture field by field, with the loops |
| `the_tap_solve_timed` (ignored) | none; run on the A9 | the solver's cost |

## Left for Andy

- **The tap answer:** solves every 0.25 s from each snapshot's field sync, about a third of an A9
  core; no LMS in the PL. Agree, or ask for more evidence (a longer evening, more channels).
- **The omni's place:** moving it clear of the Yagi and capturing again would tell whether its
  notches are the Yagi's doing.
- **Pushes:** 083's commits are local on fishball-p25.
