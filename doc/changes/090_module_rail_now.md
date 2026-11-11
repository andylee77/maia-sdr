# 090 — The module rail, the new Now page, and a switch for P25 data traffic

**Date:** 2026-11-10. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** done; in unit A's SD image since `2026-11-10-tv2` (092's status log).

## Why

Andy asked for the web UI to be redone: modern and clean, running the trunking scanner and the
ATSC TV "modules", with room for the ones to come. A review of the pages on unit A found:

- The unit's mode was a list in the header, and the other mode's tabs were hidden. The TV
  channels could not be read while scanning, nor Activity in ATSC mode. Data mode would add three
  more tabs behind the same list.
- The header's listening controls showed in every mode, though only the scanner has live audio.
- Pages were capped at 1280 px on a 1920 px design size.
- Now carried the control channel's counters above the calls.
- Every row read "TG 300" and "1014": the unit has no aliases, and nothing on the live screen
  set one.

Three shells were drawn (a module rail, a one-screen console, a launcher). Andy chose the rail,
then asked for three things as it was built:

- Hold is not needed on every row.
- The console's talkgroup bank, to set talkgroups on or off, belongs on Now.
- P25 data traffic was followed with no way to turn it off.

## Changes

### The shell

- **`js/modules.js`** is the one table the shell is built from: a module per unit mode (Scanner,
  TV) with its pages, and the tools every mode shares (Diagnostics, Settings). A new module is
  an entry there, its views, and its mode in the daemon.
- **The rail** on the left lists them; a dot marks the module that has the radio. The viewed
  module's pages are tabs in the header. On a phone the rail is a bar along the bottom.
- **Viewing a module does not move the radio.** Its pages stay readable while another module has
  it, under a notice that offers the switch ("The radio is on Scanner. … Switch the radio to
  TV"). Leaving the scanner still asks first. On the radio's own module and on the tools, the
  header offers "Return to Scanner" while another module has it.
- **The header** says which module has the radio and what it is doing ("Radio on Scanner ·
  Simulcast · 860.9625 MHz"). The listening controls show only while the scanner has the radio.
- **Opened with no address,** the page goes to the first page of the module that has the radio.
- **Tokens:** a darker ground, one accent, system fonts as before, no shadows in the dark theme.
  Card titles are sentence case. The other pages keep their layouts inside the new shell.

### Now

Now fills the screen from 1000 px wide; each pane scrolls on its own.

- **The talkgroup bank** (`views/talkgroups.js`, left from 1360 px): every talkgroup of the live
  system that has an alias of its own or was heard in the last week, by name and then number,
  those heard only encrypted last (with a lock, and no controls: they are never followed). Each
  row has:
  - a switch: followed or not (`do_not_monitor`);
  - its speaker: both, left or right;
  - its priority, 1 to 100 or none;
  - its name, typed in place.

  A change goes to the radio at once (`PUT …/talkgroups/{tg}`), and the row then shows what the
  radio holds. A row is marked while its talkgroup is on the air, and says when it was last
  heard.
- **The site strip:** the system and site lists, whether the control channel decodes, its
  percentage and message rate, the system's type and identities. The other counters (last
  message, the demodulator, grants, the window, CPU) moved to a Control channel card on
  Diagnostics.
- **The traffic channels:** as before (the call, its phase, this browser's volume and mute, the
  list that keeps the channel on one talkgroup). **Hold** is on the card of the call on the air,
  and Release there and in the site strip; no row has one.
- **The calls:** one row a call, picked with a click. Filters: all, followed, with alert tones.
  An encrypted call shows a lock.
- **The picked call** (right; below 1360 px it covers the bank until closed): its recording's
  player, its alert tones as bookmarks, download and lock, and its details. One recording plays
  at a time, in this panel, so the list moves under it without stopping it; picking another call
  stops the first.
- A talkgroup's colour is its alias's, else one from its number, so a talkgroup keeps its colour
  down the list.
- The week's talkgroups are asked of the history once as the page loads, then at most every
  5 minutes. The old page asked about a dozen times at each load: it noted the site only when the
  answer came, so every draw before then asked again. Measured on unit A: one
  `/api/v1/activity/talkgroups` request at load.

### P25 data traffic

At a P25 site with two lanes the last one waited on the site's data channel between calls,
always.

- **`radio.json` `packet_data.follow`** (default true, as before).
- **`PUT /api/v1/radio/packet_data` `{follow}`** keeps it, and the live site follows at once:
  turned off, a lane waiting on the data channel is paused and none goes back; turned on, the
  last lane goes there at the next tick.
- **Settings** has a Packet data card with the switch.

## API notes for a later review

`scanner/doc/API_INVENTORY.md`, "What a code review may change", lists what these pages lean on
that a review of the API is likely to move, each with what follows in the UI. In short:

- the new route's place and name (`/radio/packet_data` beside `/api/v1/data`, which is due to be
  renamed);
