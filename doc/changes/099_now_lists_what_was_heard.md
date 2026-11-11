# 099 — Now lists what was heard

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-11-heard1`) over the SD
image `2026-11-10-pages2`, which does not have it.

## Why

The roadmap's Q4, "what is in the talkgroup list once an import adds hundreds of names?", had
two answers before this one:

- **096:** every talkgroup an alias names, and those heard in the last week.
- **097:** those heard in the last week, and those an alias gives a priority, a tuner or a Rec
  mark; a name alone, or switched off alone, lists none.
- **098 found that not enough.** Andy's own playlist marks 274 aliases Record (60 of Duval's,
  all 118 of Alachua's, all 73 of FPL's). Each lists its talkgroup whether it is heard or not,
  and once his playlist was on unit A one did: "Hospital 321", not heard that week.

Asked "Should Now list only talkgroups heard in the last week?", Andy answered "Yes, heard only"
(2026-11-11).

## Changes

- **Now's talkgroups are the ones heard:** at the live site in the last week, or among the
  calls listed now. An alias for a talkgroup not heard lists none, whatever it sets. It is what
  the radio pushes as `talkgroups` on `/ws/live` and answers at `GET /api/v1/talkgroups`; each
  talkgroup still carries what its alias says of it.
- **The page** is as it was, but for the count's tooltip: the groups, the rows and the selected
  talkgroup's controls work on whatever is listed.

What it means in use:

- A talkgroup shows on Now when it speaks, and stays a week from its last call.
- Every alias, and every setting for a talkgroup that is silent, is on Aliases.
- A site made live for the first time lists nothing until a talkgroup is heard there.

## The check

Now's write steps (`runs/ui_check/now_rail.mjs`) need a talkgroup in the list, and a PC daemon
has no radio to hear one.

- **`runs/ui_check/seed_heard.py`** (new) gives a PC daemon's history one: a row in the hourly
  table (`tg_hour`) for talkgroup 64999 at the fixture's site, as three clear calls ten minutes
  ago. A daemon with no card keeps its history in `/tmp` (on Windows the drive's `\tmp`). It is
  run before the daemon starts, since the daemon reads the week once and keeps it five minutes.
- **The steps follow the rule:** the heard talkgroup is listed under its number with no alias;
  one never heard is not listed though its alias gives it a priority, a tuner and a Rec mark;
  named, its row takes the name; and an emptied name with no other setting removes its alias
  while it stays in the list under its number. That last path could not be checked on a PC
  daemon under 097's rule.
- **"The page asks the history nothing by itself"** no longer counts older calls. The calls'
  pane reads a page of them when its list's end comes into view, which a short list's does (see
  below).

## Found, not changed

The calls' pane reads older calls from the history when its list's end comes into view. On a
list too short to fill the pane the end is in view from the start, and whether a page is read
then depends on the order things arrive in: read if the site went live after the page opened,
not read if it was live already (the button "Load older calls" reads it). It showed in this
change's first run of the check, against a daemon started seconds before. From 096; it matters
on a quiet site after a restart. Placed in the roadmap with what is left of step 2.

## Not in this change

- A list picked by hand (a scan list), the roadmap's alternative: it needs a new field.
- A longer or shorter memory than a week.

## Checks

See the status log for the counts.

- **Host tests:** the list is the week and the calls listed now; a talkgroup whose alias sets a
  priority, a tuner and a Rec mark, never heard, is not in it.
- **`runs/ui_check/now_rail.mjs`** with the writes, on the PC daemon after `seed_heard.py`; and
  its reads with a unit serving the page.
- **The other pages:** `aliases_page.mjs`, `playlist_page.mjs`, `scanner_pages.mjs`,
  `tool_pages.mjs`, `tv_watch.mjs`, `modules_check.mjs`.

## Status log

- 2026-11-11: built. Host tests 540 pass (the list's test rewritten for the rule); the DMR
  reference's tests pass (102, holding its floor of 24,984 lines); the ARM check and the ARM
  release build are clean. BUILD_TAG `2026-11-11-heard1`. `modules_check.mjs` 39 modules.
- 2026-11-11: headless Chrome on the PC daemon, started after `seed_heard.py`:
  - `now_rail.mjs` with the writes, 29 steps. Its first run failed two, both "the page asks the
    history nothing": the one request was the calls' pane reading older calls ("Found, not
    changed"); the step counts the talkgroups' and the totals' reads since, and all 29 pass;
  - `now_rail.mjs` with unit A's reads 15, `aliases_page.mjs` 34, `playlist_page.mjs` 16,
    `scanner_pages.mjs` 40, `tool_pages.mjs` 15, `tv_watch.mjs` 12.
- 2026-11-11: `2026-11-11-heard1` on unit A (binary only, in RAM over the SD image
  `2026-11-10-pages2`, which does not have it: a reboot of A returns to that image's pages):
  - live on Clay County within 32 s; nothing in the log but its start line; the clear calls
    after the restart were followed on Traffic 1 and recorded;
  - **Now lists 41 talkgroups, every one of them heard,** 28 with a name. Under 098's build it
    listed 42: "Hospital 321" is gone from Now and is still one of Clay's 39 aliases. Twelve of
    the 41 have a Rec mark or a priority, as before;
  - with A serving the page, reads only: `now_rail.mjs` 15 steps, `aliases_page.mjs` 9,
    `playlist_page.mjs --preview-only` 13;
  - `scanner/doc/API_FIELDS.md` regenerated from A.
- **Not checked:**
  - a site made live for the first time, with nothing heard yet (the page's text for it is
    "No talkgroups heard yet.", from 096);
  - a talkgroup leaving the list a week after its last call: the week is the history's query,
    which its own tests cover;
  - Now's writes on unit A: they ran on the PC daemon, as before.
