# 097 — The Aliases page, with the RadioReference import

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-11-aliases1`) over the SD
image `2026-11-10-pages2`, which does not have it: a reboot of A returns to that image's pages.

## Why

The roadmap's step 3. Andy, 2026-11-11: "clean up the interface for use and to add the additional
features like alias mapping and alias tab". His go-ahead for this step was "continue", after
096's report named Aliases as next.

What a system's aliases were before this change:

- On Now, one talkgroup at a time: its name, tuner, Rec mark, Listen switch and priority. No
  group, no colour, no radio's name, no range.
- Through the API, the whole list replaced at once. No page did that.
- RadioReference's files had an import route and its preview, and no page.
- A system's listening settings (what its talkgroups with no priority get) had a route and no
  control anywhere.
- Unit A had eight aliases, each a talkgroup switched off and named by its number. Not one
  talkgroup or radio had a name.

## Changes

### The page

Scanner has a fourth tab, Aliases. It is one card that fills the screen.

- **The system** is picked in the card's head: the live site's system first.
- **Talkgroups, Radios, Import** choose what the card shows. An alias that names both a talkgroup
  and a radio is in both lists.
- **The table,** a row an alias:
  - its colour (pale while it is only the one its number gives it), name, numbers and group;
  - for a talkgroup's alias its priority, its tuner (Any, 1, 2), its Rec mark and its Listen
    switch, as on Now;
  - a bin to remove it, which asks first.
- **Numbers are text:** `300, 310-319` is a talkgroup and a range. An ID of the other list is
  marked: `radio 1014` in the talkgroups' list, `tg 300` in the radios'.
- **A field is edited in its row and saved at once.** Enter keeps what was typed, Escape drops
  it. What the page or the radio refuses is said, and the field goes back.
- **Find** by name, group or number (a number inside a range finds its alias); **sorted** by
  name, number, group or priority; **one group** shown. 200 rows are drawn at once, with Show
  more.
- **The picked rows together:** their group, priority, tuner, Record, Listen or Skip, colour, or
  removed (asked first). The tick in the heading picks every row the filters leave.
- **A new alias** from the foot: a name and its numbers.
- **Heard, not named:** what the history heard at the system's sites in the last week that no
  alias names, the longest on the air first, each with its calls and when it was last heard. A
  name typed beside one makes its alias, and the next one's field takes the focus. The history is
  read when this list is opened and on "Read again", not by itself.
- **The system's listening settings,** over the talkgroups' list: talkgroups with no priority
  followed or not, their tuner, and whether a higher priority takes a tuner from a lower one. The
  same strip says what decides recording, since the Rec marks decide only under Marked.
- **Where only talkgroups with a priority are followed,** a row's Listen switch shows what the
  follower does (off for an alias with no priority), and switching it on gives the alias the
  lowest priority, as `follow` does for one talkgroup.

### The import

Import takes either of RadioReference's two files for a trunked system; the radio tells them
apart by their header.

- **A talkgroups file:** how many talkgroups would become aliases and how many already have one,
  a switch for "never follow the fully encrypted ones", the first 200 as a table, then the
  button that adds them.
- **A sites file:** each site it would add or give channels to, with a tick, and the rows left
  out with why. The button imports the ticked ones.
- Nothing changes before the button. A file the radio cannot read is refused with its reason.

### The radio

- **The alias list has a revision.** `GET …/aliases` gives `rev`. `PUT …/aliases?rev=` replaces
  only the list that was read at that revision, and answers 409 when it changed since. The page
  then says so, reads the list again, and does not make its change. Without `rev` the route
  replaces the list as before.
- **A list names a talkgroup or a radio by itself once.** The first alias to name one is the one
  read for it, so a second would hold settings nothing uses. `PUT …/aliases` refuses such a
  list and says which two aliases; a configuration document with one is refused on import, and
  one found in the stored configuration is reported in the log at start.
- **`PUT …/talkgroups/{tg}`:**
  - takes `color` (`#rrggbb`, or null for none);
  - a talkgroup that an alias names among others now leaves that alias, and its own alias takes
    that place in the list. Before, the edit made an alias at the list's end, answered 200, and
    the follower went on reading the earlier one (095's review).
- **A colour is `#rrggbb`** wherever an alias is checked.
- **Now's list is bounded** (the roadmap's Q4): the talkgroups heard at the site in the last week
  or among the calls listed now, and those an alias gives a priority, a tuner or a Rec mark.
  Before, every talkgroup an alias named was listed, which an import of hundreds would fill.

## Decisions taken

- **Q4, with one difference from the roadmap's recommendation.** It counted "switched off" as a
  setting that lists a talkgroup. Built: switched off alone does not, because the RadioReference
  import switches off every fully encrypted talkgroup, and those would fill Now's "Switched off"
  group. A talkgroup switched off is listed while it is heard (within a week, or a call listed
  now) and is always on Aliases. On unit A that removes from Now any of its eight switched-off
  talkgroups not heard for a week. Andy's to change.
- **No route for one alias by itself, or for many.** The roadmap listed both as what step 3 needs
  in the API. The page does one, many and radios with the guarded replace of the whole list, so
  nothing needs them yet. The cost is the whole list sent for each edit (the status log has the
  size and the time on unit A).
- **Listen in a system that follows only talkgroups with a priority** gives the lowest priority,
  in the page, as the radio's `follow` does for one talkgroup. It is the one piece of the
  follower's rule the page holds: a whole-list replace has no `follow`.
