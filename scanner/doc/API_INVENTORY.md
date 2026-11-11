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
| `GET`/`PUT /api/v1/hold` | The talkgroup the live site is held on, and each traffic lane's own; hold or release either (`lane`) | UI Now: Hold on the card of the call on the air, Release there and in the site strip; each traffic card's talkgroup list keeps its lane on one | One route carries two things (the site's hold and a lane's), told apart by `lane` |
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
| `GET /api/v1/activity/calls` | Calls newest first in `/calls`' shape with names; `format=csv` streams the history rows as a file | UI Activity | — |
| `GET /api/v1/data` | Packet data per site (or all): counts, radios with names, recent PDUs with IP decode | UI Activity card | Memory only: lost on restart. No `system` parameter |

## Radio settings

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/radio` | Presets; the radio configuration; stored state (live site, crystal); hardware; tuning; the radio core's readback (each lane's NCO, packets on or off, tag and counters; the ring; the sample and clip counts) | UI Board card, Settings | — |
| `PUT /api/v1/radio/gain` | Gain mode and manual gain, applied at once | UI Settings | — |
| `PUT /api/v1/radio/settings` | Presets allowed, lanes, call timings, history limits | UI Settings | Lanes apply at the next start, not the next activation. Lane count isn't range-checked |
| `PUT /api/v1/radio/clock` | Clock source | UI Settings | Same as legacy `PUT /api/ui/settings`. `{}` silently sets `site` |
| `POST /api/v1/clock` | Set the board clock | UI Settings | Sits outside `/radio/`, unlike its sibling |
| `GET`/`PUT /api/v1/radio/crystal`, `POST …/calibrate` | Crystal correction: applied, tracked, calibrated | UI Settings Crystal card (then pushed on `/ws/live`) | The stored calibration is only in `/radio` `state.crystal` |
| `PUT /api/v1/radio/recording` | Recording on/off, store, limits | UI Settings | Missing fields silently take defaults. `every_call` has no control in the UI, and while it is on a talkgroup's `record` flag does nothing |
| `PUT /api/v1/radio/packet_data` | Packet data followed or not (`follow`); the live site follows the change at once | UI Settings | New in 090; see "What a code review may change" |

## Systems, sites, aliases, scan

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/systems` | Systems with their sites | UI Systems, Settings | — |
| `GET /api/v1/systems/{id}` | One system | Nothing | — |
| `PUT /api/v1/systems/{id}` | Edit a system: name, identity, details | UI Systems | — |
| `GET`/`PUT /api/v1/systems/{id}/aliases`, `PUT …/listening`, `PUT …/talkgroups/{tg}` | Aliases (names, priorities, recording, speakers), listening settings, one talkgroup's controls | UI Now's talkgroup bank (`…/talkgroups/{tg}`: followed or not, speaker, priority, name); nothing uses the other two yet | No SDRTrunk playlist import or export (UI_BRIEF). `…/talkgroups/{tg}`: see "What a code review may change" |
| `POST /api/v1/systems/{id}/radioreference`, `…/preview` | Import a RadioReference CSV of talkgroups or sites | UI Systems | — |
| `PUT /api/v1/systems/{system}/sites/{site}` | Edit a site; the live one goes live again | UI Systems | The edit is saved before re-activation, so a failed re-activation still keeps it |
| `DELETE /api/v1/systems/{id}`, `DELETE …/sites/{site}` | Remove a system (with its sites and aliases) or a site; not the live one | Nothing yet | No create by hand: sites come only from a scan |
| `GET /api/v1/sites` | Every site, flattened, live one marked | Nothing | Repeats `/systems`; no single-site GET |
| `POST /api/v1/sites/{id}/activate`, `…/stop` | Make a site live; stop it | UI Systems; `scanner_live_check.py` | — |
| `GET /api/v1/sites/{id}/learned` | Band plan, grants per channel, encrypted talkgroups, neighbours, secondary control channels, data channel | UI Systems | Neighbours keep 4 of the 10 decoded fields (no LRA, service class, conventional, failure, valid, active) |
| `GET /api/v1/sites/{id}/plan`, `POST …/recentre` | The receive window against the site's channels; move it now | UI Systems | `{id}` must be the live site |
| `GET`/`POST /api/v1/scan`, `…/options`, `…/cancel`, `…/add` | Find systems on the air; add the ticked ones | UI Systems scan card | A DMR site added by a scan has no LCN plan. Results are in memory only |

