# API inventory

What each route carries, who uses it, and what is missing or wrong. The rows were checked against
`API.md` on 2026-10-04; the "Missing or wrong" notes date from `5d7ff96` unless a row is newer.

- `doc/API.md` is the route table, generated from the code.
- `doc/API_FIELDS.md` gives every GET route's fields with their types, meanings and an example,
  generated from a unit by `tools/api_fields.py`.
- `doc/UI_BRIEF.md` is the brief for the final UI.

The cleanup list at the end ranks the backend work this inventory turned up.

## Conventions

- **Errors** are `{"ok": false, "error": "..."}` with a status. A write from a page another host served gets 403.
- **Lane numbers are split.** REST lanes are 1-based; `/ws/audio` lanes are 0-based.
- **Protocol has two spellings.** A system's `protocol` is `p25` or `dmr_tier3`. Heard identities and counters are tagged `p25` or `dmr`.
- **Empty keys are left out.** Many optional keys are omitted when empty, not sent as null:
  - a site's `alternates_hz`, `channels_hz`, `notes`, `channel_plan`, `lcn`, `timeslot`;
  - the control status's `identity`, `modulation`, `tsbks_20s`, `carrier_offset_hz`;
  - a recording's `lane`, `freq_hz`, `channel`, `voice`.
- **Tuning** (`preset`, `sample_rate_hz`, `lo_hz`, `lo_shift_hz`, `crystal_ppm`, `control_hz`, `gain`, `lanes[2]`, `rev`) appears in six responses.

## Live state

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/status` | Build, uptime, board time; the live state (`no_site`, `switching`, `scanning`, `live` with site, system, window and tuning); the mode; the control channel's health; the lease; tuning; clock; the held talkgroup | UI (after an action; otherwise each page gets it pushed by `/ws/live` each second); `scanner_live_check.py` | Tuning is there twice. When the site went live isn't exposed |
| `GET /api/v1/receivers` | The control channel's status, carrier loop and AGC, and both P25 demodulators' (or the DMR decoder's) counters; each lane's channel, call, followed talkgroup, data-channel park, voice frames, decoder counters and carrier loop | Nothing yet (for the diagnostics) | Repeats `status.control`. No modulation override. No reset of the counters |
| `GET /api/v1/system` | Board load, memory, CPU per core and per scanner thread, AD9361 and Zynq temperatures | Nothing yet | No filesystem space; the card's free space is only in `/recordings` |
| `GET /api/v1/spectrum` | The receive window's spectrum (`bins`) with the LO, rate, control channel and lanes | Tools (the UI has it pushed on `/ws/live`) | No frame time. A `bins` that doesn't divide 4096 isn't refused |
| `GET /api/v1/events` | The event log after `after` (`limit`, `routine`) | Tools (the UI's events box has it pushed on `/ws/live`) | Any extra query key is a 400. More than `limit` new events leaves a silent gap. No filter by class, talkgroup or unit |
| `GET /api/v1/survey` | The carriers heard in the live window over ten minutes: duty, peak over the floor, steady or not | Tools (the UI has it pushed on `/ws/live`) | No frame time (079 item 6) |
| `GET /api/v1/iq/control.wav` | The next seconds of the control channel's IQ, 50 kSPS stereo WAV | Captures for the SDRTrunk references | Control channel only: no lane or wideband IQ (data mode §6) |
| `WS /ws/events` | `call_opened`, `call_closed`, `recording_saved`, alert tones, `lag` | Nothing in the UI (it uses `/ws/live`) | No notice for a site switch, a scan, a recentre, a clock or crystal step, or a settings change. No site in notices |

## Calls, audio, recordings

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/calls` | The live site's open calls and its newest 100 closed ones, each with talkgroup and radio names, radios, channel, slot, lane, emergency, private, times (grant, first voice, end, open span, grant span), close reason, voice frames, codec, why not followed | UI Now | An open call that isn't followed shows no voice and no radios. Frame errors are known only once a call is stored |
| `GET /api/v1/calls/{id}` | One call in the same shape, live while recent, else from the history | Nothing | — |
| `GET /api/v1/talkgroups` | The live site's talkgroups (those an alias names, and those heard in the last week or among the calls listed now): each one's name, how it is treated by the follower's rule (followed, its lane, priority, recorded), its calls in the week and when it was last heard; and what decides recording | UI Now (pushed on `/ws/live` as `talkgroups`) | The live site only; the whole list at each change |
| `GET`/`PUT /api/v1/hold` | The talkgroup the live site is held on, and each traffic lane's own; hold or release either (`lane`) | UI Now: Hold on the tuner's card of the call on the air and in the selected talkgroup's bar, Release there and in the site bar; each tuner's list keeps its lane on one | One route carries two things (the site's hold and a lane's), told apart by `lane` |
| `WS /ws/audio` | 20 ms frames of 8 kHz audio. With `v=2`: every lane, each frame tagged; text `meta` (lane, talkgroup, radio, call, speaker) and `lag` | UI player; bench `wsaudio.py`, corpus, scoring | Lane is 0-based. Meta has no site or encryption flag, and sends `src` 0 for an unknown radio. The listener count and the audio counters (frames, errors, silent, dropped) are kept but never exposed |
| `GET /api/v1/recordings` | Recordings newest first (`limit`, `site`) with the listed site's total and the stores' full state | UI Now and Settings; `scanner_live_check.py` | Recordings from earlier runs lack lane, frequency, channel and voice counts, although the history has them. No paging |
| `GET /api/v1/recordings/{id}` | The WAV (byte ranges) | UI Now player | The whole file is read for every range request |
| `DELETE /api/v1/recordings` | Clear a store (`sd`, `ram`, `all`) | UI Settings | — |
| `DELETE /api/v1/recordings/{id}` | Delete one | Nothing | — |

