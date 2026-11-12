# 113 — The ADS-B decoder on the unit and the Aircraft page; a look at 978 MHz (UAT)

**Date:** 2026-11-12. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. In unit A's SD image, `2026-11-12-range1` (111 to 116; its record
in 116's).

## Why

112 answered whether unit A's antenna hears ADS-B (it does) and left the Aircraft module a
shell. Asked "Build the ADS-B decoder on the unit next?", Andy answered "Yes, build it". It is
the roadmap's step 8 going on.

While it was being built he asked: "can we also add 978 MHz (Universal Access Transceiver /
UAT)". That link is looked at here, as 1090 MHz was in 112 before anything was built for it: a
capture from unit A read on the PC. Its decoder on the unit is not in this change.

## The method, in whole numbers

112's look tool worked in floating point, and took the DC off each second of the capture,
which a unit that reads the window as it comes cannot do the same way. So the method was first
made one that both can run and that gives the same answer whatever the blocks: each sample as a
whole number, no DC taken off (it changed no message in either capture), every sample tried
once. What the sample is taken as was measured on 112's two captures (12 s each):

| A sample taken as | Squitters, 73 dB | All-call, 73 dB | Squitters, 55 dB | All-call, 55 dB |
|---|---|---|---|---|
| 112's tool: amplitude in floating point | 2,558 | 764 | 2,031 | 585 |
| Its power (I² + Q²) | 2,533 | 755 | 2,018 | 581 |
| The larger of \|I\| and \|Q\| and 3/8 of the smaller | 2,562 | 768 | 2,023 | 583 |
| **Its amplitude in sixteenths of the ADC's step** | **2,559** | **766** | **2,032** | **586** |

The amplitude in sixteenths loses nothing to the floating point and is exact: a square root in
single precision, which the A9 has in hardware and which rounds the same on the PC. That is
what the unit and the tool both do:

- a sample is `floor(sqrt(I² + Q²) × 16 + 0.5)`;
- a preamble is where the eight samples of its four pulses (two each, starting 0, 4, 14 and 18
  samples in) hold more than a third of the sum of its 32 samples (their mean one and a half
  times the mean of the other 24), at the sample where they hold the most;
- a bit is a one when the first two of its four samples hold more than the last two;
- the first five bits are the format: 17 and 18 are read as 112 bits, 11 as 56; a message is
  taken when the parity over the whole of it leaves nothing;
- the same message found again within 200 samples is one;
- **no bit is corrected.** A message taken was sent, but for chance: one try in 16.7 million
  passes the parity, and on unit A some 17,000 tries a second get as far as the parity (of the
  177,000 preambles tried, those whose first five bits happen to be a format read here), so
  three or four times an hour a message is noise. An aircraft is therefore listed once two
  messages gave its address.

## Changes

### `protocol::adsb`

New. Written from the formats' facts, not from another decoder's code.

- **`mode_s`:** `Demod`, the search above fed the window as it comes (the capture ring's
  bytes), keeping what a message that straddles two blocks needs; `Frame`, a message with the
  sample its preamble begins at and its pulses' level against the ADC's full scale; `afresh`
  for lost samples.
- **`squitter`:** what a message says. The address; from an extended squitter the callsign and
  category (types 1 to 4), the altitude by pressure where it is sent in steps of 25 ft and
  half a position (types 9 to 18; half a position alone from 20 to 22), the velocity over the
  ground with the track and the rate of climb (type 19, its first two kinds). An all-call reply
  gives the address alone.
- **`cpr`:** a whole position from an even and an odd half, anywhere on the globe.

### `services::adsb`

The shell of 112 is now the module (`services/adsb/`):

- **`listen`:** the `4M` preset with its LO at 1090 MHz and **a fixed gain of 73 dB** (the
  pulses are too short for a gain control to follow; of the two gains 112 tried, the higher
  heard half as many aircraft again; the nearest aircraft clip at it, which the places of their
  pulses survive); the capture ring read as it fills, as Data mode and the analog receiver read
  it; a thread of its own (`adsb-listen`) that runs the search. On a development host
  `--adsb-replay <file>` plays a capture in the radio's place.
