# 098 — SDRTrunk's playlist, in and out

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-11-playlist1`) over the SD
image `2026-11-10-pages2`, which does not have it. Andy's playlist is not imported on A: the
preview was run there, and the import is his to press.

## Why

The rest of the roadmap's step 3. The brief decided it on 2026-10-01: "Playlists import
(systems, sites, control channels, aliases) and export, so the scanner and SDRTrunk stay in
step." Andy picked it as the next work on 2026-11-11 ("SDRTrunk playlist").

Andy's own playlists are on this PC, in `C:\Users\Andy\SDRTrunk\playlist\`:

- `default.xml`: 69 channels of 9 systems and 1,019 aliases in 7 lists, as SDRTrunk writes it;
- `playlist.xml`: one channel and 22 aliases, in the older spelling (`xsi:type`).

Unit A has six systems a scan found, and not one name. Every name he has given a talkgroup is in
the first file.

## What a playlist is, and what that means for an import

Read from SDRTrunk's source (`playlist/PlaylistV2.java`, `controller/channel/Channel.java`,
`alias/Alias.java` and the classes they hold) and from his files:

- **A channel is a frequency to decode,** or several that SDRTrunk rotates through until one
  decodes, under a system name and a site name, with the name of the alias list it uses. Nothing
  says which channel is a control channel. His file has both habits: one channel a site, with
  the control channel's candidates as its rotation ("Control"); and one channel a frequency
  ("LCN 1" to "LCN 28" at one site, the inputs among them).
- **A site name is a label.** Two channels named for one county are two sites when their
  frequencies differ.
- **An alias belongs to a list** and holds its talkgroups, radios, ranges, a priority (-1 is
  "do not monitor") and a Record mark as IDs. A list is tied to a system only by the channels
  that name it; two of his systems share one list.
- **A system has no identity** in a playlist (no WACN, system ID or NAC).

So an import cannot be "the file's systems, copied". It is built as follows.

## Changes

### The import

`POST /api/v1/playlist` takes the file's text (`xml`) and, per system of the playlist, what to
take (`systems`: `name`, `into`, `channels`, `aliases`). `POST /api/v1/playlist/preview` answers
the same and saves nothing. With no `systems` the radio answers what it suggests.

- **Where a system goes:** into the configured system that has a site on one of its channels'
  frequencies (a control channel or one that takes over from it, within 3 kHz), the one with the
  most such channels; else into a system of its own. `into` names another, or none.
- **Each channel is a site it may be.** Against the system it goes into, a channel is:
  - `new`: it would be a new site, its first frequency the control channel and the others the
    channels that take over from it; a DMR channel's channel numbers become the site's plan;
  - `gains`: a configured site is on its frequencies, and gets the frequencies and channel
    numbers it lacks;
  - `configured`: a configured site's control channel, with nothing to add;
  - `known`: a traffic channel a configured site lists;
  - `unsupported`: a decoder the scanner has none for (NBFM, AM, P25 Phase 2), or no frequency
    the radio tunes.
- **What is taken unless asked otherwise:** every `gains`, and a `new` channel that rotates
  through several frequencies, that SDRTrunk starts by itself, or that stands alone at its site.
  A lone frequency among several at one site is not: the playlist does not say which is a
  control channel.
- **A system of its own is made once a channel of it is taken,** named as the playlist names
  it. Where its channel says SDRTrunk ignores talkgroups with no priority, so does the system.
- **Its alias lists** (those its channels name) go into the system as an import takes aliases
  (below). A list no channel names is reported and not taken.
- **Only what the scanner keeps:** an alias's talkgroups, radios and ranges of the system's
  protocol, its name, group, colour and icon, a priority of 1 to 100, -1 as "never followed",
  and its Record mark. Streams, tones and the other kinds of ID are left.

### How an import takes aliases

One rule for both imports (`aliases::Intake`), new in this change:

- An ID an alias of the system already has (a talkgroup, a radio, or the same range) stays that
  alias's.
- **A talkgroup held only under its number** ("TG 850": what a control on Now makes of a
  talkgroup with no alias) **takes the file's name,** group, colour and icon, and keeps its own
  settings. Unit A's eight aliases are all of this kind, so without it neither import could name
  them. The RadioReference import does the same now, and its answer says which
  (`aliases_named`).
- What is left of an alias's IDs is added as a new alias. The same file taken twice adds nothing
  the second time.

### The export

`GET /api/v1/playlist` is the configuration as a playlist (`download=true`: as the file
`scanner-playlist.xml`):

- a channel for each site, named "Control": on its control channel, or rotating through it and
  the channels that take over from it; a P25 site's modulation where a scan has settled it; a
  DMR site's channel numbers;
- each system's aliases as a list named after the system (with its id where two systems share a
  name), each with its IDs, its priority and its Record mark.

What a playlist has no place for is left out: a talkgroup's tuner, a site's identity, the
channels a site has learned. An alias both switched off and given a priority is written as
switched off, since SDRTrunk keeps one of the two.

### The page

Systems' list has a second entry under "Find systems": **SDRTrunk playlist.**

- **Export:** the download.
- **Import:** a file is chosen, and the radio's reading of it is shown before anything is saved:
  the file's totals, what importing it as ticked would add, and a card for each of its systems
  with where it goes (a list of the configured systems of its protocol, or a new one), a tick
  for its names with what they would add, and its channels, each with a tick. The channels not
  offered are folded away with the reason for each.
- A changed tick or target asks the radio again. Import does what is shown.

The Aliases page's Import points here for a playlist's names.

### Read as SDRTrunk reads it

`tools/sdrtrunk_playlist_reference.py` reads a playlist twice, with SDRTrunk's own reader (a
small harness on SDRTrunk's classes, set up as its `PlaylistManager` sets it up) and with the
scanner's, each writing it as lines of text, and compares the lines. With `--unit` it does the
same to the playlist a scanner exports: that SDRTrunk reads it, and reads what the scanner
wrote. It is the check for this module, as the DMR reference is for the decoder.

The scanner's reader is its own (`sdrtrunk/xml.rs`, some 200 lines): the daemon has no XML
library, and a playlist needs elements, attributes and text. A document type is refused.

## Decisions taken

- **A channel is a site candidate, not a (system, site name) pair.** His file has two or three
  sites under one county's name in four places.
- **Nothing is folded into a site that the file does not say belongs to it.** The other lone
  frequencies at a site are not taken as its known channels: at Clay half of his are input
  frequencies.
- **One place for it, on Systems.** A playlist spans systems, so its import is not on the
  Aliases page, which is one system's.
- **The request names channels by their number in the file.** Names repeat ("Control" at "Clay"
  three times in one system).

## Found, for Andy: Now's list after his playlist

097 bounded Now's list to the talkgroups heard in the week and those an alias gives a priority,
a tuner or a Rec mark (the roadmap's Q4). His playlist carries Record marks on 274 aliases: 60
of Duval's, all 118 of Alachua's, all 73 of FPL's. Imported, each of those lists a talkgroup on
Now whether or not it is ever heard there. At Clay it is ten talkgroups, most of them heard
anyway; on Duval it could be sixty.

Not changed here. The roadmap's Q4 now recommends the list be what was heard and nothing else,
with every setting on Aliases; that also needs Now's check to run against a daemon that has
heard something, which a PC daemon has not.

## Not in this change

- **The import run on unit A.** It would add systems, sites and over a thousand names to the
  unit in use (the status log has what its preview says). The import is Andy's to press.
- A playlist's channel maps, streams and tuner choices; the identities a playlist does not have.
- P25 Phase 2 channels, and the DMR that is not Tier III: a DMR channel is taken as Tier III.
- A system or a site added by hand; the first-load welcome.

## Checks

See the status log for the counts.

- **Host tests:** the XML reader (a document, what is refused, written text read back); a
  playlist read in both spellings; an alias taken for its system's protocol; what a preview
  suggests for a unit that has the system; what a request takes; the export read back and taken
  into an empty unit; two systems of one name; the intake's rule; the RadioReference import
  naming a talkgroup held under its number.
- **`tools/sdrtrunk_playlist_reference.py`** on both of Andy's playlists, and on the playlist a
  daemon exports after importing the larger one.
- **`runs/ui_check/playlist_page.mjs`** (new), in headless Chrome on the PC daemon: the list's
  entry and the pane; the export's counts; a file that is no playlist refused; a playlist made
  for the check previewed (where each system goes, what is ticked, what is folded and why);
  nothing saved by looking; a folded channel ticked; a system sent to a new one and back; its
  names unticked; the import; what it made, in the API; all of it taken out again.
- **`runs/ui_check/aliases_page.mjs`** has the RadioReference file name one talkgroup and keep
  another; `scanner_pages.mjs` picks its "another system" past the new entry.

## Status log

- 2026-11-11: built. Host tests 540 pass (11 new); the DMR reference's tests pass (102, holding
  its floor of 24,984 lines); the ARM check and the ARM release build are clean. BUILD_TAG
  `2026-11-11-playlist1`. `modules_check.mjs` 39 modules.
- 2026-11-11: **read as SDRTrunk reads it** (`tools/sdrtrunk_playlist_reference.py`, SDRTrunk at
  cdc590d8 with a JDK 25):
  - `default.xml`: SDRTrunk reads 69 channels and 1,019 aliases; the scanner's 1,088 lines are
    the same 1,088;
  - `playlist.xml` (the older spelling): 1 channel and 22 aliases; 23 of 23 lines the same.
- 2026-11-11: **his larger playlist imported whole into the PC daemon,** which holds unit A's
  six systems, as the radio suggests it:
  - every one of the six is found by its frequencies (Clay County, Duval, St. Johns, Clay
    Electric, and FPL and SLERS, which a scan had named "P25 system 00A" and "141");
  - three systems are new (Alachua County, Putnam County, PSIC), six sites are new, eight sites
    gain frequencies, and 1,183 aliases are added; the list two of his systems share goes into
    both;
  - the preview took 36 ms and the import 50 ms there; `systems.json` is 419 KB after it;
  - **the daemon's export after it** is 238 KB: SDRTrunk reads 21 channels and 1,184 aliases
    from it, and all 1,205 lines are what the scanner wrote;
  - the same playlist imported a second time adds nothing, and each of its systems is then
    found as the system it made.
  The PC daemon's systems were put back as they were.
- 2026-11-11: headless Chrome on the PC daemon: `playlist_page.mjs` 16 steps at 1920x1080 and at
  1280x800; `aliases_page.mjs` 34 (the history's reads from unit A); `now_rail.mjs` 29 with the
  writes and 15 with A's reads; `scanner_pages.mjs` 40; `tool_pages.mjs` 15; `tv_watch.mjs` 12.
- 2026-11-11: `2026-11-11-playlist1` on unit A (binary only, in RAM over the SD image
  `2026-11-10-pages2`, which does not have it: a reboot of A returns to that image's pages):
  - live on Clay County within 31 s; nothing in the log but its start line; recordings go on;
  - **his playlist previewed on A, nothing saved:** 0.10 to 0.12 s. It would add three systems
    and six sites, give eight sites frequencies, add 1,179 aliases and name four of A's eight
    talkgroups held under their numbers. A's systems and aliases are as they were (the same
    revision);
  - **A's own export** (its 15 sites and 8 aliases, 10 KB): SDRTrunk reads 15 channels and 8
    aliases, 23 of 23 lines the same;
  - with A serving the page: `playlist_page.mjs --preview-only` 13 steps (the check's playlist
    previewed, its ticks and targets changed, nothing saved), `aliases_page.mjs` reads 9,
    `now_rail.mjs` reads 15;
  - `scanner/doc/API_FIELDS.md` regenerated from A.
- **Not checked:**
  - the exported playlist opened in SDRTrunk's own window and a channel started from it: its
    reader reads the file, which is what the harness runs; the application was not run;
  - the import on unit A, and so the live site going live again after it gains a frequency
    (the route does what the RadioReference import does there);
  - a playlist from another SDRTrunk version or another person: both files are Andy's, written
    by his build;
  - a playlist with numeric priorities (his has only "do not monitor"): the host tests cover
    one, his file does not;
  - the RadioReference pane's "would take its name" on a file as RadioReference writes it.
