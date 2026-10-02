# API inventory

What each route carries, who uses it, and what is missing or wrong. As of `5d7ff96`.

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
| `GET /api/v1/status` | Build, uptime, board time; the live state (`no_site`, `switching`, `scanning`, `live` with site, system, profile, window and tuning); the control channel's health; the lease; tuning; clock; the held talkgroup | UI (polled every 2 s and after each `/ws/events` frame: header, Now, Systems, Diagnostics); `scanner_live_check.py` | Tuning is there twice. When the site went live isn't exposed |
| `GET /api/v1/receivers` | The control channel's status, carrier loop and AGC, and both P25 demodulators' (or the DMR decoder's) counters; each lane's channel, call, followed talkgroup, data-channel park, voice frames, decoder counters and carrier loop | Nothing yet (for the diagnostics) | Repeats `status.control`. No modulation override. No reset of the counters |
| `GET /api/v1/system` | Board load, memory, CPU per core and per scanner thread, AD9361 and Zynq temperatures | Nothing yet | No filesystem space; the card's free space is only in `/recordings` |
| `GET /api/v1/spectrum` | The receive window's spectrum (`bins`) with the LO, rate, control channel and lanes | UI Diagnostics | No frame time. A `bins` that doesn't divide 4096 isn't refused |
| `GET /api/v1/events` | The event log after `after` (`limit`, `routine`) | UI Diagnostics events box | Any extra query key is a 400. More than `limit` new events leaves a silent gap. No filter by class, talkgroup or unit |
| `WS /ws/events` | `call_opened`, `call_closed`, `recording_saved`, `lag` | UI (only as a cue to refresh) | No notice for a site switch, a scan, a recentre, a clock or crystal step, or a settings or profile change. No site in notices |

## Calls, audio, recordings

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/calls` | The live site's open calls and its newest 100 closed ones, each with talkgroup and radio names, radios, channel, slot, lane, emergency, private, times (grant, first voice, end, open span, grant span), close reason, voice frames, codec, why not followed | UI Now | An open call that isn't followed shows no voice and no radios. Frame errors are known only once a call is stored |
| `GET /api/v1/calls/{id}` | One call in the same shape, live while recent, else from the history | Nothing | — |
| `GET`/`PUT /api/v1/hold` | The talkgroup the live site is held on; hold or release it | UI Now (Hold/Release) | — |
| `WS /ws/audio` | 20 ms frames of 8 kHz audio. With `v=2`: every lane, each frame tagged; text `meta` (lane, talkgroup, radio, call, speaker) and `lag` | UI player; bench `wsaudio.py`, corpus, scoring; `p25_ws_audio_capture.py` | Lane is 0-based. Meta has no site or encryption flag, and sends `src` 0 for an unknown radio. The listener count and the audio counters (frames, errors, silent, dropped) are kept but never exposed |
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
| `GET /api/v1/activity/talkgroups` | Talkgroups by time, with names | UI Activity | — |
| `GET /api/v1/activity/radios` | Radios by time, with names | UI Activity | — |
| `GET /api/v1/activity/radio/{unit}` | A radio's talkgroups and affiliation/registration events | UI Activity | No `limit` |
| `GET /api/v1/activity/talkgroup/{tg}` | A talkgroup's radios and encryption history | UI Activity | — |
| `GET /api/v1/activity/series` | Calls and time per hour or day (`tz`, `tg`, `unit`) | UI Activity chart | — |
| `GET /api/v1/activity/calls` | Calls newest first in `/calls`' shape with names; `format=csv` streams the history rows as a file | UI Activity | — |
| `GET /api/v1/data` | Packet data per site (or all): counts, radios with names, recent PDUs with IP decode | UI Activity card | Memory only: lost on restart. No `system` parameter |

## Radio settings

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/radio` | Presets; the radio configuration; stored state (live site, crystal); hardware; tuning; register readback | UI Board card, Settings | No lane LSM/NID readback |
| `PUT /api/v1/radio/gain` | Gain mode and manual gain, applied at once | UI Settings | — |
| `PUT /api/v1/radio/settings` | Presets allowed, lanes, call timings, history limits | UI Settings | Lanes apply at the next start, not the next activation. Lane count isn't range-checked |
| `PUT /api/v1/radio/clock` | Clock source | UI Settings | Same as legacy `PUT /api/ui/settings`. `{}` silently sets `site` |
| `POST /api/v1/clock` | Set the board clock | UI Settings | Sits outside `/radio/`, unlike its sibling |
| `GET`/`PUT /api/v1/radio/crystal`, `POST …/calibrate` | Crystal correction: applied, tracked, calibrated | UI Settings Crystal card | The stored calibration is only in `/radio` `state.crystal` |
| `PUT /api/v1/radio/recording` | Recording on/off, store, limits | UI Settings | Missing fields silently take defaults |

## Systems, sites, profiles, scan

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/systems` | Systems with names and sites | UI Systems, Settings, Profiles | Every call sends the full name maps |
| `GET /api/v1/systems/{id}` | One system | Nothing | — |
| `PUT /api/v1/systems/{id}/names` | Replace talkgroup and radio names | UI Settings | No rename of a system, no edit of its identity |
| `PUT /api/v1/systems/{system}/sites/{site}` | Edit a site; the live one goes live again | UI Systems | The edit is saved before re-activation, so a failed re-activation still keeps it |
| `DELETE /api/v1/systems/{id}`, `DELETE …/sites/{site}` | Remove a system (with its sites and profiles) or a site; not the live one | Nothing yet | No create by hand: sites come only from a scan |
| `GET /api/v1/sites` | Every site, flattened, live one marked | Nothing | Repeats `/systems`; no single-site GET |
| `POST /api/v1/sites/{id}/activate` | Make a site live | UI Systems; `scanner_live_check.py` | — |
| `GET /api/v1/sites/{id}/learned` | Band plan, grants per channel, encrypted talkgroups, neighbours, secondary control channels, data channel | UI Systems | Neighbours keep 4 of the 10 decoded fields (no LRA, service class, conventional, failure, valid, active) |
| `GET /api/v1/sites/{id}/plan`, `POST …/recentre` | The receive window against the site's channels; move it now | UI Systems | `{id}` must be the live site |
| `GET /api/v1/profiles`, `PUT /api/v1/sites/{id}/profile`, `POST`/`PUT`/`DELETE /api/v1/profiles…` | Profiles (groups, speakers, monitor, ignore) and each site's choice | UI Profiles | A site's choice can't be cleared. `PUT` needs `id` and `system` in the body and ignores them |
| `GET`/`POST /api/v1/scan`, `…/cancel`, `…/add` | Find systems on the air; add the ticked ones | UI Systems scan card | A DMR site added by a scan has no LCN plan. Results are in memory only |

## Configuration

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/v1/config` | The whole configuration as one document: radio settings, systems with names and sites, profiles, live site (`download=true` as a file) | Nothing yet | What sites learned and the crystal calibration aren't included, by design |
| `PUT /api/v1/config` | Import an exported document: checked whole, then the scanner restarts | Nothing yet | — |
| `POST /api/v1/config/factory-reset` | No systems, sites, profiles, recordings or history; default settings; the crystal calibration and TLS certificates stay; the scanner restarts | Nothing yet | — |

## Legacy routes for the bench

| Route | Carries | Used by | Missing or wrong |
|-------|---------|---------|------------------|
| `GET /api/system` | Build, uptime | fbench units and sys/rf/corpus tests; many `tools/p25_*` | Some tools read identity fields it doesn't have (`p25_check.py`, `p25_status_and_next_step.py`) |
| `GET /api/ui/state` | Board time, clock valid | corpus | `live_baseline.py` reads `site.*`, which isn't there |
| `GET /api/imbe_dump` | The newest 128 raw IMBE frames | corpus, scoring, `p25_imbe_test.py`, `voice_capture.py` | No lane, call or time per frame; no v1 equivalent |
| `GET /api/ui/calls` | Calls with `open_ms`, `first_voice_ms` and `imbe` | corpus, scoring | Scoring reads `ldu`, which isn't there. At most about 100 calls |
| `GET`/`PUT /api/ui/settings` | Clock source | corpus (pins `manual`) | Same as `PUT /api/v1/radio/clock` |

Bench routes that p25-httpd served and the scanner does not:

- `/api/traffic` (lane hold, follower off), `/api/monitor`, `/api/decoder_compare`, `/api/stats`, `/api/decoder_reset`: `rf.p25_replay` and corpus mode C need them.
- `/api/dibit_delivery`: optional.

The bench agent's maintenance mode stops `S60p25-httpd`, not `S60scanner`.

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

1. **Notices for state changes.** On `/ws/events`: live state, scan progress, recentre, settings, profile and hold changes, each with its site. Then the UI can stop polling `/status` every 2 s.
2. **Bench routes for mode C and replay.**
   - Lane hold and follower off (`/api/traffic`).
   - Decoder counters and their reset (`/api/stats`, `/api/decoder_compare`, `/api/decoder_reset`), from `/receivers`.
   - `/api/monitor`.
   - The bench agent's maintenance mode stops `S60scanner`.
3. **Operator controls.**
   - Force a modulation at runtime.
   - Move to an alternate control channel.
   - Clear a site's profile.
   - Create a site by hand, rename a system.
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
7. **Tools on dead routes.** `route_shapes.py` (27 of 31 routes gone), `live_baseline.py`, `p25_check.py`, `p25_status_and_next_step.py`, `poll_recordings_persist.py`: port or retire.
8. **Stale design text.** `DESIGN.md` still names:
    - `/api/endpoints`;
    - `/scan/results/{key}/add`;
    - a modulation write;
    - `p25-json` DTOs;
    - a `control_health` block.

### For the UI session

These are reads the current pages make that can fail on a null:

- `summary.voice_per_grant` (`.toFixed`);
- `scan.sites[].modulation` (null for DMR; `.toUpperCase()`);
- `readback.control_nid` (null off the board).

`systems.js` and `settings.js` age board times against the browser's clock.
