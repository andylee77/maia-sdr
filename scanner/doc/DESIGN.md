# 076 — Refactor: a multi-band, multi-protocol scanner

**Started:** 2026-10-01. **Branch:** fishball-p25. **Bake required:** no (change 079 replaces the
core afterwards; section 13). **Brief:** `BRIEF.md`.

**Status:** design approved by Andy on 2026-10-01. It is built as a fresh crate, top-down, in
phases (section 15). Phase 0 is next.

## Summary

p25-httpd is a P25 receiver with DMR attached to its side. DMR reaches calls, recordings and the
history only by impersonating P25 call-boundary events on lane One. This change turns it into a
scanner whose core knows nothing about P25 or DMR:

1. **Protocols behind traits.** The P25 and DMR decoders emit the same trunking and voice events.
   One follower serves both, and only the live site's protocol runs.
2. **One `Call` per call.** It is owned by `trunking::calls`. The follower, recorder, history and
   UI read it. Six partial copies of a call go away.
3. **Configuration in versioned files:** the radio, the systems (each with its aliases and its
   sites), and what the radio learns. A unit starts empty (D16) and is set up by a scan, a manual
   add or an import.
4. **The live site is one server-side state** with one switch. The switch moves the radio
   window, the decoders and the system's aliases together.
5. **One history writer** with a v2 schema, **one recording manager**, **one audio path**, a typed
   `/api/v1`, and a UI built from per-protocol components.

No gateware changes. The scanner is built as a **fresh crate**, top-down, in nine phases
(section 15):

