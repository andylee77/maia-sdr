# 111 — Two more sensors read, by a table: an AcuRite 606TX and a Prologue sensor

**Date:** 2026-11-12. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. In unit A's SD image, `2026-11-12-range1` (111 to 116; its record
in 116's).

## Why

Unit A hears four devices at 433.92 MHz and 110 read one, Andy's Oregon Scientific sensor.
Asked "Identify and decode the other three devices at 433.92 MHz next?", he answered "Yes,
decode them". Two of the three are named and read; the third is not identified, and nothing
was built for it.

It is the study's table-driven decoder (`scanner/doc/DATA_MODE.md` §5): the two that were named
are keyed alike and differ only in facts.

## What the three are

Read from the three first captures of the window (109) and from the unit's own captures: eight
that Andy kept from the page at 04:14 to 04:17 UTC, of every device, besides the gates' own.

- **433.916 MHz, 44 dB over the floor, every 31.0 s: an AcuRite 606TX,** a temperature sensor.
  32 bits keyed by pulse distance (pulses of 0.53 ms; a gap of 2.03 ms a zero, 4.07 ms a one;
  9.07 ms between repeats), six times in 747 ms. The first byte is its id; then a bit that is
  set while its battery is good, three bits not read here, and its temperature as 12 signed
  bits in tenths of a degree C; the fourth byte is a digest of the first three (each bit that
  is set adds in a key, which is shifted right at every bit; 0x98 is put in when a one falls
  out; the key begins as 0xF1). **What names it:** the two messages the first captures hold,
  `b5 80 d4 be` and `b5 80 d3 10`, both fit that digest, and of the 65,536 pairs a digest of
  that kind can have, that pair alone fits both. Id 181, the battery good, 21.2 °C at
  02:25 UTC and 19.3 °C at 04:14: it is somewhere sheltered, the two outdoor sensors reading
  16 to 17 °C then.
