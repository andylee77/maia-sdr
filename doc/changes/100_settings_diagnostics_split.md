# 100 — Settings and Diagnostics, the radio's and each module's

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-11-parts1`) over the SD
image `2026-11-10-pages2`, which does not have it.

## Why

The roadmap's step 4. Andy asked for the settings and diagnostics "per mode" beside the whole
radio's, and the roadmap's Q5 asked where a module's own live. Asked "Should each module get its
own Settings and Diagnostics tabs?", he answered "Yes, module tabs" (2026-11-11).

What 095's review found of the two pages as 094 built them:

- **Settings** was one read and cards closed over what was loaded. A save sent whole objects, so
  a card wrote back the other cards' values as they stood when the page opened, and a change
  made in another browser showed only after a reload.
- **Diagnostics** was the scanner's: with TV on the radio its spectrum had no frames and three
  panes said "No site is live", while the 8-VSB receiver's state (`atsc_live`) was shown nowhere
  but as a few rows on Watch.

## Changes

### The pages

| Where | Page | What is on it |
|-------|------|---------------|
| Scanner's tabs | **Diagnostics** (`#scanner-diag`) | The spectrum of the receive window; the scanner's events, with the carriers heard behind the same tabs and the Housekeeping switch; the control channel, the receive window, the tuning |
| Scanner's tabs | **Settings** (`#scanner-settings`) | Gain; the window presets; the call timings; the history's limits; recording; packet data |
| TV's tabs | **Diagnostics** (`#tv-diag`) | TV's events; the 8-VSB receiver: its state, the block's registers, its loops at the newest field, what it decoded and lost |
| The rail | **Diagnostics** (`#diag`), headed "Radio diagnostics" | The radio's own events, with the API's routes behind the same tabs; the radio (build, uptime, lease, LO, crystal); the board as it is read back |
| The rail | **Settings** (`#settings`), headed "Radio settings" | The clock; the crystal; the configuration as a file (export, import, factory reset) |

- **A module's own two pages are tabs of it** like any other, set apart after its pages by a
  rule. In `js/modules.js` they are pages marked `tool`; a new module brings its own the same
  way.
- **TV has no Settings tab:** it has no setting in the configuration. Its scan's bands and gain
  are picked on Channels for each scan.
- **Each page says where the others are,** in one line with a link.
- **Gain is the scanner's.** TV sets its own gain while it has the radio, so the setting is not
  the whole radio's.
- **Recording's first row is "What is recorded":** Off, Marked or Every call, the same choice as
  Now's bar (one table for both, `js/recording.js`), in place of an on/off switch that hid
  `every_call`. It is saved as it is picked; where recordings go and how long they are kept
  wait for the card's Save.

### A settings card reads, saves and follows its own part

- **Reads:** a settings page reads what the radio holds into its cards, and reads it again when
  the radio announces a change to its settings (`changed`, `what: radio` on `/ws/live`), made
  here, in another browser or by a tool. The store counts those (`radioRev`).
- **What is being typed stays:** a card with a field changed and not saved keeps its fields
  through such a read, until its Save.
- **Saves:** a card sends its part and no other. `PUT /api/v1/radio/settings` now takes any of
  `presets_allowed`, `traffic_chains`, `calls` and `history`, and of `calls` and `history` any
  of their fields; what is left out stays as it is. Before, it took all four and replaced them.
  The history's limits are applied only when they changed.

### The event log is kept by part

- **Measured first.** On unit A, on Clay County, the log's 2,000 lines spanned 9.2 minutes:
  1,442 control channel messages, 470 packet data lines and 85 calls, against 2 of the crystal's
  and 1 of a lane's. A log shown by module from one ring would hold the last nine minutes of the
  radio's own lines, and of TV's none.
- **A line is one part's:** the scanner's, TV's, or the radio's own (its start, clock, crystal,
  a change of mode, the configuration replaced or reset). Each part's lines are kept in a ring
  of their own: 2,000 for the scanner, 500 each for TV and the radio. Housekeeping is the
  scanner's and is held as briefly as before.
- **`part` is in each line** and can be asked for: `GET /api/v1/events?part=scanner|tv|radio`,
  and `events: { after, routine, part }` on `/ws/live`. Without it, every part's lines come in
  the order they were written, as before.
- **The log's second column** names a control channel message by its protocol, as before, and
  what the radio did by its kind (`site`, `call`, `crystal`, `atsc`), where every such line read
  "system".

### Found by the check and fixed

- **A line of the page just left could land in the next page's log.** Lines already sent for one
  subscription arrive after the next page has subscribed, and the store hands an `events`
  message to whichever page wants events. With one log it went unseen; with a log per part the
  radio's showed the scanner's "site live" line. A log now takes only its own part's lines.
- **The Recording card's status line** read "[object HTMLParagraphElement]": the card's foot took
  text and was given the element the card rewrites. It takes either.

## Not in this change

- **TV's settings.** A place for them comes with the first one.
- **The spectrum on TV's Diagnostics.** Whether the spectrometer gives frames of a TV channel
  while live TV plays was not looked at.