## History

`site` (default the live site) or `system`, and `from`/`to` or `hours`, apply to every activity route.

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/activity/sites` | Sites with history (calls, first and last), the database's place, size and limits | UI Activity | — |
| `GET /api/v1/activity/summary` | Calls, followed, encrypted, voice and grant time, talkgroups, radios | UI Activity; `scanner_live_check.py` | `tg` and `unit` are ignored here |
| `GET /api/v1/activity/talkgroups` | Talkgroups by time, with names | UI Activity; UI Now's talkgroup bank (the last week, read at most every 5 minutes) | The name is `alias` here and `tg_name` in a call |
| `GET /api/v1/activity/radios` | Radios by time, with names | UI Activity | — |
| `GET /api/v1/activity/radio/{unit}` | A radio's talkgroups and affiliation/registration events | UI Activity | No `limit` |
| `GET /api/v1/activity/talkgroup/{tg}` | A talkgroup's radios and encryption history | UI Activity | — |
| `GET /api/v1/activity/series` | Calls and time per hour or day (`tz`, `tg`, `unit`) | UI Activity chart | — |
| `GET /api/v1/activity/calls` | Calls newest first in `/calls`' shape with names, recordings and alert tones; `format=csv` streams the history rows as a file | UI Activity (its calls; an alert's call, read by its grant time); UI Now (the calls older than the pushed ones) | No call by its id: see "What a code review may change" |
| `GET /api/v1/activity/alerts` | The alert tones heard in followed calls: grouped by kind and tones, and the newest each with its call and the call that carried what it announced | UI Activity's alerts | The dispatch has its call's id and grant time, not the call |
| `GET /api/v1/data` | Packet data per site (or all): counts, radios with names, recent PDUs with IP decode | UI Activity's packet data | Memory only: lost on restart. No `system` parameter |

## Radio settings

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/radio` | Presets; the radio configuration; stored state (live site, crystal); hardware; tuning; the radio core's readback (each lane's NCO, packets on or off, tag and counters; the ring; the sample and clip counts) | UI Board card, Settings | — |
| `PUT /api/v1/radio/gain` | Gain mode and manual gain, applied at once | UI Settings | — |
| `PUT /api/v1/radio/settings` | Presets allowed, lanes, call timings, history limits | UI Settings | Lanes apply at the next start, not the next activation. Lane count isn't range-checked |
| `PUT /api/v1/radio/clock` | Clock source | UI Settings | Same as legacy `PUT /api/ui/settings`. `{}` silently sets `site` |
| `POST /api/v1/clock` | Set the board clock | UI Settings | Sits outside `/radio/`, unlike its sibling |
| `GET`/`PUT /api/v1/radio/crystal`, `POST …/calibrate` | Crystal correction: applied, tracked, calibrated | UI Settings Crystal card (then pushed on `/ws/live`) | The stored calibration is only in `/radio` `state.crystal` |
| `PUT /api/v1/radio/recording` | Any of: recording on/off, every call or the marked talkgroups' (`every_call`), store, limits | UI Settings; UI Now's Record (Off, Marked, Every call) | — |
| `PUT /api/v1/radio/packet_data` | Packet data followed or not (`follow`); the live site follows the change at once | UI Settings | New in 090; see "What a code review may change" |

