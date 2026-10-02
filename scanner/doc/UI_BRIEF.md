# UI brief: the replacement UI

The scanner's web UI is replaced from scratch. This brief holds Andy's requirements (2026-10-01)
and the decisions taken with him. The current pages stay only until the new ones replace them.

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
2. **Aliases** replace the talkgroup and radio name maps and the profiles:
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
   | `speaker` | Left, right or both (ours; SDRTrunk has none). |

   Per system:

   - **Talkgroups with no alias:** followed at the lowest priority, or not at all. SDRTrunk calls
     the second "ignore unmonitored calls".
   - **Where they play:** left, right or both.
   - **Pre-emption:** whether priority pre-empts.

   Recording keeps a "record every followed call" switch beside the per-alias flag. The hold
   stays as it is.
3. **SDRTrunk playlist import and export:**
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

p25-httpd's Now page was the better starting point (`p25-httpd/src/httpd/ui/js/views/now.js`).
Done: the locked top, the system card with its two lists (picking a site makes it live), the
traffic channels as "Left · Traffic 1" and "Right · Traffic 2", each with a talkgroup picker
that holds that channel alone (a lane the site does not run says so), and the call history with
playback. Still to come: the talkgroup controls (monitor,
record, priority, speaker).

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
- **Import and export** an SDRTrunk playlist.
- **Import RadioReference CSVs** into a system: the preview lists what would be added or kept,
  the user ticks the sites, then imports.

### Aliases

Per system, SDRTrunk's alias list as a table:

- columns: name, group, color, IDs and ranges, priority, record, speaker;
- search and bulk edit.

### Activity, Diagnostics, Settings

- **Activity:** the history views.
- **Diagnostics:** the receivers, the event log and the spectrum. Eye and IQ plots come later
  for every received signal.
- **Settings:** the radio settings, and the configuration (export, import, factory reset).