- **Storage, the network and access** among the radio's settings: steps 5 and 6.
- **A page's requests aborted when it is left** (095's "with the next page built"): the settings
  pages carry a "gone" flag like the three before them.

## Found, not changed

- **Housekeeping switched on and off quickly can repeat lines.** The same late arrival, within
  one part: lines sent for the housekeeping subscription can arrive after the log was emptied
  for the next one, and are then sent again. It needs the subscription named in each `events`
  message.
- **The notice on TV's pages while the scanner has the radio** reads "These pages show what the
  last TV scan found", which is not what TV's Diagnostics shows.
- **`POST /api/v1/atsc/live` answers `available: false`** on a unit that has the 8-VSB block:
  only the read route and the feed fill that field in (`api/v1/atsc.rs`, from 085). No page
  reads the start's answer for it.

## Checks

See the status log for the counts.

- **Host tests:** a settings edit changes only what it gives; each part keeps its own lines, a
  busy control channel does not push out the radio's, and every part together comes in order.
- **`runs/ui_check/tool_pages.mjs`**, rewritten for the five pages. Each opens with its cards,
  among its module's tabs or as the radio's own; each log holds its part's lines and no other's,
  held against `GET /api/v1/events?part=`; each setting shows what the radio has. On the PC
  daemon it also writes: a change made elsewhere is followed with no reload while a typed value
  stays, a part's write changes that part alone, a card's Save sends its part, the recording
  choice is saved as picked, and all is put back. With `--reads <unit>` nothing is written. No
  delete and no factory reset is pressed.
- **`runs/ui_check/tv_diag_live.mjs`** (new), on a unit: it takes the radio. The unit is given
  to TV, live TV starts on an RF channel, TV's Diagnostics is read while it plays (the receiver,
  the block's registers, the loops, the counts, TV's log), then live TV is stopped and the radio
  goes back to the scanner whatever happened in between.
- **The other pages**, their tabs' expectations updated: `now_rail.mjs` (the control channel's
  counters and the packet data switch are now on the Scanner's own two), `aliases_page.mjs`,
  `playlist_page.mjs`, `scanner_pages.mjs`, `tv_watch.mjs`, `modules_check.mjs`.

## Status log

- 2026-11-11: built. Host tests 542 pass (2 new: a settings edit, the log's parts); the DMR
  reference's tests pass (102, holding its floor of 24,984 lines); the ARM check and the ARM
  release build are clean. BUILD_TAG `2026-11-11-parts1`. `modules_check.mjs` 45 modules.
- 2026-11-11: headless Chrome on the PC daemon, started after `seed_heard.py`:
  - `tool_pages.mjs` 31 steps with the writes, at 1920x1080 and at 1280x800. Its first run
    failed two, both real ("Found by the check and fixed");
  - `now_rail.mjs` with the writes 29 and with unit A's reads 15, `aliases_page.mjs` 34,
    `playlist_page.mjs` 16, `scanner_pages.mjs` 40, `tv_watch.mjs` 12.
- 2026-11-11: `2026-11-11-parts1` on unit A (binary only, in RAM over the SD image
  `2026-11-10-pages2`, which does not have it: a reboot of A returns to that image's pages):
  - live on Clay County within 35 s at 100 %; its log has no line but the start's;
  - with A serving the page, reads only: `tool_pages.mjs` 27 steps at both sizes, the spectrum
    drawn and the carriers listed; `now_rail.mjs` 15, `aliases_page.mjs` 9,
    `playlist_page.mjs --preview-only` 13, `scanner_pages.mjs` 40 (one step, a 2.5 s recording
    read at 0 s, failed in the first of two runs: the step that did the same in 097);
  - **TV's Diagnostics with a channel playing** (`tv_diag_live.mjs`, 9 steps): RF 19 playing 15 s
    after its start; the block's registers (591 fields counted, the timing loop tracking, 54 tap
    sets loaded); the loops (carrier 2 Hz off the centre, timing 0.0 ppm, MER 24.1 dB over 40
    fields); 184,376 codewords, 0.09 % corrected, 1.01 % beyond Reed-Solomon; 54 solves, the
    last in 45 ms; TV's log one line, "live TV on RF 19". Then the radio back on the scanner,
    live at 100 %, the next clear calls followed and recorded on both tuners;
  - the radio's own log after that: its start, the clock set from the internet and then from
    the control channel, "ATSC TV mode", "scanner mode";
  - `scanner/doc/API_FIELDS.md` regenerated from A.
- **Not checked:**
  - the settings' writes on unit A: they ran on the PC daemon;
  - the controls no check presses: the gain's Apply, the Saves of the window presets, the
    history and the recording's limits, the clock's two and the crystal's three. Their code is
    094's, moved into its card; the Calls card's Save and the recording choice are pressed;
  - the deletes and the factory reset, which no check presses;
  - live TV failing or searching, as TV's Diagnostics shows them; a unit with no 8-VSB block
    (the PC daemon's answer stands in for one);
  - a phone's width.
