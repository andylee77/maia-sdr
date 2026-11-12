# 115 — The Aircraft module's Map

**Date:** 2026-11-12. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. In unit A's SD image, `2026-11-12-range1` (111 to 116; its record
in 116's).

## Why

Andy, 2026-11-12, as 114 was built: "can we add a map view". The Aircraft module's second tab
was a card saying a plot was not built; it is now the map.

No drawing exists for it, so the layout is this change's own, for Andy to correct. Two things
it settles by itself, and says here:

- **The radio holds no map,** and nothing was fetched to give it one. What the page draws of
  its own is a grid of latitude and longitude and a scale. A real map under the aircraft is
  **OpenStreetMap's tiles, off until switched on:** with them on, the browser that shows the
  page fetches the tiles of what it looks at from `tile.openstreetmap.org`, so that server
  learns what part of the map is looked at, and the page needs the Internet. The choice is
  kept by the browser, not by the radio. The roadmap's Q11 had recommended "no map tiles from
  the Internet"; this keeps to it by default and leaves the other a click away. Andy, shown
  the page and asked whether the background should be on from the start: "we can leave off by
  default."
- **There are no range rings and no distances:** the radio does not know where it is.

## Changes

### The page (`js/views/aircraft_map.js`, new)

- **What is drawn:** every aircraft that has said where it is, at its place, as an arrowhead
  along its track (a dot when it has said none), in the accent colour on 1090 MHz and the
  warning colour on 978 MHz; its callsign (or address) and its altitude in hundreds of feet
  beside it where there is room, a name being left out where another aircraft or another name
  is in its way (a closer zoom gives them room); the way it has come since the page was
  opened; the grid, with its lines named when no tiles are under it; a scale in nautical
  miles.
- **Moving it:** a drag, the wheel (about the pointer), Zoom in and Zoom out, the arrow keys
  and + and −. **Fit** brings every aircraft into view and keeps them there as they move and
  come and go, until the map is moved by hand. Where the map was left, and whether it was
  following them, is kept by the browser for the next visit.
- **Picking:** a click on an aircraft picks it, and the line under the map says what it last
  said (callsign, address, link, altitude, speed and track, rate of climb, place, when).
- **Map tiles:** the switch above. The tiles are laid under the drawing, turned dark on the
  dark page, with OpenStreetMap's name on the map as its licence asks.
- It is a projection of the globe (Web Mercator, as the tiles are), in whole zooms 3 to 13.
- The page is the module's second tab, **Map**, in the place of Plot, and fills the screen.

### What went

- The page that only said what a plot would hold (`js/views/aircraft_shell.js`) and its
  styles.

Nothing changes in the daemon or its API: the map is drawn from `GET /api/v1/adsb` as the
table is.

## Not in this change

- **A way kept by the radio.** An aircraft's way begins when the page is opened, and is lost
  when it is left: the radio keeps each aircraft's last place only.
- **Where the radio is** (a setting), and with it the range rings, each aircraft's distance
  and bearing, and the map opening on the radio's own place.
- **A map of the radio's own** (coastlines, airports) for a unit with no Internet: it would
  need map data on the unit.
- Zooms between the whole ones; a touch screen's pinch; aircraft coloured by altitude.

## Checks

See the status log for the counts.

- **`runs/ui_check/aircraft_map.mjs`** (new): the page while the scanner has the radio; the
  radio given to Aircraft and the aircraft drawn, all in view, the map's middle among them;
  the zoom; a drag (the map moves with the pointer, to the pixel); Fit; an aircraft picked by
  a click where the API says it is; the tiles on (those in view asked for, by their right
  names, each laid where the map's middle says, and OpenStreetMap named) and off; the radio
  given back. The check answers the tiles' requests itself, with a plain tile that has an
  edge: none goes to the Internet, and none is made before the switch is pressed.
- **`runs/ui_check/aircraft_page.mjs`, `data_mode.mjs`:** the module's second tab is Map, with
  the aircraft on it.

## Status log

- 2026-11-12: built. BUILD_TAG `2026-11-12-map1`. Host tests 669, none new: the change is the
  page alone, and the page checks are its tests. The DMR reference's tests pass (102); the
  data captures' 30, the analog captures' 12 and the two links' captures as in 114 (3,325 and
  2,618 Mode S messages, 11 and 11 UAT). ARM check and release build clean.