- **`table`:** the aircraft heard, each with what it last said of each thing, in memory. An
  aircraft is listed at its second message and goes a minute after its last. A place is worked
  out when an even and an odd half were heard within ten seconds of each other, and again with
  each half after.
- The view: the state, the window, the messages a second over the last ten, the counts
  (squitters, all-call replies, preambles tried, aircraft, gaps, blocks dropped) and the
  aircraft. Its revision moves once a second while it listens.

### The API

- `GET /api/v1/adsb`: the view. `adsb` on `/ws/live`, pushed when its `rev` moves.
  `status.adsb`: the state, how many aircraft are listed and the messages a second.
- The radio's log has a line when it begins to listen and one when it stops (`adsb`).

### The pages

- **Aircraft** (the module's first tab) is a table of the aircraft heard in the last minute:
  address, callsign, category, altitude, speed, track, rate of climb, place, level, messages
  and when last heard. A column's head sorts by it (numbers downwards first; what an aircraft
  has not said sorts last). The header's pill says how many aircraft and how many messages a
  second. There is no distance column: the radio does not know where it is.
- **Plot** still says it is not built, and why: a plot needs to know where the radio is.
- The layout is this change's own: no drawing exists for the module.

### The tools

- **`tools/adsb_look.py`** now finds the messages by the method above, the same as the
  scanner sample for sample, and `--messages` writes each message with its sample beside the
  capture (`FILE.messages`): the list the scanner's test holds itself against. Its numbers
  moved by a message from 112's (2,559 squitters for 2,558, and 2,032 for 2,031), and it now
  counts an aircraft heard by its all-call replies alone (129 aircraft for 112's 122, and 84
  for 79).
- **`tools/adsb_listen.py`** (new): `status`, and `listen --seconds N [--take-radio]
  [--aircraft]`, which watches a unit's module and says what came in that time, whether
  anything was lost, and what the scanner's threads cost. Exit 1 when no squitter came or a
  block was dropped.
- **`tools/uat_look.py`** (new): what a capture around 978 MHz holds (below).

### A raw capture, for the looks

On a unit, with the scanner stopped (its page is down meanwhile):

```sh
/etc/init.d/S60scanner stop
cd /sys/bus/iio/devices/iio:device0
echo 4000000 > in_voltage_sampling_frequency
echo 4000000 > in_voltage_rf_bandwidth
echo manual > in_voltage0_gain_control_mode
echo 73 > in_voltage0_hardwaregain
echo 1090000000 > out_altvoltage0_RX_LO_frequency      # 979000000 for the look at 978 MHz
iio_readdev -u local: -b 2097152 -s 48000000 cf-ad9361-lpc voltage0 voltage1 > /tmp/adsb_g73_1090M_4000k.cs16
/etc/init.d/S60scanner start
```

Twelve seconds are 192 MB in `/tmp`, which is RAM: copy the file off (`scp -O`) and remove it.

## 978 MHz (UAT): what unit A's antenna hears

UAT is the second ADS-B link in the United States: small aircraft below 18,000 ft send on it
once a second, and ground stations send weather and traffic up on it. Two captures of 12 s at
4 MSPS with the LO at 979 MHz (the signal a megahertz under it, clear of the radio's own DC),
at 73 and 55 dB of gain, read by `tools/uat_look.py`:

| | 73 dB | 55 dB |
|---|---|---|
| Messages from aircraft received whole | 7 (3 short, 4 long) | 10 (6 short, 4 long) |
| Aircraft | 2 | 1 |
| Ground stations' uplinks with a whole block | 0 | 0 |

One aircraft was in both: ten of its twelve messages whole at 55 dB, six at 73 dB, 10 to 14 dB
over the capture's median level; its registration as its callsign, 4,300 to 4,600 ft, a place
west of Jacksonville. The second was heard once, at 1,300 ft and 6 dB over the median. The
places and altitudes are sensible for the area, which is what says the fields are read right.

- **The tool's method:** the capture moved to 978 MHz and low-passed; each sample's turn of
  phase from the one before as its frequency; the 36 bits of synchronisation fitted (three may
  be wrong); the bits taken 3.84 samples apart; a message taken when its Reed-Solomon check
  leaves nothing. **No error is corrected,** which that link's code is there for (it mends 6
  wrong bytes of a short message's 30, 7 of a long one's 48), so a decoder with the correction
  would take weaker ones too. Its facts are in the tool's head; that the checks leave nothing
  on what was captured is what says the code's field and generator are the right ones.
