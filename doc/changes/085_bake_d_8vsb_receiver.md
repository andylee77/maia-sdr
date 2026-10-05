# 085 — Bake D: the 8-VSB receiver in the PL, live ATSC TV

**Date:** 2026-10-05. **Branch:** fishball-p25. **Bake required:** YES (radio core 2.1.0, product
"rad1": the 8-VSB block in the banks bake A kept for it; the scanner of the same commit reads it).

**Status:** steps 1-3 done (the plan, the gateware with its tests, the bitstream); the PS's side
written and tested on the host; the image and the gates on unit A to come.

## Why

079's item 11: the PL demodulates an ATSC channel in real time from the AD9361's samples to
Reed-Solomon's syndromes; the PS corrects the flagged codewords, derandomizes, reads PSIP and the
programmes, and 084's stream route serves the live stream to the Viewer and VLC in place of the
recordings it plays now. Its evidence is in: 083's fixed-point model and long captures, 084's
player. This is bake D (079, "The bakes").

## Andy's decisions (2026-10-05)

- **No LMS in the PL.** The PL applies the taps the PS loads, at a field's start; the PS solves
  them every 0.25 s, each solve from its own snapshot's field sync, 10 data segments, one round
  (083's answer: within 0.1 dB of solving every field at the decoding edge, about a third of an
  A9 core). An LMS could be added later without changing how taps load.
