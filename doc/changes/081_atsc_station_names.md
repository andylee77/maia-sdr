# 081 — ATSC station names: the 8-VSB receiver and PSIP

**Date:** 2026-10-04. **Branch:** `081-atsc-names` (worktree `maia-sdr-080`), from fishball-p25
2dad980; merged into fishball-p25 at c08c407. **Bake required:** NO. The scanner only; the gateware is unchanged.

## Why

After the TV channel finder (080), Andy asked to "jump to the station names": each 8-VSB channel
should say which station it is and what it carries, as an HDHomeRun does.

## What

### The naming step of the TV scan (`services::atsc`)

- **Which channels:** after the sweep, every channel found as 8-VSB with 15 dB or more of
  carrier to noise (8-VSB needs about 15 dB).
- **How:** each is tuned on its own with the 10 MSPS preset and its centre at the LO. 0.5 s of raw
  IQ comes from the radio core's capture ring (`/dev/p25-wideband-iq`, read as its sub-buffers
  complete; `RadioHw::capture`). The capture is decoded on a worker thread. The sample rate
  passed to the decoder carries the crystal's error, as the LO's correction does.
- **Why 0.5 s:** PSIP sends the virtual channel table at least every 0.4 s. In unit A's captures
  the TVCT came every 350-400 ms, the PAT every 100 ms and the STT about once a second, so the
  station's clock is often missing from a 0.5 s capture.
- **Each channel's `station`:**
  - its TSID;
  - its virtual channels: number, short name, long name, program, service type, hidden or
    scrambled;
  - its clock (the STT);
  - the MER;
  - the packets decoded, and those Reed-Solomon could not correct;
  - or why it was not decoded.
- **Progress:** `identifying`, `to_identify` and `identified` in the scan.
- **Event log:** one line a channel, such as "RF 19: TSID 605, 47.1 WJAXHD, … (MER 34.5 dB)".
- **The switch:** `identify` in the scan request (default on).

### The receiver (`protocol::atsc`)

`receiver::identify` takes a channel's IQ to its PSIP. It runs `demod`, then `fec`, then `ts` and
`psip`.

- **Pilot:** RS/4 below the centre. The input is turned up RS/4 and summed over 2 µs sub-blocks.
  The pilot's frequency comes from the slope of their phase over 20 µs blocks, then 0.2 ms ones.
  Its phase comes block by block, a straight line between block centres.
- **Matched filter at any instant:**
  - In the channel-centred signal, 8-VSB's receive filter is a real root-raised cosine at RS/2
    (rolloff 0.1152).
  - It is tabulated at 64 offsets between input samples, spanning ±10 symbols of RS/2: 40 taps at
    10 MSPS. The filtered signal at an instant is then one sum over the inputs around it. There is
    no filter at the input rate and no interpolator.
  - Turned up RS/4 less the pilot's phase, the real part is the 8-level signal.
- **Timing:**
  - The segment syncs (+5 −5 −5 +5 every 832 symbols) are read at two samples a symbol around
    where each is expected.
  - They are summed over blocks of 32 segments and tracked block to block.
  - The result is fitted to a line, refitted three times without the blocks a sample off it.
- **Symbols:** the real signal at each fitted instant, with the pilot's DC taken out and the
  syncs scaled to ±5.
- **Equalizer:**
  - Field syncs are found by their PN511. On alternate fields the middle PN63 is inverted.
  - The equalizer is a least-squares one: 128 taps, 32 of them ahead of the symbol. It is
    trained on 20 field syncs' 728 known symbols, then twice on its own decisions over 16 runs of
    1,024 symbols spread across the signal.
  - A run's normal equations take one row of dot products. The rest follow down the diagonals:
    entry (i + 1, j + 1) is entry (i, j) with the row before the run in and the run's last row
    out. They are solved by Cholesky.
  - It is applied by fast convolution (realfft, overlap-save at 1024).
- **FEC:**
  - The 12 trellis decoders: symbol k of data segment s is encoder (k + 4s) mod 12. Each is a
    soft Viterbi on 4 states with integer metrics; the precoder is undone on the way.
  - The bytes are put back in field order, and the convolutional deinterleaver (52 branches) is
    undone.
  - Reed-Solomon (207,187) runs over GF(256): Berlekamp-Massey, Chien and Forney, up to 10 byte
    errors.
  - Last, the randomizer (from 0xF180, every field).
- **Transport stream and PSIP:**
  - Sections are assembled across packets and checked by CRC-32.
  - The PAT gives the TSID.
  - The TVCT or CVCT gives the virtual channels: UTF-16 short names, and the extended channel
    name descriptor (0xA0) for the long name.
  - The STT gives the clock: GPS seconds less the UTC offset.

The constants follow A/53 Part 2 and A/65. philburr/atsc, an 8-VSB modulator Andy pointed to, is
GPL-2.0, so it was used only to cross-check them; none of its code is used.

### On the A9

The decode first took about 51 s on unit A for a 0.8 s capture; it now takes 5.3 s. Per stage, on RF 19
(0.8 s, while the live scanner held about 40 % of the other core):

| Stage | db8918e | Now |
|-------|--------:|----:|
| Pilot | 1.3 s | 0.33 s |
| Receive filter (at the input rate) | 3.0 s | (in the symbols) |
| Timing | 0.8 s | 0.21 s |
| Symbols | 6.2 s | 1.71 s |
| Field syncs, copies, MER | 1.2 s | 0.13 s |
| Equalizer (convolution) | 8.6 s | 1.30 s (1.13 s) |
| FEC | 4.3 s | 1.58 s |
| **All** | **25.4 s** | **5.3 s** |

A 0.5 s capture takes about 3.3 s. In a TV scan the trunking site sleeps, so both cores are free.
What it took:

- **Library calls and stalls:**
  - On ARMv7, `f32::round`, `floor` and `min` are library calls.
  - A float compare waits for its flags, and a float-to-integer conversion waits for its result
    in an ARM register.
  - The slicer, the Viterbi and the matched filter's stepping now avoid all three: the slicer by
    truncation; the Viterbi on integer soft symbols with mask choices; the instants in 32.32
    fixed point.
- **A barrier per multiply:** `OnceLock::get` is a synchronised load, a barrier on the A9.
  Reed-Solomon fetched its tables on every GF multiply; it now fetches them once a codeword, and
  takes all 20 syndromes together from tables.
- **NEON:**
  - The matched filter sums two instants a pass over their shared inputs. Each table row is padded
    with zeros, so the later instant's taps line up with the earlier one's inputs.
  - The A9 reached about 0.7 multiply-accumulates a cycle: 222 ns an instant with the data in L1.
  - Each sum is stored from NEON instead of moved to an ARM register, which would wait for it.
- **The equalizer's convolution:**
  - The FFT at 1024 beat 2048, 4096 and 8192 on the A9.
  - A direct 128-tap NEON run costs 385 ns an output (079's measure), so the FFT stays.
- **Copies:** the symbols, the equalizer's output and the decoder's input are one flat array.
- **Both cores:** every pass over the whole signal runs half on each core. That covers the
  pilot's sums, the symbols, the convolution, the soft symbols (gathered in one pass), the
  Viterbi (six encoders each), the byte stream and Reed-Solomon.
- **The filter's span:** at ±10 symbols of RS/2 the captures decode as at ±12. At ±8 they lose
  packets.

### UI

- **Setup:** a "name the stations" switch.
- **While naming:** the progress, channel by channel.
- **The table:** a Station column, showing each named channel's first virtual channel and how
  many more ("47.1 WJAXHD +3"), or why it was not decoded.
- **Under a picked channel's spectrum:**
  - its station: TSID, MER, packets and its clock;
  - a table of its virtual channels: number, names, program, service, hidden or scrambled.

### Tools

`tools/atsc_check.py` names the stations by default (`--no-names` to skip). For each RF channel
it compares the virtual channels the unit read (number and short name) with the HDHomeRun's
programs on that channel.

- **Exit 3:** a virtual channel the unit named that the HDHomeRun does not list there.
- A station the unit could not decode is not a difference.

## Results

### Unit A (2026-10-04)

- **Setup:** unit A on the VHF/UHF directional antenna aimed at Jacksonville, moved over from the
  HDHomeRun. The HDHomeRun's lineup is from its own scan.
- **The scan:** 33 channels in 86.9 s with the AGC: 25 8-VSB, 2 without the pilot (ATSC 3.0 on
  RF 18, and RF 4), 6 vacant. Naming tried 18 channels, about 4 s each: tune, settle, capture,
  decode.
- **16 stations named:** RF 9, 13, 14, 17, 19, 20, 21, 23, 24, 27, 28, 30, 31, 33, 34 and 35.
  - Their 104 virtual channels are every program the HDHomeRun lists on those RF channels.
  - Every number and short name matches the HDHomeRun's; `atsc_check.py` exit 0.
  - RF 20 carries two stations: 4.1 WJXT-HD and 17.1 WCWJ-HD.
  - RF 27 carries 16 virtual channels.
- **MER:** 17.9-37.4 dB on the named stations.
- **Not decoded:** RF 10 (WJXX, MER 17.2 dB) and RF 11 (W11DV-D, 17.0 dB).
- **Not tried, below 15 dB of carrier to noise:** RF 15, 16, 22, 25, 29, 32 and 36.

### Captures on the PC (`runs/atsc/captures_20261003/`)

All 17 captures decode as before every speed change:

- **Named:** RF 19, 20 and 21 on the omni; RF 9, 13, 14, 17, 20 and 23 on the directional.
- **RF 11 on the directional:** its TSID (0) and 6.1 W11DV-D, with 200-220 of 9,933 packets
  beyond Reed-Solomon.
- **Not decoded:** RF 10, 15, 22 and 36 on the directional, and RF 14, 17 and 23 on the omni. Their
  MER is 12-17 dB.

## Checks

- **Host tests:** 466 (10 ignored), among them:
  - the trellis, Reed-Solomon (up to 10 errors; 11 reported) and the whole FEC chain with noise;
  - the matched filter (the table's sums against the definition, in pairs and alone);
  - the run recursion's normal equations against row-by-row sums;
  - the slicer, sections across packets with CRC-32, a TVCT with names, the STT;
  - two end-to-end tests through a test modulator: a station names itself, also off centre and
    2 ppm off clock;
  - the capture test (`ATSC_CAPTURE_DIR`).
- **On the A9:** the ATSC tests pass, the NEON paths included.
- **ARM check:** clean.
- **UI:**
  - `node --check` of `views/atsc.js` and `api.js`, and their imports;
  - the host build in headless Chrome: the switch shows; with no spectrometer, the scan fails
    cleanly;
  - unit A's page after the scan: the Station column, and RF 19's station and virtual channels
    under its spectrum.
- **Unit A:** `2026-10-04-atsc-names2` was hand-deployed for the test. A is back on maia-sdr-40's
  `2026-10-03-radio-core-cpu4` in scanner mode on Clay County.

## Next

- **Weaker stations:** RF 10 and 11 fail at a measured MER of about 17 dB, and other captures at
  12-17 dB. A decision-feedback equalizer is the usual next step for long echoes; see first which
  echoes those channels have.
- **Speed:** the Viterbi (1.1 s for 0.8 s on two cores) could run four encoders at once in NEON.
  The FFT is scalar on ARMv7.
- **The station's clock:** a longer capture, or a second one when the first has no STT.
