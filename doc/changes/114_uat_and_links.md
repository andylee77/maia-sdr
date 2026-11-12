# 114 — UAT at 978 MHz on the unit, and the Aircraft module's two links: one, the other, or both in turn

**Date:** 2026-11-12. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. In unit A's SD image, `2026-11-12-range1` (111 to 116; its record
in 116's).

## Why

113 looked at 978 MHz and found that unit A's antenna hears UAT's aircraft; the radio hears
1090 or 978 MHz, not both. Asked "Build UAT on the unit next, as a switch between 1090 and
978 MHz?", Andy answered "Yes, a switch", and then: "I mean depending on broadcast period in
the captured signals we could auto switch and maintain both right? every 5 or 10 seconds?
since the tuner can switch fast."

He is right, and it is built so: the link is a setting of three, **1090 MHz, 978 MHz, or both
in turn.** An aircraft on 1090 MHz sends its place and its velocity twice a second and its
callsign every five; one on 978 MHz sends once a second; the list keeps an aircraft a minute.
So with 10 s on 1090 MHz and 5 s on 978 MHz every aircraft of either link is heard again in
each round of 15 s, and both lists stay whole. What it costs: each link misses what is sent
while the other has its turn, and the radio is deaf for a fifth of a second at each move.

## The method, in whole numbers

As for Mode S in 113: a method the unit and the PC tool both run, the same whatever the blocks,
so that the two can be held against each other. Tried on 113's two captures (12 s each, the LO
at 979 MHz), counting the aircraft's messages taken:

| | 73 dB | 55 dB |
|---|---|---|
| 113's look: floating point, a 25-tap filter, the phase's angle, no correction | 7 | 10 |
| Four samples summed, the turn's sign at one sample | 7 | 11 |
| **Four summed, the sign over three samples together** | **11** | **11** |
| Three summed, the same | 10 | 11 |
| Five summed, the same | 10 | 11 |
| Two summed, or none | 6 | 10, 6 |

(The rows in whole numbers are with the code's correction. The third chooses the sample as
below; the others chose it by a soft correlation, with which the third took 10 and 11.)

- Each sample is turned a quarter of a turn more than the one before. The LO is a megahertz
  above the link, a quarter of the rate, so this moves the link to the middle and costs
  nothing.
- Four samples are summed, which is nothing at a megahertz and at two: the radio's own DC,
  now a megahertz off, is gone, and so is the far edge of the window.
- The way the phase turns from one sum to the next is the frequency (a cross product: no
  angle is taken). Its sign, over three samples together, is all that is kept: a bit a sample.
- A message begins where the 36 bits of an aircraft's synchronisation, read at the middle of
  each bit (3.84 samples a bit), fit with at most four wrong. The unit counts the wrong bits
  at 32 places at once, in three bits a place.
- Of the four samples from the first that fits, the message is read at the middle one of
  those that fit best.
- Its 384 bits are read; the first five say whether it is short (30 bytes with their parity)
  or long (48); the Reed-Solomon code puts right up to 6 wrong bytes of a short one and 7 of a
  long one. A wrong bit among the first five may say the other length, so that is tried too.

Chance is no matter here: about four places a second fit the synchronisation by chance, and
one such in a thousand million is within reach of a word of the code.

## Changes

### `protocol::adsb::uat`

New. Written from the link's facts, not from another decoder's code.

- **`mod`:** `Demod`, the reader above fed the window as it comes; `Frame`, a message put
  right, with the sample its synchronisation begins at, how many bytes were wrong and its
  level.
- **`code`:** the two Reed-Solomon codes (GF(256) by x^8+x^7+x^2+x+1, the generator's roots
  from alpha^120): the syndromes, Berlekamp and Massey's locator, the search for its roots,
  Forney's values.
- **`message`:** what an aircraft's message says: its address (and whether it made it up), its
  place whole, its altitude, its velocity over the ground and its rate of climb in the air,
  and in a long message of type 1 or 3 its category and its callsign (when the flag says the
  eight characters are a callsign and not a squawk).

What says the fields are read right is the air itself: one aircraft's eleven messages give a
place that moves 18 knots south and 50 west, which is the velocity it says; its altitude
climbs at the rate it says; its callsign is a registration; and another's squawk is 1200.

### `services::adsb`

- **`Listen`:** `1090`, `978` or `both` (the default), kept in the radio's state.
- **`listen`:** the radio moves to the link whose turn it is (the LO at 1090 MHz, or at
  979 MHz for UAT), the capture begins afresh there, and the thread is told, so that what
  follows is read as that link's. Of both: `ES_FOR` 10 s and `UAT_FOR` 5 s. On a development
  host `--uat-replay` gives the second link's capture.
- **`table`:** an aircraft has the link it was last heard on; a UAT message gives it its place
  whole. One heard on both links is one aircraft.
- The counts gain `uat` (messages taken), `mended` (of them, those with bytes put right) and
  `fitted` (places the synchronisation fitted).

### The API

- `GET /api/v1/adsb` gains `link` (what it is asked to listen to), `on` (the link it is on
  now) and each aircraft's `link`.
- `PUT /api/v1/adsb/link`: `{"link": "1090" | "978" | "both"}`. An administrator's.
- `status.adsb.link`.

### The pages

- **Aircraft:** a switch over the table, 1090 MHz, 978 MHz or Both, and a Link column. The
  page says which link the radio is on; the header says what it is asked for.

### The tools

- **`tools/uat_look.py`** now reads a capture by the method above, the same as the scanner
  sample for sample, with the correction, and `--messages` writes each message beside the
  capture: the list the scanner's test holds itself against (`UAT_CAPTURE_DIR`). It still
  looks for a ground station's uplinks, which the scanner does not.