- `PUT …/talkgroups/{tg}` making an alias named `TG <n>`, never removing it, and needing a
  priority sent with the switch where only prioritised talkgroups are followed;
- the pages working out a talkgroup's treatment from `systems[].aliases` by the follower's rule,
  which is then in two places;
- a whole re-read of the systems after one alias edit, and no notice for a radio setting;
- the Viewer polling `/api/v1/atsc/live`;
- `recording.every_call`, which has no control, so the bank has no Record switch.

## Not in this change

- Activity, Systems, Channels, Viewer, Diagnostics and Settings keep their layouts.
- Channels and Viewer still show their Scan and Play buttons while the scanner has the radio;
  the radio refuses them with a message. The notice above them says why.
- A Record control per talkgroup, an alias's colour and group, range aliases as rows, an Aliases
  page.
- Data mode's module.
- The phone layout beyond the rail becoming a bottom bar and the panes stacking.

## Checks

- **`runs/ui_check/modules_check.mjs`:** every module's syntax (`node --check`) and every named
  import against its module's exports: 33 modules.
- **Headless Chrome** (`runs/ui_check/now_rail.mjs`), the page from the PC daemon
  (`2026-11-10-rail1`), no exceptions or console errors:
  - With every read from unit A (its live socket, calls, history, recordings; a unit refuses
    writes from another host's page), at 1920×1080 and 1280×800, 10 steps:
    - the rail, the tabs and "Radio on Scanner · Simulcast · 860.9625 MHz";
    - Now fills the screen with no page scroll in both layouts;
    - the bank lists 41 talkgroups, 20 of them encrypted, at the end;
    - a call picked by its play button plays in the panel, the player stays while the list
      moves, picking another call stops the first, Close empties the panel;
    - no Hold on a row or in the panel.
  - With unit A's configuration on the PC daemon and the history from A, 18 steps:
    - a talkgroup switched off and on, its speaker moved, its priority set, a priority of 500
      refused with a message, a name given and emptied: the daemon's aliases hold each;
    - TV opened while the scanner has the radio: its tabs, the notice and the offer; the switch
      gives TV the radio (`/api/v1/mode` says `atsc`), Now then says it is paused and offers the
      way back, and the switch back brings the scanner;
    - Diagnostics has the control channel's counters;
    - Settings' switch turns `packet_data.follow` off and on in the daemon.
- **Host tests:** see the status log. `the_last_lane_waits_on_the_data_channel` also covers the
  setting off, and `a_waiting_lane_follows_the_packet_data_setting_at_once` a change while the
  site is live.

## Status log

- 2026-11-10: committed (8d2cc33). Host tests 516 pass; the DMR reference keeps 24,984 of 24,996
  lines; the ARM release build is clean.
- 2026-11-10: `2026-11-10-rail1` went onto unit A (binary only):
  - live on Clay within 35 s, control decode 100 % at 40 messages a second, 101,576 recordings
    indexed;
  - `now_rail.mjs` with A serving the page and its data: the 10 read steps pass (41 talkgroups
    in the bank, 20 of them encrypted; a call picked by its play button plays in the panel);
  - the packet data setting through the API on A: turned off, lane 2 left the data channel
    (857.9875 MHz then) within 2 s and stayed off it for the 26 s watched; turned on, it was
    back within 5 s; a body without `follow` is refused. It is on again, as before.
  - The write steps (the bank's controls, giving TV the radio and back) ran on the PC daemon
    only: on A they would leave an alias behind and stop the scanner twice.
- `scanner/doc/API_FIELDS.md` is regenerated from unit A (`tools/api_fields.py`), with
  `packet_data.follow`.
