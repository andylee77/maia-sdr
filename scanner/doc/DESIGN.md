# 076 — Refactor: a multi-band, multi-protocol scanner

**Started:** 2026-10-01. **Branch:** fishball-p25. **Bake required:** no (a later, separate bake is
proposed in section 13). **Brief:** `BRIEF.md`.

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
3. **Three configuration layers:** radio, systems with their sites, and profiles. They live in new
   versioned files, migrated from today's five. A fresh unit has no sites and starts with a scan.
4. **The live site is one server-side state** with one switch. The switch moves the radio
   window, the decoders and the profile together.
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
| D1 | Config files | `radio.json`, `systems.json` and `profiles.json` in `/mnt/jffs2/scanner/`, written only when the user changes something. What the radio learns by itself (crystal calibration, IDEN bands, LCNs, grant counts) goes to `state/` files beside them. | One file per layer, including learned state. The grant counts alone would rewrite the systems file every 10 minutes. |
| D2 | Talkgroup and radio names | Per **system**: IDs are system-wide in P25 and DMR. Migration merges each system's sites' names and logs any conflict. | Per site, as today. |
| D3 | Profile scope | A profile belongs to a system. Each site picks its active profile. | Per site, as today. |
| D4 | Old files | Left untouched. Migration copies into the new files, so the old binary still runs on the old files and rollback is booting the old image. | Migrate in place. |
| D5 | History migration | Copy v1 into a new `/mnt/sd/scanner-history.sqlite` (v2). Keep `p25-history.sqlite` until Andy deletes it. | In place, after a `VACUUM INTO` backup. |
| D6 | Seed sites | Removed from the binary. On units A and B, clay, duval and cec_gcs exist **only** in the binary, so frozen copies stay inside the migrator. It writes out every seed a unit references. The repo files become test fixtures and can be imported. | Keep an importable "library" in the UI. |
| D7 | API | The UI moves to a typed `/api/v1`. Diagnostic endpoints keep their paths, but every GET that writes becomes POST/PUT, with tools and bench updated in the same commit. Legacy user-facing routes stay as adapters until their consumers move. No auth in 076; writes get an Origin check. | Version everything now, or add tokens now. |
| D8 | Pages | The Radio page dissolves: config goes to Settings, and spectrum, coverage and manual tune go to Diagnostics. The recording switch and the speakers/profile picker move from Now to Settings. Now shows the active profile read-only, with a link. | Keep the speakers on Now (063). |
| D9 | Names | Superseded by D13: the fresh crate needs a name of its own. `p25-json` and `p25-pac` keep theirs until the cutover. | — |
| D10 | FPGA | Nothing moves into gateware in 076. 076 models what each chain can do. One later bake adds an IQ tap on traffic chain 2 (section 13). | — |
| D11 | Packet data | The v2 schema has a `data_packets` table. Fill it only if Andy wants: 074b was parked. | Leave the table out. |
| D12 | P25 regression gate (Andy, 2026-10-01) | B is wired into A over the bench link whenever a test needs it. The replay corpus (`rf.p25_corpus`), with B transmitting into A, is the P25 gate from phase 2, next to host replay and live A. | Host replay plus live A only. |
| D13 | Execution (Andy, 2026-10-01) | A **fresh crate** in this repo, a workspace sibling of `p25-httpd/`, built top-down in phases. The old binary stays in production, with fixes only, until the cutover. Proposed name: `scanner/` (binary `scanner`); at the cutover tezuka_fw's package and init script switch to it. | Refactor in place (the 14-stage plan this replaces). |
| D14 | Docs (Andy, 2026-10-01) | The old docs stay where they are. Docs that the refactor needs live in the fresh crate (`scanner/doc/`), starting with this design when phase 1 creates the crate. | Remove the old docs after a `pre-076` tag. |

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
  main.rs            ~40 lines: parse args, boot::run()
  boot/              version (BUILD_TAG), args, logging + panic hook, persisted (load and migrate, once),
                     radio init, streams, receivers, trunking, audio, services, state (AppState as
                     handles), server, supervise, shutdown (flush history, recordings, plan)
  util/              time (unix_ms, Stamp {mono, unix}, iso), atomic_file (tmp + fsync + rename,
                     versioned JSON), fs_usage
  hardware/          drivers only
    ad9361.rs        IIO, with a shadow of commanded values
    mmio.rs          uio, rxbuffer
    p25core/         regs, ddc, lsm, rings, spectrometer, lanes, irq, lsm_monitor (the 475-line heartbeat
                     now inline in main.rs); fpga.rs is split here
    presets/         ddc_presets (generated), ddc_fir_ram, ddc_rate; core_version
  radio/             the only way to move hardware
    tuner.rs         apply(TuningPlan), retune_lane(), nco_offset(), scale_lo_shift(), watch<TuningSnapshot>
    lease.rs         Normal | Switching | Scan (External later, for remote libiio)
    lanes.rs         LaneId, LaneCaps {hdl_dibits, iq, protocols}, LaneController (one release sequence)
    plan.rs          the window planner (the pure part of lo_plan)
    ppm.rs           crystal calibration and tracker (autoppm), recentre
    streams/         iq_hub, dibit_readers, dibit_airtime, dibit_ring, spectrum producer, wideband capture
  dsp/               fir, halfband, taps, fsk4 (DifferentialDemod, ideal_phase, interpolator), Complex32
  protocol/
    events.rs        ControlEvent, TrafficEvent, LogicalChannel, SiteIdentity (the decoders'
                     common output)
    fec/             the codes both use: Golay(24,12) with Golay(18,6), the Hamming codes
    p25/             framer, tsbk, control decoder, traffic decoder (voice_frame + forwarder parsing),
                     pdu, c4fm demod, fec (BCH NID, Reed-Solomon, trellis)
    dmr/             demod, framer, fec (bptc, cach, emb, slot type, crc, RS(12,9)), message,
                     tier3 control, traffic
  trunking/          protocol-neutral, host-tested
    ids.rs           SystemId, SiteId, TalkgroupId(u32), UnitId(u32), CallId
    follow/          one follower: ordered gates, lane choice, pre-emption
    calls/           Call, CallBook (lifecycle + counters + recent ring), CallsView, CallEvent
    site/            LiveSite: activate(), the live state, per-site runtime memory
    receivers.rs     runs the live protocol's decoders on their streams and feeds events in
  audio/
    codec/           VoiceCodec; imbe (jmbe port), ambe2 (jmbe AMBE port)
    agc.rs           PcmAgc, the only copy
    pipeline.rs      per lane: frames → codec → AGC → 20 ms chunks
    pacer.rs, live.rs   one pacer per lane; the audio broadcast
  services/
    config/          radio, systems, profiles, state, migrate (with the frozen legacy seeds)
    history/         store (schema v2), writer, queries, migrate_v1
    recordings/      manager, storage (RAM/SD), index, wav, naming
    discovery/       scan jobs (P25 + DMR), grouping into systems, merge
    clock/           site clock, NTP, clock task
    events/          event log, the typed /ws/events feed
  api/               one route table builds the router and the catalogue; ApiError; typed DTOs
    v1/              status, radio, systems, sites, profiles, calls, recordings, activity, scan, events
    diag/            protocol and hardware diagnostics (today's chain.rs, debug.rs, ...)
    legacy.rs        old user-facing paths as adapters until their consumers move
  ui/                ui_assets.rs + static files (index.html, js/, css/)
dsp-lab/             dev crate: lsm models, sw_demod, golden dumps, software_decode tests
```

Where today's files go. In the fresh crate, "goes to" means ported (leaf code, with its tests) or
rewritten there (glue); "delete" means not carried over.

| Today | Goes to | Notes |
|-------|---------|-------|
| `app/grant_follower.rs` (1711) | `trunking::calls` (lifecycle) + `trunking::follow` (pure policies) | Split |
| `app/grant_follower_routing.rs` (1138) | `trunking::follow` + `radio::lanes` | The 650-line `select!` arm becomes ordered gate functions |
| `app/grant_stats.rs`, `call_counters.rs` | `trunking::calls` | Absorbed by the CallBook |
| `app/imbe_forwarder.rs` (1723) | `protocol::p25` traffic decoder + `trunking::calls` + `audio::pipeline` | Split; its 86 fields mostly go |
| `app/dmr_task.rs` (994) | `protocol::dmr` decoders + `trunking::receivers` + api DTO | Split; the lifecycle bridge goes |
| `app/dmr_follower.rs` | `trunking::follow` | Merged into the one follower |
| `app/dmr_voice.rs`, `vocoder_task.rs`, `audio_pacer.rs` | `audio::{pipeline, agc, pacer}` | Two AGCs and the second pacer go |
| `app/c4fm_task.rs` | `trunking::receivers` (the runner and the LSM/C4FM choice) | |
| `app/autoppm.rs`, `recentre_task.rs` | `radio::ppm` | Call the Tuner, not `api::tuning` |
| `app/discovery*.rs` | `radio::lease` + `services::discovery` | Split |
| `app/iq_hub.rs`, `dibit_readers.rs`, `dibit_airtime.rs` | `radio::streams` | |
| `app/traffic_heartbeat.rs` | `hardware::p25core` register read + `protocol::p25` HDL source | Split |
| `app/lane_policy.rs`, `traffic_lane.rs` | `trunking::follow`, `radio::lanes`, `boot` | |
| `app/history_task.rs` | `services::history::writer` | No ring polling |
| `app/data_task.rs` | `protocol::p25` (data) + `services::history` | The `DATA_CHANNEL_HZ` static goes |
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
| `httpd/api/chain.rs`, `debug.rs` | `api::diag` | |
| `httpd/ui/`, `ui_assets.rs` | `ui/` | |
| `jmbe/`, `vocoder/` | `audio::codec::{imbe, ambe2}` | |
| `lsm/`, `sw_demod/` | `dsp-lab` dev crate | First move `RRC_TAPS_25K` and `Complex32` (used by production code) to `dsp` |
| `protocol/p25/c4fm.rs` | `dsp::fsk4` (shared parts) + `protocol::p25` | DMR borrows them through `pub(crate)` today |
| `protocol/p25/control_channel/mod.rs` (1756) | `protocol::p25::{framer, control, traffic, diag}` | Split; the calls into app and services become events |
| `protocol/p25/events.rs` | **delete** | Replaced by `ControlEvent` |
| `protocol/p25/traffic_chain.rs` | `radio::lanes` (hardware state only) | `grant_map` and its 2 s dedup go |
| `services/ui_settings.rs` (1318) | `services::config::{radio, profiles}`, recordings, calls, clock | Split |
| `services/sites.rs`, `lo_plan.rs`, `monitor.rs` | `services::config`, `radio::plan` | The statics go |
| `services/history.rs` | `services::history::store` | Schema v2 |

Each module gets a short header saying what it owns, and comments state intent only. History stays
in git and `CHANGELOG_FORK.md`, never in the code.

## 3. Configuration

### 3.1 Files

All files are versioned JSON, written by `util::atomic_file`: tmp, fsync, rename, then fsync the
directory. A file whose `version` is newer than the binary understands is read but never written,
so a downgrade cannot drop fields.

| File | Owner | Holds | Written |
|------|-------|-------|---------|
| `/mnt/jffs2/scanner/radio.json` | `services::config::radio` | Gain mode, presets allowed, traffic chains, call hang/grace, recording policy and storage, history limits, clock source | On a user change |
| `/mnt/jffs2/scanner/systems.json` | `services::config::systems` | Systems: protocol, identity, label, talkgroup and radio names, and their sites (control channels, alternates, known channels, channel plan seed, window policy, modulation) | On a user change or a scan "Add" |
| `/mnt/jffs2/scanner/profiles.json` | `services::config::profiles` | Profiles per system (groups, speakers, monitor, ignore) and the active profile per site | On a user change |
| `/mnt/jffs2/scanner/state/radio.json` | `services::config::state` | Live site; crystal calibration | On a switch; on a calibration |
| `/mnt/jffs2/scanner/state/sites/<id>.json` | `services::config::state` | Learned per site: identity seen, IDEN bands or LCNs, neighbours, secondary CCs, grant counts per channel, known-encrypted talkgroups, last recentre | Every 10 min if changed, on a switch, at shutdown |

Learned state is loaded when the site goes live. That fixes the 073 open item where a switch left
grants waiting for the IDEN broadcast.

IDs are slugs (`[a-z0-9_-]`, validated everywhere). **Existing site names are kept as site IDs**
(`clay`, `duval`, `cec_gcs`, `psic_st_johns`, ...). The history rows and recording file names
already use them, so nothing in the data has to be rewritten.

### 3.2 Schemas (examples)

`radio.json`:

```json
{
  "version": 1,
  "gain": { "mode": "slow_attack", "manual_db": null },
  "presets_allowed": ["8M", "12M", "16M"],
  "traffic_chains": 2,
  "calls": { "hang_ms": 3000, "end_grace_ms": 2000 },
  "recording": { "enabled": true, "storage": "sd", "ram_max_count": 40,
                 "sd_max_count": 2000, "sd_max_mb": 2048 },
  "history": { "retention_days": 365, "sd_max_mb": 2048 },
  "clock": { "source": "site" }
}
```

`systems.json`. Identities are stored as numbers, as today; the UI shows them in hex.

```json
{
  "version": 1,
  "systems": [
    {
      "id": "clay-county", "label": "Clay County", "protocol": "p25",
      "identity": { "wacn": 781824, "system": 2208 },
      "talkgroups": { "300": "EMS Dispatch" },
      "radios": { "1014": "Console 14" },
      "sites": [
        {
          "id": "clay", "label": "Clay County",
          "identity": { "rfss": 1, "site": 1, "nac": 2209, "lra": 0 },
          "control": { "freq_hz": 860962500, "alternates_hz": [859437500, 858987500, 860437500] },
          "modulation": "auto",
          "channels_hz": [852438500, 855237500, 856437500],
          "window": { "auto": true, "min_preset": "12M", "cc_position": "top" },
          "notes": ["control_freq_hz is LCN 11 ..."],
          "source": "p25-sites seed clay.json (migrated)"
        }
      ]
    },
    {
      "id": "clay-electric", "label": "Clay Electric", "protocol": "dmr_tier3",
      "identity": { "model": "small", "network": 0 },
      "talkgroups": { "87921": "Lake City", "87924": "Orange Park" },
      "radios": {},
      "sites": [
        {
          "id": "cec_gcs", "label": "Green Cove Springs",
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

`profiles.json`:

```json
{
  "version": 1,
  "profiles": [
    { "id": "clay-county/default", "system": "clay-county", "name": "Default",
      "groups": [ { "name": "Primary", "talkgroups": [300] }, { "name": "TAC", "talkgroups": [301] } ],
      "speakers": { "left": ["Primary"], "right": ["TAC"], "other": "off", "preempt": true },
      "monitor": [301, 300], "ignore": [402, 700] }
  ],
  "active": { "clay": "clay-county/default", "cec_gcs": "clay-electric/default" }
}
```

Profiles now apply to DMR too: monitor, ignore and speakers go through the one follower (phase 3).

### 3.3 Migration, file by file

Migration runs once at boot when `/mnt/jffs2/scanner/` does not exist and any legacy file does.
It writes the new files and a report (`scanner/migration-076.log`, plus an event-log entry). It
never modifies or deletes a legacy file (D4).

| Today | Becomes | Rules |
|-------|---------|-------|
| `p25-ui-settings.json` | `radio.json` (recording, call, radio gain, clock); `systems.json` names; `profiles.json` | `sites[s].tg_aliases` and `unit_aliases` go to the site's system. On a conflict between sites, the active site wins, then the site with more names; each conflict is logged. `sites[s].profiles` become profiles of the site's system; a name clash between sites gets the site label appended. The top-level live copy is dropped: after `store_live` it equals `sites[site]`. A pre-069 file with no `sites` makes its live fields the active site's "Default", as today. |
| `p25-sites/*.json` + embedded seeds | `systems.json` sites; `state/sites/<id>.json` (IDEN bands) | Sites are grouped into systems by identity: P25 by (WACN, system), DMR by (model, network). A site with no identity gets a system of its own. A seed is written out when it is referenced by `active.json`, `ui.sites`, a plan file, the history or a recording name. The overlay merge rules of `load_site` are applied first. Kept: `_notes`, `_seed_source` (as `source`). Dropped: `_runtime_overlay_path` (stale), `preset_default` (the planner decides). `modulation` becomes "auto", today's runtime behaviour. cec_gcs gets the colour code, model, network and site from the frozen seed (today only in `_notes`). |
| `p25-sites/active.json` | `state/radio.json` `live_site` | If it is missing: `ui.site`, then "clay" if clay is referenced (today's fallback), else no site. |
| `p25-plans/*.json` | `systems.json` `window {auto, min_preset}`; `state/sites/<id>.json` `{grants, last_recentre_ms}` | Grant counts carry over as they are. They counted TSBK repeats (×2–3); new counts are per call. The planner only compares a site's channels with each other, so the scale cancels. |
| `p25-ppm-cal.json` | `state/radio.json` `crystal {ppm, measured_at_lo_hz, lo_shift_hz, method, at}` | The ppm is the quantity (074c/d). The shift is kept for reference. |
| `/mnt/sd/p25-history.sqlite` | `/mnt/sd/scanner-history.sqlite` (v2) | Section 8. |
| SD recordings | unchanged files, indexed into history v2 | Section 9. |

Testing: on copies of units A's and B's real files first (phase 0 fixtures), then on A, then on
B.

### 3.4 First run

- With no legacy files and no `scanner/`, the radio boots to **NoSite**:
  - the AD9361 is configured but idle;
  - no decoders run;
  - the Now page says "No sites yet" and links to Systems, which opens on the scan.
- The init script's `--control-freq`, `--rx-lo` and `--preset` become optional; they are ignored
  once a live site exists. The tezuka_fw init script drops them (one tezuka_fw commit, at the cutover).

## 4. The live site and multi-band

"Which site is live" becomes one state owned by `trunking::site::LiveSite`, published on a
`watch`:

```text
NoSite ──activate(s)──▶ Switching{to: s} ──▶ Live{site, system, profile, plan}
Live ──activate(t)──▶ Switching{to: t} ──▶ Live{t ...}
Live ──scan──▶ Scanning (radio lease) ──▶ back to Live{same}
```

`activate(site)` is the only way to change site:

1. Take the radio lease (`Switching`). Grants are dropped from here (today's 073 hold).
2. CallBook: close open calls with reason `site_switch`. They keep their own site.
3. Stop the old protocol's receivers: control decoder, traffic decoders, its demod threads.
4. `Tuner::apply(plan)`: AD9361 LO, rate and bandwidth, control NCO, lanes idle, scaled crystal
   shift.
5. Load the site's learned state (channel plan, identity, encrypted list) and its active profile.
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
- **Lanes have capabilities** (`radio::lanes`). The follower picks only lanes that can carry the
  grant:

  | Lane | Capabilities |
  |------|--------------|
  | Chain 1 | HDL dibits and IQ: P25 and DMR |
  | Chain 2 | HDL dibits: P25 only |
  | Control slot | The control receiver's other timeslot: DMR grants to the control repeater's TS2 |

  When the chain-2 IQ tap exists (section 13), chain 2 gains DMR with no other change.
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
- **Speaker routing comes from one place.** The server already picks lanes from the profile. It
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
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);   -- schema, migrated_from, created
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

Migration from v1:

- **Detect the schema by its shape.** `user_version` is always 1, with or without the `channel`
  column.
- **Copy into the new file in one transaction** (D5):
  - calls with their columns renamed;
  - `call_units` → `transmissions` (no per-speaker times for old rows);
  - the hour tables copied as stored, not rebuilt, so the totals Andy has seen stay;
  - `site_stats` → `sites`, mapped to systems from the migrated config;
  - `talkgroups` and `radios` computed from the calls;
  - recordings linked once (section 9).
- **Old rows keep their known faults.** Not-followed rows stored with 0 grant time (defect 1)
  can't be repaired. Rows filed under the wrong site before 073 stay where they are.

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
- **The index** is the history's `recordings` table. At boot, the directory listing and the
  table are reconciled:
  - a file not in the table is parsed with today's file-name parser, inserted and linked to its
    call once;
  - a row whose file is gone is removed.

  The ±10 s full scan per file goes. Files the parser does not recognise are left alone and never
  deleted, as today.
- **SIGTERM** finishes the active recordings and drains the SD writer before exit (defect 6).

## 10. Auto-setup scan

The 071 finder becomes `services::discovery`, run on the Systems page, and on first run.

- **Bands:**
  - P25 700, 800 and 900 MHz (today);
  - UHF 450–470 MHz and VHF 150–174 MHz (today's optional list), on by default.

  About 8 windows at 16 MSPS: roughly 4–5 minutes against 2.3 today.
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
- **Proposal:** sites grouped into systems by identity. The user ticks what to keep and names it.
  "Already configured" sites are marked.
- **Rescan** matches by identity, or by control channel within 3 kHz. It never overwrites labels,
  names or profiles. It adds learned data (alternates, plan) to state, and adds new sites only when
  ticked.
- **A DMR site without an LCN map** follows grants with absolute frequencies. An LCN with no
  frequency shows as `unknown_lcn` and can be entered in the site editor. Learning an LCN by
  watching which carrier keys up after its grant is left for later.
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
| `GET/PUT /api/v1/radio`; `POST /api/v1/radio/{tune, preset, gain, ppm/calibrate, ppm/auto}` | `/api/presets`, `/api/preset`, `/api/tune`, `/api/rx_gain`, `/api/ppm*`, radio parts of `ui/settings` | UI; `runs/dmr/log_dmr.py`, `record_chunks.sh` |
| `/api/v1/systems[/{id}]` (names inside) | `/api/sites`, `/api/sites/{name}`, `/api/aliases` | UI; tool `status` |
| `/api/v1/systems/{id}/sites/{site}`; `POST /api/v1/sites/{id}/activate`; `/sites/{id}/plan`, `/recentre` | `/api/site`, `/api/site/plan`, `/api/site/recentre` | UI |
| `/api/v1/profiles...`; `PUT /api/v1/sites/{id}/profile` | profile actions in `ui/settings`, `/api/monitor`, `/api/encrypted_tgs` | UI; bench `corpus_tests` (monitor) |
| `/api/v1/recordings...` | `/api/recordings...` | UI; tools `poll_recordings_persist`, `compare_sim_vs_board`, `audit`, `capture_session` |
| `/api/v1/activity/*`, `/api/v1/data` | `/api/activity/*`, `/api/data` | UI |
| `/api/v1/scan` (GET, POST), `/scan/cancel`, `/scan/results/{key}/add` | `/api/discovery*` | UI |
| `/api/v1/events`; typed `/ws/events` | `/api/log`, `/api/recent_tsbks`, `/api/dmr/messages` | UI; 8 tools use `/api/log`; `log_dmr.py` |
| `/api/v1/receivers` (status; modulation choice) | `/api/modulation`, `/api/dmr` | UI; tool `retune_probe`; `log_dmr.py` |
| `/ws/audio` (v2 framing; speaker in meta) | unchanged path; v1 framing kept | UI; tool `ws_audio_capture`; bench `wsaudio.py`, `services.py` |

Diagnostics keep their paths in `api::diag` (D7). Examples: `hdl_lsm`, `irq_stats`,
`decoder_compare`, the dibit and IQ dumps, `*_lsm_control`, `nid_capture`, `sync_tune`, `bch_t`,
`decoder_reset`, `pipeline`, `dibit_delivery`, `spectrum*`, `wideband_iq_capture`, `imbe_dump`,
`audio_test`, `sys_health` and `/ws/iq`. The GETs that write become POST or PUT:

- `traffic`, `traffic2`, `monitor`, `rx_gain`, `sync_tune`, `decoder_reset`;
- `control_lsm_control`, `traffic_lsm_control`, `nid_capture` and the two `*_aligned` captures.

The tools and bench that use the GET forms change in the same commit.

**Not carried over** to the fresh crate, after the consumer check:

- 13 routes with no consumer: `sites/{name}`, `ps_cores`, `freq_health`, `control_dibit_capture`,
  `traffic_dibit_capture_aligned`, `traffic_iq_dump`, `ppm/nudge`, `agc_threshold`,
  `recordings/{id}/events`, `recordings/{id}/sync_trace`, `traffic2`, `deviation` and
  `distribution`;
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
| **Now** | The at-a-glance summary: the live site card, a call card per lane, recent calls (site picker as today), the active profile's name with a link to Settings |
| **Systems** | System cards with their sites (identity, control channel, health, Listen), the site editor, and the setup scan with results grouped into systems. First run opens here. |
| **Activity** | As today, with a system/site picker that includes DMR; packet data is a P25 component |
| **Settings** | Radio (gain, crystal, presets, clock, storage and recording), names per system, profiles (groups, speakers, monitor, ignore, encrypted), browser, about |
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

Andy asked for this on 2026-10-01. Summary: **nothing has to move for 076.** One small bake is
worth doing afterwards.

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
- The software C4FM control demod is 12–17 %; a DMR receiver is 12.7 %. About 60 % of each is
  FIR work.
- Worst cases:
  - a C4FM P25 site with traffic C4FM on chain 1 is about 51–61 % of the 200 %;
  - a DMR site with a followed call is about 48–55 %, or 36–40 % once C4FM stops at DMR sites
    (section 5).

### Candidates

| Candidate | Saves | Costs | Verdict |
|-----------|-------|-------|---------|
| Run only the live protocol's decoders | 12–17 % of a core at DMR sites | PS only | **076** (section 5) |
| Chain capability model | — (lets lane 2 carry DMR/C4FM when the tap exists) | PS only | **076** (section 5) |
| Scan on the existing spectrometer, DMR by software sync count | — | PS only | **076** (section 10) |
| **Chain-2 post-DDC IQ tap** (IQ packer + DMA ring at the free 0x1E00_0000, bank 15, IRQ bit 9) | Enables DMR and C4FM voice on lane 2 with the existing SDRTrunk-parity software | ~150 slices, ~1 k FF, 1.5 BRAM, 0 DSP | **Later bake (077)**, with the always-responding register bridge (no CPU stall on vacant banks) and 064's timing fixes |
| Symmetric, NEON-friendly FIR in `dsp::fsk4` | ~5–6 % of a core per software receiver | PS; parity gated by the DMR and C4FM tests | Late 076 or after |
| Host experiment: DMR framer on the LSM model's dibits | If it passes, lane-2 DMR needs no bake | ~1 day, host only | Optional; LSM on C4FM passes only 42–69 %, so likely not |
| LsmFir delay lines to SRL/LUTRAM | ~5k FF per chain of area back | Bake | Only before any new HDL block |
| HDL DMR/C4FM front end (filters + discriminator) | ~8–9 % per receiver | ~850 slices per chain (does not fit without the area recovery); fixed point cannot match SDRTrunk's f32; weeks | Only if CPU ever binds (handheld power, three or more software receivers) |
| HDL 4FSK symbol processor or C4FM demodulator | — | Months; SDRTrunk's branchy sync-driven timing; worse late entry | Never |
| Vocoders, FEC, PCM AGC, autoppm/recentre | ≤ 2.5 % each | 6–8 weeks for a vocoder alone | Never |
| A fourth chain or a polyphase channelizer | — | DSP 217/220, slices over 100 % | Never on the Z7020 (retired once, 2026-05-03) |

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
2. **Unit fixtures.**
   - **Raw copies:** unit A's `/mnt/jffs2/p25-*` files, its history database and its SD
     recording names, pulled read-only into the gitignored `runs/076/units/A/`.
   - **Committed copy:** `tools/unit_fixtures.py` writes `unit_a/`, with radio IDs replaced by
     stand-ins of the same digit count. The migration tests run on it.
   - **Unit B:** `unit_b/` has the history schema from before 074a (no `channel` column), with
     379 call ids repeated by restarts before ids continued past the history. It has no site
     files: it runs on the "clay" seed compiled into the binary (D6).
3. **Activity snapshots.** `history_snapshot_tests` runs the Activity queries on the fixture
   database and compares the answers with `unit_a/activity.json`. History v2 must give the same
   answers after the migration.
4. **Route contract.** `tools/route_shapes.py` keeps the JSON shapes of the 31 GET routes the
   tools and bench read, in `routes/shapes.json`, captured on a P25 site and on a DMR site.
   `check` lists the keys that went missing or changed type.
5. **Baseline.** `tools/live_baseline.py` runs read-only on unit A for 30–60 minutes per system,
   with today's build. It records control-channel decode % (TSBK CRC or DMR valid), calls,
   follow rate, vocoder errors, recordings, history rows and CPU. The numbers are in the status
   log.
6. **Replay corpus baseline** (`rf.p25_corpus`), once B is wired into A (D12).

**Running the fresh crate on unit A before the cutover:** stop the old binary (S60), run the new
one from `/tmp`, check it, then start the old one again. The fresh crate writes only
`/mnt/jffs2/scanner/` and the new history file, so the old binary's files stay as they were
(D4, D5). Check `ListAgents` first.

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
- **Migrations (phases 1 and 5):**
  - on host copies first;
  - the old files are untouched, so rollback is the old image;
  - check free SD space before copying the history (up to 2 GB).
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
  - the chain-2 IQ bake.

## Done means

From the brief:

- a fresh unit boots empty;
- a scan finds Clay County P25 and Clay Electric DMR (and the other local systems), and the user
  adds them;
- both systems decode, follow, play, record and fill the history;
- switching between them is one action;
- existing units keep their sites, names, profiles, history and recordings;
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
  - **`/ws/events`:** a notice when a call opens or closes or a recording is saved; the UI
    refreshes on them at once (32 in a minute on B).
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
    the process. RSS went from 9.6 to 12.2 MB, most of it in the first 12 minutes and 0.4 MB in
    the last 18; a longer soak is to show that it levels off.
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
- 2026-10-01, phase 8 (prepared, waiting for Andy's go):
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