- **`tools/adsb_listen.py`:** `link 1090|978|both`, and `listen --link`, which listens to a
  link for the stay and puts it back.

## Not in this change

- **What a ground station sends** on 978 MHz: its weather and traffic uplinks, and the other
  traffic it relays in an aircraft's kind of message (counted, and not listed). None was on
  the air here to hold a reader against.
- **The squawk** (read by the look tool, not kept by the unit: the table has no place for one
  on either link), what an aircraft on the ground says of its speed, the rest of a long
  message.
- **The turn's lengths as a setting.** They are constants.
- **Both links at once:** the radio has one receiver in use and a 4 MHz window.
- The plot and distances (as in 113).

## Checks

See the status log for the counts.

- **Host tests:** the code (a block as sent, every count of wrong bytes it mends at both ends
  and in the middle, one more than that refused or another word); the bits' places; messages
  found in a made window whatever the blocks, with a stretch silenced (put right) and too long
  a stretch (nothing), and nothing in noise; what a short and a long message heard on unit A
  say; the table and the service taking both links' messages; the thread told of the radio's
  moves; whose turn it is.
- **The captures** (`UAT_CAPTURE_DIR=<repo>/runs/adsb/look_20261112 cargo test --release
  adsb::`): each capture gives the messages the look tool lists, at the same samples, with as
  many bytes put right.
- **`runs/ui_check/aircraft_page.mjs`:** the switch to 978 MHz, an aircraft of that link
  listed, both in turn, and back.
- **`tools/adsb_listen.py listen --link 978 --take-radio`** and `--link both` on unit A.

## Status log

- 2026-11-12: built. Host tests 669 pass (13 new: the code 2, the reader 5, a message's
  fields 3, the table 1, the service 1, the turn 1); the DMR reference's tests pass (102);
  the data captures' 30 and the analog captures' 12 in release as before; the ARM check and
  the ARM release build are clean. BUILD_TAG `2026-11-12-uat1`.
- 2026-11-12: **the captures through the readers** (release, `adsb::`): the two UAT captures
  give 11 messages each (5 and 4 of them long), of 117 and 31 places fitted: each message the
  look tool lists, at the same sample, with as many bytes put right; the two Mode S captures
  as in 113 (3,325 and 2,618).
- 2026-11-12: on the PC daemon, a capture of each link replayed as its radio:
  `aircraft_page.mjs` 16 steps at two sizes (the switch to 978 MHz with the capture's
  aircraft listed among those of 1090 MHz, both in turn, and back); `data_mode.mjs` 18 at two
  sizes; `data_listen.mjs` 16 at two sizes; `analog_listen.mjs` 11 at two sizes;
  `tool_pages.mjs` 31 at two sizes; `files_page.mjs` 15 and over HTTPS; `now_rail.mjs` 29 and
  over HTTPS; `playlist_page.mjs` 16; `aliases_page.mjs` 34; `access_page.mjs` 20 over HTTPS;
  `access_api.py` 34, a listener's change of link refused among them; `modules_check.mjs`.
  `tv_watch.mjs` 10 of 12, as since 110. `tools/adsb_listen.py listen --link 978
  --take-radio` there: the replayed capture's 11 messages of 117 places fitted, as the look
  tool has them, through the whole service. The Aircraft check first failed two steps at each
  size by its own haste (the header's line comes with the status, once a second; and aircraft
  left listed from the run before stood in for new ones): its waits were made to wait.
- 2026-11-12: on unit A (binary only, in RAM over the SD image `2026-11-12-decode1`: a reboot
  of A returns to that image, which has no Aircraft module). A was restarted for it at
  19:25 UTC; the build there is the commit's (the one change after it, a method nothing
  called taken out, left the binary the same to the byte).
  - **`tools/adsb_listen.py listen --link 978 --seconds 45 --take-radio`** exits 0: 89 UAT
    messages in the 45 s, 22 of them with bytes put right, of 470 places fitted; three light
    aircraft listed, each with its callsign, altitude, speed and place (4,175 to 4,600 ft,
    82 to 165 knots); no gap, no block dropped; the link put back, the radio given back.
  - **`--link both --seconds 62`** exits 0: 9,540 extended squitters, 3,005 all-call replies
    and 35 UAT messages (14 with bytes put right); 151 aircraft listed at its end, 113 with a
    place; no gap, no block dropped across the radio's moves.
  - **The cost:** the `adsb-listen` thread takes 73 % of one core on 978 MHz alone (Mode S
    alone: 55 to 60 %, 113) and 61 % with both in turn; the workers 14 %.
  - With A serving the page: `aircraft_page.mjs --uat 60` 16 steps (an aircraft heard on
    978 MHz within the wait and listed with what it said; both links in turn; back to
    1090 MHz), `data_mode.mjs` 18; reads only, over HTTP: `scanner_pages.mjs` 40,
    `tool_pages.mjs` 27, `now_rail.mjs` 15, `files_page.mjs` 9, `aliases_page.mjs` 9,
    `playlist_page.mjs --preview-only` 13; over HTTPS: `tool_pages.mjs` 27, `now_rail.mjs` 15,
    `files_page.mjs` 9. `API_FIELDS.md` regenerated from A within the minute after a stay.
- **Seen and not changed:** UAT's reader costs more than Mode S's for a link with a hundredth
  of the messages: its loop over the samples is written plainly and not tuned (the capture
  test would hold a tuned one to the same answer). On the PC daemon a replayed capture begins
  again every 12 s, and each beginning is counted as a gap.
- **Not checked:** a ground station on the air; an aircraft's place against another
  receiver's; hours of both links in turn; the turn's lengths against others; a start of the
  unit in ADS-B mode; a browser other than headless Chrome; an image with it.