## Mode and ATSC

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET`/`PUT /api/v1/mode` | The unit's mode (`scanner`, `atsc`); change it | UI shell: the notice's switch on a module the radio is not on, and Return to Scanner | The UI's modules are `scanner` and `tv`; `js/modules.js` maps them to the modes |
| `GET`/`POST /api/v1/atsc/live`, `POST …/live/stop` | Live TV: whether the radio core has the 8-VSB block, the channel and the block's statistics; play an RF channel; stop | UI Viewer | The Viewer asks for it every second: it is not pushed on `/ws/live` |
| `GET`/`POST /api/v1/atsc/scan`, `…/options`, `…/cancel` | The TV scan: each channel's kind, pilot, carrier to noise, clipping, and the station named from PSIP | UI Channels and Viewer; `tools/atsc_check.py` | Results are in memory only |
| `GET /api/v1/atsc/scan/channel/{n}` | One RF channel's spectrum from the last TV scan | UI Channels | — |
| `GET /api/v1/atsc/recordings` | The TV recordings on the SD card: RF channel, size, programmes | UI Viewer | — |
| `GET /api/v1/atsc/stream/{rf}` | An RF channel's transport stream as it plays (`program`: one programme) | UI Viewer (its picture and sound; its address and a playlist for VLC); VLC and other players | Its source is a recording until the radio core decodes 8-VSB (bake D) |

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

Written with change 090 (2026-11-10), which built the module rail, the new Now page and the
packet data switch on these routes. Each row is something the pages lean on that a review of the
API is likely to move, with what would have to follow in the UI.

| Route or field | What may change | What follows in the UI |
|----------------|-----------------|------------------------|
| `PUT /api/v1/radio/packet_data` `{follow}`, and `config.packet_data.follow` in `GET /api/v1/radio` | It could fold into `PUT /api/v1/radio/settings` with the call timings, or keep its own route under another name: P25 packet data's read route is still `/api/v1/data`, due to become `/api/v1/packet_data` (`DATA_MODE.md` §8). The setting is P25's but its name is generic | `api.setPacketData`, Settings' Packet data card, and the label in `js/protocols.js` |
| `PUT …/talkgroups/{tg}`: a talkgroup with no alias of its own gets one named `TG <n>` | An alias may get to have no name, or the route may say it made one. Nothing removes the alias again: taking every control back leaves it in the list (only the whole-list `PUT …/aliases` removes it) | `nameOf` in `js/aliases.js` treats the name `TG <n>` as no name; an emptied name is sent as `TG <n>` |
| `PUT …/talkgroups/{tg}`: `do_not_monitor` | Where a system follows only talkgroups with a priority (`listening.follow_unmonitored` false), clearing `do_not_monitor` does not make one followed. The bank sends `priority: 100` with it. A review may put that rule in the route (a `follow` field) | The switch's handler in `views/talkgroups.js` |
| `PUT …/talkgroups/{tg}`: no `color`, no `group` shown | An alias's colour can only be set by replacing the whole list | Rows take the alias's colour when it has one, else a colour from the talkgroup's number (`colorOf`) |
| `GET /api/v1/systems`: each system's `aliases` and `listening` | The pages work out how a talkgroup is treated (followed, speaker, priority) from them, by the follower's rule: the alias naming it alone, else the first range holding it. The rule is in the daemon (`trunking::follow::routing`) and again in `js/aliases.js`. A review may serve each talkgroup's route instead | `aliasOf` and `routeOf` in `js/aliases.js`, used by the bank and the calls' colours |
| `/ws/live` `changed` (`what: systems`) after an alias edit | The page reads every system again for one talkgroup's change. A review may push the changed alias, or the system | `store.js` `readSystems`; the bank redraws its controls when `store.systems` is replaced |
| `/ws/live`: no notice for a radio setting | A packet data, gain or recording change made in another browser shows in Settings only after a reload | Settings reads `GET /api/v1/radio` once when it opens |
| `GET /api/v1/activity/talkgroups` as the bank's list | The bank's talkgroups are the aliases plus those heard in the last week (`hours=168`, `limit=1000`), asked for at most every 5 minutes, not pushed. A review may give the live site's talkgroups a route of their own, with each one's treatment | `knownTalkgroups` in `views/talkgroups.js` |
| `status.live.state` `away` | It means another mode has the radio, and also the moment a mode changes. With a third mode (`data`) the shell needs to know which mode has it: `status.mode` says so today | `js/modules.js` (`scannerLine`), the shell's notice in `js/main.js`, Now's site strip |
| `PUT /api/v1/mode` | It returns once the radio is the new mode's (seconds when a site goes live again). The values are the daemon's (`atsc`), not the UI's (`tv`). `Modes::set` assumes two modes (`DATA_MODE.md` §8) | `giveRadio` in `js/main.js`; a module's `mode` in `js/modules.js` |
| ATSC routes outside ATSC mode | `POST /api/v1/atsc/scan` and `…/live` are refused with a message when the radio is the scanner's. The pages do not check first: the shell's notice says the radio is elsewhere, and the button still shows | Channels and Viewer are unchanged in 090 |
| `GET /api/v1/atsc/live` | Polled each second by the Viewer; the brief says no page polls | `views/viewer.js` |
| `PUT /api/v1/hold` with `lane` | The site's hold and a lane's share a route and two status fields (`hold`, `lane_holds`) | Now's traffic cards |
| `recording.every_call` | No route or page sets it apart from the whole recording setting, so the per-talkgroup `record` flag has no control in the bank | When it gets one, the bank gains a Record control (`record` is already in `PUT …/talkgroups/{tg}`) |

### For the UI session

These are reads the current pages make that can fail on a null:

- `summary.voice_per_grant` (`.toFixed`);
- `scan.sites[].modulation` (null for DMR; `.toUpperCase()`).

`systems.js` and `settings.js` age board times against the browser's clock.
