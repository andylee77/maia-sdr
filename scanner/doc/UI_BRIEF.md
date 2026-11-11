# UI brief: the replacement UI

The scanner's web UI is replaced from scratch. This brief holds Andy's requirements (2026-10-01)
and the decisions taken with him. Every page there is has the new layout (changes 090 to 094),
and the Aliases page is in (097). Not built: the first-load welcome; what the other sections ask
for and no page has yet is marked "Still to come" in them.

- `doc/API_INVENTORY.md` lists each route: what it carries, what it lacks, and who uses it.
- `doc/API_FIELDS.md` gives every field of every response with its meaning.

## Decisions

| Question | Decision |
|----------|----------|
| Order | Backend first (testable through the API), then the new UI; the Now page first for Andy's review. |
| Talkgroups and radios | SDRTrunk's alias model replaces profiles: per system an alias list (name, group, color, talkgroup and radio IDs and ranges, priority or do-not-monitor, record) plus the left/right speaker. Edited from the live screen. |
| SDRTrunk | Playlists import (systems, sites, control channels, aliases) and export, so the scanner and SDRTrunk stay in step. |
| RadioReference | At least a basic import of its CSV downloads (Andy saved them while subscribed): a system's talkgroups as aliases, its sites as sites. |
| The two traffic receivers | The radio has a control tuner and two traffic channels: "Traffic 1" and "Traffic 2" in the UI (SDRTrunk's word; the API's `lane`). Andy may prefer "Channel 1/2"; "channel" also names RF channels, so that is open. |
| Updates | Everything on a page comes from the radio as it happens, over one WebSocket. No page polls. |
| Screen | 1920x1080 is the design size; a mobile layout comes later. |
| Modes | Each unit mode has its own tabs, its own Diagnostics and Settings among them (change 100), and the radio's own Diagnostics, Files (change 101) and Settings are shared: scanner (Now, Activity, Systems, Aliases), ATSC TV (Watch, Channels; changes 080, 081 and 091), data (Scan, Devices, Captures; `doc/DATA_MODE.md`). |
| Shell | A module rail on the left (Andy, 2026-11-10, from three drawn options; change 090): Scanner and TV at the top, the radio's own Diagnostics, Files and Settings at the bottom, the viewed module's pages as tabs in the header, its own Diagnostics and Settings after them (Andy, 2026-11-11: "Yes, module tabs"; change 100). A module's pages stay readable while another has the radio; a notice on them offers the switch. A new module is an entry in `js/modules.js`. |
| Hold | Not on every row (Andy, 2026-11-10): Hold is on the tuner's card of the call on the air and in the selected talkgroup's bar, Release there and in the site bar. |
| Now | Layout A, "tuners first" (Andy, 2026-11-11, from three drawn options; change 096): the site bar, the two tuners, the talkgroups with one selected and its controls in a bar under them, the calls with the picked call's player docked under them. Sound follows the tuner (Traffic 1 left, Traffic 2 right), and what is recorded is Off, Marked or Every call; both as drawn, and Andy's to change. |
| TV | Watch, then Channels (change 091). Watch cycles three views (Andy, 2026-11-10): small (the channels, the picture, the signal), large (the picture across the page, the channels a strip below) and full screen with nothing over the picture. Play and Scan take the radio themselves, after asking. |

## Backend work before the UI

In this order; each item is usable through the API on its own.

1. **Realtime push** (done: `/ws/live`). One WebSocket carries a snapshot on connect, then each change:
   - the live state;
   - the control channel's health;
   - both traffic channels (call, talkgroup, radio, voice);
   - calls opening and closing;
   - recordings;
   - the scan's progress and each control channel as it is found;
   - settings, systems and aliases changing;
   - the hold.