- **A new talkgroup alias takes the tuner its talkgroup had** with no alias (the system's
  `unmonitored_speaker`), so naming a talkgroup does not move it.

## Not in this change

- **The SDRTrunk playlist's import and export,** the other half of step 3. It needs routes of its
  own and a playlist of Andy's to read against.
- A system or a site added by hand; the first-load welcome.
- The rest of 095's review placed in this step: a call by its id, Activity's one loader,
  Systems' own copy of the systems, the scan's cards repeating the daemon's rule.
- An alias's icon (kept for SDRTrunk's round trip, shown nowhere).
- The changed alias pushed on `/ws/live`: the pages still read every system after one edit.

## Checks

See the status log for the counts.

- **Host tests:** the revision moving with the list; a talkgroup named among others getting its
  own alias, which is the one read; a colour checked; a list naming an ID twice refused; Now's
  list by its new rule.
- **`runs/ui_check/aliases_page.mjs`** (new), in headless Chrome:
  - on the PC daemon, with the history's reads from unit A: the page opening without asking the
    history anything; a new alias and each of its fields; the refusals (a priority of 0, an
    emptied name, numbers that are not numbers, a number another alias names); a radio among an
    alias's numbers; Find, the sorts and the group filter; the listening settings and Listen
    where only talkgroups with a priority are followed; the picked rows changed and removed
    together, asked first; a list changed elsewhere not written over; a heard talkgroup named in
    place; both of RadioReference's files previewed and the talkgroups file imported; the
    system's aliases and settings put back as they were;
  - with a unit's reads: the list, a sort, what was heard and has no name in both lists, and
    nothing written.
- **`runs/ui_check/now_rail.mjs`** follows the list's rule: a talkgroup with only a name is not
  listed, one given a priority is, and one whose settings are taken away leaves the list.
- **The other pages,** unchanged but for the Scanner's fourth tab: `scanner_pages.mjs`,
  `tool_pages.mjs`, `tv_watch.mjs`; `modules_check.mjs`.

## Status log

- 2026-11-11: built. Host tests 529 pass (3 new); the DMR reference's tests pass (102, which hold
  its floor of 24,984 matching lines); the ARM check and the ARM release build are clean.
  BUILD_TAG `2026-11-11-aliases1`. `modules_check.mjs` 38 modules.
- 2026-11-11: headless Chrome on the PC daemon, the history's reads from unit A:
  - `aliases_page.mjs` 33 steps at 1920x1080 and the same 33 at 1280x800;
  - `now_rail.mjs` 29 steps with the writes;
  - `scanner_pages.mjs` 40 steps. In the first of two runs one step failed (a 1.3 s recording
    read at 0 s when its player was looked at); it passed in the second. Activity is not
    touched by this change;
  - `tool_pages.mjs` 15, `tv_watch.mjs` 12.
- 2026-11-11: `2026-11-11-aliases1` on unit A (binary only, in RAM over the SD image
  `2026-11-10-pages2`, which does not have it: a reboot of A returns to that image):
  - live on Clay County within 30 s; nothing in the log but its start line;
  - `GET /api/v1/talkgroups` gives 42 as before: each of A's eight switched-off talkgroups was
    heard within the week, so the new rule drops none today;
  - with A serving the page, reads only: `aliases_page.mjs` 9 steps (its 8 aliases; 34
    talkgroups heard with no alias; the 1,000 radios read, none named) and `now_rail.mjs` 15;
  - **the page's writes on A** (`aliases_page.mjs --keep-listening`): 31 steps, and A's aliases
    at the same revision after as before. The listening settings were not changed there, since
    that stops A following for the seconds they are changed;
  - `scanner/doc/API_FIELDS.md` regenerated from A.
- 2026-11-11: **a whole-list write, measured on unit A** with 508 aliases (its eight and 500
  made for the measurement, put back after):
  - the list is 94 KB as sent and 184 KB in `systems.json`; the flash (896 KB) had 300 KB used
    before and 288 KB with it, since JFFS2 compresses;
  - replacing it took 0.33 s the first time and 0.10 to 0.11 s after; reading it 12 ms; a stale
    `rev` got 409;
  - `GET /api/v1/talkgroups` stayed at 42 talkgroups and 8.3 KB with the 500 names added, which
    is what the list's rule is for;
  - in the browser (PC daemon), 508 aliases: 200 rows drawn, Show more in 36 ms, Find in 10 ms,
    every row picked in 6 ms.
- **Found by the measurement, not settled:** the save runs inside its handler, on the two
  workers that also run trunking and live audio (095's review measured 14 to 18 ms for unit A's
  eight aliases). At 0.1 to 0.3 s for 500 aliases an edit holds a worker that long. Moving the
  save off the workers, or writing one alias by itself, is placed in the roadmap for before a
  system's list reaches thousands.
- **Not checked:**
  - a file as RadioReference itself writes it: the check's two files are written to its headers,
    and no `trs_*.csv` is under `C:\Users\Andy\Projects`;
  - a sites file imported through the page (it is previewed; the import is the daemon's tested
    route);
  - the listening settings changed on unit A (changed on the PC daemon);
  - the colour picker's own dialog (the check sets the field's value), and the page at a phone's
    width;
  - Now's emptied-name path that removes an alias, which needs a talkgroup the radio has heard
    (it was checked in 096, when a name alone listed a talkgroup).