- the clean leaf code (DSP, FEC, decoders, vocoders, drivers) is ported with its tests;
- the glue (today's `app/`, `httpd/` and `main.rs`) is written fresh against this design;
- the old p25-httpd stays the production binary until the cutover;
- every commit builds and passes the checks.

## Decisions for Andy

These are the choices that shape the result. Each has a recommendation; the sections below assume
it.

| # | Decision | Recommendation | Alternative |
|---|----------|----------------|-------------|
| D1 | Config files | `radio.json` and `systems.json` in `/mnt/jffs2/scanner/`, written only when the user changes something. What the radio learns by itself (crystal calibration, IDEN bands, LCNs, grant counts) goes to `state/` files beside them. | One file per layer, including learned state. The grant counts alone would rewrite the systems file every 10 minutes. |
| D2 | Talkgroup and radio names | Per **system**, in its aliases (D15): IDs are system-wide in P25 and DMR. | Per site, as today. |
| D3 | Profile scope | Superseded by D15: a system's aliases say what is followed, recorded and played where. | — |
| D4 | Old files | Superseded by D16: p25-httpd's files are never read. | — |
| D5 | History migration | Superseded by D16: the history starts empty in `/mnt/sd/scanner-history.sqlite`. | — |
| D6 | Seed sites | Removed from the binary (D16): sites come from a scan, a manual add or an import. | Keep an importable "library" in the UI. |
| D7 | API | The UI moves to a typed `/api/v1`. Diagnostic endpoints keep their paths, but every GET that writes becomes POST/PUT, with tools and bench updated in the same commit. Legacy user-facing routes stay as adapters until their consumers move. No auth in 076; writes get an Origin check. | Version everything now, or add tokens now. |
| D8 | Pages | The Radio page dissolves: config goes to Settings, and spectrum, coverage and manual tune go to Diagnostics. The recording switch moves from Now to Settings. The UI replacement (`UI_BRIEF.md`) lays the pages out anew. | Keep the speakers on Now (063). |
| D9 | Names | Superseded by D13: the fresh crate needs a name of its own. `p25-json` and `p25-pac` keep theirs until the cutover. | — |
| D10 | FPGA | Nothing moves into gateware in 076. 076 models what each chain can do. Change 079 then replaces the core with a channelizer whose lanes all carry IQ (section 13). | — |
| D11 | Packet data | The v2 schema has a `data_packets` table. Fill it only if Andy wants: 074b was parked. | Leave the table out. |
| D12 | P25 regression gate (Andy, 2026-10-01) | B is wired into A over the bench link whenever a test needs it. The replay corpus (`rf.p25_corpus`), with B transmitting into A, is the P25 gate from phase 2, next to host replay and live A. | Host replay plus live A only. |
| D13 | Execution (Andy, 2026-10-01) | A **fresh crate** in this repo, a workspace sibling of `p25-httpd/`, built top-down in phases. The old binary stays in production, with fixes only, until the cutover. Proposed name: `scanner/` (binary `scanner`); at the cutover tezuka_fw's package and init script switch to it. | Refactor in place (the 14-stage plan this replaces). |
| D14 | Docs (Andy, 2026-10-01) | The old docs stay where they are. Docs that the refactor needs live in the fresh crate (`scanner/doc/`), starting with this design when phase 1 creates the crate. | Remove the old docs after a `pre-076` tag. |
| D15 | Aliases (Andy, 2026-10-01) | SDRTrunk's model in place of profiles and name maps: each system has an alias list (name, group, color, talkgroup and radio IDs and ranges, priority 1–100, do-not-monitor, record, speaker) and listening settings (talkgroups with no priority, their speaker, pre-emption). SDRTrunk playlists import and export. | Profiles per system. |
| D16 | No migration (Andy, 2026-10-01) | Nothing on the units is kept. A unit starts empty: no systems, the default radio settings, an empty history. p25-httpd's files and history are never read. | Migrate p25-httpd's files and history once (as first built). |

## 1. Where things stand

The inventory ran in six parts: call tracking and audio; configuration; history and recordings;
API and UI; protocol, hardware and dead code; FPGA offload.

### Size

- 63.5k non-test lines, 13.1k test lines and 4.6k UI lines.
- `AppState` has 65 fields (54 at the 09-28 review).
- The router has 95 routes; 7 handlers return typed `p25-json` and there are 249 ad-hoc `json!`
  sites.
- 1,216 comment lines carry dated or "Change 0NN" narrative. The DMR and jmbe ports are the clean
  reference style.

### What runs where

- **Configuration is spread over five files and five writers.** None of them has a version.
  Four write atomically; the PPM file does not. The active site is held in five places: `active.json`,
  `AppState.active_site`, `PlanStore`, the `ACTIVE_SITE` static and `UiSettings.site`. The site
  file is read five times at boot.
- **The site overlay is never saved from the air.** `save_site` has one caller, discovery's "Add".
  IDEN bands, NAC and identity are relearned after every switch.
- **A site switch does not retune.** `POST /api/site` swaps state and resets the P25 decoders; the
  browser then calls `POST /api/preset auto`. On a DMR site, the LSM and C4FM control decoders,
  the P25 follower, the traffic heartbeats and autoppm keep running. Lane One then has two
  owners: the P25 follower and the DMR executor.
- **A call lives in six places:**
  - the lifecycle's `ActiveCall` and `NfCalls`;
  - the snapshot mirror;
  - the grant_stats `ActiveSummary` and rings;
  - the counter book;
  - the forwarder's per-lane atomics;
  - the recorder's own `ActiveCall`, with its own start clock.

  They are joined by five broadcast or mpsc channels and four side channels between the
  lifecycle and the follower. `voice_ms` has three definitions.
- **DMR impersonates P25:**
  - grants become `CcGrantArrival` on lane One with `nac: 0`;
  - voice becomes `TrafficNidObserved` every 500 ms;
  - the radio from link control becomes `TdulcComplete`.

  DMR has a second pacer and a second copy of the AGC. Profiles, the monitor list and the grant
  tallies do not apply to it.
- **The history polls rings every 30 s.** The two grant tallies (`grant_map`, `lo_plan`) count
  separately. Recordings have no index file: it is rebuilt from file names at boot and joined to
  the history by call id ±10 s.
- **No tuner owns the radio.** About 12 code paths move the LO or NCOs. The NCO formula
  `f − lo + shift` is written about 15 times. `RadioLease` has two states and only the sweep
  takes it.

### Defects found by the inventory

Items marked ✓ were checked by hand for this design. The rest were read in the code by the
inventory and will be confirmed when their phase touches them. "Fixed in" is the phase of
section 15 whose new code no longer has the defect.

| # | Defect | Effect | Fixed in |
|---|--------|--------|----------|
| 1 ✓ | A not-followed call is pushed with `ended = started`; the history stores it 15 s later and its `seen` set blocks the update | Encrypted or not-followed calls longer than ~15–45 s are stored with 0 grant time | 3 (the call model); 5 (history stores settled calls only) |
| 2 ✓ | `Slot::holds` compares the frequency only | A DMR grant on the other timeslot of the followed repeater ends the followed call | 3 |
| 3 ✓ | DMR grants are sent with `encrypted: false`; `FollowerAction::Encrypted` maps to nothing | Encrypted DMR calls are stored as clear with no voice | 3 |
| 4 ✓ | DMR voice keep-alive every 500 ms; the lifecycle's resume needs two within 400 ms | A DMR call in its end grace never resumes on voice | 3 |
| 5 | The enc/not-followed ring holds 50 and is polled every 30 s | A burst of over 50 such calls is never stored | 3 |
| 6 | SIGTERM flushes the history only | Active and queued SD recordings are lost; up to 10 min of plan grant counts are lost | 1 (shutdown); 4 (recordings) |
| 7 ✓ | The PPM calibration is written with a plain `fs::write` | A power cut during the write loses the calibration | 1 |
| 8 | The follower is spawned before the lifecycle subscribes | Calls in the first moments after boot are lost | 1 |
| 9 ✓ | The software C4FM control demod decodes every chunk at any site | 12–17 % of a core spent on a P25 decoder at DMR sites | 2 |
| 10 | The follower writes `current_source` for any grant on the locked talkgroup | A queued grant's radio leaks into the current call's sources and AGC resets | 3 |
| 11 | Not-followed reasons are a `&'static str` list in the refill | Reasons such as `out_of_band`, `busy` and `unknown_lcn` become "not_followed" after a restart | 3 |
| 12 | The tracker broadcast holds 64 for five subscribers, and each handles lag differently | Lag can leave a lane locked, lose a summary from the history, or leave DMR's call id stale | 3 |
| 13 | The recorder stamps its own start time | Recordings join calls by ±10 s, with a full table scan per file at boot | 4 |
| 14 | `trim_to` deletes calls but keeps their hour totals | Activity totals include calls that are gone | 5 |
| 15 | `lo_plan` clears `dirty` before the write | A failed write is never retried | 1 |
| 16 | An unaligned `&[i16]` cast in `wideband_iq_task.rs:230` | Undefined behaviour | 1 (the capture is ported without it) |
| 17 ✓ | The recorders and grant_stats subscribe to the call events after awaits at boot | Calls that open or close in the first seconds after boot get no recording or history row | 3 |
| 18 ✓ | `apply_preset` reloads both traffic DDCs with NCO 0 but `TrafficChain` skips the NCO write for a grant on its parked frequency (a word computed from the boot LO) | After a recentre, site switch or sweep, the first call on a lane's parked channel was silent | Fixed in p25-httpd (3e7983b); the Tuner tracks what each lane's NCO holds |
| 19 ✓ | The autoppm updater, the ppm endpoints, `/api/rx_gain`, the debug retunes, the DMR executor, `release_chains_on` and the PLL watchdog move hardware without the lease | A sweep's measurements and a site switch can be disturbed | 1 (only the Tuner moves hardware) |
| 20 ✓ | AD9361 writes take no lock; the follower and DMR read the LO and shift atomics lock-free | A traffic NCO can be computed against an LO that is changing | 1 (the Tuner serialises every hardware sequence) |

## 2. Module map

The target is the code review's layout, adjusted as the brief asks. Four additions:

- **`radio/`:** the only code that moves hardware (the review's H2).
- **`audio/`:** the one voice path.
- **`boot/`:** what `main.rs` does inline today.
- **`util/`:** time and atomic files.

```text
scanner/src/         the fresh crate (D13)
  main.rs            parse the arguments, boot::run()
  boot/              args, logging and the panic policy, version (BUILD_TAG), radio (open the
                     AD9361 and the radio core, start the lane ring reader, build the tuner),
                     state (AppState: handles to the services), mod (start-up: configuration,
                     storage, services, the live site; shutdown flushes the recordings, history
                     and learned state)
  util/              time (unix_ms, Stamp {mono, unix}, iso), atomic_file (tmp + fsync + rename)
  hardware/          drivers only
    ad9361.rs        IIO
    mmio.rs          UIO devices and maia-kmod's rxbuffer rings
    radiocore/       regs (the lane banks), lane (one DDC and its packets' enable and tag),
                     packet (the lane ring's packet, checked), irq (acknowledged; the reader
                     polls), mod (the core: identity, lanes, lane ring, sample count, spectrum)
    presets/         DDC presets (a generated table) and their FIR RAM images
  radio/             the only way to move hardware
    tuner.rs         apply(TuningPlan), set_control, retune_lane, set_crystal_ppm; the crystal
                     LO shift; what each lane's NCO holds
    lease.rs         Normal | Switching | Scan | Atsc (held while ATSC mode lasts)
    hw.rs            the board's RadioHw (a new tuning takes a new tag); on a development host
                     a stand-in with no radio
    lane.rs          the traffic lanes
    plan.rs          the window planner
    streams/         the lane ring's reader and the hub: each lane's IQ of its current tuning
                     to its subscribers, in blocks with an air time
  dsp/               taps (SDRTrunk's), fsk4 (DifferentialDemod, ideal_phase, interpolation)
  protocol/
    events.rs        ControlEvent, TrafficEvent, LogicalChannel, SiteIdentity (the decoders'
                     common output)
    fec/             the codes both use: Golay(24,12), the Hamming codes
    p25/             framer, tsbk, control decoder, traffic decoder, voice_frame, pdu, c4fm demod,
                     fec (BCH NID, Reed-Solomon, trellis)
    dmr/             demod, framer, fec (bptc, cach, emb, slot type, crc, RS(12,9)), message,
                     control (Tier III), traffic
    atsc/            the US TV channel plan, the windows that read it, and one channel's
                     spectrum: the 8-VSB pilot, the plateau, the floor at its edges; the 8-VSB
                     receiver (demod, fec: trellis, deinterleaver, Reed-Solomon) and PSIP
                     (ts, psip), together in receiver: a capture to its station's names
  trunking/          protocol-neutral, host-tested
    follow/          one follower: ordered gates, lane choice, pre-emption; routing (aliases)
    calls/           Call, CallBook (lifecycle and counters), CallEvent
    trunk.rs         the trunking task: follower, call book and each lane's traffic decoder; the
                     calls view; the last lane on the data channel between calls
    site.rs          LiveSite: activate(), recentre(), the live state
    learned.rs       what a site teaches: bands, grants, encrypted talkgroups, neighbours, its
                     other channels
    receivers.rs     runs the live protocol's control decoders on their streams, feeds events in
  audio/
    codec/           VoiceCodec; imbe (jmbe port), with ambe (jmbe's AMBE+2)
    agc.rs           PcmAgc, the only copy
    live.rs          per lane: frames, codec, AGC, pacer; the audio broadcast
  services/
    config/          radio, systems, aliases, radioreference, state, ids
    history/         schema v2, store, the writer
    recordings/      the recorder, storage (RAM/SD), index, wav
    discovery/       carriers, probes (P25 and DMR), the sweep, grouping and merge
    mode.rs          the unit's mode: the scanner or ATSC TV (holds the lease, pauses the site)
    atsc/            ATSC mode's TV scan, each channel's spectrum, and its station named
    clock/           site clock, internet time, the board clock
    crystal.rs       crystal calibration and tracker (autoppm)
    packet_data.rs   P25 packet data records
    events.rs        the event log
    notices.rs       what /ws/events sends
  api/               one route table builds the router and doc/API.md; ApiError
    v1/              status, radio, systems, aliases, sites, hold, calls, recordings, activity, data,
                     spectrum, scan, events, mode, atsc
    ws.rs            /ws/live, /ws/audio, /ws/events
    legacy.rs        p25-httpd's routes the bench reads, in their old shape
  ui/                mod.rs and the static files (index.html, js/, css/)
```

Where today's files go. In the fresh crate, "goes to" means ported (leaf code, with its tests) or
rewritten there (glue); "delete" means not carried over.

| Today | Goes to | Notes |
|-------|---------|-------|
| `app/grant_follower.rs` (1711) | `trunking::calls` (lifecycle) + `trunking::follow` (pure policies) | Split |
| `app/grant_follower_routing.rs` (1138) | `trunking::follow` + `trunking::trunk` | The 650-line `select!` arm becomes ordered gate functions |
| `app/grant_stats.rs`, `call_counters.rs` | `trunking::calls` | Absorbed by the CallBook |
| `app/imbe_forwarder.rs` (1723) | `protocol::p25` traffic decoder + `trunking::calls` + `audio::pipeline` | Split; its 86 fields mostly go |
| `app/dmr_task.rs` (994) | `protocol::dmr` decoders + `trunking::receivers` + api DTO | Split; the lifecycle bridge goes |
| `app/dmr_follower.rs` | `trunking::follow` | Merged into the one follower |
| `app/dmr_voice.rs`, `vocoder_task.rs`, `audio_pacer.rs` | `audio::{pipeline, agc, pacer}` | Two AGCs and the second pacer go |
| `app/c4fm_task.rs` | `trunking::receivers` (the runner and the LSM/C4FM choice) | |
| `app/autoppm.rs`, `recentre_task.rs` | `services::crystal`; `LiveSite::recentre` | Call the Tuner, not `api::tuning` |
| `app/discovery*.rs` | `radio::lease` + `services::discovery` | Split |
| `app/iq_hub.rs`, `dibit_readers.rs`, `dibit_airtime.rs` | `radio::streams` | |
| `app/traffic_heartbeat.rs` | `hardware::p25core` register read + `protocol::p25` HDL source | Split |
| `app/lane_policy.rs`, `traffic_lane.rs` | `trunking::follow`, `trunking::trunk`, `boot` | |
| `app/history_task.rs` | `services::history::writer` | No ring polling |
| `app/data_task.rs` | `services::packet_data`; the data channel in the site's learned state | The `DATA_CHANNEL_HZ` static goes |
| `app/clock_task.rs` | `services::clock` | |
| `app/ui_state.rs` | `api::v1` | |
| `app/seed_snapshot.rs`, `forensics.rs`, `traffic_pll_watchdog.rs`, `sw_demod_task.rs` | **delete** | Retired scaffolding |
| `app/wideband_iq_task.rs` | capture to `radio::streams`; the sw_demod tee is deleted | Fix the unaligned cast |
| `audio/recorder.rs`, `rec_storage.rs` | `services::recordings` | `rec_storage` stops calling `api::system::fs_usage` |
| `audio/mod.rs` | `audio::live` + `trunking` events | `CallBoundary` is replaced |
| `hardware/fpga.rs` (2444) | `hardware::p25core::*` | Split. The two private FIR loaders are replaced by `ddc_fir_ram` (quick win 2) |
| `httpd/mod.rs` | `boot::state` (AppState as handles) + `api` (router) | 54 % comments today |
| `httpd/api/tuning.rs` (1730) | `radio::tuner` (apply_preset, post_tune, ppm) + `api::v1::radio` | Domain logic leaves the handler |
| `httpd/api/ui.rs` | `services::config` (`apply_settings_patch`) + `api::v1` | |
| `httpd/api/sites.rs` | `trunking::site::activate` + `api::v1::sites` | |
| `httpd/api/chain.rs`, `debug.rs` | `api::diag` | Not built yet (status log) |
| `httpd/ui/`, `ui_assets.rs` | `ui/` | |
| `jmbe/`, `vocoder/` | `audio::codec::{imbe, ambe2}` | |
| `lsm/`, `sw_demod/` | `dsp-lab` dev crate | First move `RRC_TAPS_25K` and `Complex32` (used by production code) to `dsp` |
| `protocol/p25/c4fm.rs` | `dsp::fsk4` (shared parts) + `protocol::p25` | DMR borrows them through `pub(crate)` today |
| `protocol/p25/control_channel/mod.rs` (1756) | `protocol::p25::{framer, control, traffic, diag}` | Split; the calls into app and services become events |
| `protocol/p25/events.rs` | **delete** | Replaced by `ControlEvent` |
| `protocol/p25/traffic_chain.rs` | `radio::tuner` (what each lane's NCO holds) | `grant_map` and its 2 s dedup go |
| `services/ui_settings.rs` (1318) | `services::config::{radio, aliases}`, recordings, calls, clock | Split |
| `services/sites.rs`, `lo_plan.rs`, `monitor.rs` | `services::config`, `radio::plan` | The statics go |
| `services/history.rs` | `services::history::store` | Schema v2 |

Each module gets a short header saying what it owns, and comments state intent only. History stays
in git and `CHANGELOG_FORK.md`, never in the code.

## 3. Configuration

### 3.1 Files

All files are versioned JSON, written by `util::atomic_file`: tmp, fsync, rename, then fsync the
directory. A missing file is its default. A file whose `version` is newer than the binary
understands is read but never written, so a downgrade cannot drop fields.

| File | Owner | Holds | Written |
|------|-------|-------|---------|
| `/mnt/jffs2/scanner/radio.json` | `services::config::radio` | Gain mode, presets allowed, traffic chains, call hang/grace, recording policy (every call, or only aliases that say record) and storage, history limits, clock source | On a user change |
| `/mnt/jffs2/scanner/systems.json` | `services::config::systems` | Systems: protocol, identity, label, details (location, county, type, voice: descriptive only), aliases and listening settings (`services::config::aliases`), and their sites (identity, control channels, alternates, known channels, channel plan, window policy, modulation) | On a user change or a scan "Add" |
| `/mnt/jffs2/scanner/state/radio.json` | `services::config::state` | Live site; crystal calibration | On a switch; on a calibration |
| `/mnt/jffs2/scanner/state/sites/<id>.json` | `services::config::state` | Learned per site: identity seen, IDEN bands or LCNs, neighbours, secondary CCs, grant counts per channel, known-encrypted talkgroups, last recentre | Every 10 min if changed, on a switch, at shutdown |

Learned state is loaded when the site goes live. That fixes the 073 open item where a switch left
grants waiting for the IDEN broadcast.

IDs are slugs (`[a-z0-9_-]`, validated everywhere). A scan makes a site's id from its system's
label and its own; the history rows and recording file names carry it.

### 3.2 Schemas (examples)

`radio.json`:

```json
{
  "version": 1,
  "gain": { "mode": "slow_attack", "manual_db": null },
  "presets_allowed": ["8M", "12M", "16M"],
  "traffic_chains": 2,
  "calls": { "hang_ms": 3000, "end_grace_ms": 2000 },
  "recording": { "enabled": true, "every_call": true, "storage": "sd", "ram_max_count": 40,
                 "sd_max_count": 2000, "sd_max_mb": 2048 },
  "history": { "retention_days": 365, "sd_max_mb": 2048 },
  "clock": { "source": "site" }
}
```

`systems.json`. Identities are stored as numbers; the UI shows them in hex.

```json
{
  "version": 1,
  "systems": [
    {
      "id": "clay_county", "label": "Clay County Public Safety", "protocol": "p25",
      "identity": { "wacn": 781824, "system": 2208 },
      "details": { "location": "Green Cove Springs, FL", "county": "Clay",
                   "system_type": "Project 25 Phase I", "voice": "APCO-25 Common Air Interface Exclusive" },
      "aliases": [
        { "name": "EMS Dispatch", "group": "Primary", "ids": [{ "type": "talkgroup", "value": 300 }],
          "priority": 1, "do_not_monitor": false, "record": true, "speaker": "left" },
        { "name": "TAC", "ids": [{ "type": "talkgroup_range", "min": 301, "max": 310 }],
          "priority": 2, "do_not_monitor": false, "record": false, "speaker": "right" },
        { "name": "Console 14", "ids": [{ "type": "radio", "value": 1014 }],
          "priority": null, "do_not_monitor": false, "record": false, "speaker": "both" }
      ],
      "listening": { "follow_unmonitored": true, "unmonitored_speaker": "both", "preempt": true },
      "sites": [
        {
          "id": "clay_county_site_1", "label": "Site 1",
          "identity": { "rfss": 1, "site": 1, "nac": 2209, "lra": 0 },
          "control": { "freq_hz": 860962500, "alternates_hz": [859437500, 858987500, 860437500] },
          "modulation": "auto",
          "channels_hz": [852438500, 855237500, 856437500],
          "window": { "auto": true, "min_preset": "12M", "cc_position": "top" }
        }
      ]
    },
    {
      "id": "clay_electric", "label": "Clay Electric", "protocol": "dmr_tier3",
      "identity": { "model": "small", "network": 0 },
      "aliases": [
        { "name": "Lake City", "ids": [{ "type": "talkgroup", "value": 87921 }],
          "priority": null, "do_not_monitor": false, "record": false, "speaker": "both" }
      ],
      "listening": { "follow_unmonitored": true, "unmonitored_speaker": "both", "preempt": true },
      "sites": [
        {
          "id": "clay_electric_green_cove_springs", "label": "Green Cove Springs",
          "identity": { "site": 2, "colour_code": 0 },
          "control": { "freq_hz": 454368750, "lcn": 5, "timeslot": 1 },
          "channel_plan": { "lcn_hz": { "5": 454368750, "6": 451087500 } },
          "window": { "auto": true, "min_preset": null, "cc_position": "top" }
        }
      ]
    }
  ]
}
```

### 3.3 Aliases

A system's aliases are what the radio knows of its talkgroups and radios (D15). The follower,
the recorder and every name shown read them, for P25 and DMR alike:

- **Lookup:** an exact ID first, then the first range that holds it.
- **Do not monitor:** never followed.
- **Priority:** 1 is the highest, 100 the lowest. With `preempt`, a call of a higher priority
  takes a traffic channel from a call of a lower one.
- **No priority** (no alias, or an alias without one): followed at the lowest rank on
  `unmonitored_speaker` while `follow_unmonitored` is on, else not followed (`unmonitored`).
- **Speaker:** the traffic channel and the side a talkgroup plays on (`both`, `left`, `right`).
- **Record:** with `recording.every_call` off, only these talkgroups' calls are recorded.
- **Hold** (`/api/v1/hold`): one talkgroup followed whatever its alias says, until released or a
  site switch.

`/api/v1/systems/{id}/aliases` reads and replaces the list, `/listening` the settings, and
`/talkgroups/{tg}` sets one talkgroup's controls from the live screen. The live site follows
each change at once.

### 3.4 RadioReference files

RadioReference's CSV downloads import into a configured system
(`POST /api/v1/systems/{id}/radioreference`; `/preview` shows what would change and saves
nothing). The header row says which file it is, and a file is checked whole first
(`config::radioreference`).

- **Talkgroups** (`trs_tg_*.csv`: decimal ID, alpha tag, mode, category): an alias for each
  talkgroup no alias covers yet, named by the alpha tag and grouped by the category, as
  SDRTrunk's import makes them. Talkgroups an alias already covers are kept. Fully encrypted
  talkgroups (mode `E`; `e` is partly) are never followed unless `encrypted_do_not_monitor` is
  false (SDRTrunk's default too).
- **Sites** (`trs_sites_*.csv`: RFSS or region, site, NAC, description, then the frequencies,
  control channels marked `c`). The file's protocol must be the system's: an RFSS and NAC mean
  P25, a region DMR.
  - A new site starts on the first `c` frequency, with the others as alternates and every other
    frequency as a known channel. The county, location and range go in its notes.
  - A configured site (a P25 site by RFSS and site, else by a control channel within 3 kHz)
    gains the channels it lacks and a missing NAC; its name and control channel stay.
  - `sites` limits the import to the rows a preview listed and the user ticked.
- **Not in the files:** the system's WACN and system ID (the scan that added the system gave
  them) and a DMR site's LCN plan (the site editor).
- **Alternate control channels:** RadioReference lists every channel that can carry control,
  and the receiver stays on the site's one control channel. A new site whose first `c` is not
  the one in use needs its control channel set in the site editor.

### 3.5 A new unit

- Nothing is migrated (D16). With no `/mnt/jffs2/scanner/`, the radio has no systems and the
  default settings; the history and the recordings start empty on the card.
- The radio boots to **NoSite**:
  - the AD9361 is configured but idle;
  - no decoders run;
  - the UI offers the scan, a manual add or an import.
- A factory reset (`POST /api/v1/config/factory-reset`) returns a unit to this state; the
  crystal calibration stays (it is the board's).

## 4. The live site and multi-band

"Which site is live" becomes one state owned by `trunking::site::LiveSite`, published on a
`watch`:

```text
NoSite ──activate(s)──▶ Switching{to: s} ──▶ Live{site, system, routing, plan}
Live ──activate(t)──▶ Switching{to: t} ──▶ Live{t ...}
Live ──scan──▶ Scanning (radio lease) ──▶ back to Live{same}
```

`activate(site)` is the only way to change site:

1. Take the radio lease (`Switching`). Grants are dropped from here (today's 073 hold).
2. CallBook: close open calls with reason `site_switch`. They keep their own site.
3. Stop the old protocol's receivers: control decoder, traffic decoders, its demod threads.
4. `Tuner::apply(plan)`: AD9361 LO, rate and bandwidth, control NCO, lanes idle, scaled crystal
   shift.
5. Load the site's learned state (channel plan, identity, encrypted list) and its system's aliases.
6. Start the new protocol's receivers with that context.
7. Release the lease, publish `Live`, log "site switched", persist `live_site`.

Boot is `activate(live_site)` or `NoSite`. `POST /api/v1/sites/{id}/activate` returns once the
site is live. The old `POST /api/site` does the whole switch too, so the browser's second call
(`preset auto`) becomes harmless and `applied_preset` tells the truth.

Room for scanning between sites: a later `SiteSelector` (priority list, dwell, hold) only calls
`activate`. Nothing else needs to change for it, and 076 does not build it.

## 5. Protocols behind traits

Protocol modules stay pure and synchronous, and are tested on the host. Runners in
`trunking::receivers` own the threads and feed them the stream they ask for. Data arrives in two
shapes, and the traits accept both:

- **HDL LSM dibits:** DMA words with air-time epochs; P25 control and both traffic chains.
- **Software IQ:** 50 kSPS from the IQ hub; P25 C4FM control, DMR control and traffic.

```rust
pub enum Protocol { P25, DmrTier3 }
pub enum RxInput<'a> { Iq(&'a [i16]), Dibits { dibits: &'a [u8], epoch: AirtimeEpoch } }

pub trait ControlDecoder: Send {
    fn protocol(&self) -> Protocol;
    fn input(&self) -> InputKind;                 // HdlDibits | Iq
    fn push(&mut self, input: RxInput, now: Instant, out: &mut Vec<ControlEvent>);
    fn retuned(&mut self);
    fn new_system(&mut self);
    fn health(&self) -> ControlHealth;            // msgs/s, ok %, last message age, CPU
}
pub trait TrafficDecoder: Send {
    fn follow(&mut self, ch: LogicalChannel, expect: Expect);   // NAC lock, or DMR slot + colour code
    fn push(&mut self, input: RxInput, now: Instant, out: &mut Vec<TrafficEvent>);
}
pub trait VoiceCodec: Send {                      // Imbe (jmbe), Ambe2 (jmbe AMBE)
    fn frame_bits(&self) -> usize;                // 144 / 72
    fn decode(&mut self, frame: &[u8], pcm: &mut [i16; 160]) -> FrameQuality;
    fn reset(&mut self);
}

pub struct LogicalChannel { pub id: ChannelId, pub slot: Option<u8>, pub freq_hz: Option<u64>, pub tdma: bool }
pub enum ChannelId { P25 { iden: u8, number: u16 }, DmrLcn(u16), Absolute }

pub enum ControlEvent {
    Grant(Grant),                 // group/unit/data; tg u32; source; channel; encrypted; emergency;
                                  // update; not_followable (Phase 2 today)
    ChannelRelease { channel: LogicalChannel, tg: Option<u32> },   // DMR P_CLEAR
    Identity(SiteIdentity),       // P25 {wacn, system, rfss, site, nac, lra} | Dmr {model, network,
                                  // site, colour_code}; observed_at
    ChannelPlan(PlanEntry),       // P25 IDEN band (FDMA/TDMA) | DMR LCN → Hz
    Neighbour(Neighbour), SecondaryControl(LogicalChannel), DataChannel(LogicalChannel),
    SiteTime(SiteSync),
    Unit { unit: u32, group: Option<u32>, kind: UnitKind },        // affiliation, (de)registration
    Pdu(PduFrame),
    Message(LogLine),             // SDRTrunk text; the one Diagnostics events feed
}
pub enum TrafficEvent {
    Sync { id: u32 },             // NAC / colour code: the channel carries this system
    Header { tg: u32, source: Option<u32>, enc: Option<Encryption> },
    LinkControl { tg: u32, source: Option<u32>, encrypted: bool, emergency: bool },
    Encryption(Encryption),       // alg, key id (LDU2 ESS / DMR PI)
    Voice { codec: Codec, frames: SmallVec<[Frame; 9]>, air_ms: u64 },
    End { kind: EndKind, source: Option<u32> },   // TDU, TDULC call term / talk complete,
                                                  // DMR terminator, CLEAR
    Pdu(PduFrame),
}
```

Rules:

- **Only the live protocol's receivers run.** At a DMR site, the HDL LSM decoder, C4FM, autoppm's
  LSM measurement and the P25 clock source stop. That saves 12–17 % of a core (defect 9).
  Autoppm at DMR sites reads the DMR equaliser's carrier offset.
- **Side effects become events.** Today the P25 decoder calls into `data_task` and `site_clock`;
  those become `DataChannel` and `SiteTime` events.
- **Lanes have capabilities** (`Setup::lanes`, by protocol). The follower picks only lanes that can carry the
  grant:

  | Lane | Capabilities |
  |------|--------------|
  | Chain 1 | HDL dibits and IQ: P25 and DMR |
  | Chain 2 | HDL dibits: P25 only |
  | Control slot | The control receiver's other timeslot: DMR grants to the control repeater's TS2 |

  With change 079's core every lane carries IQ and any protocol, and the table goes away.
- **SDRTrunk constants and texts stay as they are.** The traits wrap the ported code; they do not
  rewrite it. The DMR reference (24,984+ of 24,996 lines) and the P25 tests gate every step.

## 6. The call model

One `Call` per call, in `trunking::calls`:

```rust
pub struct Call {
    pub id: CallId,                     // u64, continues past the history's highest id
    pub site: SiteId,
    pub protocol: Protocol,
    pub target: Target,                 // Group(TalkgroupId) | Unit(UnitId)
    pub channel: LogicalChannel,        // freq, P25 channel id or DMR LCN, timeslot
    pub follow: Follow,                 // Followed { lane } | NotFollowed(Reason enum)
    pub phase: Phase,                   // Granted → Voice ⇄ Hang → Closed(CloseReason) → Settled
    pub granted_to: Option<UnitId>,
    pub transmissions: Vec<Transmission>,   // unit, first/last voice, frames
    pub encryption: Encryption,         // grant flag, in-band alg/key, known-encrypted TG
    pub times: Times,                   // Stamp {mono, unix}: opened, first/last voice,
                                        // last CC update, end marker, closed
    pub voice: VoiceStats,              // codec, frames, errors, silent, dropped, chunks, AGC gain
    pub detail: ProtocolDetail,         // P25 {nac, hdu, ldu1, ldu2, tdu, tdulc, end_lc} |
                                        // Dmr {bursts, superframes, end}
    pub recording: Recording,           // Off | Pending | Saved(RecordingRef) | Discarded(why)
    pub rev: u64,
}
```

**Owner: the `CallBook`**, one task and the only writer. It merges the lifecycle, grant_stats,
the counter book and the forwarder's call state.

**Inputs** come over one bounded, lossless mpsc:

- grant decisions from the follower (followed or not, which lane);
- control updates (grant repeats, releases);
- traffic events per lane (voice frames batched per LDU or burst, link control, encryption, end);
- recording results;
- the site switch;
- a 100 ms tick.

**Rules.** The lifecycle rules move unchanged from `grant_follower.rs`: hang, end grace, voice-NID
pairing, the queued hand-over and merged not-followed repeats. Two things change:

- they run on `Instant` (the review's H3; a clock step no longer closes calls);
- the channel compares (freq, timeslot), not the frequency alone (defect 2).

**Outputs:**

- `watch<Arc<CallsView>>`: open calls per lane, the recent ring (one ring, refilled from the
  history at boot) and a rev. The follower's gates, the HTTP API and the UI read it.
- Dedicated mpsc channels to the two durable consumers. The recorder gets `Opened` and `Closed`;
  the history gets `Settled`. A Settled call is closed, its 2 s drain is over, and the recorder
  has reported the recording or its absence.
- A `broadcast<CallEvent>` for `/ws/events` (`call_opened`, `call_closed`, `recording_saved`).
  The UI refreshes on these for every protocol.

**One follower.** `trunking::follow` makes protocol-neutral decisions over `Grant`s:

- the gates today's 650-line arm applies, in the same order, as small functions: lease, hold,
  update fast path, refollow, ignore, not-followable, monitor, speaker route, out of band, channel
  reuse, encrypted;
- then lane choice by capability, and pre-emption;
- one release sequence through `LaneController`.

DMR's `dmr_follower` logic (a talkgroup's transmissions moving between LCNs and slots) is grant
handling the shared follower already does: each new grant is a new call, as for P25 since 057.
The 20:57 Clay Electric call is the test.

**Merged:**

- the two grant tallies become one count per channel, taken at `Opened`, feeding the planner;
- the monitor roster comes from the history's `talkgroups` table plus the recent calls;
- the encrypted list moves to the site's learned state, so it now survives a restart.

**Deleted:**

- `CallBoundary`, `CallTrackerEvent`, `ActiveCallSnapshot`/`mirror_active`;
- the grant_stats rings, `synthetic_not_followed_summary` and the refresh;
- `call_counters`;
- the forwarder's `current_*` and `call_baseline_*` atomics, the end-marker mutex and
  `vocoder_reset_pending`;
- the recorder's call windows and draining slot;
- `LifecycleLink` and `boundary_events` in `dmr_task`;
- `TrafficChain.grant_map`;
- the dead variants (`StreamLag`, `SyncLost`, `ArrivalDisposition::Ignore`, `BareTdu`, ...).

## 7. One audio path

- `audio::pipeline`, one per lane: `VoiceBatch {call_id, codec, frames, encrypted}`, then the
  `VoiceCodec`, then the one `PcmAgc`, then 20 ms chunks, then that lane's pacer. DMR voice feeds
  its lane's pipeline, so the second pacer and the second AGC go.
- **AGC reset rule:** P25's (a talkgroup or call change; a speaker change when both IDs are known),
  applied to both protocols. DMR resets on any source change today, including None → Some.
- **AGC start:** each transmission starts at the gain its radio's speech level needs: the
  median of the radio's last 5 transmissions, none older than 30 minutes (`audio::levels`,
  shared by the lanes; a transmission that carried an alert tone is not counted). The first
  500 ms of voice track fast, then over about a second; the gain spans x0.25 to x16.
- **Speaker routing comes from one place.** The server already picks lanes from the aliases. It
  now also sends the speaker (`left` / `right` / `both`) in the `/ws/audio` meta frame, so the
  browser stops recomputing routing from the settings.
- The recorder routes chunks by `call_id` only. Every chunk carries one, DMR included.
- The lifecycle's voice keep-alive comes from the traffic decoder, not from the paced audio. It
  stops seeing voice late, after the vocoder and pacer.

## 8. History

One service, `services::history`:

- **One writer thread** owns the write connection and is fed by an mpsc: settled calls, unit
  events, recordings, sites seen, prune, flush. It commits every 10 s or on flush; the WAL
  reasons for batching (072) still hold.
- **A second, read-only connection** with a busy timeout serves the API. A CSV export no longer
  blocks the writer.

Schema v2:

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);   -- schema, created
CREATE TABLE systems (id TEXT PRIMARY KEY, protocol TEXT NOT NULL, label TEXT, identity TEXT);
CREATE TABLE sites (id TEXT PRIMARY KEY, system TEXT REFERENCES systems(id), label TEXT,
  identity TEXT, calls INTEGER NOT NULL DEFAULT 0, first_ms INTEGER, last_ms INTEGER);  -- was site_stats
CREATE TABLE calls (
  id INTEGER PRIMARY KEY, site TEXT NOT NULL, call_id INTEGER NOT NULL,
  started_ms INTEGER NOT NULL, ended_ms INTEGER NOT NULL,
  target TEXT NOT NULL DEFAULT 'group', tg INTEGER NOT NULL, source INTEGER,
  freq_hz INTEGER, channel TEXT, timeslot INTEGER, lane INTEGER NOT NULL DEFAULT 0,
  encrypted INTEGER NOT NULL, enc_alg INTEGER, enc_key INTEGER,
  followed INTEGER NOT NULL, not_followed TEXT,
  voice_ms INTEGER NOT NULL, grant_ms INTEGER NOT NULL,
  codec TEXT, frames INTEGER NOT NULL DEFAULT 0, frame_errors INTEGER NOT NULL DEFAULT 0,
  close_reason TEXT, end_kind TEXT,
  UNIQUE(site, call_id, started_ms));
CREATE INDEX calls_site_time ON calls(site, started_ms);
CREATE INDEX calls_site_tg ON calls(site, tg, started_ms);
CREATE INDEX calls_time ON calls(started_ms);
CREATE INDEX calls_call_id ON calls(call_id);
CREATE TABLE transmissions (                                   -- was call_units
  call INTEGER NOT NULL REFERENCES calls(id) ON DELETE CASCADE, site TEXT NOT NULL,
  unit INTEGER NOT NULL, started_ms INTEGER NOT NULL, ended_ms INTEGER, voice_ms INTEGER,
  primary_src INTEGER NOT NULL);
CREATE TABLE talkgroups (system TEXT, tg INTEGER, first_ms INTEGER, last_ms INTEGER,
  calls INTEGER, first_encrypted_ms INTEGER, last_encrypted_ms INTEGER, last_clear_ms INTEGER,
  PRIMARY KEY(system, tg)) WITHOUT ROWID;
CREATE TABLE radios (system TEXT, unit INTEGER, first_ms INTEGER, last_ms INTEGER, calls INTEGER,
  PRIMARY KEY(system, unit)) WITHOUT ROWID;
CREATE TABLE radio_events (...);   -- was unit_events, same columns
CREATE TABLE tg_hour (...);        -- was hour_tg, same columns
CREATE TABLE radio_hour (...);     -- was hour_unit, same columns
CREATE TABLE recordings (id INTEGER PRIMARY KEY, file TEXT NOT NULL UNIQUE, store TEXT NOT NULL,
  site TEXT, call_id INTEGER, started_ms INTEGER NOT NULL, tg INTEGER, source INTEGER,
  bytes INTEGER NOT NULL, duration_ms INTEGER,
  call INTEGER REFERENCES calls(id) ON DELETE SET NULL);
CREATE TABLE alerts (site TEXT NOT NULL, call_id INTEGER NOT NULL, call_started_ms INTEGER NOT NULL,
  at_ms INTEGER NOT NULL, tg INTEGER NOT NULL, source INTEGER, lane INTEGER NOT NULL DEFAULT 0,
  kind TEXT NOT NULL, tones TEXT NOT NULL, segments INTEGER NOT NULL, offset_ms INTEGER NOT NULL,
  duration_ms INTEGER NOT NULL, UNIQUE(site, call_id, call_started_ms, offset_ms));  -- v4
CREATE TABLE data_packets (...);   -- D11
```

Rules:

- **Rollups** stay incremental, in the same transaction as the call.
- **Retention trims whole hours**, so the rollups always match the calls (defect 14). The limits
  are the same, now set in `radio.json`.
- **Codec-neutral names:** `frames` and `frame_errors` (were `imbe` and `vocoder_errors`), plus
  `codec`. New columns: `protocol` (through the site's system), `timeslot`, `target`,
  encryption alg/key, `end_kind`.
- **Activity endpoints** return the same JSON. A `system` filter is added.

## 9. Recordings

One module, `services::recordings`, driven by the call:

- **Start:** a followed call `Opened` with recording on.
- **Audio:** chunks appended by `call_id`.
- **Finish:** on `Closed` plus the 2 s drain, write the WAV, then report `Saved(ref)` or
  `Discarded(why)` to the CallBook. The history row then carries the recording.
  - An encrypted call is `Discarded("encrypted")`, not "saving" then "missing" (defect 3's
    in-band side).
- **Storage** is today's `rec_storage`: RAM ring, SD writer thread with `.part`, fsync and
  rename, the fallback to RAM, and the retention limits. It moves; it is not rewritten.
- **Naming is unchanged:** `rec_<start_ms>_<call_id>_tg<tg>_from<src>.<site>.wav`. The start is
  the call's own `opened.unix`, not the recorder's clock (defect 13).
- **The index** is the history's `recordings` table. At boot, the directory listing (names only:
  on the card's FAT each file's size costs a scan of the whole directory) and the table are
  reconciled:
  - a file in the table takes its size and length from it;
  - a file not in the table is measured, parsed with today's file-name parser, inserted and
    linked to its call once;
  - a row whose file is gone is removed.
- **Bookmarks:** the alert tones heard in a call (`audio::alert`) are its recording's bookmarks,
  in the list and in the WAV as cue points with a label and a region (`cue `, `LIST adtl`).

  The ±10 s full scan per file goes. Files the parser does not recognise are left alone and never
  deleted, as today.
- **SIGTERM** finishes the active recordings and drains the SD writer before exit (defect 6).

## 10. Auto-setup scan

The 071 finder becomes `services::discovery`, run on the Systems page, and on first run.

- **Bands:**
  - P25 700, 800 and 900 MHz;
  - UHF 450–470 MHz and VHF 150–174 MHz.

  All five are ticked by default; the user unticks bands and may add a range of their own.
  `/api/v1/scan/options` gives the bands by name, the default settings (spectrum frames per
  window, time on each carrier, the wait for a site's identity, the most carriers probed), the
  16 MHz window and its 12.96 MHz step. All five bands take 8 windows: about 4–5 minutes.
- **Progress** (`/ws/live`): the band and window being read, and what the scan is doing:
  reading the spectrum, listening to a carrier for a control channel, checking a control channel
  a found site announced, handing the radio back.
- **Detection:**
  - the HDL spectrometer and `find_carriers`, as today (continuous vs bursty);
  - each carrier is probed by **every protocol's control decoder at once** on the control IQ:
    - P25 by TSBK CRC (LSM and C4FM);
    - DMR by BS-data sync count, CACH and valid CSBKs.
  - optional: probe on traffic chain 1's IQ in parallel to halve the probe time.
- **Identity and plan per site:**
  - P25 as today;
  - DMR: colour code and system identity code (model, network, site); channel plan from
    channel-announcement CSBKs and MBC absolute parameters (parsed today but never fed back);
    neighbours from adjacent-site and vote-now (LCN only).
- **Proposal:** a card per found system, sites grouped by identity: the system's name, details
  and identity on two lines, then its sites in a table (name, identity, control channel, the
  others it announced, reception), every value editable in place. "Add" sends the card
  (`POST /api/v1/scan/add`: a new system's name, identity and details; each ticked site's name,
  identity and channels). A configured system keeps its own name; its configured sites only gain
  alternates.
- **Rescan** matches by identity, or by control channel within 3 kHz. It never overwrites labels,
  aliases. It adds learned data (alternates, plan) to state, and adds new sites only when
  ticked. A configured site heard on another control channel is shown as such, and moves there
  when ticked (its alternates become the ones it announces).
- **Each probe hears only its own carrier.** After the control channel moves, the probe waits
  250 ms (the control DDC's IQ comes in sub-buffers of 8192 samples, 164 ms at 50 kSPS, read
  every 40 ms) and drops what is queued before its decoders start. Without this, the last
  carrier's messages were decoded as the next one's: Clay Electric's Tier III identity landed
  on a Connect Plus control channel at 454.11875 MHz, and the scan, keeping one find per
  identity, dropped the real 454.36875 MHz.
- **A DMR site's channel table is learned** (`trunking::lcn`; Andy: "if grants are given and a
  traffic channel isn't mapped to it then it needs to auto identify"). Clay Electric's grants
  carry only the LCN: nothing on its control channel maps one to a frequency (no absolute
  channel parameters, no channel frequency announcements in SDRTrunk's decode of ten
  minutes), so SDRTrunk needs a hand-made map. Each channel does name its network and site in
  its CACH (the short LC's system parameters; Andy: "the grants identify # of slots we need to
  fill, traffic identifies how many extra freq we found"): the LCNs grants name are the rows to
  fill (`lcns_granted`), and the channels a lane heard name this site's network, site and colour
  code are the frequencies to fill them with (`channels_heard`). While a granted LCN has no
  frequency, the idle lane identifies the carriers on the air: it tunes to one no lane has heard
  for up to 1.5 s, until its short LC names it (one every 3 s at most; one that names nothing is
  left for ten minutes; a grant takes the lane at once). A grant on an LCN the plan lacks waits
  800 ms while the spectrum is watched, then is followed on a candidate frequency:
  - the site's own channels that keyed up after it (10 dB over each bin's usual level, read
    every 5 s; a traffic repeater keys up for its call, and Clay Electric's LCN 6 stands 52 dB
    over the floor, louder than the control channel);
  - other carriers that keyed up, the biggest rise first;
  - the site's own channels on the air meanwhile;
  - the control channel (a control repeater carries calls on its other timeslot and never keys
    up: Clay Electric's LCN 5 TS2);
  - the site's other own channels (one LCN to fill and one own channel left: that one);
  - the site's known channels;
  - the intermittent carriers the survey heard, the strongest first.

  A carrier that named another network or site is never a candidate, and a trial whose channel
  names one is over at once.

  The candidate is kept when the call's voice link control there names the granted talkgroup
  within 4 s, and is not tried again for that LCN when it does not; the grant's repeats while
  it is tried are the same call. Only then does its frequency count as the site's traffic
  channel. Learned channels go to the site's state (`lcn_hz`); a configured plan wins over
  them; the site row shows the table, edited in place. A grant with an absolute frequency
  needs none of this; an LCN with no candidate left shows as `unknown_lcn`. Both the watch and
  the survey read the whole window a lane can receive (±3.6 MHz at 8 MSPS): ignoring the outer
  tenth on each side had hidden LCN 6, 3.3 MHz from the centre.
- **The survey** (`trunking::survey`, `/api/v1/survey`, Diagnostics): while a site is live its
  trunking reads every spectrometer frame (131 ms each, 7.6 a second; the spectrum page is
  served the newest) and counts, per bin, how often it stands 10 dB over the frame's floor,
  decaying over ten minutes. Adjacent active bins are a carrier on the raster, placed at the
  middle of its bins within 6 dB of its peak: steady ones (on 90 % of the time) are control
  channels and the like, intermittent ones carry calls, data and keep-alives (Andy: "short
  data blips ... identify other possible channels"). A scan stops the live site, so it has the
  spectrometer to itself.
- **Probes are one per protocol, behind one small trait.** A later spectrum survey (Andy,
  2026-10-01: other DMR and digital systems, ATSC, ADS-B, ISM sensors, strong-signal activity)
  can add classifiers without changing the scan itself.

## 11. API

The UI moves to `/api/v1`. Every v1 route is typed: DTOs in `p25-json`, `ApiError`, and one route
table that builds both the router and `GET /api/endpoints`.

Protocol-specific fields become tagged: `identity: {protocol, ...}` and a neutral
`control_health {msgs_per_s, ok_pct, last_msg_age_ms}`. That retires `UiSite`'s P25 fields plus
its `dmr` attachment, which is what forces `site_card.js` to branch.

| v1 | Replaces | Consumers to move |
|----|----------|-------------------|
| `GET /api/v1/status` | `/api/ui/state`; parts of `/api/system`, `/api/stats` | UI; bench `corpus.py` |
| `GET /api/v1/calls`, `/calls/{id}` | `/api/ui/calls`, `/api/grant_decode_stats`, `/api/grants` (chain 1 only), `/api/traffic` `current_call` | UI; bench `corpus.py`, `p25_score`; tools `audit`, `poll_grants_persist`, `sdrtrunk_teardown_stats`, `capture_session`, `check`, `status`, ... |
| `GET /api/v1/radio`; `PUT /api/v1/radio/{gain, settings, clock, recording, crystal}`; `GET /api/v1/radio/crystal`; `POST /api/v1/radio/crystal/calibrate` | `/api/presets`, `/api/preset`, `/api/tune`, `/api/rx_gain`, `/api/ppm*`, radio parts of `ui/settings` | UI; `runs/dmr/log_dmr.py`, `record_chunks.sh` |
| `/api/v1/systems[/{id}]` (aliases inside) | `/api/sites`, `/api/sites/{name}`, `/api/aliases` | UI; tool `status` |
| `/api/v1/systems/{id}/sites/{site}`; `POST /api/v1/sites/{id}/activate`; `/sites/{id}/plan`, `/recentre`, `/learned` | `/api/site`, `/api/site/plan`, `/api/site/recentre` | UI |
| `/api/v1/systems/{id}/aliases`, `/listening`, `/talkgroups/{tg}`; `/api/v1/hold` | profile actions in `ui/settings`, `/api/monitor`, `/api/encrypted_tgs` | UI (not `/api/monitor`); bench `corpus_tests` (`/api/monitor`, mode C) |
| `/api/v1/recordings...` | `/api/recordings...` | UI; tools `poll_recordings_persist`, `compare_sim_vs_board`, `audit`, `capture_session` |
| `/api/v1/activity/*`, `/api/v1/data` | `/api/activity/*`, `/api/data` | UI |
| `/api/v1/scan` (GET, POST), `/scan/cancel`, `/scan/results/{key}/add` | `/api/discovery*` | UI |
| `/api/v1/events`; typed `/ws/events` | `/api/log`, `/api/recent_tsbks`, `/api/dmr/messages` | UI; 8 tools use `/api/log`; `log_dmr.py` |
| `/api/v1/receivers` (status; modulation choice) | `/api/modulation`, `/api/dmr` | UI; tool `retune_probe`; `log_dmr.py` |
| `/ws/audio` (v2 framing; speaker in meta) | unchanged path; v1 framing kept | UI; tool `ws_audio_capture`; bench `wsaudio.py`, `services.py` |

Diagnostics keep their paths in `api::diag` (D7; not built yet, see the status log). Examples: `hdl_lsm`, `irq_stats`,
`decoder_compare`, the dibit and IQ dumps, `*_lsm_control`, `nid_capture`, `sync_tune`, `bch_t`,
`decoder_reset`, `pipeline`, `dibit_delivery`, `spectrum*`, `wideband_iq_capture`, `imbe_dump`,
`audio_test`, `sys_health` and `/ws/iq`. The GETs that write become POST or PUT:

- `traffic`, `traffic2`, `monitor`, `rx_gain`, `sync_tune`, `decoder_reset`;
- `control_lsm_control`, `traffic_lsm_control`, `nid_capture` and the two `*_aligned` captures.

The tools and bench that use the GET forms change in the same commit.

**Not carried over** to the fresh crate, after the consumer check:

- 12 routes with no consumer: `sites/{name}`, `freq_health`, `control_dibit_capture`,
  `traffic_dibit_capture_aligned`, `traffic_iq_dump`, `ppm/nudge`, `agc_threshold`,
  `recordings/{id}/events`, `recordings/{id}/sync_trace`, `traffic2`, `deviation` and
  `distribution` (`ps_cores` has one: `tools/live_baseline.py`);
- the `traffic_bins` 410 stub and the forensics routes;
- 7 unused verbs on used paths.

`traffic_iq_dump` is live again since 071b and is in the 075 recipe; it stays unless Andy says
otherwise. Seven tools still call routes that no longer exist (`/api/constellation` ×6,
`/api/lsm`, `/api/lsm_control`, `/api/grants_active`, `/api/talkgroups`,
`/api/voice_follow_targets`, `/api/debug`). Those calls are fixed, or the tool is retired.

**Docs:** the fresh crate's `scanner/doc/API.md` covers both protocols; the catalogue is
generated, so it cannot drift. Today the hand-written `/api/endpoints` catalogue is missing 34
paths and `doc/P25_API.md` is missing 13. `API_CONSUMERS.md` is updated.

## 12. UI

Same stack: plain ES modules embedded by `ui_assets.rs`, no build step, no CDN.

| Page | Contents |
|------|----------|
| **Now** | The at-a-glance summary: the live site card, a call card per lane, recent calls (site picker as today), the hold |
| **Systems** | The scan (bands and settings, progress, a card per found system to fill in and add), then the configured systems in the same card: name, details and identity, sites with their identity, control channel, alternates and traffic channels (the configured ones and those heard on the air), all edited in place with Save; the site's receiver settings behind a button. First run opens here. |
| **Activity** | As today, with a system/site picker that includes DMR; packet data is a P25 component |
| **Settings** | Radio (gain, crystal, presets, clock, storage and recording), the configuration (export, import, factory reset), browser, about |
| **Diagnostics** | The one **events box** for every protocol (CC messages off by default, calls, traffic lines), receivers (a protocol component), radio window (spectrum, coverage, manual tune), lanes, board, raw JSON |

Protocol-specific parts are small components chosen by the site's protocol:

```text
ui/js/protocols/index.js    proto(site.protocol) → { label, identityRows, healthRows,
                                                     callDetailRows, scanColumns, scanRow }
ui/js/protocols/p25.js      NAC / WACN / SYS / RFSS, TSBK rate, IMBE / LDU detail, TDULC names
ui/js/protocols/dmr.js      colour code, network / site, CSBK rate, AMBE detail, terminator / CLEAR
```

Generic components call `proto(...)` and never test the protocol themselves. A host test in
`ui_assets_tests` fails if a file outside `protocols/` mentions `p25`, `dmr`, `NAC` or `TSBK`. The
store's refresh "kicks" use the typed call events, so DMR calls appear at once instead of on the
1 Hz poll. The new UI is written against v1 from the start; the old UI stays with the old binary
until the cutover.

## 13. FPGA: what to move, and when

Andy asked for this on 2026-10-01. Summary: **nothing has to move for 076.** Afterwards, change
079 (`doc/changes/079_general_radio_core.md`, approved 2026-10-03) replaces the core: a polyphase
channelizer in the PL, every demodulator in software.

### Fabric and CPU today

**Fabric** (075b bake, `impl_1` reports):

| Resource | Used |
|----------|------|
| Slices | 12,176 of 13,300 (**91.6 %**) |
| DSP48 | 172 of 220 (78 %) |
| LUTs | 55 % |
| BRAM | 45 % |

- Timing closed at WNS +0.208 ns. The bake before closed at +0.021 ns on the same path.
- Each traffic chain costs about 5.5k LUTs, 8.3k FFs and 45 DSPs. The 121-tap LPF keeps its
  delay line in flip-flops.
- Slices are the binding resource.

**PS CPU** (two A9s at 666.67 MHz, 200 % in all):

- IMBE is 2.4 % of a core per stream.
- A software receiver on a lane's 50 kSPS IQ (`dsp::cost_tests` on unit A, since 079's FIR
  speed-up): LSM 4.1 %, C4FM 4.3 %, DMR 9.3 % of a core, of which the filters are about 2 %.
- Unit A on Clay County (079's radio core; the control channel's LSM decoder, two LSM lanes):
  the scanner 15.0 % of one core, the control thread 4.0 % of it. The filters are about 6.4 %,
  the kernel 1.7 %, the spectrometer's frames about 1 % (`fbench-agent profile`).

### Candidates

| Candidate | Saves | Costs | Verdict |
|-----------|-------|-------|---------|
| Run only the live protocol's decoders | 12–17 % of a core at DMR sites | PS only | **076** (section 5) |
| Chain capability model | — (lane 2 carries P25 LSM only until 079) | PS only | **076** (section 5) |
| Scan on the existing spectrometer, DMR by software sync count | — | PS only | **076** (section 10) |
| Chain-2 post-DDC IQ tap on core 0.3.0 | DMR and C4FM voice on lane 2 | ~150 slices, ~1 k FF, 1.5 BRAM, 0 DSP | Not built: 079 gives every lane IQ |
| Symmetric, NEON-friendly FIR in `dsp::fsk4` | 9–10 % of a core per software receiver | PS; parity gated by the DMR and C4FM tests | **Done in 079**: byte-identical dibits |
| Host experiment: DMR framer on the LSM model's dibits | Lane-2 DMR without a bake | ~1 day, host only | Not needed (079); LSM on C4FM passes only 42–69 % |
| LsmFir delay lines to SRL/LUTRAM | ~5k FF per chain of area back | Bake | Not needed: 079 removes the LSM chains from the PL |
| **Polyphase channelizer in the PL, every demodulator in software** | 8–16 identical lanes; ~60–70 DSPs instead of 172 | A new core and Vivado project; a fixed-point model first | **079** |
| HDL channel filters (half-band, LPF, RRC), time-shared across lanes | ~2 % of a core per software receiver | Parity with SDRTrunk's f32 shown on a fixed-point model first | **079 step 5**, when lanes outgrow the CPU |
| Software LSM demodulator | Retires the gateware LSM; P25 voice on any lane | PS; SDRTrunk port, checked against `p25-httpd/src/lsm` | **079 step 1** |
| HDL 4FSK symbol processor or C4FM demodulator | — | Months; SDRTrunk's branchy sync-driven timing; worse late entry | Never |
| Vocoders, FEC, PCM AGC, autoppm/recentre | ≤ 2.5 % each | 6–8 weeks for a vocoder alone | Never |
| A fourth copy of today's DDC and LSM chain | — | DSP 217/220, slices over 100 % | Never; more lanes come from 079's channelizer |

## 14. Test strategy

**Every commit, in whichever crate it touches:**

- **Host tests:** `cargo test`. The old crate has 474 after phase 0; the fresh crate brings each
  ported module's tests with it. The golden-dump tests are `#[ignore]` since phase 0, so a run
  no longer rewrites `maia-hdl/test/golden_vectors/`.
- **ARM check:** `cargo-zigbuild check --target armv7-unknown-linux-gnueabihf.2.31`. The
  Windows host check skips the `cfg(linux)` code. Rules for the Tezuka toolchain: no `///` on fn
  params, no recently stabilised features.
- **DMR reference** (from phase 2, on the fresh crate's decoder):
  `DMR_CAPTURE_DIR=... cargo test --release dmr::` keeps 24,984+ of 24,996 lines matching, and
  the follower test keeps the 20:57 call.

**Phase 0, before the fresh crate starts:**

Committed fixtures live in the fresh crate's folder, `scanner/tests/fixtures/`.

1. **Pipeline replay (characterization)**, against the old crate.
   - **Recording:** with `P25_TRUNK_TRACE=<file>` set, the old binary logs the call lifecycle's
     inputs and its call events (`app::trunk_trace`). The inputs are grants with the follower's
     decision, grant updates, traffic NIDs, link control, end markers and voice chunks.
   - **Replay:** a 30-minute trace from each system on unit A runs through the lifecycle on the
     host, with its clock pinned to the trace (`trunk_replay_tests`). The result is a list of call
     records: talkgroup, lane, frequency, encryption, follow decision, radios, start and end,
     close reason, end marker and frames. Scenarios are in
     `replay/<name>/{trace.jsonl, expected.json}`.
   - **Phase 3:** the fresh crate's follower and CallBook must reproduce these lists, apart from
     listed, intended differences (the defects above). The trace also holds the old follower's
     decisions, so the fresh follower's gates are checked on the same grants.
   - **DMR captures:** `runs/dmr/cc_*.wav` stay with the DMR reference and the 20:57 follower
     test.
2. **Route contract.** `tools/route_shapes.py` keeps the JSON shapes of the 31 GET routes the
   tools and bench read, in `routes/shapes.json`, captured on a P25 site and on a DMR site.
   `check` lists the keys that went missing or changed type.
3. **Baseline.** `tools/live_baseline.py` runs read-only on unit A for 30–60 minutes per system,
   with today's build. It records control-channel decode % (TSBK CRC or DMR valid), calls,
   follow rate, vocoder errors, recordings, history rows and CPU. The numbers are in the status
   log.
4. **Replay corpus baseline** (`rf.p25_corpus`), once B is wired into A (D12).

**Live:**

- **P25 regression:** the replay corpus `rf.p25_corpus`, with B transmitting into A over the
  bench link (D12). It checks TSBK CRC %, follow rate and vocoder errors.
- **Unit A at the end of every phase from 1:** Clay County P25 (CC 860.9625) and Clay Electric DMR
  (`cec_gcs`, CC 454.36875), against the phase 0 baseline.
- **Unit B** at the cutover image, once its SD card has been checked.

## 15. Phase plan

The fresh crate is built top-down:

1. a binary that boots, loads its configuration and tunes the radio;
2. then the receivers;
3. then trunking;
4. then audio, recordings and history;
5. then the scan and the remaining consumers.

Rules for the build-up:

- **Leaf code is ported with its tests.** The logic stays as it is (SDRTrunk-matched). Comments
  shrink to a module header and intent; dated and phase narrative is dropped.
- **Glue is written fresh against this design.** The rules the field taught us (lifecycle
  timings, follower gates, air-time gating) come across with their tests.
- **The old p25-httpd is frozen** apart from fixes until the cutover. A fix that lands meanwhile
  also goes into the fresh crate if the module already exists there.
- **Shared crates.** `scanner` uses `p25-pac` by path from `p25-httpd/` until the cutover. They
  are two packages, not a cargo workspace: the image build syncs only `p25-httpd/` and builds it
  on its own. The fresh crate's API types live in the crate (`api::v1`), so `p25-json` stays
  with the old crate.
- **Each commit builds and passes the checks**, and each phase ends with the live check on A.

| Phase | What | Live check on unit A | Size |
|-------|------|----------------------|------|
| 0 | Groundwork in the current tree: baseline, fixtures, replay / Activity / route tests, golden dumps `#[ignore]`, the `pre-076` tag, a short current `CLAUDE.md`, the B→A bench link (D12) | Baseline numbers on both systems and on the replay corpus | M |
| 1 | **Skeleton.** Crate and workspace. Boot (args, logging, supervision, shutdown flush) and util. Config layers, migration and learned state. Radio: drivers ported, `fpga.rs` split, Tuner, lease, lanes, planner, ppm. `LiveSite::activate`. API core: route table, `ApiError`, status, radio, systems, sites, profiles. UI shell: pages, store, protocol registry, Systems and Settings. | Boots, migrates A's files into `scanner/`, tunes the live site, switches sites in one action; the old binary's files untouched | L |
| 2 | **Receivers.** dsp, `protocol::fec`, P25 control (HDL LSM and C4FM, with the choice), DMR control, the receivers runner (live protocol only), events into the event log and the Diagnostics events box, site cards with identity and health | Same TSBK CRC % and DMR valid % as the baseline; CC messages in the events box | L |
| 3 | **Trunking.** One follower, the CallBook, lanes with capabilities, the P25 traffic decoder on the HDL lanes, DMR traffic on chain 1's IQ and the control slot. Now page with call cards and recent calls. | The replay scenarios reproduce; follow rate on both systems as in the baseline | XL |
| 4 | **Audio and recordings.** IMBE and AMBE+2 ported, the pipeline, one AGC, pacers, `/ws/audio`, the recording manager with the existing SD files indexed | Live audio and recordings on both systems; vocoder errors as in the baseline | L |
| 5 | **History v2.** Writer, reader, copy-migration, Activity page, packet data if D11 says so | Activity snapshots match after the migration; new calls stored | M |
| 6 | **Scan.** P25 and DMR probes, UHF/VHF, grouping into systems, first-run flow, rescan merge | From an empty config, a scan finds Clay County and Clay Electric, and adding them works | M |
| 7 | **Diagnostics and consumers.** The diagnostic endpoints the tools and bench use (writes as POST/PUT), tools and bench moved, stale tools retired, the rest of Settings | fbench and the key tools run against the fresh crate | M |
| 8 | **Cutover.** tezuka_fw's package and init script switch to the fresh crate; SD image for A, then B. The old p25-httpd, `lsm/` and `sw_demod/` are removed (to `dsp-lab` if wanted). The crate's docs (API reference, README, inventory) and `CHANGELOG_FORK.md` are written. | "Done means" on both units | M |

Notes:

- **Phase 1 is broad but shallow.** Each part is a thin vertical slice (config → tuner → API →
  UI), so the binary is useful on A from the first phase.
- **Phase 3 is the largest.** The replay scenarios from phase 0 are its acceptance test.
- **Units:** before any deploy, restart or retune, check `ListAgents` and message any session that
  may be using the unit.

## 16. Risks and open questions

- **Parity before the cutover.** The old binary stays in production until the fresh one matches
  it on both systems. Phases 3 and 4 (call model, audio) carry the most risk. Safeguards: the
  phase 0 replay scenarios, live comparison against the baseline, and the old binary always one
  restart away. Measure CPU and audio jitter on A against the baseline (`top`, `sys_health`):
  the two-core runtime's latency is the real limit, not throughput.
- **Two crates for a while.** Fixes during the build-up go to the old crate; keep them rare.
- **Bench time (D12).** A corpus run takes both units, and A listens to B instead of the air
  while it runs.
- **Shared units:** other sessions may drive A and B.
- **Not in 076:**
  - auth and the MCP server;
  - scanning or priority between sites (room is left);
  - Phase 2 TDMA voice;
  - C4FM voice on traffic channels;
  - Harris and Motorola vendor grants;
  - packet-data contents;
  - the 20 Hz plots;
  - the new core (change 079).

## Done means

From the brief:

- a fresh unit boots empty;
- a scan finds Clay County P25 and Clay Electric DMR (and the other local systems), and the user
  adds them;
- both systems decode, follow, play, record and fill the history;
- switching between them is one action;
- the tree matches the module map with no dead code;
- every phase passed its checks;
- the crate's docs match the code: API reference, inventory, this design and the status log.

## Status log

- 2026-10-01: analysis of the tree at a2c6615, in six parts:
  - call tracking and audio;
  - configuration and persistence;
  - history and recordings;
  - API and UI;
  - protocol, hardware and dead code;
  - FPGA offload (added at Andy's request).
- 2026-10-01: Defects 1, 2, 3, 4, 7 and 9 checked by hand, along with `save_site`'s only caller,
  the "clay" fallback and the fabric numbers. Design written.
- 2026-10-01: **Andy approved the design.** At his request it is built as a fresh crate,
  top-down, in phases 0–8 (D13, section 15). The 14-stage in-place plan is retired. Unit A is
  the unit in use. Phase 0 is next.
- 2026-10-01: Andy decided D12 (B is wired into A for testing as needed) and D14 (the old docs
  stay; the refactor's docs live in the fresh crate).
- 2026-10-01, phase 0:
  - **Done:** `CLAUDE.md` rewritten; golden emitters ignored; the trunking trace tap and the
    host replay; unit fixtures for A and B; Activity snapshots; route shapes; `pre-076` tagged.
    B's SD card passed `fsck.fat -n`.
  - **Replay check on B:** a 37-minute Clay trace replays to the unit's own calls, 317 of 319
    identical. The other two are one race: an end-grace close and the next grant 2 ms apart,
    ordered differently by the sweep's 100 ms phase.
  - **Baseline, DMR** (unit A, `cec_gcs`, 60 min, build `2026-10-01-dmr-075`):
    - control messages 100 % valid (41.7/s), CACH 100 %;
    - 58 calls, all followed, 57 with voice (198.5 s);
    - 57 recordings, 58 history rows;
    - CPU: system 14.6 % busy, daemon 28.7 % of a core.
  - **DMR reference:** 24,984 of 24,996 lines match SDRTrunk.
  - **Fix found on the way:** defect 18, committed to p25-httpd (`2026-10-01-nco-fix-076`).
- 2026-10-01, phase 1 (in progress):
  - **Built:** `scanner/` with boot, util, the configuration layers and their migration, the
    hardware layer (one implementation for the three chains), the radio (planner, lease, Tuner),
    `LiveSite::activate`, the `/api/v1` core and the UI shell.
  - **Unit B** (scanner run from `/tmp`, p25-httpd restored after): it migrated B's files,
    brought Clay up at 8M on LO 858.7 MHz (the old binary's LO), and the control chain decoded
    NAC 0x8A1 with valid NIDs.
  - **Unit A** (2026-10-01, with the phase 2 receivers): it migrated A's files (9 systems,
    21 sites, 9 profiles), came up on `cec_gcs`, switched to Clay and back through
    `POST /api/v1/sites/{id}/activate` (0.14 s), and A's old flash files and history were
    byte-identical afterwards. Clay took the 12M window (A's plan sets a 12M minimum).
- 2026-10-01, phase 0 (finished except the replay corpus, which needs B wired into A):
  - **Baseline, P25** (unit A, Clay, 30 min, build `2026-10-01-nco-fix-076`):
    - TSBKs 99.6 % OK, 39.6/s, LSM;
    - 228 calls (132 encrypted), all 96 clear ones followed, 94 with voice (281.9 s);
    - vocoder 1.62 % frames with errors; 94 recordings, 225 history rows;
    - CPU: system 17.3 % busy, daemon 31.9 % of a core.
  - **Traces:** A's 31-minute Clay trace replays to A's own calls, 232 of 232 identical; A's
    38-minute Clay Electric trace 19 of 20 (one call end 32 ms from its timeout, the same race
    as on B). Fixtures: `tests/fixtures/replay/b_clay_p25` (12 min of B's trace, 81 calls) and
    `a_cec_gcs_dmr` (20 calls), radio IDs redacted; the route shapes now cover a P25 site too.
- 2026-10-01, phase 2 (in progress):
  - **Built:** the P25 framer, control decoder and C4FM demodulator; the DMR control decoder;
    `ControlEvent`; the dibit ring tracker and production clock; the control-chain readers; the
    receivers runner with the LSM/C4FM choice; the event log, `/api/v1/events`, the control
    channel card and the events box.
  - **Framer parity:** the new P25 framer and p25-httpd's give identical output on all 331
    SDRTrunk `.bits` recordings (471,076 TSBKs, 7,037 LDUs, 13,011 TDULCs, 38 PDUs;
    `framer_dump` in both crates).
  - **DMR:** the control decoder on the 10 Clay Electric captures gives the SDRTrunk reference's
    24,996 messages, identity SMALL/0/2 colour code 0, and 15 grants on LCNs 5 and 6.
  - **Live, unit A:** Clay 41.8 TSBK/s, 100 % (LSM 836, C4FM 816 in 20 s; 20 % of a core for
    both demodulators); Clay Electric 46.5 messages/s, 100 %, carrier offset -147 Hz
    (p25-httpd: -144 Hz), 12.4 % of a core.
  - **Changed from p25-httpd:** the auto choice waits for 10 s of counts before its first
    decision. Without it, B switched to C4FM in the first second (the LSM path starts a few
    hundred ms later) and the 60 s dwell held it there on an LSM site.
  - **FEC:** one Golay(24,12) and one set of Hamming codes in `protocol::fec`, shared by both
    protocols. The P25 voice parsers keep SDRTrunk's Golay behaviour (a 3-bit correction stands
    whatever the parity bit says), DMR keeps the parity check. Checked: the parsed link control,
    HDU and encryption sync of all 331 recordings (20,491 voice units) match p25-httpd's, and
    the DMR reference still matches 24,984 of 24,996 lines.
  - **Next:** the replay corpus with B wired into A (the last phase 2 gate).
- 2026-10-01, phase 3 (in progress):
  - **Call book** (`trunking::calls`): p25-httpd's lifecycle rules on the monotonic clock, with
    channels compared by frequency and timeslot. Its 24 tests ported, and the three replay
    fixtures (Clay on A and B, Clay Electric on A: 199 calls) replay to exactly the calls
    p25-httpd's lifecycle makes of them. A third fixture, `a_clay_p25` (10 minutes of A's
    Clay trace, 98 calls, two on lane 2), was added; the fixture traces are now tracked (the
    tree ignores `*.jsonl`).
  - **Follower** (`trunking::follow`): the gates in p25-httpd's order as small steps, the lane
    choice, re-follow from grant updates; pure, it returns the call book's record and the lane
    commands. The lane-policy tests are ported.
  - **Changed from p25-httpd:** a P25 grant whose channel the IDEN table does not name yet is
    not followed (`unknown_lcn`, as for DMR) instead of opening a call with no frequency;
    "busy" replaces "sticky_lock"; a site switch closes the open calls (`site_switch`).
  - **Traffic:** the P25 traffic decoder (p25-httpd's forwarder rules: voted link-control
    source, HDU encryption latch, end of transmission, talk complete) checked on SDRTrunk's 287
    traffic recordings; the DMR traffic decoder on lane one's IQ, checked on the 20:57 call.
  - **Trunking task** (`trunking::trunk`): follower, call book and each lane's decoder in one
    task; lane dibit readers with the production clock (voice attributed to calls by air time,
    dibits from before a retune dropped), the lanes' NID status every 16 ms; `/api/v1/calls` and
    the calls card. A lane resumes on its channel without a reset only when it carried voice in
    the last second and its PLL is under half its clamp (p25-httpd's `resume_needs_reset`).
  - **Live, unit B** (Clay, 3 min): 38 calls, 14 followed on lane 1, all 14 with voice.
  - **Live, unit A:** Clay Electric DMR (10 min, a quiet hour): both calls followed on lane 1
    with voice and their talkers, one on LCN 6 TS2 and one on the control repeater's TS2. Clay
    P25 (4 min): 35 calls, 18 followed on both lanes (14 and 4), all 18 with voice; 17
    encrypted listed; 18 % of a core.
  - **Changed from p25-httpd:** VoteNowAdvice, CallTimerParameters and Announcement count as
    DMR housekeeping in the events box.
  - **Learned state** (`trunking::learned`): the stored IDEN bands seed the control decoders at
    activation; grant counts (one per opened call) and encrypted talkgroups are kept; saved
    every 10 minutes when changed, on a switch and at shutdown.
- 2026-10-01, phase 4 (in progress):
  - **Codecs** (`audio::codec`): IMBE and AMBE+2 from p25-httpd's jmbe port, with their
    reference tests, behind `VoiceCodec`.
  - **Live audio** (`audio::live`): per lane, a decode thread (codec, then the one `PcmAgc`) and
    a pacer releasing one 20 ms chunk per 20 ms, into one broadcast; `/ws/audio` sends
    lane-tagged frames, a meta frame per call and lag frames. The meta frame carries the speaker
    (left, right or both) from the follower's routing, so the browser no longer works routing
    out from the profile (section 7). The browser player is p25-httpd's, with a Listen button.
  - **Recordings** (`services::recordings`): driven by the call book (a followed call's open
    starts one, its chunks are appended by call id, its close starts a 2 s drain); p25-httpd's
    RAM and SD stores, card writer, retention and file names; the card's files listed at boot,
    call ids continuing past them; each followed call's frames, vocoder errors and silent frames
    counted; `/api/v1/recordings` (list, the WAV with byte ranges, deletes) and
    `PUT /api/v1/radio/recording`; playback on Now and the Recording card in Settings. SIGTERM
    saves the open recordings and waits for the card writes (defect 6).
  - **Changed from p25-httpd:** a recording starts at its call's own open time (defect 13);
    chunks go to recordings by call id only.
  - **Live, unit B** (Clay, internal antenna; recordings in a test directory on the card,
    `--recordings-dir /mnt/sd/scanner_recordings`, seeded with five of p25-httpd's files):
    - `/ws/audio`, 15 min: 34 calls, 117 s of audio, RMS 2421 (AGC target 2500), no malformed
      frames;
    - 26 recordings on both lanes, none failed, the slowest card write 17 ms; SIGTERM exited in
      1 s with every file written and no `.part` left; a restart listed all 21 then on the card
      and continued the call ids;
    - vocoder errors: 16.7 % of 2,655 frames; p25-httpd on the same antenna in the same hour,
      25.7 % of 1,449 (same 4-bit threshold). A's baseline (1.62 %) is on its external antenna.
  - **Not yet:** DMR audio and recordings live, and the phase gate, on A.
- 2026-10-01, phase 5 (in progress):
  - **History** (`services::history`): schema v2 (section 8; `data_packets` waits for D11). One
    writer thread fed by a channel commits every 10 s or on a flush, and prunes once an hour by
    whole hours, so the totals always match the calls (defect 14); the API reads through a
    second, read-only connection.
  - **The v1 copy** (`migrate_v1`): p25-httpd's history attached read-only and copied in one
    transaction into `scanner-history.sqlite.part`, renamed once complete; the v1 file is left
    as it is. Both units' fixture databases give exactly their phase 0 Activity snapshots after
    the copy.
  - **What is stored:** calls at their close, with the recorder's vocoder counts merged in
    after; radio events from the control channel; sites and systems at activation; recordings,
    linked to their call once (by call id, site and a start within 10 s, through the call id
    index) and reconciled with the card at boot. Call ids continue past the history and the
    recordings.
  - **API and UI:** `/api/v1/activity/*` (sites, summary, talkgroups, radios, one radio, one
    talkgroup, series, calls with CSV), each for a site or a whole system (`system`), and the
    Activity page; the recent calls are refilled from the history at boot and kept across a
    site switch.
  - **Live, unit B:** the copy took 1.2 s (5,274 calls, 5,338 radio rows, 1,662 hour rows,
    2,761 radio events). Summary, talkgroups and radios for the 24 hours to 16:00 UTC are
    identical to p25-httpd's answers before the switch. The scanner's calls continue from
    p25-httpd's last id, with their vocoder counts; radio registrations are stored. After a
    restart the recent calls come back from the history, and this run's recordings come back
    with their calls' lane and channel.
  - **The copy happens once** (when there is no v2 file). p25-httpd keeps writing the v1 file
    while it runs, so before the cutover the test `scanner-history.sqlite` (and
    `scanner_recordings`) are deleted on each unit, and the cutover copies the final v1.
  - **Not yet:** per-speaker times in `transmissions`.
- 2026-10-01, phase 6 (in progress):
  - **Scan** (`services::discovery`): p25-httpd's finder (step plan, continuous carriers in the
    wideband spectrometer, neighbour pass) over the design's bands (P25 700/800/900, UHF, VHF:
    eight windows). Each carrier is probed by the P25 (LSM and C4FM) and DMR control decoders at
    once, each behind `Probe`. The live site pauses for the scan (`scanning`) and comes back
    after.
  - **Adding:** found sites are keyed by identity and grouped into systems; a site already
    configured (same identity, or a control channel within 3 kHz) only gains the alternate
    control channels it announced; a new site joins its system or a new one named by the user;
    a P25 site's band plan seeds its state; with no live site, the first one added goes live.
    `/api/v1/scan` and the scan card on Systems.
  - **Live, unit B** (internal antenna), from an empty configuration (`--flash-dir` on an empty
    directory): the scan took 24 s and found Clay County (860.9625 MHz, LSM, 93 %, six bands,
    two secondaries) and Clay Electric (454.36875 MHz, timeslot 1, colour code 0, SMALL network
    0 site 2), plus two P25 traffic channels in calls. Adding both made Clay live at 97 %. A
    rescan matched Clay to its site and put it back live; that pass missed the DMR carrier
    (marginal on the internal antenna).
  - **Live, unit A** (external antenna, empty configuration): 14 control channels with their
    identities in 208 s: Clay County; two more BEE00 systems on 800 MHz (3BD at 855.4875,
    4D6 with two sites); a 700 MHz system (WACN 9254A at 770.20625); eight FPL 900 MHz sites,
    two of them from neighbour lists; Clay Electric at 454.36875 MHz. Clay Electric carries
    control messages on both timeslots (B counted more on one, A on the other), so a control
    timeslot is kept only when one slot carries three times the other's (it is informational:
    the receivers decode both).
  - **Changed from p25-httpd:** a DMR find is put on the channel raster (2.5 kHz in VHF,
    6.25 kHz above): it announces no frequency of its own, and the spectrum's estimate was
    625 Hz off.
  - **Not yet:** the DMR channel plan (LCNs) from the air: a DMR site added by a scan follows
    no grant until its LCNs are entered in the site editor (`unknown_lcn` meanwhile).
- 2026-10-01, phase 7 (in progress):
  - **Editing:** profiles created, edited (groups, speakers, pre-emption, follow-only and
    never-follow lists, checked) and deleted (not a site's active one); a change to the live
    site's profile goes to the follower at once. A system's names. The site editor (control
    channel and alternates, DMR LCN plan and control timeslot, P25 modulation, known channels,
    window); the live site goes live again with the change. `PUT /api/v1/radio/settings`
    (presets, lanes, call timings at the next activation; history limits at once). Settings
    has the radio card, the profile editor and the names; Systems has the site editor.
  - **Live, unit B** (on a copy of its configuration): each endpoint did what it says, and a
    profile following TGs 300 and 319 only took effect from the next call (TGs 850, 600 and
    403 then `speaker_off`).
  - **`/ws/events`:** a notice when a call opens or closes or a recording is saved (32 in a
    minute on B). The UI now takes everything from `/ws/live`.
  - **Spectrum:** `/api/v1/spectrum` from the wideband spectrometer and its card on
    Diagnostics (B: Clay's control channel 30 dB over a -99 dB floor).
  - **Clock** (`services::clock`): p25-httpd's site clock (SYNC_BCST, with its tests), internet
    time, or by hand, as `radio.json` says; `POST /api/v1/clock` sets it from a browser.
    Calls are timed on the monotonic clock, so a step needs no idle lanes.
  - **Also:** `GET /api/v1/calls/{id}`; the Board card on Diagnostics (the hardware readback);
    Now names the profile followed; only the UI's protocol registry names protocols (a host
    test keeps it so).
  - **Soak, unit B** (the full build, 30 min, Clay on the internal antenna): no panic or error
    logged, 11 threads throughout, 110 recordings, no audio chunk missed, 28.7 % of a core for
    the process. RSS went from 9.6 to 12.2 MB, most of it in the first 12 minutes. A second
    soak of the next build (32 min, stopped for the bench wiring) levelled off: 11.0 MB at 20
    minutes, 11.04 MB at 32; no panic, 145 recordings, no audio chunk missed.
  - **Review of phases 4 to 7** (nine defects, each checked and fixed):
    - a switch the radio refuses after the old site stopped (an AD9363 told to tune VHF) left
      the old site shown live with nothing running; the old site now comes back (a test), or
      none is live;
    - the learned state was written to flash under the lock the trunking task takes on every
      grant; it is now written from a copy, on the blocking pool;
    - a profile change could act on a stale live site during a switch or scan; it now takes
      the radio lease, and every activation ends by following the active profile;
    - the CSV export built the whole file in memory; it now streams from a connection of its
      own (JSON listings stop at 5,000 calls);
    - a scan's timing parameters were unbounded and a cancel waited for the probe; both
      checked now, and the probes start before the streams;
    - a failed or timed-out card listing at boot removed every recording from the history;
      only a complete listing is reconciled;
    - a failed card write could leave a RAM copy of a recording deleted meanwhile;
    - a huge history size wrapped to zero; sizes are bounded (16 MB to 1 TB) and saturate;
    - the Activity size query waited behind the writer; it reads on the read connection.
  - **For the bench (with the D12 session):** the corpus test parks a lane through the
    follower, then holds it on the channel with the follower off (`/api/traffic`
    `lock=on&follower=off`) and reads `/api/imbe_dump`, `/api/ui/calls`, `/api/ui/state` and
    `/api/system`. `/api/system` and `/api/ui/state` are there as legacy adapters; the lane
    hold, the frame dump and the bench's move to `/api/v1` are built and checked with the bench
    running.
- 2026-10-01, phase 8 (the image is on both units; p25-httpd, `lsm/` and `sw_demod/` are still in the repo):
  - **Done ahead:** `doc/API.md` generated from the route table (a test keeps it current) and the
    crate's `README.md`. `Cargo.lock` pinned to the versions p25-httpd's image build uses (its
    later resolution had pulled hyper-util, jobserver and zeroize versions needing Rust 1.85,
    newer than the image toolchain is known to be; p25-httpd shows only 1.82 or later).
  - **Before the cutover, on each unit:** delete the test files from this work (the card's
    `scanner-history.sqlite*` and `scanner_recordings/`, and `/mnt/jffs2/scanner/`), so the
    first start migrates p25-httpd's configuration and copies its history as they are then.
  - **tezuka_fw:** a `scanner` package like `p25-httpd`'s (rsync `/scanner/` and
    `/p25-httpd/p25-pac/` without their target dirs or `scanner/tests/`; the same RUSTFLAGS and
    cc toolchain; `/usr/bin/scanner.xz`), an `S60scanner` from `S60p25-httpd` (card fsck and
    mount, respawn loop, log rotation, the certificates, `--listen 0.0.0.0:8080`), and
    `fishball_p25_7020_defconfig` selecting it in place of `p25-httpd`.
  - **Then:** the image on A, the checks of "Done means", then B; p25-httpd, `lsm/` and
    `sw_demod/` leave the repo; `CHANGELOG_FORK.md`. Rolling back is booting the old image.
- 2026-10-01, the bench session (D12) and the dead-code pass:
  - **Link:** B's TX1 into A's RX1 through 40 dB of pads; `rf.cw_ppm` B->A +0.659 ppm at 32 dB
    SNR (+0.662 on 09-26).
  - **p25-httpd baseline, mode B focus item** (A, Clay, `tx_atten_db=35`, -55 dBm): 639 of
    SDRTrunk's 639 IMBE frames, 99.7 to 99.8 % bit-exact (the others one raw bit apart, which
    the FEC corrects); the two-tone alert with no dropout on two runs of three, one 20 ms dip
    on the other (not in SDRTrunk's decode of the same transmission).
  - **The replay corpus, mode B, all 42 items** (231 followable clear transmissions, 35,514 of
    SDRTrunk's IMBE frames):
    - p25-httpd: 35,325 (99.47 %), none missed, no relay underrun, the focus tone whole.
    - the scanner (175e042): reported 95.6 % with 7 missed, but its `/api/ui/calls` gave no
      call span (`open_ms`), so the scorer matched calls only by source. Rescored with the span
      from each call's end: 35,289 (99.37 %), none missed. Every remaining difference is one
      LDU or a few at a change of talker on one channel, either way (p25-httpd also loses one
      where the scanner does not). The adapter now sends the span.
    - the scanner again (4b9052b, the span sent; voice aired after a call's end marker going to
      the next call): 35,244 (99.24 %), none missed, the focus tone whole. The same transitions
      still lose one LDU, and others moved by one LDU either way between the two runs: the
      boundary depends on timing, and its cause is not found yet (a trace of the lane's voice
      and the grants around a talker change is the next step). The scanner meets the gate;
      p25-httpd keeps about 0.1 to 0.25 % more of the frames.
    - the scanner's crystal calibration on the replayed control channel (during the focus item,
      which still decoded 639 of 639): LO shift 509 Hz, where p25-httpd's tracker had estimated
      504 Hz on the same signal. The tracker then followed each recording's offset within the
      50 Hz anchor.
  - **Found on the bench:**
    - with its clock source `site`, a unit sets its clock from the replayed control channel (a
      capture's date). The corpus test pins `manual` for the run and restores the source; the
      site time p25-httpd still held then stepped A's clock back to the capture's day, so the
      next run's calls fell outside `/api/ui/calls`. The scanner no longer uses a site time
      not refreshed for two minutes.
    - the bench recorded `/ws/audio` without `?v=2`: lane one only on p25-httpd, and lane
      headers in the PCM on the scanner. It now asks for v2 and keeps each lane apart; the focus
      tone is scored on the lane that carried it.
    - the scanner serves `/api/ui/calls` (each call's voice frames) and `/api/ui/settings`
      (the clock source) in p25-httpd's shape for the bench.
  - **Dead code:** the board build's warnings went from 92 to 21: the retune epochs nothing
    recorded (the lanes cut by air time instead), the wideband IQ ring, IRQ notifiers and
    counts (the readers poll), the AD9361 shadow, decoder timing, unread accessors, and helpers
    only tests used (now in the tests or `cfg(test)`). The TSBK log lines now show every field
    the decoder reads. On a development host the board-only code is allowed to look unused.
  - **Found missing, then built the same day.** The other 21 warnings were their parts, ported
    and waiting; p25-httpd has the first four in production:
    - **Crystal tracking** (`services::crystal`): A runs p25-httpd's autoppm (on, 50 Hz anchor,
      tracking +504 Hz at 856 MHz); the scanner applied the stored ppm at start and never again.
      Now it calibrates once the live site is decoded after start (the spectrometer's peak within
      ±10 kHz of the control channel, then the P25 carrier loop's mean residual; the DMR
      equaliser's offset at a DMR site; the spectrum alone at a C4FM site), and tracks: a sample
      a second while a burst is on the channel, the five-minute trimmed mean applied each minute
      within 50 Hz of the calibration, kept in `state/radio.json` on 5 Hz moves.
      `/api/v1/radio/crystal` (GET, PUT tracking and anchor), `POST .../crystal/calibrate`, and
      the Crystal card in Settings. The correction moves the LO (the tuner's design); a host test
      runs a calibration and the tracker against a radio that misplaces the channel, so the signs
      are pinned.
    - **Recentre** (`LiveSite::recentre`): the window was planned only when a site went live. Now
      `GET /api/v1/sites/{id}/plan` shows it against the site's channels and the planner's
      choice; the window moves there under idle lanes without stopping the site, by hand
      (`POST .../recentre`) or automatically (an automatic window, two minutes after going live,
      at most every ten minutes); Systems shows it under the live site.
    - **Neighbours, secondary control channels and the data channel** go into the site's learned
      state (`GET /api/v1/sites/{id}/learned`) and show on Systems.
    - **Packet data** (`services::packet_data`, `GET /api/v1/data`, the Activity card): the
      control channel's PDUs, and the data channel's (below).
    - **`/ws/audio`:** p25-httpd's first framing (lane one, samples only) unless `?v=2`.
  - **Then the data channel:** as in p25-httpd, the last lane of a P25 site waits on the announced
    data channel between calls (a voice grant still takes it) and its PDUs go to packet data.
  - **The module map** (section 2) now describes the tree as built.
  - **Still open:** `api::diag` (D7). Of p25-httpd's 100 routes, those the scanner lacks, by who
    reads them (the inventory of 2026-10-01):
    - **the bench:** `/api/traffic` (decoder counters, read and carried on without; mode C's
      `lock`/`follower` hold, which aborts without it), `/api/monitor` (mode C's precondition),
      `/api/decoder_compare` and `/api/stats` (`rf.p25_replay` aborts without them),
      `/api/decoder_reset` (optional), `/api/dibit_delivery` (optional);
    - **tools only** (22 diagnostics, e.g. `hdl_lsm`, `irq_stats`, the dibit and IQ dumps,
      `control_iq_dump` for the DMR captures, `sys_health`, `ps_cores`, `/ws/iq`), and 14
      user-facing routes whose v1 replacements exist (`grants`, `log`, `recordings`, `ppm`,
      `dmr`, `tune`, ...); `tools/route_shapes.py` checks the old shapes and will flag them;
    - **p25-httpd's UI only:** replaced by the scanner's UI, except the per-chain narrowband
      spectrum and the pipeline card;
    - **no consumer:** 12 routes (below).
    The bench's routes come first (the corpus in mode C and `rf.p25_replay` on the scanner);
    the tools' are ported or the tools moved to v1 before the cutover.
- 2026-10-01, the image on both units, and the API cleanup:
  - **The image:** tezuka_fw `a7b2174` (the `scanner` package and `S60scanner`) and `f103dde` (the
    post-build drops the daemons the defconfig does not select). A, then B (A's proven BOOT.bin
    on both: B's older bitstream lacked the traffic IQ tap). First boots migrated p25-httpd's
    files and copied its history: A 9 systems, 21 sites, 1,439 calls; B 1 system, 1 site, 5,765
    calls. `S60scanner stop` unmounts the card, so a file put on the card by hand needs the card
    mounted again first.
  - **Live, unit B** (its antenna):
    - Clay P25, 22 minutes (the run ended when A's reboot cut the route): TSBKs 83.7 % or better,
      29.7 messages a second or more, no grant dropped, no resync, 24 recordings, 12.5 MB, no
      panic or error. Auto modulation chose C4FM on one start (about 18 % more TSBKs) and LSM on
      the next (3 to 6 % more): the two are close at B, and the 25 % margin keeps either.
    - Clay Electric DMR, 18 minutes: 26 % of messages, no grant. With the IQ capture
      (`/api/v1/iq/control.wav`), B's control IQ is 10 dB over the noise at 454 MHz, where Clay
      P25 at 860 MHz is 24 dB on the same antenna and A's capture of 2026-09-30 (outdoor
      antenna) 37.5 dB. SDRTrunk's own decoder passes 23.6 % of B's capture and 100 % of A's:
      the decoder is not the cause. B's antenna is an indoor TV antenna (450-800 MHz) and Clay
      Electric is weak indoors; DMR checks need the outdoor antenna.
  - **Open, crystal at DMR:** at Clay Electric the calibration took the DMR equaliser's -136 Hz
    and moved the correction from -0.061 to +0.214 ppm (then +0.319 by tracking), while the
    spectrum put the carrier 10 Hz from centre. Back on Clay P25 the loop returned it to -0.048
    ppm, and there the spectrum's 304 Hz matched the 315 Hz error. B's DMR signal was 10 dB
    over the noise (indoors), so this is to re-check on the outdoor antenna before the
    calibration changes.
  - **API**, from the inventory (`doc/API_INVENTORY.md`, with its cleanup list) and Andy's asks:
    - `GET /api/v1/receivers`: the control channel's and each lane's state, decoder counters
      and carrier loop.
    - The recent calls are the live site's (from its history after a restart or a switch).
    - Remove a system or a site; export and import the configuration as one document; a factory
      reset (no systems, sites, profiles, recordings or history; the crystal calibration stays).
      An import or a reset restarts the scanner. On A: export, two deletes refused (the live
      site), two done, the reset, the import: radio, systems, profiles and live site as
      exported. Settings and Systems have the buttons.
    - A talkgroup hold (`/api/v1/hold`): on B with Clay's grants, every other talkgroup was
      refused as `held` while 300 was held.
    - Wrong data: `/status`'s live tuning is current; `/recordings`' total is the listed site's;
      `/data` counts per site and names each radio from its own system.
    - One call shape: `/calls`, `/calls/{id}` and `/activity/calls` with names, emergency,
      private, first voice, open and grant spans, codec and frame errors; history schema 3.
  - **UI:** `doc/UI_BRIEF.md` holds Andy's requirements for the final layout. The pages stay
    templates of what the API offers until then.
- 2026-10-01, the backend for the UI replacement (`doc/UI_BRIEF.md`):
  - **`/ws/live`:** the radio's state pushed as it changes, so no page polls: a snapshot on
    connect, the status each second, the traffic channels when they change, each call as it
    opens and closes, each saved recording, the scan's progress and found sites, and `changed`
    after every successful write.
  - **Aliases (D15)** replace profiles and the name maps (section 3.3): the follower, the
    recorder and every name read them; `/api/v1/systems/{id}/aliases`, `/listening` and
    `/talkgroups/{tg}`; recording of every call or only aliases that say record.
  - **No migration (D16).** Andy: nothing on the units is kept. The p25-httpd configuration
    migration (with its frozen seeds), the history's v1 copy and the profile conversion are
    gone, with the unit fixtures and the Activity snapshot test that only they used. A unit
    with no `/mnt/jffs2/scanner/` starts empty; p25-httpd's files on the flash and its history
    on the card are never read.
  - **Field inventory:** `doc/API_FIELDS.md` (`tools/api_fields.py` against a unit) gives
    every field of every GET route and `/ws/live` its type, an example and its meaning.
  - **RadioReference CSV import** (section 3.4): a system's talkgroups file becomes aliases and
    its sites file sites, with a preview first. Andy's saved downloads are the test files.
  - **Systems setup** (Andy: "a nice simple clean setup"): systems carry RadioReference's
    details (location, county, type, voice); `PUT /api/v1/systems/{id}` edits a system and the
    site editor its identity; the scan takes bands and settings from the page and reports its
    band and window; the Systems page shows found and configured systems in one compact card,
    edited in place (section 10). The receive window moved to Diagnostics.
  - **Scan fixes** (section 10): each probe hears only its own carrier (a probe had been given
    the previous carrier's identity), and a rescan moves a configured site to the control
    channel it is heard on.
  - **DMR channel tables are learned** (section 10): a grant on an unmapped LCN is followed on
    a candidate (the carrier that keyed up for it, the control channel, the known channels, the
    strongest intermittent carrier), kept once the call's voice header names the granted
    talkgroup. Live on Clay Electric: LCN 5 learned as 454.36875 MHz.
  - **The survey** (section 10): every spectrometer frame of the live window, averaged over ten
    minutes into the carriers heard, steady or intermittent; the intermittent ones are DMR
    channel candidates.
  - **The Now page** (`UI_BRIEF.md`): the system card (a system and a site list that make a
    site live; its details, identity and control channel health) and the two traffic channels,
    left and right, stay on screen; the live site's calls scroll below in their own pane.
  - **Two DMR calls at once without the gateware** (Andy: "handle traffic from the control
    channel without needing the traffic channel"). DMR is decoded in software from IQ, and only
    the control chain and traffic chain 1 send IQ (chain 2 sends the gateware's P25 dibits
    alone), so a DMR site's lanes 1 and 2 are two calls, not two receivers: a call on the
    control channel's carrier is followed from the control decoder's own messages (a control
    repeater carries calls on its other timeslot: Clay Electric's LCN 5 TS2), and lane one's
    receiver carries the rest, both timeslots of its carrier at once. The follower keeps two
    calls off the control channel on one frequency (the other waits as busy). A second carrier
    needs an IQ tap on chain 2: a later gateware change.
  - **A lane holds a talkgroup** (Andy: "hold the tg to the lane vs just holding a tg overall"):
    `PUT /api/v1/hold` with `lane`; the lane takes only its talkgroup, the talkgroup goes only
    to it, whatever its alias says, and the other lane follows as before. The Now page's
    traffic cards each have the picker.
  - **No page polls** (UI_BRIEF): the state the pages share (status, calls, traffic channels,
    recordings, systems) comes from `/ws/live`, and so does what only some pages show, while a
    page subscribes: each spectrometer frame, the event log's new lines, the radio's readback
    every 3 s, the window and survey every 5 s, the crystal when it changes. The Activity page
    reloads its history when calls close. On unit A every page made no HTTP request in 12 s
    after loading.
- 2026-10-03, alert tones, listening and the whole call list (Andy, from a review of TG 300's
  tone-outs):
  - **What Clay sends:** no two-tone pages on any talkgroup (those go out on VHF 154.205 MHz,
    simulcast from the trunked system). Its dispatch consoles (1011-1014) key a hi-lo warble
    before a dispatch, 806.5 and 1506 Hz as decoded (each 240 ms, 2 to 4 cycles), and sometimes
    a 1010 Hz beep (pulsed or steady). The vocoder rebuilds a tone as a harmonic of its pitch, so
    the console's tones are known only to about 1 % (800-814 and 1488-1524 Hz).
  - **Alert tones** (`audio::alert`): each lane's decoder finds them in the audio before the AGC
    (a 64 ms FFT each frame; a tone holds 85 % of the energy from 100 Hz; a sequence is an alert
    when two tones switch, one tone pulses or one tone lasts 500 ms). On A's 2,143 Clay
    recordings it finds the 46 warbles and 4 beeps on TG 300 and the 2 beeps on TG 301, and
    nothing else (a voice whose fundamental sat below 250 Hz had passed for a tone until the
    band took in 100 Hz and tones began at 280 Hz). About 1 % of a core per decoding lane.
  - **Where they go:** the history (schema v4 `alerts`), the recording's bookmarks (also WAV cue
    points), `/ws/live` (`alert`) and `/ws/events`, `/ws/audio` (`alert`, before the frame that
    makes one) and `GET /api/v1/activity/alerts` (grouped by kind and tones, and the newest, each
    with its dispatch: its own call when its voice runs on 4 s past the tone, else the first of
    the sending radio's next transmissions on the talkgroup with 4 s of voice, each within 10 s
    of its last, other radios' between passed over, since a unit may answer the console before it
    speaks; without one, the longest of those with 3 s; Andy's limits). On the 52 alerts in A's
    recordings it links at least 50 to speech (13 to the alert's own call). On the 12 in A's
    history it links 11, 8718's past a unit's answer and 8622's a 3.96 s message; 8782 (a beep
    from 1011) was followed only by 2.52 s of voice.
  - **Listening** (this browser's, `prefs.js`): Listen comes back after a reload (the browser
    plays once the page is clicked); the volume and the leveller are back in the header; each
    traffic card has its own volume and mute; "Alerts only" plays only the talkgroups an alert
    tone opened, for the time chosen: a lane's call is held back until its alert, then plays
    from its start, tone included (`audio/gate.js`).
  - **Now's calls:** every call of the live site, older ones read from the history as the list
    reaches its end; each row has its details, an alert badge and a play button per bookmark.
    The lane pickers list the talkgroups the history heard in the clear in the last week and
    never one heard only encrypted.
  - **Fix:** the boot listing of the card's recordings stat'ed every file; on the card's FAT
    each stat scans the directory, so 2,260 files took 16 s, past the 15 s limit, and a restart
    listed none. The listing reads names only and the sizes come from the history.
  - **Quiet consoles** (Andy: 1013 is very quiet): the AGC never cut them; it started each call
    at unity and took about a second to rise, so a console's 1-2 s transmissions gained 1-3 dB.
    Undoing the AGC on 2,149 TG 300 recordings showed the source: a console's level follows the
    dispatcher at its mic (1013 near -30 dBFS, then -46 to -48 from 08:06; 1012 -45 overnight),
    against -32 for field radios and -22 for the consoles' own tones. The AGC now starts each
    transmission from its radio's recent level (Andy: the last few, recent ones only) and tracks
    the first 500 ms fast, up to +24 dB. On those calls' rebuilt raw audio the scanner's own code
    puts 82 % within 6 dB of the target (41 % before); every sender's median is -18 to -21 dBFS.
    The browser's Level (x0.25 to x8 toward -20 dBFS, set from each transmission's first 20 ms)
    already levelled live audio; recordings had nothing after the AGC.
- 2026-10-03, change 080 (branch `080-atsc`, worktree `maia-sdr-080`): **ATSC TV mode** (Andy:
  the unit does whatever mode it is in; scanner tabs in scanner mode, an ATSC tab in ATSC mode).
  `services::mode` keeps the mode in `state/radio.json`; ATSC mode holds the radio lease with the
  live site paused (`LiveState::Away`). Its first part is the TV channel finder
  (`protocol::atsc`, `services::atsc`): RF 4-36 in 16 MSPS windows of two channels, each
  channel 8-VSB (its pilot), no 8-VSB pilot (ATSC 3.0 or other) or vacant, with its carrier to
  noise and its spectrum. On unit A (UHF omni) it read 33 channels in 15 s and agreed with
  Andy's HDHomeRun (VHF/UHF directional) on 18 of the 19 channels that receives now, ATSC 3.0
  on RF 18 included; the miss is VHF RF 11, under a third-harmonic image of the 600 MHz band.
  Record: `doc/changes/080_atsc_tv_mode.md`.
- 2026-10-04, change 081 (branch `081-atsc-names`, worktree `maia-sdr-080`): **ATSC station
  names** (Andy: "jump to the station names"). After its sweep the TV scan tunes each 8-VSB
  channel of 15 dB or more on its own, captures 0.5 s from the radio core's IQ capture ring
  (`RadioHw::capture`) and decodes it in software (`protocol::atsc`: `demod`, `fec`, `ts`,
  `psip`, `receiver`). The receiver is a matched filter evaluated at each symbol instant
  (NEON), segment-sync timing, a least-squares equalizer, 12 soft Viterbi decoders,
  Reed-Solomon and PSIP. On the A9 it takes 5.3 s for 0.8 s of signal on both cores. On unit A
  (directional antenna) 16 stations named themselves with 104 virtual channels, every number
  and name as the HDHomeRun has them; RF 10 and 11 (MER about 17 dB) did not decode. Record:
  `doc/changes/081_atsc_station_names.md`.