## Systems, sites, aliases, scan

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/systems` | Systems with their sites | UI Systems, Settings | — |
| `GET /api/v1/systems/{id}` | One system | Nothing | — |
| `PUT /api/v1/systems/{id}` | Edit a system: name, identity, details | UI Systems | — |
| `GET`/`PUT /api/v1/systems/{id}/aliases`, `PUT …/listening`, `PUT`/`DELETE …/talkgroups/{tg}` | Aliases (names, priorities, recording, lanes), listening settings, one talkgroup's controls, its alias removed | UI Now's talkgroups (`…/talkgroups/{tg}`: `follow`, its lane as `speaker`, `record`, priority, name; the alias removed when a name is emptied on a talkgroup with no other setting); nothing uses the other two yet | No SDRTrunk playlist import or export (UI_BRIEF). `…/talkgroups/{tg}`: see "What a code review may change" |
| `POST /api/v1/systems/{id}/radioreference`, `…/preview` | Import a RadioReference CSV of talkgroups or sites | Nothing yet | No page offers the import |
| `PUT /api/v1/systems/{system}/sites/{site}` | Edit a site; the live one goes live again | UI Systems | The edit is saved before re-activation, so a failed re-activation still keeps it |
| `DELETE /api/v1/systems/{id}`, `DELETE …/sites/{site}` | Remove a system (with its sites and aliases) or a site; not the live one | UI Systems | No create by hand: sites come only from a scan |
| `GET /api/v1/sites` | Every site, flattened, live one marked | Nothing | Repeats `/systems`; no single-site GET |
| `POST /api/v1/sites/{id}/activate`, `…/stop` | Make a site live; stop it | UI Systems; `scanner_live_check.py` | — |
| `GET /api/v1/sites/{id}/learned` | Band plan, grants per channel, encrypted talkgroups, neighbours, secondary control channels, data channel | UI Systems | Neighbours keep 4 of the 10 decoded fields (no LRA, service class, conventional, failure, valid, active) |
| `GET /api/v1/sites/{id}/plan`, `POST …/recentre` | The receive window against the site's channels; move it now | UI Diagnostics' receive window | `{id}` must be the live site |
| `GET`/`POST /api/v1/scan`, `…/options`, `…/cancel`, `…/add` | Find systems on the air; add the ticked ones | UI Systems (the scan's state itself is pushed on `/ws/live`) | A DMR site added by a scan has no LCN plan. Results are in memory only |

## Mode and ATSC

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET`/`PUT /api/v1/mode` | The unit's mode (`scanner`, `atsc`); change it | UI shell: the notice's switch on a module the radio is not on, and Return to Scanner | The UI's modules are `scanner` and `tv`; `js/modules.js` maps them to the modes |
| `GET`/`POST /api/v1/atsc/live`, `POST …/live/stop` | Live TV: whether the radio core has the 8-VSB block, the channel and the block's statistics; play an RF channel; stop | UI Watch (play and stop; the state itself is pushed on `/ws/live` as `atsc_live` since 091) | Stop has no notion of who else is on the stream: Watch stops the receiver only when the pushed `viewers` is at most 1 |
| `GET`/`POST /api/v1/atsc/scan`, `…/options`, `…/cancel` | The TV scan: each channel's kind, pilot, carrier to noise, clipping, and the station named from PSIP; after a restart, the last scan that finished (092) | UI Channels and Watch; `tools/atsc_check.py` | Nothing says a scan came back from before a restart, or how old the antenna's state is: only its `finished_unix_ms` |
| `GET /api/v1/atsc/scan/channel/{n}` | One RF channel's spectrum from the last TV scan | UI Channels | Not kept across a restart: 404 until the next scan |
| `GET /api/v1/atsc/recordings` | The TV recordings on the SD card: RF channel, size, programmes | UI Watch | — |
| `GET /api/v1/atsc/stream/{rf}` | An RF channel's transport stream as it plays (`program`: one programme) | UI Watch (its picture and sound; its address and a playlist for VLC); VLC and other players | Live from the radio core's 8-VSB block (bake D) when it plays that channel, else its recording |

