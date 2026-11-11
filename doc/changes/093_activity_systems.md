# 093 — Scanner: the Activity and Systems pages in the rail's layout

**Date:** 2026-11-10. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** on unit A in the test build `2026-11-10-pages2` (094's status log), in RAM. A reboot
returns it to the SD image `2026-11-10-tv2`, which has the old Activity and Systems pages.

## Why

After the rail with the new Now page (090) and TV's pages (091, 092), Andy asked for the Scanner
module's other two pages.

- **Activity** was one column of 1,280 px on a 1,920 px screen and, on a day of Clay County,
  7,200 px long: the alert tones, the talkgroups and radios, the calls and the packet data each
  a table under the last. A picked talkgroup's details opened in a card in the middle of the
  page, the calls could not be played, and only the 25 busiest talkgroups and radios could be
  picked (a finding of 089's review).
- **Systems** had the scan's setup open above everything and showed one system at a time from a
  pull-down. Its scan card opened a second WebSocket to the radio.

## Changes

### Activity (`views/activity.js`)

The page fills the screen, each pane scrolling on its own, as Now does.

- **The bar:** the site (or every site of a system), the period (24 h, 7 days, 30 days, or
  Dates with From and To on whole hours, 089), where the history is kept, and the CSV export.
- **Who talks** (left): the talkgroups or the radios, the 200 busiest, each a row with its name,
  its time on the air as a bar against the busiest (voice in green; grant time in red where
  every call is encrypted, blue where none is, grey where they are mixed), and its counts. The
  box narrows the list by name or number; a number that is not among the 200 is offered by
  itself, so any talkgroup or radio can be reached.
- **A row picked narrows the page** to that talkgroup or radio. The left column then shows what
  is known of it (a talkgroup's encryption history and its radios; a radio's talkgroups and its
  affiliations and registrations), each of those rows picking in turn; the totals and the chart
  are its own; the calls and the alerts are its own. The arrow goes back to the list.
- **The totals and the chart** (middle): the nine totals in a row and the airtime per hour or
  day. What voice and grant time are is behind the card's "i".
- **The lists** (middle, under the chart): Calls, Alerts or Packet data, one at a time. Only
  what is on screen is read: five requests at a load, where the old page made eight.
  - **Calls** are Now's rows (the talkgroup's colour and name, the radio, the channel, the
    recording's play button or why there is none), with the day in the time on another day.
  - **Alerts:** the alerts grouped by kind and tones, then the newest, each with its talkgroup,
    its sender, its kind and tones, its dispatch's length and a play button. How an alert and
    its dispatch are found is behind the "i".
  - Each list shows its newest 50 rows and "Show more" (089).
- **The picked call** (right): Now's panel. A call's play button, or an alert's, plays it
  there: the recording's player, its alert tones as bookmarks, download, lock, and what is
  known of the call. An alert plays the call that carried what it announced, from where the
  speech starts; the page reads that call from the history by its grant time.
- **One recording plays at a time, in the panel,** so the lists reload under it. 089 held a
  reload back while a recording played in a row; with no players in the rows that wait is gone.
- **Widths:** from 1,600 px the three columns; from 1,000 px who talks beside the lists, the
  picked call covering it until it is closed (Now does that from 1,000 to 1,360 px; Activity's
  lists need more width); narrower, one column and the page scrolls.

### Systems (`views/systems.js`, `views/scan.js`)

- **A list on the left:** "Find systems" with what the scan is doing or what the last one found,
  then each system with its protocol, its sites and "live" on the one with the live site.
- **The picked one on the right:** a system's card as before (its name, details and identity,
  its sites with their channels, every value edited in place, Save once something changed), or
  the scan (its bands and settings, its progress, then the found systems to add).
- The live site's system is shown until another row is picked. Leaving a system with unsaved
  changes asks first.
- A site's buttons say what they do: "Make live", "Stop".
- **The scan comes on the page's one socket.** The store holds the scan (`store.scan`, from the
  snapshot and each `scan` message); the scan's own WebSocket is gone.
