# 116 — Where the radio is: distances, bearings and range rings

**Date:** 2026-11-12. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-12-range1`) over the
SD image `2026-11-12-decode1`, which does not have it. Unit A has not been told where it is:
that is Andy's to enter, in Settings.

## Why

113 to 115 left the Aircraft module with no distances and no range rings, because the radio
did not know where it is. Asked "Add the unit's position as a setting, for range rings and
distances?", Andy answered "Yes, add it".

No drawing exists for any of it: where the card sits, the columns and the rings are this
change's own, for Andy to correct.

## Changes

### The setting

- **The radio's place is a setting of the radio's own,** not the Aircraft module's: a
  latitude and a longitude in degrees, north and east positive, or none. It is in
  `radio.json` (`position`) with the other settings, so it goes with the exported
  configuration and comes back with an imported one.
- **`PUT /api/v1/radio/settings`** takes `position`: `{ "lat": …, "lon": … }`, or `null` to
  forget it; left out, it stays. A latitude beyond 90 or a longitude beyond 180 is refused.
  `GET /api/v1/radio` has it in `config.position`.
- **Settings, Position** (the rail's Settings, the radio's own): the two numbers, Save and
  Forget. The card says what uses it, and that whoever may open the pages can read it.

### The aircraft's distance and bearing

- **`GET /api/v1/adsb`** has `position` (the radio's place, as its settings have it) and, for
  each aircraft that has said where it is, `distance_nm` and `bearing_deg`: how far its place
  is from the radio's over the ground, in nautical miles, and which way from the radio, in
  degrees from north, each to a tenth. Both are null until the radio has been told where it
  is. A change of the setting is in the next answer, and moves the view's `rev`.
- **The sum** (`services::adsb::range`): the great circle on a sphere of the Earth's mean
  radius (6,371.0088 km). That is within half a percent of the ellipsoid's answer, which is
  less than an aircraft moves between two of its messages. It is the distance over the
  ground: an aircraft overhead at 40,000 ft is 0 NM away.
- **The Aircraft page** has two more columns, Distance and Bearing, after Place; Distance
  sorts the farthest first. Until the radio has a place, the cells are empty and a line under
  the table says where it is set, with a link.

### The map

- **The radio is marked** at its place, and **range rings** are drawn round it: at whichever
  of 1, 2, 5, 10, 25, 50, 100, 250 or 500 nautical miles apart first puts them 70 pixels
  apart or more, as many as cross the view (24 at most), each named. A ring is drawn through
  the places that far from the radio, so it is the true shape on this projection, a little
  taller than wide.
- **Fit** keeps the radio in view with the aircraft, and with no aircraft shows the radio
  with the country round it.
- **A picked aircraft's line** says its distance and bearing from the radio.
- Until the radio has a place, the bar over the map says so, with a link to Settings.

### With it

- **The header's line** says "1 message a second" when one a second comes (it said "1
  messages"; seen in 115).
- **`tools/adsb_listen.py`:** `listen` and `status` say how far the farthest aircraft is
  (`farthest_nm`), once the radio has a place.

## Not in this change

- **The radio finding its own place.** It has no GPS; the place is typed.
- **A height for the radio,** and with it an aircraft's distance through the air and its
  angle above the horizon.
- **The place set from the map** (a click where the radio is).
- **Kilometres or statute miles:** distances are in nautical miles, as the map's scale is.
- **The farthest aircraft heard kept** (by bearing, over a day: what an antenna reaches).
- **Anything of the radio's place outside the Aircraft module:** the TV channels' towers,
  the sites of a trunked system.

## Checks

See the status log for the counts.

- **Host tests** (4 new): the sum against a degree of the sphere each way, a place due west,
  two long great circles and half the world; the table's distances with and without a place
  for the radio; the service's view following the setting at once; the setting given,
  forgotten, left and refused.
- **`runs/ui_check/aircraft_page.mjs`:** with no place, no distances and the line that says
  where it is set; with a made place (the middle of the aircraft heard, put through the API),
  each aircraft's distance and bearing against the check's own sum, the table's cells, and
  the Distance head's order; what the radio had is put back.
- **`runs/ui_check/aircraft_map.mjs`:** with no place, no rings and the bar's line; with a
  made place, the mark drawn there, a ring's top drawn where the check's own sum puts it, the
  radio in view, the rings fewer miles apart two steps closer; forgotten, gone; what the
  radio had is put back.
- **`runs/ui_check/tool_pages.mjs`:** the Position card among the radio's settings; on a
  development host, a latitude off the globe refused, a place typed and saved, Forget.

## Status log

- 2026-11-12: built. BUILD_TAG `2026-11-12-range1`. Host tests 673 (4 new). The DMR
  reference's tests pass (102); the data captures' 30, the analog captures' 12 and the two
  links' captures as in 114 (3,325 and 2,618 Mode S messages, 11 and 11 UAT). ARM check and
  release build clean.
- 2026-11-12: on the PC daemon, a capture of each link replayed as its radio:
  `aircraft_page.mjs` 20 steps at two sizes: with a made place, the 84 aircraft that said
  where they are each have a distance within 0.05 NM and a bearing within 0.05 degrees of the
  check's own sum (the tenth they are rounded to), the 45 others none, and the Distance head
  puts the farthest first. `aircraft_map.mjs` 16 steps at two sizes: the radio's mark drawn
  at its place, 11 rings 25 NM apart in view and a ring's top drawn where the check's sum
  puts it; two steps closer, 5 NM apart. `tool_pages.mjs` 34 steps at two sizes (the Position
  card: a latitude of 95 refused, a place typed and saved and the aircraft module told,
  Forget). `data_mode.mjs` 18 at two sizes, `data_listen.mjs` 16 at two sizes,
  `analog_listen.mjs` 11 at two sizes, `files_page.mjs` 15 and over HTTPS, `now_rail.mjs` 29
  and over HTTPS, `playlist_page.mjs` 16, `aliases_page.mjs` 34, `access_page.mjs` 20 over
  HTTPS, `access_api.py` 34, `modules_check.mjs` 55 modules; `tv_watch.mjs` 10 of 12, as
  since 110. `tools/analog_listen.py` there exits 1 with 7.7 s of audio in 10 s, the PC
  daemon being a debug build (112).
- 2026-11-12: on unit A (binary only, in RAM over the SD image `2026-11-12-decode1`: a reboot
  of A returns to that image, which has no Aircraft module). A was restarted for it at
  21:47 UTC and was live again within a minute; the build there is the commit's. Two other
  sessions had begun on this PC: both were told before the restart, and the one that
  answered did not use the unit.
  - **`aircraft_page.mjs --uat 60`** 20 steps: with a made place put through the API, 105
    aircraft each with a distance and bearing as the check's own sum has them, the farthest
    123 NM from that place; an aircraft heard on 978 MHz; both links in turn.
    **`aircraft_map.mjs`** 16 steps: the mark and 11 rings 25 NM apart, and 10 NM apart two
    steps closer. **`data_mode.mjs`** 18. The radio was off the scanner's site for these
    three from 21:48:16 to 21:54:13 UTC. The unit had no place before them and has none
    after: each check put back what it found.
  - Reads only, over HTTP: `scanner_pages.mjs` 40, `tool_pages.mjs` 27 (the Position card
    among the radio's settings, empty), `now_rail.mjs` 15, `files_page.mjs` 9,
    `aliases_page.mjs` 9, `playlist_page.mjs --preview-only` 13; over HTTPS: `tool_pages.mjs`
    27, `now_rail.mjs` 15, `files_page.mjs` 9.
  - `scanner_pages.mjs` first failed two steps there, and passed when run again with its
    wait mended: the newest alert's recording was 0.9 s long and had ended before the check
    made it go round. It now makes it go round as soon as it is in the panel.
  - **`tools/adsb_listen.py listen --seconds 25 --take-radio`** with a made place set for it
    (a round one, off the coast): exits 0, 115 aircraft listed, 82 with a place, and
    `farthest_nm` given. `API_FIELDS.md` was regenerated from A within the minute, so its
    example of a place is that made one; the place was taken away again. The radio was off
    the scanner's site for it from 22:01:38 to 22:02:04 UTC.
- **Seen and not changed:** a ring's name is drawn before the aircraft, so an aircraft or its
  name may lie over it. At a zoom that shows 250 NM the rings, the grid and the aircraft
  make a busy picture.
- **Not checked:** unit A's own place (it has none: Andy's to enter), and so no distance
  that means anything yet; a place near a pole or across the date line on the map; the
  setting through an exported and imported configuration; a browser other than headless
  Chrome; an image with it.