2. **Aliases** (done: `/api/v1/systems/{id}/aliases`, D15) replace the talkgroup and radio name
   maps and the profiles:
   - The follower follows by alias: monitor priority, do-not-monitor, and speaker.
   - Recordings follow each alias's record flag.
   - Nothing migrates: a unit starts with no aliases.

   The model is SDRTrunk's, so a playlist round-trips. Each system has one alias list; an alias
   has:

   | Field | Meaning |
   |-------|---------|
   | `name` | Shown wherever its talkgroup or radio appears. |
   | `group` | A free label to sort and filter by (SDRTrunk's alias group). |
   | `color` | Optional. |
   | `ids` | Talkgroups, talkgroup ranges, radios, radio ranges. |
   | `priority` | 1 (highest) to 100 (lowest), or "do not monitor" (SDRTrunk's -1). A higher priority takes a traffic channel from a lower one. |
   | `record` | Its calls are recorded. |
   | `speaker` | Its traffic tuner: Traffic 1 (`left`), Traffic 2 (`right`) or either (`both`). Ours; SDRTrunk has none. |

   Per system:

   - **Talkgroups with no alias:** followed at the lowest priority, or not at all. SDRTrunk calls
     the second "ignore unmonitored calls".
   - **Their traffic tuner:** Traffic 1, Traffic 2 or either.
   - **Pre-emption:** whether priority pre-empts.

   Recording keeps a "record every followed call" switch beside the per-alias flag. The hold
   stays as it is.
3. **SDRTrunk playlist import and export** (done: `/api/v1/playlist` and its `/preview`,
   change 098):
   - systems, sites, control channels (SDRTrunk's channels), the alias lists and their IDs;
   - priority, record, group and color.

   **RadioReference CSV import** (done: `/api/v1/systems/{id}/radioreference` and its
   `/preview`; `DESIGN.md` section 3.4): talkgroups as aliases, sites as sites.
4. **Manual add and edits:**
   - create a system (protocol, identity, name) and its sites (control channel and alternates, the DMR LCN plan);
   - edit a system's name, identity and details, and a site's identity (done: `PUT /api/v1/systems/{id}`, the site editor).

## Pages

### First load: welcome and radio check

On a unit with no systems, "Welcome to Fishball scanner" and a radio check:

- the hardware, the gateware version, the AD9361;
- temperatures;
- the crystal correction;
- the network.

Then three ways in: scan for systems, add one by hand, or import an SDRTrunk playlist.

### Now (home)

p25-httpd's Now page was the better starting point (`p25-httpd/src/httpd/ui/js/views/now.js`, in git history since p25-httpd left the repo).
Done (change 096, from the layout Andy picked of three drawn; 090 had laid it out before): the
site bar (one list of every site by system, whether the control channel decodes, the radio's
hold, what is recorded: Off, Marked or Every call), the two tuners side by side ("Traffic 1" and
"Traffic 2": the call in large type, Hold, the talkgroup the tuner is kept on, its last calls to
play again, this browser's volume and mute for it), the talkgroups (those listened to, those
switched off, those heard only encrypted; a row has its tuner, its Rec mark and its Listen
switch; one is selected, by a click or the arrow keys, and a bar holds its controls: Hold, Skip,
Record, tuner, priority, Rename), and the calls with the picked call's player docked under them.
Everything on it is pushed, the talkgroups with how each is treated included. The header keeps
Listen across reloads, the volume, Left / Right (Traffic 1 on the left speaker, Traffic 2 on the
right), the leveller and "Alerts only". The control channel's counters are on the Scanner's Diagnostics.
Still to come: an alias's colour and group, range aliases as rows of their own, and the day's
totals in the calls' heading.

- **Locked top:** the header, the system card and the active channels stay on screen. Only the
  call history scrolls, in its own pane.
- **System card at the top:**
  - a system selector, then a site selector;
  - the system's and site's details;
  - the control channel's health.
  - With no systems, a scan button, or a link to the scan.
- **Dual traffic channels:** Traffic 1 and Traffic 2 side by side, each showing:
  - the call, the talkgroup and its alias, the radio and its alias;
  - how long it has been on;
  - left/right audio;
  - hold.
- **Call history** for the selected site, laid out like p25-httpd's calls list
  (`components/calls_list.js`):
  - time;
  - talkgroup and alias;
  - radios;
  - voice and time on air;
  - inline playback.
- **Talkgroup controls on the live screen,** like the front of a scanner or a trunking radio:
  - which talkgroups to monitor;
  - which to record;
  - which has priority;
  - left or right (p25-httpd's speakers panel, `components/speakers_panel.js`, did left and right).
- **Dual audio:** left and right.

### Systems

Done in change 093 as far as the routes go: the scan and the configured systems in a list on the
left, the picked one on the right (a system edited in place, or the scan). The SDRTrunk playlist
is the list's second entry (098), and the RadioReference import is on the Aliases page (097).
Still to come: adding by hand.

- Systems, then their sites:
  - edit;
  - stop the live site;
  - delete.
- **Scan** (done on the current Systems page; Andy: "a nice simple clean setup, not some form"):
  - before it: the bands to scan (ticks, plus a range of one's own) and its settings (spectrum
    frames a window, time on each carrier, the wait for a site's identity, the most carriers);
  - during it: the band, the window and what it is doing, and each system as it is found;
  - after it: a compact card per found system in RadioReference's layout (name, location,
    county, type, voice, Sysid and WACN; a table of its sites with RFSS, site, NAC, control
    channel, the others it announced, reception), every value edited in place, then "Add".
- **Configured systems** in the same card, edited in place with Save; traffic channels show the
  configured ones and those heard on the air.
- **Add by hand** (decision above).
- **Import and export** an SDRTrunk playlist (done in 098): the import is shown system by
  system before it is made, with where each goes, its channels as sites and its names.
- **Import RadioReference CSVs** into a system: the preview lists what would be added or kept,
  the user ticks the sites, then imports.

### Aliases

Done in change 097 (`doc/changes/097_aliases_page.md`). A playlist's names come in with its
systems, on Systems (098).

Per system, SDRTrunk's alias list as a table, the talkgroups' aliases and the radios' apart:

- columns: colour, name, numbers and ranges, group; for a talkgroup its priority, its tuner (the
  alias's `speaker`), its Rec mark and its Listen switch;
- each field edited in its row and saved at once; the picked rows changed together;
- found by name, group or number; sorted by a column; one group shown;
- what was heard in the last week and has no alias, each named in place;
- the system's listening settings (talkgroups with no priority, pre-emption);
- RadioReference's files imported, each after a preview of what it would change.

### Activity, Diagnostics, Settings

- **Activity:** the history views. Done in change 093, filling the screen: who talks (the
  talkgroups or the radios, one picked narrowing the page to it), the totals with the airtime
  chart, the calls, the alerts or the packet data, and the picked call with its recording.
- **Diagnostics:** the receivers, the event log and the spectrum, filling the screen (change
  094), and since change 100 a page for each part, each with its own lines of the event log:
  - **the Scanner's,** among its tabs: the spectrum, its events (with the carriers heard behind
    the same tabs), and the control channel, the receive window and the tuning beside them;
  - **TV's,** among its tabs: its events, and the 8-VSB receiver beside them (its state, the
    block's registers, its loops, what it decoded and lost);
  - **the radio's own,** in the rail: what the radio did (its start, clock, crystal, each change
    of mode), the API's routes, the radio and the board.

  Still to come: eye and IQ plots for every received signal.
- **Settings:** two columns of cards, a setting a row (change 094), and since change 100 a page
  for each part. A card saves its own part and shows a change made elsewhere at once:
  - **the Scanner's,** among its tabs: gain, the window presets, the call timings, the history,
    recording (Off, Marked or Every call, as on Now; where and how long), packet data;
  - **the radio's own,** in the rail: the clock, the crystal, and the configuration as a file
    (export, import, factory reset).

- **Signing in** (change 102): a radio with a password shows its sign-in page first and nothing
  else; the header then says who is signed in (Administrator or Listener), with Sign out. The
  radio's Settings begins with **Access**: on an open radio the first password; to the
  administrator the passwords, the tokens for tools, the browsers signed in, the names the radio
  answers to, and opening it again. A listener sees the pages and changes nothing: the radio
  refuses and the page says whose it is.
- **Files** (change 101, the radio's own, in the rail): what is on the card, to take away. The
  recordings as one ZIP archive of a selection (dates, a site, a talkgroup, a radio, only the
  locked ones), shown with what it takes before its download, or of one day from the table; the
  history's database; each TV recording. One download at a time, shown across the top while it
  runs.

TV's Channels page (the TV scan's table with the picked channel beside it) is in change 094 too.

### Later: a system notebook

Andy's idea (2026-10-03), not built: a wiki or scratch pad per system, in Markdown, with
collapsible sections and an index. It holds what a listener needs at hand, each kind of page
from its own template:

- the system's details and its talkgroups' meanings;
- its 10-codes, signals and plain-language codes;
- station information and a station map;
- emergency procedures;
- the user's own notes.

Kept on the radio with the system's configuration, so every browser sees the same notes, and
exported and imported with it.
