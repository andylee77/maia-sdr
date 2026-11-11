# 095 — Goals, capabilities and features; a review of the UI and the API under it

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. Not on a unit: A runs the SD image `2026-11-10-pages2`, and nothing
was deployed.

## Why

Andy, 2026-11-11, after the UI's rebuild (090 to 094): "I like the new layout and progress." He
asked for a code review, then for the interface cleaned up for use with the features still
missing (alias mapping and an Aliases tab; Settings and Diagnostics split into the whole radio's
and each mode's; taking the data off the SD card; auth and security; data monitoring and ADS-B
modes; later a stand-alone handheld), with the Now page redone first as a trunked monitoring
station with two traffic tuners. "A larger part is organizing and structuring around goals,
capability, features."

## Changes

### The roadmap

`doc/ROADMAP.md` is reworked into the product's goals, the capabilities each needs (gateware,
the daemon and its API, the UI, storage, security, hardware controls), the features by module,
the order of work with what each step needs first, and fifteen questions for Andy. The
findings of the upstream review (`doc/UPSTREAM_REVIEW.md`, written the same day by another
session) are placed in it: a kernel check on unit A now, three driver patches with the next
image, two gateware items with the next bake. What exists,
what is decided and what is open are marked. The ideas the old roadmap held are kept at its end.

### The review

Every file under `scanner/src/ui/static/` and `scanner/src/api/`, and `scanner/src/ui/mod.rs`,
was read whole, with what the handlers call. Each finding below was then checked in the code
before it was listed. "Confirmed" means traced in the code; none was reproduced on a unit. Line
numbers are those of d4faa74, the commit reviewed.

#### Fixed in this change

