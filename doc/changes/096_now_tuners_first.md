# 096 — Now as a monitoring station's front: the tuners first

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-11-now1`) over the SD image
`2026-11-10-pages2`, which does not have it: a reboot of A returns to the old page.

## Why

Andy, 2026-11-11: "clean up the now page to be way more user friendly and clean. something more
grounded into a trunked monitoring station that has 2 traffic tuners and allows easier control of
what to listen, record, etc." Three layouts were drawn with unit A's data (095): A "tuners first",
B a console of talkgroup tiles, C the handheld's face beside a log. He picked A.

What the page of 090 got in the way of, on unit A:

- Every row of the bank read "TG 300" with a dash, "L+R" and a switch; what "L+R" set was not
  said, and nothing recorded could be chosen.
- A third of the calls listed were encrypted grants nobody can hear, each as tall as a call with a
  recording, each with its frequency and channel number.
- The right third of the screen was empty until a call was picked.
- The site strip carried the system's identities; the listening controls were in three places.
- The page asked the history for the week's talkgroups every 5 minutes and for the site's call
  count every 30 s, worked out each talkgroup's treatment from a copy of the follower's rule,
  and kept every call it had seen (095's review, D and E).

## Changes

### The page

Now fills the screen from 1000 px wide: the site bar, the two tuners side by side, and under them
the talkgroups (a third of the width) beside the calls. Below that width it is one column.

- **The site bar:** one list of every site by system (picking one makes it live), whether the
  control channel decodes, the radio's hold with Release, **what is recorded** (Off, Marked,
  Every call), and what the card holds. The system's identities are on Systems and Diagnostics.
- **The tuners,** Traffic 1 and Traffic 2, each a card: the talkgroup on it in large type with
  its radio and frequency, how long it has been on the air and how much of that was voice, its
  phase; Hold on the call it carries; the talkgroup it is kept on ("Follows any talkgroup");
  **its last three calls, to play again**; this browser's volume and mute for it.
- **The talkgroups,** in three groups: those listened to, those switched off, those heard only
  encrypted (the last two closed until opened, each saying what it hides). A row has the
  talkgroup's **tuner** (Any, 1, 2), its **Rec** mark and its **Listen** switch. The list is in a
  fixed order (by name, then number), so a row does not move under the pointer.
- **One talkgroup is selected,** by a click or the arrow keys, and the bar under the list holds
  its controls: Hold, Skip or Listen, Record, its tuner, its priority, Rename. Away from a
  field the keys are H, L, R and 0, 1, 2. This is the model the handheld takes (the roadmap's
  "Now's model"): the list is what its knob steps through, the bar its buttons.
- **The calls:** one row a call, the followed ones with their tuner's number, their frequency
  left to the picked call's details. All, Heard, Alerts. The picked call's recording plays in
  a panel docked under the list, with its alert tones as bookmarks; what else is known of the
  call is behind Details.
- **With the radio not heard from,** the page dims and says so, and the tuners' clocks stop.

### Sound by tuner

- Traffic 1 plays on the left speaker and Traffic 2 on the right, whatever the talkgroup.
  Before, the browser panned by the talkgroup's `speaker`, so a talkgroup set to "both" played
  in both ears on either tuner while the card read "Traffic 2 · Right".
- **Left / Right** in the header turns it off: both tuners on both speakers. A site that runs
  one lane plays on both.
- An alias's `speaker` is unchanged in the radio, where it always was the talkgroup's lane:
  `left` lane 1 only, `right` lane 2 only, `both` either. The page calls it the tuner.

### What is recorded

- The radio already had two ways (`recording.every_call`, on by default, or the talkgroups whose
  alias says `record`), with no control for the first. The site bar's Record is that control:
  **Off**, **Marked** (a talkgroup's Rec mark decides), **Every call** (the marks are lit and
  locked).
- **`PUT /api/v1/radio/recording` takes any of its fields.** Before, it replaced the whole
  object, and a body without `every_call` set it back to on.

### What the radio pushes

- **`talkgroups` on `/ws/live`,** and `GET /api/v1/talkgroups`: the live site's talkgroups (those
  an alias names, and those heard in the last week or among the calls listed now), each with how
  it is treated by the follower's own rule (`trunking::follow::routing`): followed, its lane,
  its priority, whether its calls are recorded; its calls in the week, how many encrypted, when
  it was last heard; and what decides recording.
- It comes in the snapshot, and again when the live site changes, an alias, the listening
  settings or the recording settings change, or a call opens on a talkgroup not listed yet. The
  history is asked for the week at most every 5 minutes, by the radio; the calls it lists cover
  the time since.
- **The page asks the history nothing by itself** (older calls still come as the list is
  scrolled to its end), and `js/aliases.js` no longer holds the follower's rule.

### One talkgroup's controls

- **`follow`** in `PUT …/talkgroups/{tg}`: followed or not, whatever the system asks of a
  followed talkgroup (where only those with a priority are followed, one with none gets the
  lowest). The page no longer sends a priority with its switch.
- **`DELETE …/talkgroups/{tg}`** removes the alias that names one talkgroup alone. The page
  uses it when a name is emptied on a talkgroup with no other setting of its own; one with
  settings keeps them under its number.

### The calls kept

The page keeps the newest 300 closed calls, a page more for each page read from the history, and
2,000 at most; the closed rows are drawn again only when one closed, learned its recording or
was read. Before, it kept every call it had seen and drew the whole list each second.

## Decisions taken with the layout

Layout A was drawn with the roadmap's recommendations for Q2 to Q4, and is built so:

- **Q2:** recording as Off, Marked or Every call. Unit A stays on Every call until changed.
- **Q3:** sound by tuner, with Left / Right in the header to turn it off.
- **Q4:** the list is today's (an alias, or heard in the last week). The bound for imports of
  hundreds of names waits for the import (step 3).

Each is Andy's to change: none adds a field to the configuration.

## Not in this change

- The 24-hour totals the drawing showed in the calls' heading ("7,699 in 24 h"): they would need
  a poll or another push. The heading counts what is on the air and listed.
- An alias's colour and group from this page, radios' aliases, the Aliases tab (step 3).
- `/ws/live` sending only what changed, and the hold's two routes, which 095's review placed in
  this step: not done, and noted in the roadmap. The third thing it placed here is measured (the
  status log): a talkgroup write, its save to the flash included, takes 14 to 18 ms on unit A.
- The handheld's face as a page.

## Checks

See the status log for the counts.

- **Host tests:** new ones for the talkgroups view (the aliases, the week and the calls listed
  now as one list; what is recorded under each policy) and for `follow`.
- **`runs/ui_check/modules_check.mjs`:** 36 modules.
- **The other pages,** unchanged but for the call row's tuner badge: `scanner_pages.mjs` 40 steps,
  `tool_pages.mjs` 15, `tv_watch.mjs` 12.
- **`runs/ui_check/now_rail.mjs`** is rewritten for the page:
  - on the PC daemon, the writes, to a talkgroup made for the check: the Listen switch (it moves
    to the switched-off group and stays in view), the bar's Listen, the tuner chip and the 0
    key, Record as Marked and Every call with the Rec mark under each, the priority and its
    refusal, Rename, Hold and the H key, an emptied name removing the alias; then the other
    modules' steps as before;
  - with a unit's reads: the groups, the encrypted group opened, the calls with their tuners, a
    talkgroup picked and the arrow keys, a call played in the docked panel with Details, the
    player kept while the list moves, a tuner's last call played again;
  - in both: the layout, and that the page asked the history nothing by itself.

## Status log

- 2026-11-11: committed. Host tests 526 pass (2 new); the DMR reference keeps 24,984 of 24,996
  lines; the ARM release build is clean. BUILD_TAG `2026-11-11-now1`. Headless Chrome on the PC
  daemon: `now_rail.mjs` 26 steps with the writes, `scanner_pages.mjs` 40, `tool_pages.mjs` 15,
  `tv_watch.mjs` 12; `modules_check.mjs` 36 modules.
- 2026-11-11: `2026-11-11-now1` on unit A (binary only, in RAM):
  - live on Clay County within 34 s, the control channel at 100 % and 40 messages a second;
    101,497 recordings listed;
  - `GET /api/v1/talkgroups`: 42 talkgroups (14 listened to, 8 switched off, 20 heard only
    encrypted) in 8.3 KB; 0.16 s the first time, with the week read from the history, and 0.02 s
    after;
  - `now_rail.mjs` read only, with A serving the page and its data: 15 steps, and the same 15 at
    1280x800 with the page from the PC daemon; the page asked the history nothing by itself;
  - a talkgroup write through the API (a name three times, `follow`, then `DELETE`, which left
    A's eight aliases as they were): 14 to 18 ms each;
  - `scanner/doc/API_FIELDS.md` regenerated from A.
- **Not checked:**
  - the sound by tuner by ear: headless Chrome plays nothing a check can hear, so that Traffic 1
    is on the left and Traffic 2 on the right, and what Left / Right does, wait for Andy's ears;
  - the page's writes on unit A: they ran on the PC daemon, since on A they would change what is
    recorded and a talkgroup's alias;
  - a site that runs one lane, and a system that follows only talkgroups with a priority, through
    the page (the daemon's rule for the second is tested).
