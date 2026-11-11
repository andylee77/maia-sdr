# Scanner API

Generated from the route table (`src/api/mod.rs`) by the test `the_api_reference_is_the_route_table`; `API_DOC_BLESS=1 cargo test` rewrites it. Writes from another origin are refused; errors are `{"ok": false, "error": "..."}` with a status.

| Method | Path | What |
|--------|------|------|
| GET | `/api/v1/routes` | this list |
| GET | `/api/v1/status` | build, uptime, the live site, its control channel and the tuning |
| GET | `/api/v1/calls` | the live site's open calls and its newest closed ones (from its history after a restart or switch), with names |
| GET | `/api/v1/talkgroups` | the live site's talkgroups: those heard there in the last week or among the calls listed now (an alias for a talkgroup not heard lists none), each with how it is treated (followed, its traffic lane, priority, recorded) and when it was last heard; and what decides recording |
| GET | `/api/v1/hold` | the talkgroup the live site is held on, if any, and each traffic lane's own |
| PUT | `/api/v1/hold` | hold the live site on one talkgroup (`tg`; null releases): only it is followed, whatever the aliases say; with `lane` (1 or 2) only that lane: it takes only that talkgroup, the other lane follows as before |
| GET | `/api/v1/calls/{id}` | one call, live while recent, else from the history; the same shape either way |
| GET | `/ws/live` | the radio's state pushed as it changes: a snapshot, then status, traffic channels, calls, recordings, alert tones, the live site's talkgroups, the scan and configuration changes; and what a page subscribes to (the spectrum, the event log, the radio's readback, the window, the crystal) |
| GET | `/ws/events` | a text frame when a call opens or closes, a recording is saved or a call's alert tones are known |
| GET | `/ws/audio` | live audio: with `v=2` every lane, each binary 20 ms frame tagged with its lane (text meta, alert and lag frames); without, lane one untagged |
| GET | `/api/v1/data` | packet data of a site (`site`, default the live one; `all`): totals, radios and recent records (`limit`) |
| GET | `/api/v1/activity/sites` | sites with history; where it is kept, its size and limits |
| GET | `/api/v1/activity/summary` | calls, voice and grant time, talkgroups, radios (`site`, `from`/`to` or `hours`) |
| GET | `/api/v1/activity/talkgroups` | talkgroups by time, with names (`limit`) |
| GET | `/api/v1/activity/radios` | radios by time, with names (`limit`) |
| GET | `/api/v1/activity/radio/{unit}` | the talkgroups a radio used, its affiliations and registrations |
| GET | `/api/v1/activity/talkgroup/{tg}` | a talkgroup's radios and encryption history |
| GET | `/api/v1/activity/series` | calls and time per hour or day (`bucket`, `tz`, `tg`, `unit`) |
| GET | `/api/v1/activity/calls` | calls newest first in `/calls`' shape with names, recordings and alert tones (`tg`, `unit`, `limit`; `format=csv`: history rows as a file) |
| GET | `/api/v1/activity/alerts` | the alert tones heard in followed calls (console warbles and beeps, two-tone pages): grouped by kind and tones, and the newest each with its call (`tg`, `unit`: the sending radio, `limit`) |
| GET | `/api/v1/survey` | the carriers heard in the live site's receive window over the last ten minutes: how often each is on, its peak above the floor, steady or not (from every spectrometer frame) |
| GET | `/api/v1/spectrum` | the receive window from the wideband spectrometer (`bins`), with the control channel and lanes |
| GET | `/api/v1/events` | the event log after `after` (newest `limit`; housekeeping too with `routine=true`) |
| GET | `/api/v1/system` | the board's health: load, memory, CPU per core and per scanner thread, temperatures |
| GET | `/api/v1/iq/control.wav` | the next `seconds` (default 10, at most 120) of the control channel's IQ as the decoder gets it: 50 kSPS stereo WAV, I left |
| GET | `/api/v1/receivers` | the control channel and each lane: status, decoder counters, carrier loop |
| GET | `/api/v1/config` | the whole configuration as one document: radio settings, systems with their aliases and sites, the live site (`download=true`: as a file) |
| PUT | `/api/v1/config` | replace the configuration with an exported document (checked whole first); the scanner restarts |
| GET | `/api/v1/playlist` | every system as an SDRTrunk playlist (XML): a channel for each site on its control channel and those that take over from it, each system's aliases as a list named after it (`download=true`: as a file) |
| POST | `/api/v1/playlist` | import an SDRTrunk playlist (`xml`): each of its systems into the configured system on its channels' frequencies or into a new one, its channels as sites, its alias lists as aliases; `systems` says what to take of each (`name`, `into`, `channels` by number, `aliases`), absent: what the preview suggests |
| POST | `/api/v1/playlist/preview` | what that import would change; nothing is saved |
| POST | `/api/v1/config/factory-reset` | back to a new unit: no systems, sites, aliases, recordings, history or TV scan, default settings (the crystal calibration stays); the scanner restarts |
| GET | `/api/v1/radio` | radio configuration, hardware and tuning |
| PUT | `/api/v1/radio/gain` | receiver gain mode and manual gain |
| PUT | `/api/v1/radio/settings` | presets the planner may use, traffic lanes, call timings, history limits |
| PUT | `/api/v1/radio/clock` | where the board clock comes from: site, ntp or manual |
| PUT | `/api/v1/radio/packet_data` | packet data followed or not (`follow`): followed, the last traffic lane of a site with a data channel waits there between calls; the live site follows the change at once |
| GET | `/api/v1/radio/crystal` | the crystal correction: applied, calibrated and tracked |
| PUT | `/api/v1/radio/crystal` | crystal tracking on or off, and its anchor (Hz from this run's calibration; 0: no limit) |
| POST | `/api/v1/radio/crystal/calibrate` | measure the crystal correction on the live control channel now (about 7 s) |
| POST | `/api/v1/clock` | set the board clock (`unix_ms`, a browser's time) |
| PUT | `/api/v1/radio/recording` | any of: recording on/off (`enabled`), every followed call or only those of talkgroups marked record (`every_call`), where new recordings go, how long the card keeps them (locked ones longer), its share of the card, files a day folder, the count RAM keeps |
| GET | `/api/v1/recordings` | recordings newest first (`limit`, `site`), with the stores' state |
| DELETE | `/api/v1/recordings` | delete every recording of `store` (sd, ram or all) |
| GET | `/api/v1/recordings/{id}` | one recording's WAV (`{id}` or `{id}.wav`; byte ranges) |
| DELETE | `/api/v1/recordings/{id}` | delete one recording |
| PUT | `/api/v1/recordings/{id}/lock` | lock (`locked`: true) or unlock a card recording: kept for the locked days, in the locked tree |
| GET | `/api/v1/systems` | systems with their sites |
| GET | `/api/v1/systems/{id}` | one system |
| PUT | `/api/v1/systems/{id}` | edit a system: its name, identity (P25: WACN and system ID; DMR: model and network) and details (location, county, type, voice) |
| GET | `/api/v1/systems/{id}/aliases` | a system's aliases (names, priorities, recording, speakers of its talkgroups and radios) and listening settings |
| PUT | `/api/v1/systems/{id}/aliases` | replace a system's aliases (the live site follows them at once); with `?rev=` (the `rev` the list was read with) only that list is replaced: 409 when it changed since. No talkgroup or radio may be named by itself in two aliases |
| PUT | `/api/v1/systems/{id}/listening` | how a system treats talkgroups with no priority, and pre-emption |
| PUT | `/api/v1/systems/{id}/talkgroups/{tg}` | one talkgroup's controls: name, group, color, priority, follow (or do-not-monitor), record, speaker (its traffic lane: left is lane 1, right lane 2, both either); a talkgroup with no alias of its own gets one, and leaves an alias that named it among others |
| DELETE | `/api/v1/systems/{id}/talkgroups/{tg}` | remove the alias that names one talkgroup alone: it is then treated as a talkgroup with none |
| POST | `/api/v1/systems/{id}/radioreference` | import a RadioReference CSV (`csv`; talkgroups or sites, told by its header): talkgroups no alias covers become aliases (fully encrypted ones never followed unless `encrypted_do_not_monitor` is false); new sites are added (`sites`: only these rows), configured ones gain the channels they lack |
| POST | `/api/v1/systems/{id}/radioreference/preview` | what that import would change; nothing is saved |
| PUT | `/api/v1/systems/{system}/sites/{site}` | edit a site: its name, identity (kept when absent), channels and receiver settings (the live site goes live again with the change) |
| DELETE | `/api/v1/systems/{system}/sites/{site}` | remove a site (not the live one) and what it learned; the history keeps its calls |
| DELETE | `/api/v1/systems/{id}` | remove a system with its sites and aliases (none of its sites live); the history keeps their calls |
| GET | `/api/v1/sites` | every site, with the live one marked |
| POST | `/api/v1/sites/{id}/activate` | make a site live (returns once it is) |
| POST | `/api/v1/sites/{id}/stop` | stop the live site: no site is live until one is made live |
| GET | `/api/v1/sites/{id}/learned` | what a site taught the radio: band plan, grants, encrypted talkgroups, neighbours, its other channels |
| GET | `/api/v1/sites/{id}/plan` | the live site's receive window against its channels, and the planner's choice |
| POST | `/api/v1/sites/{id}/recentre` | move the live site's window to the planner's choice now (both lanes idle) |
| GET | `/api/v1/scan` | the scan's progress (the band and window read now) and what it found |
| GET | `/api/v1/scan/options` | what a scan offers: its bands by name, the default settings, the window it reads at once and its step |
| POST | `/api/v1/scan` | find the systems on the air (the live site pauses meanwhile) |
| POST | `/api/v1/scan/cancel` | stop the scan |
| POST | `/api/v1/scan/add` | add one found system from its card: a new system's name, identity and details; each ticked site's name, identity and channels (as heard when absent) |
| GET | `/api/v1/mode` | the unit's mode: `scanner` (P25 and DMR trunking) or `atsc` (ATSC TV) |
| PUT | `/api/v1/mode` | change mode (`mode`): ATSC mode has the radio to itself with the live site paused; scanner mode brings the site back with the configured gain; kept across restarts |
| GET | `/api/v1/atsc/scan` | the TV scan's progress and each channel read: 8-VSB (its pilot found), a signal without the 8-VSB pilot (ATSC 3.0 or other) or vacant; the pilot's offset and level, the carrier to noise, the power; the station decoded (TSID, virtual channels and names, its clock); after a restart, the last scan that finished |
| GET | `/api/v1/atsc/scan/channel/{n}` | one RF channel's spectrum as the last TV scan read it: the channel and 0.5 MHz either side, dB per bin (about dBm); not kept across a restart |
| GET | `/api/v1/atsc/scan/options` | the TV channel plan (RF 2-36 and which the radio reaches), the default settings and the window read at once |
| POST | `/api/v1/atsc/scan` | scan the TV channels in ATSC mode (`channels`, default every one reached; `frames` a window; `gain_db`, default the AGC), then decode each 8-VSB channel of 15 dB or more for its station's names (`identify`, default on) |
| POST | `/api/v1/atsc/scan/cancel` | stop the TV scan |
| GET | `/api/v1/atsc/recordings` | the TV recordings a player can be served (`tv/rf<N>.ts` on the SD card): each one's RF channel, size and programmes |
| GET | `/api/v1/atsc/stream/{rf}` | an RF channel's transport stream as it plays, for VLC and other players and the Watch page (`video/mp2t`): live from the radio core's 8-VSB block when it plays that channel, or in ATSC mode when nobody watches another (it tunes it); otherwise its recording at the pace of its clock, from the start again at its end; `program`: that programme alone, under a PAT naming only it |
| GET | `/api/v1/atsc/live` | live TV: whether the radio core has the 8-VSB block, the channel, the block's state (segments, fields, tracking, taps), the loops at the last field sync (carrier Hz, timing ppm, gain word, sync), the latest fields' MER, codewords clean, corrected and failed, tap solves and their time, PSIP's names, players |
| POST | `/api/v1/atsc/live` | play RF channel `rf` live in ATSC mode (the radio core's 8-VSB block; refused during a TV scan) |
| POST | `/api/v1/atsc/live/stop` | stop live TV |
| GET | `/api/v1/diag/burst` | the burst timing check's progress (what to do now) and result: where one burst starts, as an AD9361 sample index, in a spectrum frame's header, in the control lane's packets and in the capture, and whether they agree to within one transform |
| POST | `/api/v1/diag/burst` | check the radio core's timing with a burst (a key fob, the ESP32-DIV) at `freq_hz`: a first press finds it (`find`, default on), the next is measured; `offset_hz` (300 kHz) above the LO, `span_hz` (150 kHz) searched, `wait_s` (60) for each press, `threshold_db` (15). The live site pauses until it ends |
| POST | `/api/v1/diag/burst/cancel` | stop the burst timing check |
| GET | `/api/system` | legacy, for the bench: the build |
| GET | `/api/ui/state` | legacy, for the bench: the unit's wall clock |
| GET | `/api/imbe_dump` | legacy, for the bench: the newest raw IMBE frames |
| GET | `/api/ui/calls` | legacy, for the bench: the newest calls with their voice frame counts (`limit`, default 40) |
| GET | `/api/ui/settings` | legacy, for the bench: the clock source |
| PUT | `/api/ui/settings` | legacy, for the bench: set the clock source |