| # | Where | What went wrong | The fix |
|---|-------|-----------------|---------|
| 1 | `api/mod.rs:177` | **Any web page could write to the unit over HTTPS.** The check that refuses writes from another site's page compared `Origin` with the `Host` header and passed when there was none. HTTP/2 has none (port 8443 offers it first), so a page on any site could send a write without a body from a browser that had accepted the unit's certificate: a factory reset, a site stopped, a scan started | The host is the `Host` header or the request's authority, and a write with an `Origin` and no host to hold it against is refused |
| 2 | `services/iq.rs:47`, `api/v1/iq.rs:28` | **A GET could exhaust the daemon's memory.** Each `GET /api/v1/iq/control.wav` reserved up to 24 MB and stayed registered when it gave up with no site live, to fill at once when one went live. Any number could wait | A capture whose request ended is dropped, its memory is taken only as samples come, and two may wait at once |
| 3 | `api/v1/data.rs:61` | **A GET could rename files.** `site` went unchecked into the path of a site's learned state, and a file there that does not parse is moved aside: `GET /api/v1/data?site=../../x` moved `x.json` | Only a configured site's state is read |
| 4 | `api/v1/aliases.rs:70` | **A refused talkgroup edit was kept.** `PUT …/talkgroups/{tg}` changed the live configuration, then checked it: `{"priority": 0}` answered 400 and left priority 0 in memory, for the next save to keep | The system is changed on a copy, which replaces it once checked and saved |
| 5 | `services/atsc/stream.rs:265` | **A paused player of a TV recording made the daemon hold what it missed.** After a pause the recording was read on unsent until its clock was caught up: about 2.4 MB a second of pause, in memory | A clock reference more than a second overdue starts the pacing again from there |
| 6 | `api/v1/config.rs:105` | Import and factory reset fail while TV has the radio, saying "a scan or a site switch" | The refusal says TV has the radio |
| 7 | `js/store.js:232` | **A page that failed on a pushed message failed silently:** the exception was swallowed, so the page stopped updating with nothing in the console, and the headless checks could not see it | Only the message's parsing is caught |
| 8 | `js/views/settings.js:63`, `:116` | **Save deleted data without asking.** A lowered limit (days kept, MB, the RAM count, the history's days or MB) deletes what is past it at once | Save asks first when a limit is lower than the one loaded |
| 9 | `js/views/settings.js:40`, `:121`, `:196` | An emptied number box was sent as 0: a manual gain of 0 dB applied at once, an end grace of 0 ms, a crystal anchor of 0 (no limit) | Empty and non-numeric text is refused |
| 10 | `js/views/watch.js:227` | Play pressed and the page left while the radio changed mode: live TV still started, its sound played on another page, and the unit stayed on it | A start under way stops when the page is left, and stops a receiver it tuned |
| 11 | `js/views/watch.js:425` | Live TV stopped from elsewhere (another browser, Return to Scanner) left this page's picture frozen and its player running for good; a later start on another RF channel played that station under the old title | The player stops when the receiver leaves its channel |
| 12 | `js/views/watch.js:302` | Each visit to Watch left a click listener on the page's host, holding that visit's whole page | The listener is on the page's own root |
| 13 | `js/views/activity.js:436` | An alert's call arriving late replaced a newer pick, or after the page was left played its recording with no page to stop it | A pick is shown only while it is the newest and the page is there |
| 14 | `js/views/activity.js:453` | With no live site and no history the page reported an error at each load | It shows its empty state and asks nothing |
| 15 | `js/views/activity.js:226`, `:239` | Days in the chart took the previous day's date after a clock change inside the picked dates | A day is labelled by its midday |
| 16 | `js/views/scan.js:226`, `:277` | After adding one of a found system's sites the card stayed as it was with a dead button; finds drawn before the systems were read showed configured sites as new; a move reported "0 sites added" | The card is drawn again after an add, the finds wait for the systems, and the report counts what was updated |
| 17 | `js/views/systems.js:54` | Removing a site dropped the unsaved edits to its system without saying so | The question says so |
| 18 | `js/views/cards.js:272` | A mistyped control LCN was saved as none | It is refused |
| 19 | `js/views/now.js:256` | A traffic channel's list did not show it was kept on a talkgroup until another part of the page was clicked, and showed a pick the radio had refused | The list is redrawn from what the radio holds after a pick |
| 20 | `js/views/talkgroups.js:25` | A failed read of the week's talkgroups was not tried again for five minutes | It is tried again after 15 s |
| 21 | `js/api.js:15` | A refusal in plain text (an unknown field, a value out of its type's range) showed as "HTTP 422" | Its text is shown |
| 22 | `js/views/settings.js:225`, `js/views/window.js:58` | Two ages were counted against the browser's clock, not the board's | `boardNow()` in `js/store.js`, which Now uses too |
| 23 | `js/views/settings.js`, `index.html:41` | A switch stayed where it was put when its save failed (crystal tracking, the clock's source); Import and Factory reset could be pressed twice; the reset's texts left out the TV channel list it deletes; the whole page was announced to a screen reader at every push | Each put right |
| 24 | `js/format.js`, `js/dom.js`, `js/protocols.js`, `css/` | Code nothing used: ten exports of `format.js`, `clear`, `protocolNames`; the styles of the old speakers and groups panel, two grids and six log classes | Removed |

#### Open, by rank

Each is placed in the roadmap's step that settles it.

| # | Rank | Where | What | Step |
|---|------|-------|------|------|
| A | high | `api/mod.rs:177`, `api/ws.rs:43`, `:111`, `:148` | No login. The origin check does not cover the three sockets (any site's page can open them in the user's browser and hear the audio), and a name re-pointed at the unit passes it, reads included | 6 |
| B | medium | `api/v1/config.rs:88`, `services/recordings/mod.rs:403` | A factory reset queues a delete per recording and restarts half a second later; shutdown waits 10 s. With 101,000 recordings most stay on the card and are listed again at the next start, while the answer counts them as removed. The delete rate was not measured | 5 |
| C | medium | `aliases.rs:118`, `radio.rs:71`…, `systems.rs:54`…, `util/atomic_file.rs:13` | Every configuration write is a write, two syncs and a rename on the flash, inside its handler and under the configuration's lock, on a runtime of two workers that also run trunking, the recorder and live audio. Now's bank writes one per click. The cost on a unit was not measured | 2 |
| D | medium | `js/views/now_calls.js:137` | Now keeps every closed call it has seen and draws the whole list at each push (the traffic push comes every second): a page left open for days grows without end | 2 |
| E | medium | `js/views/talkgroups.js:44`, `js/aliases.js` | The bank lists every talkgroup with an alias of its own, each row with six controls, and works out its treatment from the follower's rule kept a second time in the page. An import of a large system's talkgroups makes thousands of rows. It asks the history every 5 minutes and the site's call count every 30 s, against the rule that no page polls | 2 |
| F | medium | `js/views/channels.js:244`, `services/atsc/sweep.rs:205` | A TV scan cancelled or failed has replaced the channel list until the scanner restarts | TV |
| G | medium | `services/atsc/live.rs:119` | One stream for the receiver's life: a player on one RF channel's address goes on receiving another's after a retune, a stopped one's response stays open and still counts as a viewer | TV |
| H | medium | `css/components.css:622` | Watch at 1000 to 1359 px wide with live TV: the programme card should overlap the signal card (worked out from the CSS, not rendered) | TV |
| I | medium | `api/v1/aliases.rs:71`, `services/config/aliases.rs:155` | `PUT …/talkgroups/{tg}` on a talkgroup that is one of several IDs of an earlier alias answers 200 and changes nothing the follower reads | 3 |
| J | medium | `api/v1/radio.rs:83`, `:107` | `PUT /radio/recording` and `/radio/settings` replace whole objects: a body without `every_call` sets it back to on, and two pages saving parts of one object overwrite each other | 2, 4 |
| K | low | `api/ws.rs:355`, `:419` | `/ws/live` per client per second: the status, the scan and the kept TV scan (85 KB) are built and compared; the traffic goes every second because one age in it moves; a call notice copies the calls twice and searches the recordings from the oldest | 2 |
| L | low | `js/views/activity.js:626` | One load serves every change: five requests a click, none aborted, on a history with one read connection; 800 alerts cost 800 look-ups a reload; packet data ignores the page's site and period | 3 |
| M | low | `js/views/systems.js:162`, `js/views/scan.js:162` | Systems keeps its own copy of the systems and reads each site's learned state at every change; the scan's cards repeat the daemon's rule for "the same site" and ignore the answer it sends | 3 |
| N | low | `api/v1/recordings.rs:32`, `activity.rs:294`, `ws.rs:425` | Missing bounds: a bare `GET /recordings` copies every entry; a CSV reader that stops holds a thread and a read transaction; a socket has no send timeout and sockets no cap | 6 |
| O | low | `api/v1/scan.rs:94`, `systems.rs:126` | A change saved, then an activation that fails: the answer is 500 and no notice is sent, so open pages keep the old systems | 3 |
| P | low | every view | A page has no signal that it was left and no boundary for its errors: each invents its own "gone" flag or lacks one. A per-mount abort signal passed to the requests would end stale answers with the page | with the next page built |
| Q | low | `js/views/window.js:61`, `channels.js:316`, `board.js:43` | Panes rebuilt at every push whether or not they changed: a button press that spans a push is lost, a text selection lasts 3 to 5 s | 4 |
| R | low | `css/components.css:695`, `js/views/channels.js:399` | Full screen hides every failure (the overlay and the toast are outside it); any failure of a channel's spectrum read is reported as "not kept across a restart" | TV |
| S | low | `services/recordings/mod.rs:362` | A recording's file, delete and lock take the oldest with an id, and ids can repeat when recordings and history are both lost at one start (plausible, not established) | 5 |

Written the same way twice, where a difference would be a bug: MHz as text (four places, one of
them wrong until this change), an alert's label (two), the receiver "is running" (three, in
`watch.js`), the definition-list builder (six files, two of which skip an unchanged draw), and
`tv/player.js`, which repeats `audio/player.js`'s start.

#### The API review table

`scanner/doc/API_INVENTORY.md`, "What a code review may change", now says for each row what the
review recommends and how large it is. One row was wrong: the daemon does announce a changed
radio setting (`changed`, `what: radio`), and it is the page that does not act on it.

## Not in this change

- Nothing the open table lists.
- The Now page: its layouts are drawn for Andy to pick from; nothing is built before he does.

## Checks

- **Host tests:** see the status log. New: the origin check against a host from the `Host`
  header or the authority; an abandoned IQ capture leaving the tap, and the limit of two; a
  refused talkgroup edit leaving the system as it was; a stalled TV player not sent what it
  missed at once.
- **`runs/ui_check/modules_check.mjs`:** 36 modules.
- **Headless Chrome,** the pages from the PC daemon (`2026-11-11-review1`), no exceptions or
  console errors, which a failing page now reports:
  - `now_rail.mjs` with every read from unit A: 10 steps. With the writes on the PC daemon: 18
    steps (a talkgroup switched off and on, its speaker, its priority, a priority of 500
    refused, a name given and emptied; TV given the radio and back; Settings' packet data
    switch). Three of that run's expectations dated from before 091 and 094 (TV's tab names,
    Diagnostics' rows) and are brought up to the pages.
  - `scanner_pages.mjs` with A's reads: 40 steps.
  - `tool_pages.mjs` with A's reads: 15 steps; with a TV scan replayed: 18.
  - `tv_watch.mjs` with a saved scan: 12 steps.
  - Activity on a daemon with no systems: its empty state, no error.
- **Not checked:**
  - nothing ran on a unit;
  - the origin check over HTTP/2 against a browser (its two functions are tested);
  - Watch's two new stops, which need the unit on TV;
  - Settings' new questions (no check presses a Settings button);
  - the chart's dates across a clock change, not rendered.

## Status log

- 2026-11-11: committed in three parts (the daemon's fixes, the pages' fixes, the documents).
  Host tests 524 pass (5 new); the DMR reference keeps 24,984 of 24,996 lines; the ARM check is
  clean. BUILD_TAG `2026-11-11-review1`.
