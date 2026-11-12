# 109 — Data mode listens at 433.92 MHz and keeps bursts' IQ

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-11-listen1`) over the SD
image `2026-11-11-ssh1`, which does not have it.

## Why

108 left Data mode a shell: it took the radio and nothing listened. As it was built Andy said he
has an Oregon Scientific outdoor temperature sensor that unit A should hear. Asked "Have Data
mode listen on 433.92 MHz and capture your Oregon sensor next?", he answered "Yes, do that".

This is a first slice of the study's steps 3 (the scan) and 4 (captures), for one window, ahead
of its steps 0 and 1, which still wait for him (`scanner/doc/DATA_MODE.md` §9). It began with a
look at what unit A hears there, because the study did not know ("Its response at 315-433 MHz is
unknown; the first scan shows it").

## The first look

Three captures on unit A at 02:25 to 02:30 UTC on 2026-11-12, with existing tools (the scanner
stopped, the AD9361 set through sysfs, `iio_readdev`): 40 s each of a 2.5 MSPS window at an LO
of 434.42 MHz, manual gain 55 dB. They are in `runs/data/look_20261112/`
(`look<N>_434.42M_2500k.cs16`), read on the PC with numpy.

- **Level:** -39 dBFS rms, the largest sample 309 of 2047: nothing clips at 55 dB.
- **Four devices in 40 s, all within 50 kHz of 433.92 MHz:**

  | MHz | Over the floor | Every | What |
  |-----|----------------|-------|------|
  | 433.880 | 31 dB | 30.0 s | **Andy's sensor: Oregon Scientific version 1.** Manchester at 342 bits a second (1.5 and 3.0 ms pulses), a 5.6 ms sync pulse, 32 bits, sent twice in 392 ms. Decoded on the PC with a good checksum each time: bytes `03 59 01 5d`, id 3, channel 1, 15.9 °C, battery ok |
  | 433.916 | 44 dB | about 31 s | A 32-bit pulse-position message six times in 747 ms (0.53 ms pulses, gaps of 2.03 and 4.07 ms, 9.07 ms between repeats). Not identified |
  | 433.871 | 20 dB | about 53 s | Pulse position, about 36 bits, 780 ms (0.48 ms pulses, gaps of 1.96 and 3.91 ms, 8.8 ms between repeats). Too weak for the look's slicer to read the same bits twice. Not identified |
  | 433.924 | 12 to 15 dB | 16.0 s | Pulses of 0.1 to 0.3 ms, 110 to 250 ms. At the limit of detection. Not identified |

- **A steady carrier** at 434.0318 MHz, about 5 kHz wide. Unknown; it may be a spur.
- **Broadband clicks, about 130 a second:** noise-like bursts of 1 to 3 ms over the whole window,
  7 to 12 dB over the floor, with peaks every 44.7 kHz and the strongest around 433.30 to
  433.34 MHz. Their frequency while on is one hump, with no keying: no data. The even spacing
  suggests a switching supply; these captures do not say whether it is near the antenna, on its
  feed or in the unit. They are 17 % of the milliseconds.

What that decided:

- **A channel of 250 kSPS at 433.92 MHz holds all four devices.** A 50 kSPS lane (7.25 kHz
  passband) would hold none of them whole: the devices sit 4 to 49 kHz off the nominal frequency.
- **Bursts are found in the channel's spectrum, not in its power.** The sensor at 31 dB over a
  3.9 kHz bin's floor is 5 dB over the whole window's noise, and the weakest device is under it.
- **The detector has to tell a click from a device,** and the first try at it (a click is a
  step with many bins up) let clicks through at the frequencies of the comb's teeth, two of
  which are within 2 kHz of the sensor and of the strongest device. What works was read from a
  trace of every step of the three captures: with a device on, the bins outside its own stay at
  their floors (they average under 1.3 times them); in a click they do not, and a click's
  strongest bin is 5 dB over the rest (14 dB at most in a hundred) where a strong device's is
  30 dB (17 dB at least).
- **Manual gain at 55 dB** leaves the strongest device at an eighth of full scale.

## Changes

### The radio: the capture ring read as it fills

`RadioHw` gains `capture_begin`, `capture_read` and `capture_end`: the capture ring started
afresh, its sub-buffers handed on in order as they complete, each with the AD9361 sample index
of its first sample, and whether the reader fell behind or the core lost words. The ring was
only read whole after a stop until now (ATSC's station names, the burst timing check). The
radio core is unchanged.

### `protocol::ism`: the channel and its bursts

- **`channel`:** the window's raw IQ mixed down to a frequency in it, low-pass filtered
  (Hamming-windowed, 12 taps a window sample dropped) and decimated to 250 kSPS: flat to 90 kHz
  either side, and at least 45 dB down on what would alias there. Any window whose rate is a
  multiple of 250 kSPS. The mixer is a table of its phasor's turns, which repeat (in six
  samples for a channel 500 kHz from the LO of a 3 MSPS window): exact, with no phasor to
  carry from sample to sample and keep at length one.
- **`bursts`:** the channel's spectrum a millisecond at a time (four 64-point transforms
  averaged, 3.9 kHz bins, 98 kHz either side). Each bin's floor is its median over the last
  quarter second. A step whose power stands 9 dB over the floor in a run of nine bins is a
  device; up to two runs are read in a step. A step where the bins outside the runs average
  over 1.3 times their floors is a broadband click and is left out, unless a run's peak is 15 dB
  over the rest (a strong device's own skirts). Steps at one frequency (within 9.8 kHz) are one
  burst until 200 ms pass without it, so a message's repeats stay together. A burst needs 8 ms
  on and 15 % of its length; it is cut at 3 s. Each has its start and end, its centre by power,
  its width 10 dB down, its time on, its strongest bin over the floor and its power.

### `services::data`: listening, and the captures

- **`listen`:** when the mode gets the radio it sets the manual gain (the radio's own when that
  is manual, else 55 dB), tunes the one window so far (the `3M` preset at an LO of 434.42 MHz,
  the spot 433.92 MHz half a megahertz under it, clear of the DC spur) and reads the capture
  ring every 20 ms. 3 MSPS is the narrowest preset the radio takes that divides down to the
  channel's rate: the first build asked for `2M`, and the AD9361 refused it on unit A
  ("Invalid argument": without its own filter it samples at 2.083 MSPS or more). The mode said
  why it did not listen and the radio went back to the scanner; a host test now holds the
  window's preset to that. A thread of its own (`data-listen`) cuts the channel, finds the bursts and
  keeps the channel's last five seconds for the captures. A burst's time is its samples': the
  ring's sample index against the clock, by the least delay seen between a block's last sample
  and its reading. When the ring is read too late or the core loses words the capture starts
  afresh and the gap is counted. Leaving the mode stops the listening before the radio is given
  on.
- **What it tells:** the window, the noise floor, the counts (bursts, the milliseconds left out
  as clicks, the gaps), the **emitters** (the bursts within 3 kHz of each other: how many, how
  often by the median of the last 16, their length and level) and the newest 100 bursts. All in
  memory: a restart forgets it.
- **`captures`:** "keep the IQ of the next N bursts", any or those around a frequency. A kept
  burst is two files under `data_captures/<UTC day>/` on the card:
  `<time>_<number>_433.92M_250k.cs16` (the channel's samples, 16-bit I and Q, the ADC's 12 bits
  in the upper 12, 20 ms before the burst and after it) and a SigMF description of the same
  name (`.sigmf-meta`: type, rate, centre, time, and the burst as an annotation). A thread of
  their own writes them, when the card can take them. The list is read from the descriptions at
  start. The newest 500 or 256 MB are kept. A factory reset removes them.
- **On a development host** `--data-replay <file>` plays a raw capture in the radio's place
  (its name gives its LO and rate), at its own rate and from its start again at its end: the
  page checks' radio.

### The API

- `GET /api/v1/data`: the listening as above; `/ws/live` pushes it to a page that subscribes
  (`data`), at each burst and once a second while it listens.
- `PUT /api/v1/data/arm` (`count`, `freq_hz`, `span_hz`) and `DELETE /api/v1/data/arm`.
- `GET /api/v1/data/captures`, `GET /api/v1/data/captures/{name}` (either file),
  `DELETE /api/v1/data/captures/{name}` (by id).
- `status.data`: the listening in a line, for every page's header.

### The pages

Scan is laid out as the module was drawn in 095's canvas (its A3, "Data · Scan (planned
module)"). The first build of the page was three cards of its own making; as it was shown Andy
sent the drawing ("here is the artifact layout that was proposed earlier for the data view"),
and the page was made again to it, with what is built:

- **The strip:** the window's chip (`433`, parked: the one window), and on the right, where the
  drawing has Park here, Next window and Stop, "Keep IQ": the next N bursts, whatever their
  frequency.
- **The map:** what is heard (433.830 to 434.010 MHz) across, the last three minutes down, the
  newest at the top; each burst a dash at its frequency, as wide as the burst, with its time,
  length and level when pointed at; a kept burst's dash in the accent colour. Under it, what was
  counted. It is drawn from the bursts, not from a spectrum: the drawing's lanes to drag are not
  there, the whole map being the one channel.
- **Heard in this window:** the emitters, in the drawing's columns: MHz, Device ("Not
  identified": nothing is decoded or named), Bursts, Every, the level (over the floor; the
  drawing's RSSI in dBm needs a calibration the capture ring's path has not), Signature (the
  length and width of its bursts; the drawing's modulation and pulse times are the pulse front
  end's), and last heard; each row's "Keep" keeps that emitter's next bursts.
- **Captures:** the list, each capture's two files to download, and its deletion.
- **Devices** still says it is not built.
- The header reads "Radio on Data · 433 window · Scanner paused", as drawn.
- While another module has the radio, Scan shows what was last heard.

### Temperatures in °C or °F

As this was built Andy asked: "please make sure we have the output in °F for my dumb American
brain. well °C/°F in settings". The radio's Settings has a Units card with the temperature's
unit, kept on the radio (`units.temperature` in `radio.json`: `celsius`, the default, or
`fahrenheit`) and set with `PUT /api/v1/radio/settings`. The API's own fields stay in °C. No
page shows a temperature yet: the first will be a decoded sensor's reading, and the card says
so. `tools/data_captures.py` prints both. Unit A's is set to `fahrenheit`.

### The tool

`tools/data_captures.py`: `status`, `keep` (with `--wait`: wait for the bursts, download them,
hold them against the list, look at each; `--take-radio`: put the unit in data mode first and
give the radio back), `list`, `fetch`, `delete`, and `look` (a capture's pulses and gaps on the
PC; `--oregon-v1`: whether it holds an Oregon Scientific version 1 message, and what it says).
`look` is a check of a capture, not the scanner's decoder.

## Where it departs from the study, and why

- **The IQ comes from the capture ring, not from a wide lane.** The study's lanes at 250 kSPS
  are its step 1: presets nobody has tested, to be proven on the bench. A window of 3 MSPS can
  be read whole from the capture ring (12 MB a second), so the channel is cut in software and a
  burst's IQ is the very samples it was found in. That holds for a parked narrow window only:
  the scan's 16 MSPS windows are 64 MB a second, and need the spectrometer and the wide lanes as
  the study has them.
- **Captures are listed from their descriptions,** not in the history's database (the study's
  §6): there are at most 500, read once at start.
- **Bursts and emitters are in memory.** The data log per device or frequency, the names and the
  history's tables are the study's step 3.

## Not in this change

- The band plan and the scan across its windows; other windows than 433.92 MHz; a setting for
  the gain or the window.
- The waterfall and the spectrum (`/ws/spectrum`).
- Names for emitters, their signatures by modulation, the data log, the Devices page.
- Any decoder: the pulse front end is the study's step 5. The Oregon Scientific decode is the
  PC tool's.
- A capture of the whole window, and raw IQ as a stream (`/ws/iq`).
- The study's steps 0 and 1 (the RTL-SDR's ground truth, the wide lanes on the bench), which
  need Andy.

## Checks

See the status log for the counts.

- **Host tests:** the channel (flat in its pass band, shut to what would alias, a tone at its
  offset, any block size); the detector on made signals (a keyed burst's time, place and level;
  repeats as one burst and two devices as two; frequency-shift keying as a burst at each tone;
  the weakest device found and a weaker one not; broadband clicks counted and no burst; a
  carrier that stays; a stray millisecond; a very strong burst not a click; a gap); the
  captures (the two files and their names, the list read again, deletion, the oldest going);
  the service (emitters and their interval, arming, a card that cannot take a capture).
- **The first captures** (`DATA_CAPTURE_DIR=<repo>/runs/data/look_20261112 cargo test --release
  the_first_captures -- --nocapture`): every burst found is one of the four devices, the
  sensor's transmissions are where the look found them (392 ms each), and the clicks are
  counted.
- **`runs/ui_check/data_listen.mjs`** (new): the pages, with the radio taken and given back.
- **`tools/data_captures.py keep --take-radio --oregon-v1`** on unit A: the gate, a capture of
  the sensor taken by the unit that decodes.

## Status log

- 2026-11-11: built. Host tests 614 pass (31 new: the channel 5, the detector 10, a capture's
  name and the first captures 2, the captures 4, the listening 2, the service 7, the
  temperature's unit 1); the DMR reference's tests pass (102); the ARM check and the ARM
  release build are clean. BUILD_TAG `2026-11-11-listen1`.
- 2026-11-11: **the first captures through the detector** (release, 1.2 s for the three): six
  bursts in each, every one of them one of the four devices; the sensor's at 7.992 and 38.003 s,
  6.936 and 36.946 s, and 17.880 s, 392 ms each; 7,274, 7,880 and 7,338 of each capture's 39,062
  milliseconds left out as clicks. One of the weakest device's seven transmissions is not found
  (at 7.75 s of the first capture, 12 dB over the floor, under clicks).
- 2026-11-11: on the PC daemon, the first capture replayed as its radio: `data_listen.mjs`
  (new) 12 steps at two sizes (Scan while the scanner has the radio; the radio given to Data
  and the strip, the map and the header saying so; a burst as a dash at its frequency and its
  emitter in the table; "Keep IQ" arming, the next burst kept, its dash marked; Captures
  listing it with its two files, downloaded and held against the list and the SigMF
  description; deleted through the page; Devices not built; the radio given back and Scan
  keeping what was heard; the radio's log). `data_mode.mjs` 13 at two sizes, `tool_pages.mjs`
  31 at two sizes (the Units card among the radio's settings), `files_page.mjs` 15 and over
  HTTPS, `now_rail.mjs` 29 and over HTTPS, `playlist_page.mjs` 16, `aliases_page.mjs` 34,
  `tv_watch.mjs` 12, `access_page.mjs` 20 over HTTPS, `modules_check.mjs` 50 modules.
  `tools/data_captures.py keep --freq 433880000 --wait 45 --oregon-v1` against it: the capture
  the daemon kept decodes twice (15.9 °C, the look's reading). The PC's unoptimised build
  cannot keep up with the replay and counts gaps; unit A's build does not.
- 2026-11-11: `2026-11-11-listen1` on unit A (binary only, in RAM over the SD image
  `2026-11-11-ssh1`: a reboot of A returns to that image, where Data is a shell):
  - **the first build asked for the `2M` preset and the AD9361 refused it** ("AD9361
    in_voltage_sampling_frequency: Invalid argument"): the mode's state said so, the tool
    exited 3 and the radio went back to the scanner, live. The window is the `3M` preset since;
  - **the gate, a capture of the sensor taken by the unit that decodes:**
    `tools/data_captures.py --host 192.168.120.50 keep --count 2 --freq 433880000 --span 6000
    --wait 110 --take-radio --oregon-v1` exits 0. Two captures, taken at 04:07:04.171 and
    04:07:34.182 UTC (30.011 s apart), the burst at 433.8794 MHz, 395 and 392 ms, 30.2 and
    29.0 dB over the floor; each holds the message twice with a good checksum: bytes
    `03 45 01 49`, id 3, channel 1, 14.5 °C (58.1 °F), battery ok. (The look, 100 minutes
    before, read 15.9 °C.) A run before it with the default span of 20 kHz also kept a burst of
    the device 8.4 kHz under the sensor, which is within it;
  - **what the scanner hears there is what the look found:** the four emitters, at 433.8710
    (18 to 19 dB over the floor), 433.8794 (29 to 30 dB, 392 ms), 433.9163 (44 dB, 770 ms) and
    433.9239 MHz (13 to 14 dB, every 15,998 ms); the floor at -51 to -52 dBFS in the 196 kHz
    heard; no gap and no dropped block in any run (the longest about two minutes);
  - **what it costs:** the listening thread takes 73 % of one core (78 % with the mixer's
    phasor carried from sample to sample, before its table), the runtime's workers 13 %, the
    scanner as a whole 86 % of the two cores' 200 %; following calls it takes 12 %. Most of
    the thread is the channel's filter, 145 taps computed one output at a time;
  - `data_listen.mjs` with A serving the page: 12 steps. `data_mode.mjs` there: 13 steps (Data
    listens again when the radio is handed back from TV);
  - with A serving the page, reads only, over HTTP: `scanner_pages.mjs` 40, `tool_pages.mjs`
    27 at two sizes, `now_rail.mjs` 15, `files_page.mjs` 9, `aliases_page.mjs` 9,
    `playlist_page.mjs --preview-only` 13; over HTTPS: `tool_pages.mjs` 27, `now_rail.mjs` 15,
    `files_page.mjs` 9;
  - **A's temperature unit is set to °F** through `PUT /api/v1/radio/settings`, as Andy asked;
    its Settings shows it;
  - `scanner/doc/API_FIELDS.md` regenerated from A;
  - A is left in scanner mode, live on Clay County at 100 %, open, with twelve captures on its
    card: four from the gate's two runs, and eight kept from its page between 04:14 and 04:17
    UTC by another client (no session of this work: Andy, by the page's default of five
    bursts). The scanner was restarted under that client at 04:19 for the last test build.
- **Seen on A and not changed:**
  - **two devices within 10 kHz of each other that send at the same time are one burst:** the
    sensor and the device 8.4 kHz under it do every few minutes (a burst of 485 ms at
    433.879 MHz on A's page, and 539 ms among the captures);
  - the `2M` preset is in the presets' table and cannot be tuned on this radio;
  - the broadband clicks' source, and what the steady carrier at 434.0318 MHz is.
- **Not checked:** hours of listening (whether a gap ever comes; the card's captures as they
  pass 500); a capture opened in inspectrum or URH, or read by rtl_433, which is not on this PC
  (the names' centre and rate are read back by the scanner's own reader only); a manual gain
  against an AGC on bursts, and other gains than 55 dB; a start of the unit in data mode; a
  device keyed in frequency on the air (a made one is in the host tests); the Units card
  pressed in a browser (the API set it and the page showed it); a card that is absent or full
  on a unit (a host test refuses there).
