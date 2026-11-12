# 112 — An analog receiver (wide FM, narrow FM, AM) and the Aircraft module's shell

**Date:** 2026-11-12. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-12-analog1`) over the
SD image `2026-11-12-decode1`, which does not have it.

## Why

Andy, 2026-11-12: "can you add a radio section to the page that plays FM, Air, HAM WFM, NFM
analog signals and are we able to decode any adsb signals with the current antenna? add a
skeleton for it".

Two things, then:

- **An analog receiver,** a module of its own in the rail: broadcast FM, the air band, the
  amateur bands the board reaches and other two-way radio, heard in the browser.
- **ADS-B:** the answer to the question, by a capture from unit A read on the PC (the roadmap's
  Q11 and the first half of its step 8), and the module's skeleton: its mode, which takes the
  radio and gives it back, and its pages saying what they will hold.

It is the roadmap's Q12 ("which other modes") answered for analog signals, and its step 8
begun. No drawing exists for either module (095's canvas and the study's draw the Scanner, TV
and Data), so both pages' layouts are this change's own, for Andy to correct.

## What unit A's antenna hears

Raw captures with the scanner stopped (`iio_readdev`, the radio set through sysfs), read on the
PC. They are in `runs/adsb/look_20261112/` and `runs/analog/look_20261112/`.

### 1090 MHz: yes, plenty

Two captures of 12 s at 4 MSPS, the LO at 1090 MHz, manual gain, read by `tools/adsb_look.py`
(a preamble's four pulses found in the amplitude, the 112 bits sliced by which half of each
microsecond is the stronger, the message taken when its 24 bits of parity are right; no bit is
corrected, so a message it counts is one that was sent).

| | Gain 73 dB | Gain 55 dB |
|---|---|---|
| Extended squitters with a good parity | 2,558 (213 a second) | 2,031 (169 a second) |
| All-call replies with a good parity | 764 | 585 |
| Aircraft | 122 | 79 |
| with a position (an even and an odd message both heard) | 84 | 57 |
| with a callsign | 64 | 46 |
| Their pulses over the median amplitude | 6 to 41 dB, median 22 | 4 to 52 dB, median 21 |
| Samples at the ADC's full scale | 157,133 | 9,123 |

The positions lie from 28.2 to 31.8 °N and 82.7 to 79.6 °W, the farthest 221 km from their own
middle (the tool does not know where the unit is), at 1,375 to 45,000 ft; airliners' callsigns
among them (DAL1326, SWA2394, AAL2042, UAL2746). The aircraft heard most sent 60 to 70 messages
in the 12 s, which is about what one sends. At 73 dB the strongest clip; at 55 dB few do and a
third of the aircraft go, the far ones. This is the scanner's own antenna and feed, with no
filter or amplifier for 1090 MHz.

So the antenna is no obstacle. What a decoder on the unit needs is the window's samples at
2 MSPS or more without gaps; Data mode and this change's receiver read exactly that from the
capture ring, at 3 MSPS. Whether the A9 keeps up with a preamble search at that rate is not
measured.

### The analog bands

- **Broadcast FM:** a second at 16 MSPS around 98 MHz. The station at 102.9 MHz, through the
  wide FM receiver: its stereo pilot at 19 kHz stands 60.9 dB over what is around it in the
  discriminator's output, which says the discriminator and the rates are right.
- **Weather radio:** 12 s at 3 MSPS with the LO at 163.05 MHz. 162.550 MHz is on the air,
  20.9 dB over the floor in 15 kHz; the six other weather channels hold nothing and stay shut
  under the squelch. The audio the narrow FM receiver made of it, given to Whisper on the PC,
  reads: "Tides, Vilano Beach, high 10:49 AM Low 5:18 PM St. Augustine Beach, high 10:25 AM Low
  4:50 PM".
- **The air band:** 2.5 s at 16 MSPS around 127.5 MHz showed a steady carrier at 125.000 MHz
  and something that comes and goes at 133.3 MHz. On the unit, through the receiver itself: a
  pass over the band's 760 channels by the meter found nothing steady but that carrier, 10 dB
  over the floor, and what AM makes of it is tones (120 Hz, 1.84 and 3.09 kHz) and noise, not
  a voice (125 MHz is the Ethernet's clock, so it is probably the unit's own; not looked into
  further). Five channels watched for 25 s each, 133.3 MHz among them, stayed shut. No voice
  was heard, so **AM is checked with made signals only.**

## Changes

### `protocol::analog`: the three receivers

New. A `Receiver` takes the channel at 250 kSPS (`protocol::ism::channel`, as Data mode cuts
it) and makes audio at 31,250 Hz, a channel's rate over eight.

- **Wide FM (`wfm`):** the discriminator at 250 kSPS (the phase from each sample to the next,
  scaled by a deviation of 75 kHz), 75 µs of de-emphasis, then a low-pass at 13.5 kHz that
  takes one sample in eight. Mono: the pilot and the stereo difference are filtered away. It
  has no squelch.
- **Narrow FM (`nfm`):** the channel filtered to 7.5 kHz either side and decimated by eight,
  the discriminator there (a deviation of 5 kHz), 530 µs of de-emphasis, a high-pass at 250 Hz
  (the sub-audible tones) and a low-pass at 3.4 kHz.
- **AM (`am`):** filtered to 5 kHz either side and decimated, the envelope over its own mean
  taken over a tenth of a second (so the audio's level does not follow the carrier's), the same
  high-pass and low-pass.
- **The meter and the squelch:** a 256-point spectrum of the channel every 2,048 samples, four
  of them averaged (a reading every 33 ms): the power within the receiver's width, and for the
  narrow receivers that power over the floor's in the same width, the floor being the median of
  the bins 20 to 85 kHz either side. The squelch opens when the carrier is that many dB over
  the floor, shuts 2 dB under it, and holds a third of a second after the carrier went.

### `services::analog`: the module

New. While the mode has the radio it is parked on one frequency:

- **The window:** the `3M` preset (the narrowest the AD9361 takes that divides to the channel's
  rate), **the LO half a megahertz above the frequency,** so the LO's own spur and the DC
  offset are outside what is heard; the AD9361's own gain control on (slow attack), since a
  broadcast station and a handheld differ by 60 dB.
- **The samples:** the capture ring read as it fills, as Data mode reads it (109), and a thread
  of its own (`analog-listen`) that cuts the channel and runs the receiver. A change of
  frequency moves the LO and starts the capture afresh; a change of kind or squelch is the
  thread's alone. From a tuning to another frequency until the radio is there, nothing is
  played and the meter has no reading (`level` is null): what the radio still hears is not
  passed off as the new frequency's. Lost samples are counted as a gap, and the audio breaks
  there.
- **The radio's log** has a line where the listening began and one when it stopped; a turn of
  the dial is not an event.
- **The audio:** frames of 20 ms (625 samples, 16 bits, mono) to whoever listens; nothing while
  the squelch is shut.
- **What it is tuned to** (`freq_hz`, `demod`, `squelch_db`) is kept across restarts in the
  radio's state, written five seconds after the last change. A unit that has never been tuned
  starts on 162.550 MHz, narrow FM, squelch 8 dB.
- **The bands a page offers,** served with the view: FM broadcast (87.9 to 107.9 MHz in
  200 kHz, wide FM), the air band (118 to 136.975 MHz in 25 kHz, AM), 2 m amateur (144 to
  148 MHz in 5 kHz), marine VHF (156 to 162.025 MHz in 25 kHz), weather radio (162.400 to
  162.550 MHz in 25 kHz) and 70 cm amateur (420 to 450 MHz in 12.5 kHz), the last four narrow
  FM. Any frequency from 70 MHz to 6 GHz can be typed.
- On a development host `--analog-replay <file>` plays a raw capture in the radio's place:
  what its window holds can be tuned, and nothing else.

### The modes

`Mode::Analog` (`analog`) and `Mode::Adsb` (`adsb`, "ADS-B mode" in the log), each handed the
radio and handing it on as 108 made every mode do. `services::adsb` is the shell: it holds the
radio while the mode is on and does nothing with it.

### The API

- `GET /api/v1/analog`: the receiver's view (its state, what it is tuned to, the window, the
  meter's last reading, the listeners, the counts, the bands).
- `PUT /api/v1/analog/tune`: `{freq_hz, demod, squelch_db}`, the whole of it each time. It may
  be set while another mode has the radio; it is an administrator's, as every write is.
- `GET /ws/analog`: a text frame `{"type": "format", "rate_hz": 31250}`, then binary frames of
  audio while the squelch is open; `{"type": "lag", "skipped": n}` to a listener that fell
  behind. A listener's level is enough.
- `status.analog`, the receiver in a line; `analog` on `/ws/live`, pushed when its `rev` moves
  (at a tuning, when the squelch opens or shuts, four times a second with the meter).
- `GET` and `PUT /api/v1/mode` take `analog` and `adsb`.

### The pages

- **Analog** (the rail, after Data), one page, **Listen:** the bands as chips; the frequency
  large, with a step down and a step up of the band's own step and a box to type one; wide FM,
  narrow FM or AM; the squelch (on or off, and how many dB); the meter with the squelch's mark
  on it; and Listen, which takes the radio when another mode has it and starts the sound. The
  sound is played as the scanner's live audio is (a ring fed from the socket, never an
  `<audio>` element on a stream) and follows the header's volume.
- **Aircraft** (after Analog), two pages, **Aircraft** and **Plot,** each a card saying it is
  not built and what it will hold: a table of the aircraft heard, and a plot with range rings
  and no map from the Internet (the roadmap's Q11 as recommended).
- The header's radio pill says what the module with the radio does: "Radio on Analog ·
  162.5500 MHz NFM", "Radio on Aircraft · nothing listens yet".

### The tools

- **`tools/analog_listen.py`** (new): `status`; `tune --mhz --demod --squelch`; `listen
  --seconds N [--take-radio] [--silent]`, which records what `/ws/analog` sends to a WAV under
  `runs/analog/<host>/` and says how much audio came and what the meter read. It exits 1 when
  no audio came (or, with `--silent`, when some did).
- **`tools/adsb_look.py`** (new): reads a 4 MSPS capture at 1090 MHz as above. It is the look's
  tool and the reference a decoder on the unit is to be held against, as `data_captures.py
  look` is Data's.

## Not in this change

- **The ADS-B decoder,** the aircraft's table and the plot: the mode is a shell.
- **AM heard on the air:** no voice was captured in the air band (above).
- **Stereo, RDS.** Wide FM is mono.
- **Single sideband and CW,** so no HF amateur bands; and the board does not tune under 70 MHz
  (the AD9361's range; unit B's AD9363 begins at 325 MHz), so neither 6 m nor shortwave.
- **Memories, a scan of a band, the stations found, recording, a waterfall.**
- **A listener tuning.** A listener hears what an administrator tuned.
- **A lane for the narrow receivers.** A 50 kSPS lane holds a narrow FM or an AM channel and
  costs the A9 nothing; all three kinds go through the capture ring here because wide FM
  needs it and one path was built.
- **The sound on the unit itself** (the handheld's listening path): it plays in the browser.

## Checks

See the status log for the counts.

- **Host tests:** each receiver on a made signal (wide FM gives its tone and shuts out the
  pilot; narrow FM gives a voice tone and keeps shut on noise; the squelch opens with a
  carrier and shuts a third of a second after it; AM gives its tone whatever the carrier's
  level or offset); the service's tuning (refused out of the radio's range or with a squelch
  out of 0 to 40 dB; wide FM takes none), the bands, the meter moving the view and its reading
  dropped at a tuning elsewhere; the listening thread on a made window (frames for a signal,
  none under the squelch, the kind followed, nothing played between a tuning elsewhere and
  the radio's move); the window's preset; the modes handed round through `analog` and `adsb`.
- **The captures** (`ANALOG_CAPTURE_DIR=<repo>/runs/analog/look_20261112 cargo test --release
  analog::`; with `ANALOG_AUDIO_OUT` the audio is written as WAV): the weather channel heard
  and the six beside it shut; the broadcast station with its pilot at 19 kHz.
- **`runs/ui_check/analog_listen.mjs`** (new): the Listen page takes the radio, tunes by band,
  by step and by a typed frequency, changes the kind and the squelch, shows the meter, and
  gives the radio back; `--fm <MHz>` names a station for a unit.
- **`runs/ui_check/data_mode.mjs`:** the five modes handed round, the rail's order, each
  module's line, the radio's log.
- **`tools/analog_listen.py listen --take-radio`** against the PC daemon and on unit A.

## Status log

- 2026-11-12: built. Host tests 638 pass (the analog module's 12 among them); the DMR
  reference's tests pass (102); the data captures' 30 in release as before; the ARM check and
  the ARM release build are clean. BUILD_TAG `2026-11-12-analog1`.
- 2026-11-12: **the captures through the receivers** (release, `analog::`). The broadcast
  station at 102.9 MHz: a second of audio, rms 0.233, its pilot at 19 kHz 60.9 dB over what is
  1 kHz from it in the discriminator's output. The weather channel at 162.550 MHz: 12.0 s of
  audio, rms 0.169, 20.9 dB over the floor; the six other weather channels: no audio, −0.8 to
  0.9 dB over the floor.
- 2026-11-12: on the PC daemon, the weather capture replayed as its radio:
  `analog_listen.mjs` 11 steps at two sizes; `data_mode.mjs` 18 at two sizes (the five modes
  handed round); `data_listen.mjs` 16 at two sizes; `tool_pages.mjs` 31 at two sizes;
  `files_page.mjs` 15 and over HTTPS; `now_rail.mjs` 29 and over HTTPS; `playlist_page.mjs` 16;
  `aliases_page.mjs` 34; `access_page.mjs` 20 over HTTPS; `access_api.py` 34, the analog
  receiver's doors among them (nobody reads it or opens its socket; a listener reads it and
  hears it; a listener's tuning is refused); `modules_check.mjs` 54 modules. `tv_watch.mjs`
  10 of 12, as since 110: this PC's headless Chrome refuses full screen.
  `tools/analog_listen.py listen --take-radio` there exits 1: 6.8 to 8.9 s of audio in 10 s,
  the PC daemon dropping blocks of the window (counted in `counts.dropped`). It is a debug
  build, which takes 1.5 s for a second of the window (the capture test's seven channels of
  12 s in 124 s; in release 1.3 s), so it cannot keep up; the page check there counts that
  frames come and go on coming, not how many. The checks' own expectations were brought up
  to the rail's eight entries and the radio's log's new kind.
- 2026-11-12: on unit A (binary only, in RAM over the SD image `2026-11-12-decode1`: a reboot
  of A returns to that image, which has neither module). A was restarted for it at 16:10 UTC
  and, with the two fixes below, at 16:24 (the commit's build).
  - **`tools/analog_listen.py listen --mhz 162.55 --demod nfm --squelch 8 --seconds 12
    --take-radio`** exits 0: 12.08 s of audio in 12 s, no gap, no block dropped, none lagged;
    the meter 20.2 to 22.3 dB over the floor, the squelch open throughout; the radio given
    back. Whisper reads the recording as the marine forecast ("northwest winds 15 to 20 knots,
    becoming north 10 to 15 knots in the afternoon. Seas 3 to 5 feet").
  - **Wide FM, 102.9 MHz:** 10.0 s of audio in 10 s, rms 0.22, no gap; the meter −13 dBFS.
    Whisper reads it as an advertisement, word for word.
  - **An empty channel, 162.400 MHz with the squelch at 8 dB** (`--silent`): no audio, the
    meter −0.8 to 1.0 dB over the floor.
  - **AM:** no voice heard (above). The carrier at 125.000 MHz opens the squelch at 6 dB and
    plays as tones and noise.
  - **The cost:** the `analog-listen` thread takes 75 to 76 % of one core in every kind (the
    channel's cut, as Data mode's 73 %), the workers 11 %, the process 87 %; no gap and none
    dropped in the stays.
  - **What it is tuned to is kept:** tuned to 162.550 MHz, restarted, it came back there.
  - With A serving the page: `analog_listen.mjs --fm 102.9` 12 steps (the bands, the steps, a
    typed frequency, the kinds, the squelch, the broadcast station heard, the radio given
    back), `data_mode.mjs` 18, `data_listen.mjs` 15; reads only, over HTTP:
    `scanner_pages.mjs` 40, `tool_pages.mjs` 27, `now_rail.mjs` 15, `files_page.mjs` 9,
    `aliases_page.mjs` 9, `playlist_page.mjs --preview-only` 13; over HTTPS: `tool_pages.mjs`
    27, `now_rail.mjs` 15, `files_page.mjs` 9. `API_FIELDS.md` regenerated from A.
- 2026-11-12: **two things found on unit A and fixed before the commit.**
  - A tuning that changed the frequency and the kind at once played a third of a second of
    the frequency before as the new kind (17 frames counted going from the broadcast station
    to a weather channel): the thread took the new kind at once and the radio moved a moment
    later. Now nothing is played, and the meter has no reading, from a tuning to another
    frequency until the radio is there.
  - The radio's log had a line for every frequency tuned, which a pass over a band fills it
    with. It has one where the listening began and one when it stopped.
- **Seen and not changed:** the carrier at 125.000 MHz; a narrow FM voice's energy sits low
  (58 % of the weather channel's audio is between 200 and 300 Hz), which the de-emphasis and a
  first-order high-pass at 250 Hz leave in: how it sounds is Andy's to say.
- **Not checked:** AM on a voice; a two-way radio or a repeater in narrow FM (the weather
  channel is the one narrow FM signal heard); the marine and amateur bands; the sound in a
  browser by ear (the checks count the frames the page plays); hours of listening; a start of
  the unit in analog or ADS-B mode; a browser other than headless Chrome; an image with it.