## Configuration

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/config` | The whole configuration as one document: radio settings, systems with their aliases and sites, live site (`download=true` as a file) | Nothing yet | What sites learned and the crystal calibration aren't included, by design |
| `PUT /api/v1/config` | Import an exported document: checked whole, then the scanner restarts | Nothing yet | — |
| `POST /api/v1/config/factory-reset` | No systems, sites, aliases, recordings or history; default settings; the crystal calibration and TLS certificates stay; the scanner restarts | Nothing yet | — |

## Legacy routes for the bench

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/system` | Build, uptime | fbench units and sys/rf/corpus tests | — |
| `GET /api/ui/state` | Board time, clock valid | corpus | — |
| `GET /api/imbe_dump` | The newest 128 raw IMBE frames | corpus, scoring | No lane, call or time per frame; no v1 equivalent |
| `GET /api/ui/calls` | Calls with `open_ms`, `first_voice_ms` and `imbe` | corpus, scoring | Scoring reads `ldu`, which isn't there. At most about 100 calls |
| `GET`/`PUT /api/ui/settings` | Clock source | corpus (pins `manual`) | Same as `PUT /api/v1/radio/clock` |

Bench routes that p25-httpd served and the scanner does not:

- `/api/traffic` (lane hold, follower off), `/api/monitor`, `/api/decoder_compare`, `/api/stats`, `/api/decoder_reset`: `rf.p25_replay` and corpus mode C need them.
- `/api/dibit_delivery`: optional.

The bench agent's maintenance mode stops `S60scanner` since 078; the agents installed on the units predate it until `fbench setup agent` redeploys them.

## Kept but not reachable

- **Calls:** the control channel's NAC, how the radio was learned, the first HDU time.
- **Neighbours:** the six dropped fields.
- **Live site:** when it went live.
- **Audio:** listener count, frames, errors, silent and dropped counts.
- **Recordings from earlier runs:** lane, frequency, channel, voice counts (in the history).
- **Raw IMBE frames:** only through the legacy dump.
- **Tuner operations with no route:**
  - move the control channel to an alternate;
  - hold or park a lane;
  - force LSM or C4FM without re-activating the site.

## Cleanup list

Backend work, most useful first. Each item is a separate change.

Done since the inventory: the talkgroup hold (`5f2fde5`), wrong data in `/status`,
`/recordings` and `/data` (`d1651c9`), and one call shape with names (`5d7ff96`).

The UI replacement (`UI_BRIEF.md`) sets the order. Its backend items come first:

- realtime push over one WebSocket (done: `/ws/live`);
- aliases replacing profiles (done, D15);
- SDRTrunk playlist import and export;
- manual add and rename.

The list below follows them.