- 2026-11-12: on the PC daemon, a capture of each link replayed as its radio:
  `aircraft_map.mjs` 12 steps at two sizes (1920 by 1080, 1280 by 800): 84 of the capture's
  129 aircraft on the map, all in view at zoom 8; a drag of 200 by 80 pixels moves the map
  200 by 80; opened again, the map is where it was left; 40 tiles asked for at the larger
  size, each by its right name and laid within a pixel of where the map's middle says.
  `aircraft_page.mjs` 16 steps at two sizes, `data_mode.mjs` 18 at two sizes,
  `data_listen.mjs` 16 at two sizes, `analog_listen.mjs` 11 at two sizes, `tool_pages.mjs` 31
  at two sizes, `files_page.mjs` 15 and over HTTPS, `now_rail.mjs` 29 and over HTTPS,
  `playlist_page.mjs` 16, `aliases_page.mjs` 34, `access_page.mjs` 20 over HTTPS,
  `access_api.py` 34, `modules_check.mjs` 55 modules; `tv_watch.mjs` 10 of 12, as since 110.
  That set ran on the build before the map's last edit (whether it follows the aircraft kept
  with its place); after it the map's check at both sizes, `aircraft_page.mjs`,
  `data_mode.mjs` and `modules_check.mjs` were run again on the commit's build.
- 2026-11-12: looked at, on the PC daemon:
  - **OpenStreetMap's own tiles, once,** at a zoom that shows a continent (zoom 5, some
    forty tiles, so that the tiles asked for say nothing closer than North America): they
    come, they turn dark as meant, and the capture's aircraft lie over north-east Florida,
    where unit A heard them. No closer zoom was tried with the real tiles.
  - **The 978 MHz link's aircraft:** the capture's three, in the warning colour, each with
    its name and altitude.
- 2026-11-12: on unit A (binary only, in RAM over the SD image `2026-11-12-decode1`: a reboot
  of A returns to that image, which has no Aircraft module). A was restarted for it at
  21:01 UTC and was live again within a minute; the build there is the commit's.
  - **`aircraft_map.mjs`** 12 steps: 98 of 125 aircraft heard on the map at zoom 8, all in
    view; zoom, drag, the map kept across a reload, Fit; an aircraft picked by a click where
    the API has it; the tiles (40, answered by the check) on and off; the radio given back,
    the scanner live on the site it was on.
  - **`aircraft_page.mjs --uat 60`** 16 steps (an aircraft heard on 978 MHz and listed; both
    links in turn; the Map tab with 114 aircraft on it), **`data_mode.mjs`** 18.
  - The radio was off the scanner's site for these three from 21:02:42 to 21:08:31 UTC.
  - Reads only, over HTTP: `scanner_pages.mjs` 40, `tool_pages.mjs` 27, `now_rail.mjs` 15,
    `files_page.mjs` 9, `aliases_page.mjs` 9, `playlist_page.mjs --preview-only` 13; over
    HTTPS: `tool_pages.mjs` 27, `now_rail.mjs` 15, `files_page.mjs` 9.
  - `files_page.mjs` over HTTPS first failed one step, and passed when run again: the
    history's database had grown by a tenth of a megabyte between the listing the check read
    and the page's own. The check now reads the two together, again when they differ; run so
    on A (both ways in) and on the PC daemon, it passes.
- **Seen and not changed:** the header says "1 messages a second" when one a second comes
  (114's line; seen on the 978 MHz link). The names are all to the right of their aircraft,
  so in a crowd the ones that show are those on its right edge.
- **Not checked:** OpenStreetMap's tiles at a closer zoom, or from a page served by a unit;
  the light theme's map; a touch screen; a browser other than headless Chrome; hours with the
  page open (an aircraft's way is held to 240 places); what the drawing costs a slow
  browser; a place across the date line; an image with it.
