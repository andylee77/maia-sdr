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
   - The present names and profiles migrate into aliases.
3. **SDRTrunk playlist import and export:**
   - systems, sites, control channels (SDRTrunk's channels), the alias lists and their IDs;
   - priority, record, group and color.
4. **Manual add and edits:**
   - create a system (protocol, identity, name) and its sites (control channel and alternates, the DMR LCN plan);
   - rename a system and edit its identity.

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
- **Scan:**
  - each control channel appears as it is found, grouped by system, with its identity, signal and decode rate;
  - the user picks what to add and names it;
  - what is saved is readable.
- **Add by hand** (decision above).
- **Import and export** an SDRTrunk playlist.

### Aliases

Per system, SDRTrunk's alias list as a table:

- columns: name, group, color, IDs and ranges, priority, record, speaker;
- search and bulk edit.

### Activity, Diagnostics, Settings

- **Activity:** the history views.
- **Diagnostics:** the receivers, the event log and the spectrum. Eye and IQ plots come later
  for every received signal.
- **Settings:** the radio settings, and the configuration (export, import, factory reset).