1. **Notices for state changes** (done: `/ws/live` pushes the live state, the scan's progress and every configuration change; the UI no longer polls `/status`).
2. **Bench routes for mode C and replay.**
   - Lane hold and follower off (`/api/traffic`).
   - Decoder counters and their reset (`/api/stats`, `/api/decoder_compare`, `/api/decoder_reset`), from `/receivers`.
   - `/api/monitor`.
   - Or port `rf.p25_replay` and corpus mode C to `/api/v1` instead.
3. **Operator controls.**
   - Force a modulation at runtime.
   - Move to an alternate control channel.
   - Create a site by hand (renaming a system is done: `PUT /api/v1/systems/{id}`).
4. **Diagnostics streams.** The IQ stream and IQ capture (control, lanes) for eye and IQ plots.
5. **Keep what is decoded.**
   - Neighbour flags.
   - Recording details from the history for older recordings.
   - Packet data in the history.
   - Filesystem space in `/system`.
   - Audio counters.
6. **Tidy duplicates and names.**
   - One tuning source.
   - `/sites` folded into `/systems`.
   - `/api/v1/clock` under `/radio`.
   - One protocol spelling.
   - 1-based lanes in `/ws/audio` v3.
   - Remove the legacy routes once the bench reads v1.
7. **Tools on dead routes** (done: retired to the archive in the 2026-10-04 cleanup).
8. **Stale design text.** `DESIGN.md` still names:
    - `/api/endpoints`;
    - `/scan/results/{key}/add`;
    - a modulation write;
    - `p25-json` DTOs;
    - a `control_health` block.

### What a code review may change

Written with changes 090 to 093 (2026-11-10), which built the module rail, the new Now page,
the packet data switch, TV's Watch page, the kept TV scan and the new Activity and Systems pages
on these routes. Each row is something the pages lean on that a review of the API is likely to
move, with what would have to follow in the UI.

| Route or field | What may change | What follows in the UI |
|----------------|-----------------|------------------------|
| `PUT /api/v1/radio/packet_data` `{follow}`, and `config.packet_data.follow` in `GET /api/v1/radio` | It could fold into `PUT /api/v1/radio/settings` with the call timings, or keep its own route under another name: P25 packet data's read route is still `/api/v1/data`, due to become `/api/v1/packet_data` (`DATA_MODE.md` §8). The setting is P25's but its name is generic | `api.setPacketData`, Settings' Packet data card, and the label in `js/protocols.js` |
| `PUT …/talkgroups/{tg}`: a talkgroup with no alias of its own gets one named `TG <n>` | An alias may get to have no name, or the route may say it made one. `DELETE …/talkgroups/{tg}` removes the alias again (096) | The radio's `talkgroups` give no name for `TG <n>`, and `nameOf` in `js/aliases.js` does the same for the calls; an emptied name removes the alias when nothing else is set on it, else is sent as `TG <n>` |
| `PUT …/talkgroups/{tg}`: `follow` (096) and `do_not_monitor` | `follow` is followed or not, whatever the system asks: where only talkgroups with a priority are followed, it gives one with none the lowest. `do_not_monitor` is still taken, and alone does not do that | The Listen switch and the bar's Skip in `views/talkgroups.js` send `follow` |
| `PUT …/talkgroups/{tg}`: no `color`, no `group` shown | An alias's colour can only be set by replacing the whole list | Rows take the alias's colour when it has one, else a colour from the talkgroup's number (`colorOf`) |
| `GET /api/v1/systems`: each system's `aliases` | Since 096 a talkgroup's treatment comes from the radio (`talkgroups`), and the pages hold no copy of the follower's rule. The calls' rows, on Now and Activity, still find a talkgroup's alias in `systems[].aliases` for its colour, by the index's rule (the alias naming it alone, else the first range holding it) | `aliasOf` in `js/aliases.js` |
| `/ws/live` `changed` (`what: systems`) after an alias edit | The store reads every system again, and the calls, for one talkgroup's change (the calls carry the names). Now's list itself follows the pushed `talkgroups`. A review may push the changed alias, or the system | `store.js` `readSystems` and `readCalls` |
| `/ws/live` `changed` (`what: radio`) | The daemon announces every write under `/api/v1/radio` and `/api/v1/clock`, and no page acts on it: a packet data, gain or recording change made in another browser shows in Settings only after a reload. `PUT /api/v1/mode`, `PUT /api/v1/config` and the legacy settings route announce nothing | `js/store.js` acts on `systems` and `recordings` only; Settings reads `GET /api/v1/radio` once when it opens |
| `/ws/live` `talkgroups` (096) | The live site's talkgroups come whole at each change (6 KB for unit A's 42): the aliases plus the week, which the radio reads from the history at most every 5 minutes and completes from the calls it lists. Between pushes the page keeps each one's `last_ms` and what is on the air from the calls it is pushed. A review may push one talkgroup's change alone | `talkgroupsOf` in `views/talkgroups.js` |
| `status.live.state` `away` | It means another mode has the radio, and also the moment a mode changes. With a third mode (`data`) the shell needs to know which mode has it: `status.mode` says so today | `js/modules.js` (`scannerLine`), the shell's notice in `js/main.js`, Now's site strip |
| `PUT /api/v1/mode` | It returns once the radio is the new mode's (seconds when a site goes live again). The values are the daemon's (`atsc`), not the UI's (`tv`). `Modes::set` assumes two modes (`DATA_MODE.md` §8) | `giveRadio` in `js/main.js`; a module's `mode` in `js/modules.js` |
| ATSC routes outside ATSC mode | `POST /api/v1/atsc/scan` and `…/live` are refused with a message when the radio is the scanner's. Since 091 the pages change the mode first (`PUT /api/v1/mode`, after asking), then call them: two requests where a review may want one (a scan or a play that takes the radio) | `giveRadio` in `js/mode.js`, called by Watch's Play and Channels' Scan |
| `/ws/live` `atsc_live` (091) | Live TV's whole state once a second while it changes (the codeword counts move every second while a channel plays), 40 fields of MER each time. A review may send less, or only what moved | `views/watch.js` (`want({ atsc_live: true })`) |
| `POST /api/v1/atsc/live/stop` and `viewers` | The receiver has one stop for every player. Watch decides from the last pushed `viewers` whether another player is on the stream, up to a second old | `stopLive` in `views/watch.js` |
| The kept TV scan (092) | The last finished scan comes back after a restart as `state: done` with nothing to tell it from one run since, and without its spectra (`…/scan/channel/{n}` is a 404). A review may mark it (`kept: true`), keep the spectra on the card, or let a scan of some channels add to the kept list where it now replaces it | Channels reads the 404 as "not kept across a restart"; Watch shows the scan's date in the programme's Read row |
| `PUT /api/v1/hold` with `lane` | The site's hold and a lane's share a route and two status fields (`hold`, `lane_holds`) | Now's traffic cards |
| `recording.every_call` | Now's Record sets it since 096 (Off, Marked, Every call) through `PUT /radio/recording`, which takes any of its fields. With it on, a talkgroup's `record` decides nothing, and the page shows the marks lit and locked | The site bar in `views/now.js`; the Rec marks in `views/talkgroups.js` |
| `GET /api/v1/activity/calls`: no call by its id (093) | An alert names its call and its dispatch's by id and grant time. To show one in the picked call's panel, Activity asks for the talkgroup's calls granted in that one millisecond (`site`, `tg`, `from` the grant time, `to` one more, `limit=8`) and takes the one with the id. A review may add a `call` filter, or put the call in the alert | `pickAlert` in `views/activity.js` |
| `GET /api/v1/activity/summary` ignores `tg` and `unit` (093) | Narrowed to a talkgroup or a radio, Activity adds up the chart's buckets (`…/series` takes both) for the calls, the encrypted ones and the three times; the followed count, the estimate and the counts of talkgroups and radios are shown for a whole site only. The summary counts from the start of the window's first hour, a call listing from `from`, so the count beside the list can pass the list's rows | `renderSummary` in `views/activity.js` |
| `GET /api/v1/activity/talkgroups`, `…/radios` `limit` (093) | Activity lists the 200 busiest and reaches any other by its number (`…/talkgroup/{tg}`, `…/radio/{unit}`), which answer for an id never heard with empty lists rather than a 404. A review may add a search by name or number | `renderWho` in `views/activity.js` |
| `/ws/live` `scan` and the snapshot's `scan` (093) | The scan for systems comes whole at each change, with every site found so far. The Systems page takes it from the store; it no longer opens a second socket | `store.scan`; `scanPane` in `views/scan.js` |