- The list follows the systems as the radio reports them changed (an add from the scan, a change
  from another browser), except under a system being edited.

### Shared

- **`views/call_rows.js`:** a call as a list row and the picked call's panel, moved out of
  `now_calls.js` for the two pages. A page may give the rows its own time text (Activity's
  carries the day). The panel's class is `call-detail`.
- `js/modules.js` marks both pages `fill`. `dom.js` gains two icons (back, search).
  `packet_data.js` draws into Activity's list.

## Not in this change

- **Systems:** adding a system or a site by hand, the SDRTrunk playlist import and export, and a
  page for the RadioReference import (its routes exist; no page uses them). A system's
  talkgroups and aliases are still set only from Now's bank.
- **Activity:** the followed count, the estimate of encrypted voice and the counts of talkgroups
  and radios are a whole site's; narrowed to a talkgroup or a radio the page shows the five
  totals the chart's data gives. A list's rows end at 800 ("Pick dates to see the others").
- Diagnostics, Settings and TV's Channels keep their layouts.

## Checks

- **`runs/ui_check/modules_check.mjs`:** 36 modules, syntax and imports.
- **Headless Chrome** (`runs/ui_check/scanner_pages.mjs`, sound muted), the pages from the PC
  daemon (`2026-11-10-pages1`) and every read from unit A (its history, systems, socket and
  recordings; a unit refuses another host's writes): 40 steps at 1920×1080, 1440×900 and
  1280×800, no exceptions:
  - Activity opens on the last 24 hours with who talks, the totals and 50 calls, fills the
    screen, and read neither the alerts, the radios nor the packet data; Show more gives 100;
  - a call's play button plays it in the panel, and it plays on while the radios replace the
    talkgroups in the list and the page reloads;
  - Alerts lists the groups and the newest; an alert's play button reads one call by its grant
    time and plays it in the panel; Show more keeps the picked row marked and playing; Close
    stops it;
  - a talkgroup picked narrows the page (its details, five totals, the calls and the chart asked
    for with its number), one of its radios in turn, and Back returns;
  - talkgroup 16777000, never heard, is offered by its number;
  - Dates start as the period, a past afternoon lists its calls per hour, nine days are drawn
    per day, backwards dates are refused, and 7 days asks by hours again;
  - Systems lists the scan and six systems with the live one picked, fills the screen, shows
    another system from the list, shows Save on an edit and puts the name back on Cancel, and
    shows the scan's setup with the page on one socket; nothing was written.
- **`now_rail.mjs`,** read only, on the same build: 10 steps (Now with the shared rows and panel).
- **Host tests:** 519 pass.
- **Not checked:** Save, Make live, Stop, the two deletes and a found system's Add were not
  clicked on a unit (their code is the old page's, behind new buttons); the phone width and the
  light theme were looked at in a picture each, not stepped through.

## Status log

- 2026-11-10: committed. Host tests 519 pass; the DMR reference keeps 24,984 of 24,996 lines;
  the ARM release build has its standing warnings only.
- 2026-11-10: `2026-11-10-pages1` went onto unit A (binary only), on the radio antenna: live on
  Clay within 30 s, control decode 100 %.
  - `scanner_pages.mjs` with A serving the pages: 40 steps, no exceptions, nothing written;
    `now_rail.mjs`, read only: 10 steps.
  - **A scan from the Systems page** (`runs/ui_check/systems_scan.mjs`, 800 MHz alone, two
    windows): the page followed it on its one socket (the band and the window, then each of 21
    carriers as it was listened to), the list's row and the header said it was scanning, and it
    ended after 99 s with 4 control channels found and the one system not configured (P25
    system 1D5) offered with "Add system", which was not clicked. The row then read "Last scan:
    4 control channels found" and Clay was live again.
