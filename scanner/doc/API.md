# Scanner API

Generated from the route table (`src/api/mod.rs`) by the test `the_api_reference_is_the_route_table`; `API_DOC_BLESS=1 cargo test` rewrites it. Writes from another origin are refused; errors are `{"ok": false, "error": "..."}` with a status.

| Method | Path | What |
|--------|------|------|
| GET | `/api/v1/routes` | this list |
| GET | `/api/v1/status` | build, uptime, the live site, its control channel and the tuning |
| GET | `/api/v1/calls` | the open calls and the newest closed ones |
| GET | `/api/v1/calls/{id}` | one call: live while recent, else from the history |
| GET | `/ws/events` | a text frame when a call opens or closes or a recording is saved |
| GET | `/ws/audio` | live audio: with `v=2` every lane, each binary 20 ms frame tagged with its lane (text meta and lag frames); without, lane one untagged |
| GET | `/api/v1/activity/sites` | sites with history; where it is kept, its size and limits |
| GET | `/api/v1/activity/summary` | calls, voice and grant time, talkgroups, radios (`site`, `from`/`to` or `hours`) |
| GET | `/api/v1/activity/talkgroups` | talkgroups by time, with names (`limit`) |
| GET | `/api/v1/activity/radios` | radios by time, with names (`limit`) |
| GET | `/api/v1/activity/radio/{unit}` | the talkgroups a radio used, its affiliations and registrations |
| GET | `/api/v1/activity/talkgroup/{tg}` | a talkgroup's radios and encryption history |
| GET | `/api/v1/activity/series` | calls and time per hour or day (`bucket`, `tz`, `tg`, `unit`) |
| GET | `/api/v1/activity/calls` | calls newest first (`tg`, `unit`, `limit`; `format=csv` as a file) |
| GET | `/api/v1/spectrum` | the receive window from the wideband spectrometer (`bins`), with the control channel and lanes |
| GET | `/api/v1/events` | the event log after `after` (newest `limit`; housekeeping too with `routine=true`) |
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
| PUT | `/api/v1/systems/{id}/names` | a system's talkgroup and radio names |
| PUT | `/api/v1/systems/{system}/sites/{site}` | edit a site (the live site goes live again with the change) |
| GET | `/api/v1/sites` | every site, with the live one marked |
| POST | `/api/v1/sites/{id}/activate` | make a site live (returns once it is) |
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
| GET | `/api/v1/profiles` | profiles and each site's active one |
| PUT | `/api/v1/sites/{id}/profile` | choose a site's active profile |
| POST | `/api/v1/profiles` | a new profile of a system, empty or a copy |
| PUT | `/api/v1/profiles/{*id}` | edit a profile (the live site follows it at once) |
| DELETE | `/api/v1/profiles/{*id}` | delete a profile no site uses |