- **Verilator in a local Docker image** (Ubuntu 24.04's package) for the whole-field simulations;
  Amaranth's simulator runs the stage tests.

## The block

```
 sync           clk3x (187.5 MHz)                                         sync (62.5 MHz)
 AD9361 ──> input buffer ─> matched filter ─> gain, turn, DC ─> y ─> equalizer ─┬─> soft symbols, MER
 samples    (LUT RAM,       (5 DSP, 16        (1-2 DSP; the     │   (8 DSP,     │   12 trellis decoders
 10 MSPS     5 read ports)   cycles each)      loops)           │   16 cycles)  │   bytes, reorder
                                                                ├─ timing loop  │   deinterleaver
                                                                ├─ segment and  │   syndromes ─> codeword packets ─┐
                                                                │  field syncs  │                                  ├─> lane ring
                                                                └───────────────┴──> snapshots ─> snapshot packets ─┘
```

Each stage is the model's (`scanner/src/protocol/atsc/pl`) at its chosen widths, bit for bit. The
symbols come 1.076 to an input sample (10.762 MBd from 10 MSPS); the block runs as fast as its
input lets it and stalls when the next symbol's inputs have not arrived.

### Clocks

- **clk3x (187.5 MHz):** everything from the input buffer to the equalizer's output. A symbol has
  17.42 clk3x cycles on average. The multipliers are DSP cascades fed from registered RAM outputs,
  each stage with its own enable, none on a common-edge pulse; the only common edge is the input
  buffer's write (its own generator, `common_edge.py`).
- **sync (62.5 MHz):** the decoder from the soft symbols on, the packets and the registers. A symbol
  has 5.81 sync cycles; the decoder takes one a cycle at most.
- **The crossings:** the AD9361's samples into clk3x at the block's common edge (as the DDCs take
  them); the equalizer's output and the snapshot values into sync, each a register held for at
  least three clk3x cycles with a toggle (the clocks come from one MMCM and Vivado times the
  paths); the registers' values into clk3x the same way, their pulses as toggles.

### The front end (clk3x)

- **Input buffer:** the last 64 samples (I and Q, 24 bits), in five copies of LUT RAM so the five
  matched-filter DSPs each read their own sample every cycle.
- **Matched filter:** 64 phases × 40 taps (12 bits), the row the instant's fraction rounded to 1/64
  (the model's `MatchedFilter::at`). DSP d (0-4) takes taps 8d to 8d + 7: eight cycles on I, eight on
  Q, 16 cycles an output; the five partial sums are added and requantized to 13 bits. The table is
  one RAMB36 (512 words × 72 bits: the five DSPs' taps of row p, step c at word 8p + c), written by
  the PS (it depends on the true sample rate, `MatchedFilter::new(fs)`), so the PS computes it with
  the model's own code.
- **The per-symbol datapath:** the gain (I and Q times the gain word, requantized to 12 bits), the
  turn (times cos θ and sin θ from an 11-bit table, requantized), the DC taken out, y saturated to
  12 bits; then the loops: carrier (θ and its frequency sum), gain, DC. Each symbol's state comes
  from the symbol before, so the datapath finishes a symbol (about 14 cycles) before the next
  output of the matched filter (at least 16 cycles later) enters it.
- **The timing loop:** at each segment sync symbol the matched filter also runs half a symbol early
  and late (θ turned back and on by half a symbol's turn): three outputs that symbol, issued early,
  late, then the symbol itself, so the two sides pass the datapath with the symbol's state before
  its update. After the fourth sync symbol the next instant waits for the timing error (late less
  early): one bubble of about 35 cycles a segment. The matched filter is busy 13,440 of the 14,494
  cycles a segment holds (93 %).
- **Segment syncs:** the 832 sums over 64 segments in one RAMB36 (1K × 36), the best place taken
  during the last pass (the last of equal sums, as the model's `max_by_key`); then counted.
- **Field syncs:** the PN511 and middle PN63 correlations each segment; the search over 313
  segments, then the count, four misses starting it again; each field sync segment's states latched
  for the packets (its first instant, the carrier frequency, the timing rate, the gain word, the
  sync correlation, the parity, found or counted, the timing loop's gear).

### The equalizer (clk3x)

128 taps, 32 ahead: eight DSPs, DSP d taking taps 16d to 16d + 15 over 16 cycles, the eight partial
sums added and requantized to 12 bits. The window is eight 16-entry LUT RAMs, each passing its
oldest value to the next as a symbol comes in. Two tap sets in LUT RAM: the PS writes the idle one
and arms it; the equalizer swaps them at the next field's first symbol at its output. Until the
first set is loaded the taps are a unit tap at 32 (the model passes its input). The symbols'
places (segment, symbol in the segment, field) go through a 32-symbol delay to meet the output.

### The decoder (sync)

- **MER:** the nearest level and the squared error of each data symbol, summed per field (one DSP).
- **Soft symbols:** the output rounded to 5 bits below a level, limited to ±12 levels (10 bits).
- **The 12 trellis decoders:** one step a data symbol, decoder (k + 4s) mod 12. A step reads its
  decoder's state from LUT RAM (four path metrics of 19 bits: the costs are below 2^17 and two
  steps join any two states, so the spread stays below 2^18; four survivors of 40 decisions; its
  step count; its last Z2), computes the four branch metrics (squarers in LUTs), the four
  add-compare-selects with the model's tie rules, the decision from the best state's register and
  the precoder undone, and writes the state back. The same decoder comes back 12 data symbols
  later, so the steps pipeline freely.
- **Bytes back in field order:** a symbol's byte is known when it arrives. Data symbol k of data
  segment s goes to decoder e = (k + 4 s) mod 12 as its symbol q = 69 s + ⌊k/12⌋ of the field, so
  it carries the pair at place q mod 4 of that decoder's byte j = ⌊q/4⌋.
  - The transmitter's byte map (`fec::byte_map`) as arithmetic: the bytes go to the encoders in
    groups of 12, one each, field byte b to encoder (b + shift) mod 12, the shift up by 4 at every
    data segment's start rounded up to a group. So encoder e's byte j is field byte
    b = 12 j + ((e − 4 ⌊12 j / 207⌋) mod 12), with ⌊12 j / 207⌋ followed as j counts up.
  - The byte and place go through a delay of 468 data symbols (one RAMB18) to meet the decision for
    that symbol: each decoder decides 39 of its own symbols late, and a decoder steps every 12 data
    symbols.
  - Each decoder gathers its four pairs into a byte. A group's 12 bytes finish within 12 data
    symbols and the groups in order, so a 32-byte reorder buffer hands the deinterleaver the
    field's bytes in order.
- **The deinterleaver:** 52 FIFOs of 4 (51 − j) bytes in one RAMB36 and one RAMB18 (5,304 bytes),
  each branch's pointer in LUT RAM; the first 10,608 bytes out are the FIFOs' fill and are
  dropped, as the model's.
- **Reed-Solomon's syndromes:** the 20 Horner sums of each 207-byte codeword (constant
  multipliers over GF(256), XOR networks), the flag their OR.

### Packets, through the lane ring

Two packet sources beside the lanes in the lane ring's round robin, each with a double buffer of
two RAMB36, 4 KB packets of the lane packet's form (079), lane 15:

- **Codewords, format 2** (083's `packets.rs`): an 8-word header and 19 codewords of 26 words (207
  bytes and the flag). 12,894 codewords a second, 679 packets, 2.78 MB/s.
- **Snapshots, format 3** (new): the equalizer's input around a field sync for the PS's solve, on
  the PS's request: the segment before the next field sync, the field sync segment and 10 data
  segments (9,984 values), in five packets.

| Word | Bits | Format 3 field |
|---|---|---|
| 0 | 15:0 | magic `0x5243` |
| 0 | 19:16 | format (3) |
| 0 | 23:20 | lane (15) |
| 0 | 31:24 | flags: bit 0 lost (a value could not be stored: the snapshot is incomplete), bits 1-3 as format 2 (segments counted, fields counted, the timing loop tracking at the snapshot's field sync), bit 4 the field sync's middle PN63 inverted, bit 5 its PN511 passed the threshold (else only counted) |
| 0 | 47:32 | values in the packet (at most 2,016) |
| 0 | 63:48 | sequence (snapshot packets) |
| 1 | 63:0 | the AD9361 sample index at the snapshot's field sync |
| 2 | 31:0 | the field of the snapshot's field sync (counted as format 2's) |
| 2 | 47:32 | the place of the packet's first value in the snapshot (0, 2,016, 4,032, 6,048, 8,064) |
| 2 | 63:48 | tap sets loaded before that field |
| 3-6 | | reserved (0) |
| 7 | 31:0 | check: the XOR of the packet's other 1,023 32-bit halves |
| 8-511 | | the values, 16-bit two's complement (a level is 2^7), four a word, the earliest lowest |

### What changes in 083's model, and why

The model is ours and test-only; the HDL matches it, so where a hardware register needs it the
model changes first and its gate runs again.

- **The packet header's error energy** (format 2, word 3) becomes that of the field *before* the
  first codeword's field. The first codewords of field f leave the deinterleaver about 52 segments
  into field f, long before field f's last symbol is equalized; a packet closed then cannot carry
  its own field's energy. The field before is complete by then. The PS's MER per field reads it
  so.
- **The gain loop's sum is bounded above** where the gain word reaches its largest value (2^28 − 1;
  the model's sum reached 27 bits on the captures). On an empty or very weak channel the gain word
  sits at its limit and the sum otherwise grows by up to 512 a symbol without end: past 32 bits in
  a second.
- **The other registers need no bound:** θ wraps as the model's; the carrier loop's sum matters
  only modulo 2^37 for θ (and the header keeps 32 bits of the frequency, as the model's); the DC
  sum follows its input; the timing loop's sum (48 bits) random-walks by a few ppm an hour on noise.
- **After the fields are lost and found again** (the model decodes only its first unbroken run)
  the decoder starts afresh at the next field found: the trellis decoders' states, the byte
  delay, the reorder and the deinterleaver's fill; the packets' field numbers carry on and a header
  flag (format 2, flags bit 4) marks the first packet after.

## Registers (0x080-0x0FF, kept by bake A)

The banks are in sync like the others; their values reach clk3x as above.

| Offset | Register | Fields |
|---|---|---|
| 0x080 | `vsb_control` | `enable` [0] (0 holds the block in reset: a retune restarts it); `snapshot` [1] pulse (take the next field's snapshot); `taps_arm` [2] pulse (the taps written since take effect at the next field's first symbol) |
| 0x084 | `vsb_step` | the nominal symbol step, input samples × 2^32 (the model's `step0`, from the true sample rate) |
| 0x088 | `vsb_carrier` | the carrier loop's starting frequency (θ a symbol beyond the quarter turn, 2^32 a turn: the channel's centre off the LO) |
| 0x08C | `vsb_status` | `segments` [0], `fields` [1], `tracking` [2], `taps_armed` [3], `snapshot_pending` [4], `overrun` [5] (the symbols outran the front end: the PS restarts the block) |
| 0x090 | `vsb_taps_loaded` | tap sets loaded since the enable [15:0] |
| 0x094 | `vsb_fields` | fields counted since the enable |
| 0x098 | `vsb_lost` | codewords and snapshot values the packet buffers could not take |
| 0x0A0 | `vsb_mf_taps01` | the taps for DSPs 0 and 1 ([11:0], [27:16]) |
| 0x0A4 | `vsb_mf_taps23` | DSPs 2 and 3 |
| 0x0A8 | `vsb_mf_tap4` | DSP 4 [11:0], the table's word `addr` [20:12] (8p + c), `write` [31] pulse commits the word |
| 0x0C0 | `vsb_eq_tap` | the tap [15:0], its `index` [22:16], `write` [31] pulse, into the idle set |
| 0x09C, 0x0AC-0x0BC, 0x0C4-0x0FF | | kept, answered with 0 |

- **Version 2.1.0.** A block in the space 2.0.0 kept answered with 0; nothing else moves, so a 2.0
  scanner runs on it. `capabilities` gains `vsb` [14]. This commit's scanner reads 2.x and offers
  live TV where `vsb` is set, the recordings where it is not.
- **Loading taps:** the PS writes the 128 taps with their indices, then `taps_arm`; `vsb_taps_loaded`
  counts up at the field they start, and the packets say from which field. A set written while one
  is armed waits for it to load first (the PS reads `taps_armed`).

## How ATSC mode sets the radio for live TV

As 081's naming tunes it: the 10M preset (10 MSPS, 8 MHz bandwidth), the LO on the channel's centre
plus the crystal shift, the slow-attack AGC. The PS writes the matched filter's table for the true
sample rate (10 MSPS × (1 + crystal ppm)), `vsb_step` for the same rate, `vsb_carrier` 0 (the centre
is at the LO, as the gate's captures were decoded), then `enable`. A retune drops `enable` first.
The lanes and the spectrum stay idle in ATSC mode (the mode has the radio to itself); the lane
ring carries the block's packets.

## The PS

- **`hardware::radiocore`:** the 8-VSB banks; `parse` for formats 2 and 3 beside format 1; the ring
  reader hands the block's packets to the ATSC service and the lanes' to the streams.
- **What the PS and the model share (`protocol::atsc::live`):** the matched filter's table and its
  words for the block, the step word, the packets' layout and the snapshot gatherer, the tap solve
  (083's `pl::ps`, moved out of test-only code), and a codeword's correction and derandomization.
  The model (`pl`) calls the same code, so the PS gives the block what the vectors were made with.
- **The live receiver (`services::atsc::live`):** it takes the radio as a TV scan does (the two
  exclude each other), tunes, writes the table, the step and `vsb_carrier` 0, and enables the
  block; then each packet on a thread of its own: the codewords counted against the packets'
  sequence, the flagged ones corrected (081's Reed-Solomon decoder), derandomized, read for PSIP,
  and handed to the stream's players; the snapshots gathered and solved on a worker thread, the
  taps quantized, written and armed; the next snapshot asked for 10 fields after the last once the
  timing loop tracks. The front end finds the segment syncs once, at its enable, so the receiver
  restarts the block when it overruns, when its fields stay lost for 3 s, or when it finds none in
  8 s.
- **The stream route:** `/api/v1/atsc/stream/{rf}` serves the live multiplex when the receiver is
  on that channel (live data needs no pacing), the recording otherwise. A request for another
  channel while nobody watches retunes the receiver; while someone watches one channel, another
  channel's request gets its recording if there is one, else 409.
- **Live TV's routes:** `GET /api/v1/atsc/live` (the statistics and whether the core has the
  block), `POST /api/v1/atsc/live` `{rf}` (tune and start, retuning whatever plays), `POST
  /api/v1/atsc/live/stop`. Leaving ATSC mode stops it; a TV scan is refused while it plays.
- **The Viewer:** where the core has the block, Play for every channel the scan named: it starts
  the receiver on the channel's RF channel before the player asks for the stream (so a change of
  channel never meets the last one's stream), and Stop ends both. Under the controls, live TV's
  statistics: the state, the last field's MER, the carrier offset, the timing against nominal,
  codewords corrected and beyond Reed-Solomon, tap solves and their time, packets missed or
  dropped, restarts. Without the block, the recordings play as before.

## Resources against 079's budget

| Part | DSP | LUTs (estimate) | Block RAM tiles |
|---|---:|---:|---:|
| Input buffer, matched filter (table one RAMB36) | 5 | 600 | 1 |
| Gain, turn, DC, carrier and gain loops (the 11-bit table as a quarter wave) | 1-2 | 700 | 0-0.5 |
| Timing loop, segment and field syncs (the sums one RAMB36) | 0 | 600 | 1 |
| Equalizer (window and two tap sets in LUT RAM) | 8 | 700 | 0 |
| MER, soft symbols, 12 trellis decoders (state in LUT RAM), the byte delay | 1 | 1,500 | 0.5 |
| Reorder, deinterleaver, syndromes | 0 | 700 | 1.5 |
| Codeword and snapshot packets | 0 | 700 | 4 |
| **Total** | **15-16** | **about 5,500** | **8.5-9** |

079's budget: about 15 DSP, 7,000 LUTs, 8 tiles. The design keeps about 62,000 LUTs, 131 DSPs and 73
tiles free beside bake A's core.

**Timing risks:** the clk3x chains (each DSP fed from registered RAM, its own enable); the
datapath's loop within 16 cycles (it is a recursion, so its stages are counted, not pipelined
away); the 64-bit instant and the 37- to 48-bit loop sums (split adders); the decoder in sync is
relaxed. Bake A's clk3x slack was +0.605 ns; the target here stays +0.4 ns at clk3x and no waiver.

## The bitstream

Vivado 2023.2, `build_fpga_p25_pretty.sh`, the fourth build; XSA sha256 `cdbb148d…`.

**Timing** (worst setup slack, no failing endpoint, no waiver; worst hold +0.019 ns, clk_fpga_0):

| Clock | Setup | Bake A |
|---|---:|---:|
| clk3x (187.5 MHz) | **+0.179 ns** | +0.605 |
| sync → clk3x | +0.538 | |
| clk3x → sync | +1.229 | |
| clk_fpga_0 (100 MHz, the AXI side) | +0.413 | |
| rx_clk (the AD9361's interface) | +0.536 | |
| sync (62.5 MHz) | +3.665 | |

clk3x is the design's worst and below the plan's +0.4 ns target; 15 paths are under +0.4 ns, in
three places in the front end: the field search's PN511 sign into its correlation (+0.179, 7
levels), the datapath's turned I into y (+0.238), the matched filter's rounding into its output
(+0.375).

**Utilization** (the route hook's `utilization_hier.rpt`):

| Block | LUTs (LUT RAM) | FFs | RAMB36 | RAMB18 | DSP |
|---|---:|---:|---:|---:|---:|
| Matched filter | 429 (160) | 488 | 1 | | 5 |
| Datapath: gain, turn, DC, loops | 426 | 642 | | 1 | 2 |
| Scheduler, timing loop, segment and field syncs | 941 | 1,944 | 1 | 1 | |
| Equalizer | 603 (240) | 687 | | | 8 |
| MER, soft symbols, reorder | 157 (12) | 285 | | | 1 |
| Trellis decoders and bytes | 1,469 (266) | 1,449 | | | |
| Deinterleaver, syndromes | 510 (8) | 375 | 2 | | |
| Codeword packets | 809 | 1,044 | 2 | | |
| Snapshot packets | 602 | 634 | 2 | | |
| The block's crossings, field table, status | 104 | 1,872 | | | |
| **The block** | **6,044 (686)** | **9,420** | **8** | **2** | **16** |
| Its registers (three banks and their crossings) | 199 | 667 | | | |
| **The core** | 13,564 | 20,265 | 36 | 37 | 62 |
| Bake A's core | 7,124 | 10,136 | 28 | 35 | 46 |
| **The design** | 21,689 (40.8 %) | 33,074 (31.1 %) | 46 | 42 | 84 (38.2 %) |

Block RAM tiles 67 of 140 (47.9 %), slices 77.7 % (bake A 59.5 %). Against 079's budget (about
15 DSPs, 7,000 LUTs, 8 tiles): 16 DSPs, 6,243 LUTs with the registers, 9 tiles; the tile over is
the deinterleaver's 5,304 bytes, which need a RAMB36 and a RAMB18 but took two RAMB36. The
flip-flops are many more than the plan counted (the pipelines' registered steps, the crossings'
held registers): 9,420 of the 83,455 bake A left free.

How timing was closed at clk3x (each build's worst
paths, and what changed; none changed what the block computes, and the stage and whole-field
tests ran again after each):

- **Build 1: −1.320 ns** over many paths: the scheduler's issue decision from the input count, the
  field search's decision from its PN511 compare, the equalizer's place memory into its tap counts
  and its sum into the output, the matched filter's sum into its output, the segment search's
  block RAM into its best-place compare, the datapath's products into its saturation, the
  instant's 96-bit adds. Each became registered steps where an evaluation's 16 cycles leave room:
  the instant split at bit 48 with its carry a cycle later, the step and half step registered, the
  rounding and the saturation a cycle each, the search's sum registered before its write and
  compare, the field search's decision a stage after its sums, the equalizer's next field start
  read a symbol ahead.
- **Build 2: −0.177 ns,** 262 endpoints, all from one compare: whether the evaluation's last input
  was in (`lead`) drove the main-symbol queue's 512 clock enables and the next step. The queue now
  takes its entry the cycle after the issue (neither the instant nor the rate has moved by then),
  the step is loaded every cycle the scheduler waits to issue, the compare has its own register
  behind one more scheduler cycle (the matched filter, at 16 cycles an evaluation, stays the
  bottleneck), and the timing error's shifted moves are registered when it arrives. The matched
  filter's table reached its DSPs at +0.167 ns: synthesis had folded the table's output register
  into the DSPs' B registers, leaving the block RAM's unregistered output a cycle to cross to five
  DSPs; that register is kept in the fabric.
- **Build 2's resources** were over the plan: 23 DSPs and 15 tiles. Synthesis put the trellis
  decoders' 397-bit state (12 words) in six RAMB36 and their four squares in DSPs, and the matched
  filter's three adds after its DSPs in three more DSPs. The state is LUT RAM and the squares and
  adds are carry chains now (`ram_style`, `use_dsp`), as the plan has them; an out-of-context
  synthesis of the two modules shows no block RAM or DSP in the decoders and five DSPs in the
  matched filter.
- **Build 3** was stopped: build 2's resources changed the design again.
- **Build 4: +0.179 ns,** met (above).

## Tests

- **Vectors from the model** (an ignored Rust test writes them from 081's and 083's captures):
  each stage's inputs and outputs, so each HDL stage is driven with the model's inputs and checked
  against its outputs: the matched filter's evaluations; the front end symbol by symbol (instant,
  filter, gain word, gained, θ, turned, DC, y; the early and late values, the timing error, each
  segment's and field's records); the equalizer with its tap loads; the decoder (decisions, bytes,
  codewords and flags, MER); the packets.
- **Amaranth stage tests** (`scanner-hdl/test/test_vsb_*.py`, the default suite): each stage on
  thousands of symbols of real captures.
- **Whole fields in Verilator** (`MAIA_HDL_SLOW_TESTS=1`, Docker): the block from the AD9361's
  samples of a capture to the lane ring's packets, compared word for word with the model's, for the
  fields after the first field sync (about 900,000 symbols in), the model's tap loads applied as it
  applies them; on a clean channel and on channels at the decoding edge, with corrections and
  failures.
  - `test_vsb_whole.py` runs two benches (`test_verilator/`): the front end alone (every symbol,
    early and late value, segment and field record) and the whole block (every codeword packet,
    every snapshot value and header, the tap loads, the counters), each against a vector set
    (`VSB_VECTORS`, written by `pl::vectors`).
  - The sets: RF 14 directional (MER 23.8 dB, 7 fields, 10 codewords corrected, none failed,
    one tap load); RF 14 omni, RF 21 and RF 20 directional and 081's RF 11 directional (31 fields each,
    17.6-18.8 dB MER, 5-33 % of codewords beyond Reed-Solomon), three tap loads and 15 snapshot
    packets each.
- **The PS's packet path:** `services::atsc::live`'s test plays a vector set's packets through the
  live receiver's packet thread (`ATSC_VECTOR_DIR`): its players get the model's transport stream
  byte for byte, its counts and PSIP are the model's.
- **The core:** the map with the new banks, the version, `capabilities`, the block's packets
  through the lane ring and its DMA beside a lane's.

## Steps and gates

1. This plan.
2. The gateware (`scanner-hdl/radio_core/vsb/`) and its tests: every stage bit for bit against the
   model on recorded inputs; whole fields end to end.
3. The bitstream: timing met with no waiver, the clk3x slack reported; the hierarchical
   utilization down to the block's parts, against bake A's core and this budget; the XSA committed.
4. The PS: the driver, live mode, the solver live, the stream route, the Viewer's statistics.
5. tezuka_fw: the XSA (the carve-outs stay: the lane ring's 8 MB hold 2.9 s of the block's
   packets); an SD image on unit A, the replaced boot files backed up on the card.
6. On unit A: 079's gates for every bake (an hour on Clay County with no packet fault, lost or
   missed packet and the control channel at 40 or more messages a second at 99.8 % or better; Clay
   Electric DMR; fbench's core tests), and D's own: the stations named live against 081's and 083's
   lists on the same antenna; 30 minutes of one HD channel in the Viewer and in VLC, with the
   codeword failure rate, the MER against the model's on the long captures, and the A9's load with
   the solver running.

## Status log

- **2026-10-05.** Plan. Andy: no LMS; Verilator in Docker.
- **2026-10-05.** The gateware and its tests (87c352f): every stage bit for bit against the model
  on five captures' vectors, the whole block in Verilator word for word. Four builds to close
  timing and hold the resources to the plan (above); the fourth meets it, clk3x +0.179 ns. Its
  XSA committed.