- **433.871 MHz, 18 to 20 dB, about every 53 s: a Prologue sensor,** temperature and humidity
  (the make's protocol, sold under several names). 36 bits and a spare one, keyed as the
  AcuRite's (pulses of 0.48 ms, gaps of 1.96 and 3.91 ms, 8.8 ms between repeats), six or seven
  times in 780 ms. Four bits that are always 1001, eight of id, a bit set while the battery is
  good, one set when its button sent the message, two of channel, the temperature as 12 signed
  bits in tenths of a degree C, and eight of humidity. **It has no check,** so what names it is
  less than the AcuRite's: its keying and its first four bits; its last eight bits going from
  75 to 76 while no other bit changed, which a check over the others could not do (that rules
  out the protocols of the same keying and length that end in one); and readings that make
  sense: 17.1 °C at 02:25 UTC and 15.7 °C at 04:05, each time 1.2 °C over Andy's sensor, with
  75, 76, 84 and 85 % as the night cooled. Id 86, channel 2. Its battery bit says low.
- **433.924 MHz, 12 to 15 dB, every 16.0 s: not identified.** What was read of it:
  - it is on-off keyed, **Manchester coded at 1024 bits a second** (976.5 µs a bit), its
    envelope on half of the time. 109's look had it as "pulses of 0.1 to 0.3 ms": its slicer
    sat between this signal's own median and its peak, and cut noise;
  - a message is 123 ms: a preamble of ones, the bits 0101, then about 29 nibbles. It is sent
    once, then 16.0 s later twice (130.4 ms apart), turn about;
  - its nibbles, each one's last bit first: `C813 0000 0` the same in every message, three
    that change (`811`, `863`, `851`, `841`), `00850F3` the same, two that crept from `96` to
    `A6` over the three captures, `EFF7F7` the same, and a tail that does not read the same
    twice;
  - the framing is Oregon Scientific version 3's (that rate, that preamble, those four bits),
    and the content is not one of that make's sensors': no id of theirs, and no sum of the
    nibbles where theirs is;
  - it is 7 to 8 dB over the noise in its own bandwidth: a matched filter over each bit reads
    it, the scanner's slice would not.
- **A fifth emitter at 433.970 MHz** (seen in 110): pairs of bursts of about 100 ms, 31 s
  apart, 7 dB over the noise in a 610 Hz bin. Too weak to read.

## Changes

### `protocol::ism`: a table of the devices keyed by pulse distance

- **`distance`** (new): the devices whose pulses are all alike, the gap after each a bit and a
  longer gap between the repeats of the message. Each is a row of a table: its keying's times,
  how many bits (and how many spare ones a word may end with), how many times a burst must hold
  the same message, what guards the message (a digest by its two numbers, or bits that are
  always the same), and where its id, channel, battery bit, temperature and humidity are. The
  two rows are the AcuRite 606TX (the digest, twice in a burst at least) and the Prologue
  sensor (its first four bits, three times at least). Another such device is another row, and
  a capture of it to test with.
- **`pulses`: a narrower filter, for every decoder.** A burst's envelope is taken through a
  filter of 4 kHz either side and smoothed over 120 µs (it was 15 kHz and 60 µs): every keying
  read so far has pulses of half a millisecond or longer, and the band's clicks, which are as
  wide as the channel, lose more to a narrow filter than a burst does. `joined` leaves the
  glitches out of an envelope's runs; the table's devices are read with every run shorter than
  160 µs given to the run before it.
  - **That is what reads the weaker of the two new ones:** through 15 kHz the clicks made
    pulses of their own in the Prologue sensor's gaps, and none of its three bursts in the
    first captures read; so read, each does, four to six times of its six.
  - **And it reads Andy's sensor better.** At 13 h UTC the band was noisier than in the
    night's gates: its floor 2 to 8 dB higher (-43 to -50 dBFS, from -51 to -52) and about
    5,600 of a minute's milliseconds clicks, from about 2,600; the devices' own levels were the
    same, so each was that much lower over the floor (his sensor 24 dB, from 31). One burst of
    his sensor then read once of its twice on the unit, through 15 kHz, and not at all in the
    PC's look. Of the eleven captures of it on hand, every one reads twice through 8 kHz or
    less, and none reads worse than before.
- **`Reading`** has `humidity_pct`. A device's key is `<model>.<id>` when it has no channel
  (`acurite_606tx.181`, `prologue_th.86.2`).
- A burst is tried by the Oregon Scientific decoder first, then by the table.

### The pages

- **Devices:** a device's Latest says its humidity too ("60.3 °F · 84 % · battery low"), and
  its data log draws a line for each thing it measures. The note of what is read names the
  three kinds.
- **Scan** and **Captures** name the new devices as they did the first (nothing changed in
  them but what a reading says).

### The API

- `humidity_pct` in every reading (`GET /api/v1/data`, its devices' `latest`, a device's log,
  the captures' list); `null` for a device that sends none.

### The tool

`tools/data_captures.py`: `look --decode` says what any device the tool knows says in a capture
(the two new ones by its own table and its own reading of the envelope), and `keep --decoded`
holds the scanner's reading of each kept burst against it: the model, the id, the channel, the
temperature, the humidity and the battery. How many times a weak burst held its message is each
reader's own count and is not compared. Its readers take a burst through the same narrow filter
as the scanner's now, `--oregon-v1`'s too.

## Not in this change

- **rtl_433 as the reference** (the study's gate for a decoder): it is not on this PC. The
  AcuRite's digest stands in well; the Prologue sensor's naming rests on what is listed above.
- **The fourth device:** not identified, so nothing reads it. A Manchester front end (a matched
  filter over each bit, which is what read it on the PC) is what such a device would need, and
  none is built on a guess of what it is.
- **The bits not read:** three of the AcuRite's second byte, the Prologue sensor's button bit.
  The Prologue sensor's battery bit is read as its protocol's description has it (set while
  good) and nothing here checks which way it is: this sensor's says low.
- **A burst taken apart by frequency:** Andy's sensor and the Prologue sensor are 8.4 kHz
  apart and, by their periods (30.01 and 53.00 s), send at once about every twenty minutes.
  The detector would make one burst of the two, read at its own centre through 4 kHz either
  side: one of them at most. No capture of it is on hand. The longer bursts at his sensor's
  frequency that 109 and 110 took for it (485 to 589 ms) are not that: the Prologue sensor
  was not due at their times, and its message is not in the two that were captured. What
  lengthens a burst by up to 0.2 s, or the 1.3 s one at the Prologue sensor's frequency, was
  not looked at.
- The devices' log on the card, names, filters, kinds beyond "weather".

## Checks

See the status log for the counts.

- **Host tests:** the table's two rows on made messages (the messages unit A heard; under zero,
  a low and a good battery, another id and channel, a humidity over 100; too few repeats, a
  wrong digest, other first bits, another length, another keying's times, noise, a repeat that
  noise broke); the digest on the two messages that named it; the filter and the glitches (a
  weak burst's half-millisecond pulses read through 4 kHz and not through 15; a glitch given to
  the run before it).
- **The captures** (`DATA_CAPTURE_DIR=<repo>/runs/data/look_20261112 cargo test --release
  ism::`): in the three captures of the window every burst of the three sensors reads as the
  looks read it, each time it was sent, and the fourth device's read as nothing; the unit's own
  captures (twelve, copied there) read as the PC tool reads them.
- **`runs/ui_check/data_listen.mjs`:** any device a decoder reads on Scan and on Devices; on
  the PC daemon, whose replayed capture holds the three, each a device with what it last said.
- **`tools/data_captures.py keep --take-radio --decoded`** on unit A, near each of the two:
  the gate, the scanner's own reading of each kept burst the same as the PC's reading of its
  capture.

## Status log

- 2026-11-12: built. Host tests 628 pass (6 new: the table's rows and the digest 4, the filter
  and the glitches 2); the DMR reference's tests pass (102); the ARM check and the ARM release
  build are clean. BUILD_TAG `2026-11-12-distance2`.
- 2026-11-12: **the captures through the decoders** (release, the 30 tests of `ism::` in 1.5 s).
  The three captures of the window: every burst of the three sensors reads, each time it was
  sent. The Oregon Scientific sensor's five, twice each (15.9 °C); the AcuRite's four, six
  times each (21.2 °C three times, then 21.1); the Prologue sensor's three, six, four and five
  times (17.1 °C, 75 then 76 %); the fourth device's six as nothing. The unit's own
  eighteen captures read as the PC tool reads them, the count of repeats too; the one of his
  sensor that had read once reads twice.
- 2026-11-12: on the PC daemon, the first capture replayed as its radio: `data_listen.mjs` 16
  steps at two sizes (two more than 110's: the three sensors each a device with what it last
  said; a device that sends a humidity, picked, with a line for it in its data log).
  `data_mode.mjs` 13, `tool_pages.mjs` 31 at two sizes, `files_page.mjs` 15 and over HTTPS,
  `now_rail.mjs` 29 and over HTTPS, `playlist_page.mjs` 16, `aliases_page.mjs` 34,
  `access_page.mjs` 20 over HTTPS, `modules_check.mjs` 51 modules. `tv_watch.mjs` 10 of 12, as
  in 110: this PC's headless Chrome refuses full screen.
- 2026-11-12: on unit A (binary only, in RAM over the SD image `2026-11-12-decode1`: a reboot
  of A returns to that image, which reads Andy's sensor alone). A was restarted for it at
  13:01 UTC (`distance1`, the table's devices through the narrow filter and his sensor through
  the wide one as before) and at 13:39 (`distance2`, the commit's).
  - **The gate, `tools/data_captures.py keep --count 2 --freq <the device's> --span 6000
    --decoded`** during one stay in data mode, on `distance2` (`runs/data/gate_111_*.json`):
    - the AcuRite: two captures 31.0 s apart, the scanner's reading of each the PC's: id 181,
      14.8 °C (58.6 °F), the battery good, six times. All eight of its bursts in the stay read;
    - Andy's sensor: two captures 30.0 s apart, each the PC's: id 3, channel 1, 15.4 °C
      (59.7 °F), twice. All eight of its bursts read twice;
    - the Prologue sensor: none kept in 150 s the first time (below); asked again with 290 s,
      two captures 53.0 s apart, each the PC's: id 86, channel 2, 17.1 °C (62.8 °F), 75 then
      76 %, the battery low, six times;
    - no gap in the run, no burst dropped, the radio given back each time.
  - **The burst detector found four of the Prologue sensor's nine transmissions in those
    eight minutes.** It sends every 53.0 s, so which were missed is known: one was sent while
    the AcuRite was sending (the detector takes a second device in a step only within 10 dB
    of the first, and it is 25 dB under); four were not found at all; one was found 0.3 s
    early and too long (1,054 ms) and did not read. The three found whole read six times
    each. Half an hour before, on `distance1`, it found three of three, and they read. The
    detector is 109's and is not changed here; this sensor is the weakest that is read (17 to
    19 dB over the floor, its pulses half of a detector's step).
  - **On `distance1`, at 13:03 UTC,** the same gate: the AcuRite's two (14.5 °C, six times)
    and the Prologue sensor's two (15.4 and 15.5 °C, 89 %, five and four times) each the PC's;
    of his sensor's two, one read once of its twice on the unit and not at all on the PC,
    through the wide filter. That capture reads twice through the narrow one, on both.
  - **The cost:** the listening thread at 72 % of one core, as before; the thread that reads
    the bursts 0 to 1 %; the process 84 %.
  - `data_listen.mjs` 15 steps and `data_mode.mjs` 13 with A serving the page: Devices lists
    the three, "AcuRite 606TX, 181, 58.8 °F", "Oregon Scientific v1, 3, channel 1, 60.1 °F"
    and "Prologue TH, 86, channel 2, 62.8 °F · 76 % · battery low", the last with a line for
    its temperature and one for its humidity. Reads only, over HTTP: `scanner_pages.mjs` 40,
    `tool_pages.mjs` 27, `now_rail.mjs` 15, `files_page.mjs` 9, `aliases_page.mjs` 9,
    `playlist_page.mjs --preview-only` 13; over HTTPS: `tool_pages.mjs` 27, `now_rail.mjs` 15,
    `files_page.mjs` 9. `scanner/doc/API_FIELDS.md` regenerated from A. A is back in scanner
    mode, live on Clay County at 100 %, open as before.
- **Seen and not changed:**
  - the burst detector misses the weakest sensor's bursts in a noisy stretch, as above;
  - the band is not as noisy from hour to hour: the floor 2 to 8 dB higher at 13 h than in the
    night's gates, the clicks from about 2,600 of a minute's milliseconds to 10,900;
  - the floor moves within minutes too: the AcuRite, steady at -24.8 dBFS, was 34 to 44 dB
    over it in one stay. Its frequency went from 433.9163 MHz at 21 °C to 433.9172 at 14.5.
- **Not checked:** rtl_433 on the same captures; the Prologue sensor's battery bit, its button
  bit and what a second such sensor reads as; on the air, a temperature under zero or a good
  battery on a Prologue sensor, a low one on an AcuRite (made ones are in the host tests);
  hours of listening; the pages in a browser other than headless Chrome; an image with it.
