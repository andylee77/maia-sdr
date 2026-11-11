# 094 — The last three pages in the rail's layout: Channels, Diagnostics, Settings

**Date:** 2026-11-10. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** unit A runs the test build `2026-11-10-pages2` in RAM (it holds 093 and 094). A
reboot returns it to the SD image `2026-11-10-tv2`, which has the old layouts of the five pages
of 093 and 094.

## Why

After the Scanner's pages (093) Andy asked for the last three still in their old layouts. Each
was one column of 1,280 px:

- **Channels** (TV) put the picked channel's spectrum and its station under the scan's table: a
  click on a row showed nothing until the page was scrolled past 33 rows.
- **Diagnostics** was 7,400 px long: the event log, the spectrum, the receive window with every
  carrier heard (89 on Clay County), three cards of values and the API's 87 routes, each under
  the last. Its cards of values were tables styled as grids, which set two pairs side by
  side.
- **Settings** was six cards of sentences with inputs in them, the last ones below the screen.

## Changes

### Channels (`views/channels.js`)

The page fills the screen.

- **The scan on the left:** its counts in a row over the table (channels read, with a signal,
  8-VSB, without the 8-VSB pilot, vacant, stations named, their channels, the gain) with New
  scan; while it runs, where it is, its progress and Cancel. The table is as before and scrolls
  under its heading. A new scan's bands and settings take the table's place.
- **The picked channel on the right:** what was read of it (what it holds, its carrier to noise,
  its pilot and the pilot's offset, its power, the gain and the clipping), its spectrum (or that
  a kept scan has none), its station (transport stream, MER, packets, its clock) and the
  station's virtual channels, each with Watch.
- From 1,000 px the two are side by side; narrower, the picked channel comes first.

### Diagnostics (`views/diagnostics.js`, `window.js`, `spectrum.js`, `board.js`)

The page fills the screen.

- **The spectrum** across the top, its LO, rate and control channel in its heading.
- **The main pane** under it: Events (the log, with Housekeeping), Carriers (what was heard in
  the receive window, which used to be a table inside the Receive window card) or API routes.
  The log fills the pane and keeps to its end as lines come.
- **The cards down the right,** scrolling on their own: Control channel, Receive window (the
  plan and Move to the planner's window), Tuning and Board. Their values are definition lists,
  one pair a line. The two drawn from the status are drawn again only when a value changed.

### Settings (`views/settings.js`)

- **Two columns of cards** from 1,100 px: Receiver, Radio, Clock and Configuration on the left;
  Recording, Crystal and Packet data on the right. The page keeps a readable width (1,280 px).
- **A setting is a row:** its name, and a note under it where one helps, on the left; its
  control on the right. A card's Save sits on its last line with the card's note.
- The clock has a card of its own (it was two controls in Radio). Configuration's three actions
  each say what they do beside their button.
- What each control does, and what is sent, is unchanged.
- Fixed in passing: leaving Settings before the radio had answered left the crystal's feed
  subscribed for good.

### Shared

- `.metrics` is the row of totals for Activity and Channels. The unused `.spectrum` style is
  gone.
- `js/modules.js` marks Channels and Diagnostics `fill`.

## Not in this change

- No setting, route or message changed: the three pages show and send what they did.
- **Settings:** a change made in another browser still shows only after a reload (the radio
  pushes no notice for a setting).
- **Diagnostics:** no filter on the event log; the eye and IQ plots the brief names.
- **Channels:** nothing filters the table (vacant channels are listed, greyed).

## Checks

- **`runs/ui_check/modules_check.mjs`:** 36 modules, syntax and imports.
- **Headless Chrome** (`runs/ui_check/tool_pages.mjs`), the pages from the PC daemon
  (`2026-11-10-pages2`) and every read from unit A: 15 steps at 1920×1080, 1440×900 and 1280×800,
  no exceptions, nothing written:
  - Channels opens on the kept scan (33 rows, eight counts), fills the screen with the table
    beside the picked channel, and a named station's row shows what was read of it, why it has no
    spectrum, its station and its Watch buttons; the heading stays as the table scrolls; Watch
    opens Watch on that channel; New scan shows the bands and settings;
  - Diagnostics opens with the spectrum drawn, lines in the log and the four cards filled, and
    fills the screen; Carriers lists what was heard; API routes lists the routes; Housekeeping
    starts the log again;
  - Settings opens with its seven cards in two columns, every control showing what
    `GET /api/v1/radio` holds, and the rows as named settings.
  No button on Settings is pressed by the check.
- **A scan followed, without the radio** (`tool_pages.mjs --scan`, a saved scan's stages put in
  the page's store as the radio pushes them; 18 steps with the 15 above): the fourth window being
  read shows "Window 4 of 18 · around 198.0 MHz", its progress, Cancel, the six channels read and
  "Reading…" on RF 10 and 11; the naming shows "Naming the stations: RF 19 · 3 of 13" with the
  picked channel kept; when the scan is over the counts and New scan are back, and the picked
  RF 19's panel has taken its station's names and its Watch buttons.
- **The other pages' checks on the same build:** `tv_watch.mjs --kept` on the PC daemon 13 steps
  (Channels' rows, the picked channel and Watch as the TV check uses them), `scanner_pages.mjs`
  40, `now_rail.mjs` 10.
- **Host tests:** 519 pass.
- **Not checked:** no setting was saved and no button on Settings pressed, on the PC or a unit
  (each control sends what it sent before); a real TV scan was not run through the new Channels
  page, since a finished one replaces the kept channel list; Move to the planner's window was
  not pressed. The phone width and the light theme were looked at in a picture each, not
  stepped through: at phone width Channels' table is cramped (its cells wrap and it scrolls
  sideways; the brief leaves the phone layout for later).

## Status log

- 2026-11-10: committed. Host tests 519 pass; the DMR reference keeps 24,984 of 24,996 lines;
  the ARM release build has its standing warnings only.
- 2026-11-10: `2026-11-10-pages2` went onto unit A (binary only), on the radio antenna: live on
  Clay within 30 s. With A serving the pages, no exceptions:
  - `tool_pages.mjs`: 15 steps, nothing written;
  - `scanner_pages.mjs`: 40 steps; `now_rail.mjs`, read only: 10;
  - `tv_watch.mjs --unit --kept`: 17 steps. From Channels, RF 19's row opened in the picked
    channel's panel without its spectrum, with four Watch buttons; Watch on 47.1 WJAXHD played
    live (MER 23.9 dB), then the large view, 47.2, full screen, 30.1 on RF 14 and Stop; Return
    to Scanner brought Clay back, control decode 100 %.