- **So the antenna hears UAT aircraft.** It is a much thinner link here than 1090 MHz: two
  aircraft in 12 s against 129.
- **No ground station was heard** in either capture. Whether one is in range is not known: a
  station sends in its own slot once a second, and none of an uplink's six blocks came whole.
- **What it means for the module:** the radio hears 1090 or 978 MHz, not both: they are
  112 MHz apart and its window is 4 MHz. So UAT on the unit is a choice of link on the Aircraft
  page, or a turn about between the two that loses each a share of its messages.
- **What a decoder on the unit needs,** none of it built: the discriminator at 4 MSPS (a
  quarter of the rate is a free move of a megahertz, so the LO can stay off the signal); bits
  at 3.84 samples each; the Reed-Solomon codes with their correction; the message's fields
  (its place is sent whole, with no halves to pair); and the aircraft's table taking a second
  link.

## Not in this change

- **UAT on the unit** (above).
- **The plot, and distances:** they need to know where the radio is, which nothing says yet
  (a setting, the roadmap's Q11).
- **What is not read of a squitter:** positions on the ground (they need the receiver's
  place), the height by satellite (types 20 to 22), the 100 ft altitude code, airspeed and
  heading (type 19's other kinds), the squawk and emergency state (type 28), the status and
  intent types (29, 31).
- **The other Mode S replies** (formats 0, 4, 5, 16, 20, 21): their parity is mixed with the
  address, so they can only be checked against an aircraft already known.
- **The correction of a wrong bit.**
- **A setting for the gain;** the messages in the Beast format on a port (Q11).
- **The aircraft kept** across a restart, or logged.

## Checks

See the status log for the counts.

- **Host tests:** the parity on messages as sent and with a bit wrong; messages found where
  they are in a made window whatever the blocks it comes in, and nothing in noise or of a
  message with a wrong bit; lost samples; a callsign, an altitude with the two halves of a
  place, a velocity, an all-call reply (the worked examples of the format's literature, whose
  own parity holds them); the zones of longitude and a whole position; the table (listed at
  the second message, a place only from halves heard close together, gone after a minute);
  the rate; the service's counts and its view's revision; the thread fed a made window.
- **The captures** (`ADSB_CAPTURE_DIR=<repo>/runs/adsb/look_20261112 cargo test --release
  adsb::`): each capture that has the look tool's list beside it (`tools/adsb_look.py FILE
  --messages`) gives the same messages at the same samples, fed in blocks of a megabyte.
- **`runs/ui_check/aircraft_page.mjs`** (new): the page while the scanner has the radio, the
  radio given to Aircraft, the aircraft listed as the API gives them with what they said, the
  heads' sorting, the Plot tab, the radio given back, nothing dropped, the radio's log.
- **`runs/ui_check/data_mode.mjs`:** the five modes handed round, Aircraft now listening.
- **`tools/adsb_listen.py listen --take-radio`** on unit A.

## Status log

- 2026-11-12: built. Host tests 656 pass (18 new: the search and the parity 5, a squitter's
  fields 4, positions 3, the table 2, the service 2, the listening 2); the DMR reference's
  tests pass (102); the data captures' 30 and the analog captures' 12 in release as before;
  the ARM check and the ARM release build are clean. BUILD_TAG `2026-11-12-adsb1`.
- 2026-11-12: **the captures through the search** (release, `adsb::`, both in 0.45 s): the
  73 dB capture gives 3,325 messages (2,559 long) of 1,844,965 preambles tried, the 55 dB one
  2,618 (2,032 long) of 1,496,458: each message the look tool lists, at the same sample, and
  the same count of preambles.
- 2026-11-12: on the PC daemon, the 73 dB capture replayed as its radio (a debug build, which
  keeps up with this window and drops nothing): `aircraft_page.mjs` (new) 12 steps at two
  sizes; `data_mode.mjs` 18 at two sizes; `data_listen.mjs` 16 at two sizes;
  `analog_listen.mjs` 11 at two sizes; `tool_pages.mjs` 31 at two sizes; `files_page.mjs` 15
  and over HTTPS; `now_rail.mjs` 29 and over HTTPS; `playlist_page.mjs` 16; `aliases_page.mjs`
  34; `access_page.mjs` 20 over HTTPS; `access_api.py` 34, the aircraft's route among its doors
  (nobody reads it, a listener does); `modules_check.mjs` 55 modules. `tv_watch.mjs` 10 of 12,
  as since 110: this PC's headless Chrome refuses full screen. `tools/analog_listen.py` there
  still exits 1 for the reason 112's record gives (the debug build and the analog window).
- 2026-11-12: on unit A (binary only, in RAM over the SD image `2026-11-12-decode1`: a reboot
  of A returns to that image, which has no Aircraft module). A was restarted for it at
  18:02 UTC and, with the fix below, at 18:27 (the commit's build).
  - **`tools/adsb_listen.py --host 192.168.120.50 listen --seconds 40 --take-radio`** exits 0:
    5,973 extended squitters and 1,605 all-call replies in the 40 s (189 a second) of 124,000
    preambles tried a second; 107 aircraft listed, 95 with an altitude, 93 with a speed, 81
    with a place, 62 with a callsign; no gap, no block dropped; the radio given back. On the
    first build half an hour before: 203 a second, 126 aircraft listed, 93 with a place.
  - **The cost:** the `adsb-listen` thread takes 55 to 60 % of one core, the workers 14 to
    15 %, the process 70 to 74 %; the two cores at 42 and 36 %.
  - The view of 107 aircraft is 30 kB, sent once a second to a page that shows it.
  - With A serving the page, on the commit's build: `aircraft_page.mjs` 12 steps (131
    aircraft listed on the first run, with their callsigns, altitudes and places),
    `data_mode.mjs` 18, `data_listen.mjs` 15, `analog_listen.mjs --fm 102.9` 12; reads only,
    over HTTP: `scanner_pages.mjs` 40, `tool_pages.mjs` 27, `now_rail.mjs` 15, `files_page.mjs`
    9, `aliases_page.mjs` 9, `playlist_page.mjs --preview-only` 13; over HTTPS:
    `tool_pages.mjs` 27, `now_rail.mjs` 15, `files_page.mjs` 9. `API_FIELDS.md` regenerated
    from A within the minute after a stay, so the aircraft's fields have their examples.
- 2026-11-12: **found on unit A and fixed before the commit:** the aircraft were dropped a
  minute after their last message only while the module listened, so once it gave the radio
  back the last ones stayed listed for good (132 of them in `status`, minutes later). The list
  is now brought up to the clock whenever it is read.
- 2026-11-12: **978 MHz.** A's scanner was stopped for 32 s at 17:32 UTC for the two captures
  (`runs/adsb/look_20261112/uat_g73_979M_4000k.cs16`, `uat_g55_979M_4000k.cs16`). Read by
  `tools/uat_look.py`: at 73 dB, 73 fits of the aircraft's synchronisation and 2 of the ground
  stations' (about five of each are chance in 12 s), 3 short and 4 long messages whole, two
  aircraft; at 55 dB, 6 short and 4 long, one aircraft, the first capture's nearer one. That
  one says its registration in some messages and a squawk in others, as the link has it.
- **Seen and not changed:** the search's cost is mostly the square root of every sample; the
  cheaper stand-in the table above lists would about halve it for a few messages in a
  thousand at low levels, and nothing needs the core it would free.
- **Not checked:** an aircraft's place against another receiver's or a tracker's; hours of
  listening; the cost at a busier hour; a start of the unit in ADS-B mode; a browser other
  than headless Chrome; an image with it. For UAT: anything on the unit; a ground station.