### What the review recommends (095)

The review of 2026-11-11 (`doc/changes/095_structure_and_review.md`) read each row above against
the code. What it recommends, how large the change is, and the roadmap's step it belongs to:

| Row | Recommended | Size | Step |
|-----|-------------|------|------|
| `PUT /radio/packet_data` | Keep its own route (it applies at once; `/radio/settings` replaces a whole object for the next activation); rename it with `/data` | small | 7 |
| `TG <n>` aliases | `DELETE …/talkgroups/{tg}` removes the alias the route made; an alias with no name is a larger change | small | done (096) |
| `do_not_monitor` | A `follow` field that also gives the lowest priority where only prioritised talkgroups are followed | small | done (096) |
| `color`, `group` | `color` in `PUT …/talkgroups/{tg}`, checked as `#rrggbb` (`group` is taken already) | small | 3 |
| The rule in two places | Each talkgroup with its route as the follower reads it, pushed on `/ws/live` for the live site | medium | done (096: `GET /api/v1/talkgroups`, the live site's) |
| `changed` (`what: systems`) | Push the changed alias or system instead of having every page read all of them | small to medium | 2 |
| `changed` (`what: radio`) | The pages act on it | small | 4 |
| The bank's list | Served with the talkgroups' routes (above), not asked of the history | medium | done (096) |
| `away` | The mode in `away`, or the pages read `status.mode` (they do) | small | 7 |
| `PUT /mode` | Leave the mode it is in, then enter the new one, for a third mode | medium | 7 |
| ATSC routes outside ATSC mode | `take_radio: true` on the scan and the play | small to medium | TV |
| `atsc_live` | The counters apart from the names | small | TV |
| Stop and `viewers` | The receiver stops with its last viewer, on the daemon's side | medium | TV |
| The kept TV scan | `kept: true`; a cancelled scan leaves the kept list | small; merging medium | TV |
| Hold with `lane` | `PUT /lanes/{n}/hold` | small | 2 |
| `recording.every_call` | `PUT /radio/recording` as a patch | small | done (096) |
| A call by its id | A `call` filter with its start time; `GET /calls/{id}` gives the newest call with an id, and ids can repeat | small | 3 |
| Summary filters | `tg` is one more clause; `unit` needs the hourly table by radio | small; medium | 3 |
| `limit` 200 | A search by name or number (the names are in the configuration, not the history) | medium | 3 |
| `scan` whole | The progress apart from the sites found | small to medium | 3 |

Found by the review and not in the table above:

- **Whole-object writes:** `PUT /radio/settings` carries the scanner's call timings with the
  history's limits, and the page echoes fields it has no control for (`traffic_chains`,
  `every_call`). Two pages saving parts of one object overwrite each other.
- **`PUT …/talkgroups/{tg}`** on a talkgroup that is one of several IDs of an earlier alias
  answers 200 and changes nothing the follower reads (the index keeps the earlier alias).
- **Refusals in plain text:** an unknown field, a bad id or an extra query key gets axum's own
  text with 400, 415 or 422, not `{"ok": false, "error": "..."}`. The pages show that text.
- **`GET /api/v1/recordings` without `limit`** copies every entry (101,000 on unit A).
- **`GET /api/v1/iq/control.wav`:** two captures at once at most; a third gets 409.
- **A change saved, then a failed activation** (`POST /scan/add`, a RadioReference import, a
  site edit) answers 500 with no `changed` notice.
