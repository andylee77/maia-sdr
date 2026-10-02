# Scanner API

Generated from the route table (`src/api/mod.rs`) by the test `the_api_reference_is_the_route_table`; `API_DOC_BLESS=1 cargo test` rewrites it. Writes from another origin are refused; errors are `{"ok": false, "error": "..."}` with a status.

| Method | Path | What |
|--------|------|------|
| GET | `/api/v1/routes` | this list |
| GET | `/api/v1/status` | build, uptime, the live site, its control channel and the tuning |
| GET | `/api/v1/calls` | the live site's open calls and its newest closed ones (from its history after a restart or switch), with names |
| GET | `/api/v1/hold` | the talkgroup the live site is held on, if any |
| PUT | `/api/v1/hold` | hold the live site on one talkgroup (`tg`; null releases): only it is followed, whatever the aliases say |
| GET | `/api/v1/calls/{id}` | one call, live while recent, else from the history; the same shape either way |
| GET | `/ws/live` | the radio's state pushed as it changes: a snapshot, then status, traffic channels, calls, recordings, the scan and configuration changes |
| GET | `/ws/events` | a text frame when a call opens or closes or a recording is saved |
| GET | `/ws/audio` | live audio: with `v=2` every lane, each binary 20 ms frame tagged with its lane (text meta and lag frames); without, lane one untagged |
| GET | `/api/v1/data` | packet data of a site (`site`, default the live one; `all`): totals, radios and recent records (`limit`) |
| GET | `/api/v1/activity/sites` | sites with history; where it is kept, its size and limits |
| GET | `/api/v1/activity/summary` | calls, voice and grant time, talkgroups, radios (`site`, `from`/`to` or `hours`) |
| GET | `/api/v1/activity/talkgroups` | talkgroups by time, with names (`limit`) |
| GET | `/api/v1/activity/radios` | radios by time, with names (`limit`) |
| GET | `/api/v1/activity/radio/{unit}` | the talkgroups a radio used, its affiliations and registrations |
| GET | `/api/v1/activity/talkgroup/{tg}` | a talkgroup's radios and encryption history |
| GET | `/api/v1/activity/series` | calls and time per hour or day (`bucket`, `tz`, `tg`, `unit`) |
| GET | `/api/v1/activity/calls` | calls newest first in `/calls`' shape with names (`tg`, `unit`, `limit`; `format=csv`: history rows as a file) |
| GET | `/api/v1/spectrum` | the receive window from the wideband spectrometer (`bins`), with the control channel and lanes |
| GET | `/api/v1/events` | the event log after `after` (newest `limit`; housekeeping too with `routine=true`) |
| GET | `/api/v1/system` | the board's health: load, memory, CPU per core and per scanner thread, temperatures |
| GET | `/api/v1/iq/control.wav` | the next `seconds` (default 10, at most 120) of the control channel's IQ as the decoder gets it: 50 kSPS stereo WAV, I left |
| GET | `/api/v1/receivers` | the control channel and each lane: status, decoder counters, carrier loop |
| GET | `/api/v1/config` | the whole configuration as one document: radio settings, systems with their aliases and sites, the live site (`download=true`: as a file) |
| PUT | `/api/v1/config` | replace the configuration with an exported document (checked whole first); the scanner restarts |
| POST | `/api/v1/config/factory-reset` | back to a new unit: no systems, sites, aliases, recordings or history, default settings (the crystal calibration stays); the scanner restarts |
| GET | `/api/v1/radio` | radio configuration, hardware and tuning |
| PUT | `/api/v1/radio/gain` | receiver gain mode and manual gain |
| PUT | `/api/v1/radio/settings` | presets the planner may use, traffic lanes, call timings, history limits |
| PUT | `/api/v1/radio/clock` | where the board clock comes from: site, ntp or manual |
| GET | `/api/v1/radio/crystal` | the crystal correction: applied, calibrated and tracked |
| PUT | `/api/v1/radio/crystal` | crystal tracking on or off, and its anchor (Hz from this run's calibration; 0: no limit) |
| POST | `/api/v1/radio/crystal/calibrate` | measure the crystal correction on the live control channel now (about 7 s) |
| POST | `/api/v1/clock` | set the board clock (`unix_ms`, a browser's time) |
| PUT | `/api/v1/radio/recording` | recording on/off, where new recordings go, how many each store keeps |
| GET | `/api/v1/recordings` | recordings newest first (`limit`, `site`), with the stores' state |
| DELETE | `/api/v1/recordings` | delete every recording of `store` (sd, ram or all) |
| GET | `/api/v1/recordings/{id}` | one recording's WAV (`{id}` or `{id}.wav`; byte ranges) |
| DELETE | `/api/v1/recordings/{id}` | delete one recording |
| GET | `/api/v1/systems` | systems with their sites |
| GET | `/api/v1/systems/{id}` | one system |
| GET | `/api/v1/systems/{id}/aliases` | a system's aliases (names, priorities, recording, speakers of its talkgroups and radios) and listening settings |
| PUT | `/api/v1/systems/{id}/aliases` | replace a system's aliases (the live site follows them at once) |
| PUT | `/api/v1/systems/{id}/listening` | how a system treats talkgroups with no priority, and pre-emption |
| PUT | `/api/v1/systems/{id}/talkgroups/{tg}` | one talkgroup's controls: name, group, priority, do-not-monitor, record, speaker |
| PUT | `/api/v1/systems/{system}/sites/{site}` | edit a site (the live site goes live again with the change) |
| DELETE | `/api/v1/systems/{system}/sites/{site}` | remove a site (not the live one) and what it learned; the history keeps its calls |
| DELETE | `/api/v1/systems/{id}` | remove a system with its sites and aliases (none of its sites live); the history keeps their calls |
| GET | `/api/v1/sites` | every site, with the live one marked |
| POST | `/api/v1/sites/{id}/activate` | make a site live (returns once it is) |
| POST | `/api/v1/sites/{id}/stop` | stop the live site: no site is live until one is made live |
| GET | `/api/v1/sites/{id}/learned` | what a site taught the radio: band plan, grants, encrypted talkgroups, neighbours, its other channels |
| GET | `/api/v1/sites/{id}/plan` | the live site's receive window against its channels, and the planner's choice |
| POST | `/api/v1/sites/{id}/recentre` | move the live site's window to the planner's choice now (both lanes idle) |
| GET | `/api/v1/scan` | the scan's progress and what it found |
| POST | `/api/v1/scan` | find the systems on the air (the live site pauses meanwhile) |
| POST | `/api/v1/scan/cancel` | stop the scan |
| POST | `/api/v1/scan/add` | add the ticked sites of the last scan |
| GET | `/api/system` | legacy, for the bench: the build |
| GET | `/api/ui/state` | legacy, for the bench: the unit's wall clock |
| GET | `/api/imbe_dump` | legacy, for the bench: the newest raw IMBE frames |
| GET | `/api/ui/calls` | legacy, for the bench: the newest calls with their voice frame counts (`limit`, default 40) |
| GET | `/api/ui/settings` | legacy, for the bench: the clock source |
| PUT | `/api/ui/settings` | legacy, for the bench: set the clock source |
