# 076 — Refactor brief: a multi-band, multi-protocol scanner, organised properly

**Written:** 2026-10-01, at the end of change 075 (DMR), for a new Claude Code session.
**Branch:** fishball-p25 (one line; DMR is merged in). **Owner:** Andy.

## Andy's request

> Refactor the radio to handle a multi-band setup, and clean up the UI into a clean modular
> layout that supports P25/DMR systems and cards. Include clean profiles, site configs and a
> radio config, defaulting to no sites with an auto-setup scan for sites. Simplify the call
> tracking, the database use, the recording manager, etc. A full refactor and professional
> organisation.

Andy decided on 2026-09-30 that this comes after DMR is complete. DMR is now on fishball-p25
(change 075: control channel live, the follower, AMBE+2, UI). The refactor is the next
numbered change, 076.

## Where things stand

`p25-httpd` (Rust, runs on the Zynq PS of the Fishball Z7020 board) has grown change by
change. It works, but P25 assumptions run through the shared layers, and three generations
of call tracking sit side by side. Non-test lines by area:

| Area | Lines | Notes |
|------|-------|-------|
| `app/` | 14.7k | grant follower (1.7k + 1.1k routing), grant_stats, call counters, lanes, discovery, DMR task / follower / voice, history task, recentre, autoppm, forensics, seeding... |
| `httpd/` | 10.6k | axum routes, API handlers; `ui/` is 4.1k lines of plain JS modules |
| `protocol/p25` | 9.7k | control channel, TSBK, PDU, traffic chain, C4FM demod, FEC |
| `protocol/dmr` | 6.7k | 075: demod, framer, FEC, messages (SDRTrunk text) |
| `hardware/` | 5.6k | IIO, FPGA registers (svd2rust PAC), DMA rings, lanes |
| `services/` | 4.2k | sites, lo_plan, history (SQLite), ui_settings (1.3k), event log, NTP, clock |
| `jmbe/` + `vocoder/` | 4.3k | IMBE (P25) and AMBE+2 (DMR, 075) |
| `audio/` | 2.4k | recorder (1.4k), rec_storage, call boundaries |
| `lsm/`, `sw_demod/` | 3.0k | LSM / software DDC models |
| `main.rs` | 2.2k | boot orchestration inline |

Persisted state on a unit (all of it must survive the refactor):

- **`/mnt/jffs2/p25-ui-settings.json`:** profiles, talkgroup and radio names, monitor,
  routing, ignore lists, recording policy and clock, per site.
- **`/mnt/jffs2/p25-sites/*.json` + `active.json`:** site overlays. 19 on unit A, many from
  the 071 finder.
- **`/mnt/jffs2/p25-plans/*.json`:** LO plans and grant counts per site.
- **`/mnt/jffs2/p25-ppm-cal.json`:** crystal calibration and the LO it was measured at.
- **`/mnt/sd/p25-history.sqlite`:** the activity history (072/073).
- **`/mnt/sd` recordings:** WAVs and their index (057/073).

The binary also embeds seed sites (`p25-httpd/sites/{clay,duval,cec_gcs}.json`) and falls back
to "clay" when no site is active.

## Read first

1. `CLAUDE.md` (repo) and your memory notes. They hold the build and deploy rules and the
   units' addresses.
2. `doc/CODE_REVIEW_2026_09_28.md`: the analysis, findings and target layout for this
   refactor. Stages 0–2 are done; stage 3 "the refactor proper" is this change.
3. `doc/ROADMAP.md`: direction (edge device and handheld; P25 is the first protocol, not the
   only one) and the refactor section.
4. `doc/changes/075_dmr_clay_electric.md`: how DMR was added, and every place it had to
   work around P25-shaped code:
   - `u32` talkgroups (075a);
   - the site `protocol` field and LCN map;
   - lane-One call boundaries from a second follower;
   - a second pacer;
   - `PcmAgc` duplicating the P25 vocoder's AGC;
   - the UI's protocol branches.
5. `doc/changes/066_dual_traffic_chain_ps.md`, `070`, `071`, `072`, `073`, `074` (in
   `CHANGELOG_FORK.md` where there is no doc): lanes, the planner, the finder, history,
   per-site data, packet data.
6. `doc/API_CONSUMERS.md` and `doc/P25_API.md`: who calls which endpoint (tools, the bench,
   the UI). Don't remove an endpoint without checking.

## Requirements

### Radio and configuration

- **Three clean layers of configuration, each with one owner module and one file (or
  table):**
  - **Radio:** the hardware. Gain mode, crystal calibration, presets allowed, chains
    available, audio and recording storage.
  - **Systems and sites:**
    - a system: protocol (P25 / DMR Tier III / later others), identity (P25
      WACN/system/NAC; DMR network, colour code), talkgroup and radio names;
    - its sites: control channels, alternates, channel plan (P25 IDEN bands learned on air;
      DMR LCN → frequency), neighbours, LO plan state.
  - **Profiles:** what to follow and how (monitor, ignore, priority groups, speaker
    routing), per system or site as today.
- **No sites by default.** A fresh unit starts with no sites and no hardcoded seeds. Remove
  the "clay" fallback and the embedded `sites/*.json`; keep them only as optional test
  fixtures or an importable library.
- **First run is an auto-setup scan.** It extends the 071 finder:
  - sweep the bands the AD9361 can cover (700/800/900 MHz P25, VHF/UHF);
  - recognise P25 and DMR control channels (DMR: BS data sync / CACH, Tier III ALOHA
    identity);
  - propose sites grouped into systems;
  - the user picks what to keep.
  - Re-running the scan later adds sites without losing names or history.
- **Multi-band.** The radio watches one AD9361 window at a time. A configured set of sites
  can span bands (Clay P25 at 851–861 MHz, Clay Electric DMR at 451–455 MHz). Make "which
  site is live now" a first-class state with a clean switch: the window plan, the decoders
  for its protocol, its profile.
  - Leave room for scanning or priority between sites, but don't build it unless it falls out
    naturally.
- **Migrate every existing file listed above, losslessly.** That includes the history
  database and the recording index. Use a versioned schema and atomic writes.

### Protocol-agnostic core

- **Protocol modules behind traits, with a shared call model:** `protocol::p25` and
  `protocol::dmr` sit behind small traits:
  - a control-channel decoder emitting events: grant, identity, neighbour, message for the
    log;
  - a traffic decoder emitting voice frames and link control;
  - a vocoder (IMBE, AMBE+2).
  The trunking layer (follower, call lifecycle, lanes or chains, routing, profiles) then
  works on protocol-neutral events, and P25 and DMR share one follower. Today DMR has its own
  follower that talks to the P25 lifecycle through call-boundary events.
- **Simplify call tracking.** Today a call is spread over:
  - `grant_follower` (lifecycle with CallBoundary / CallTrackerEvent);
  - `grant_follower_routing`;
  - `grant_stats` rings;
  - `call_counters`;
  - `imbe_forwarder` state;
  - the recorder's own call windows.
  Aim for one call object per call, owned by one module, that the follower, recorder,
  history and UI all read.
- **Database:** one history service with one writer and a clean schema: calls, transmissions,
  radios, talkgroups, sites/systems, data. Use clear rollups; keep the 072 Activity
  features.
- **Recording manager:** one module for storage (RAM / SD), the index, retention and naming,
  driven by the call object. It must keep the existing files readable.
- **One live-audio path** with a shared AGC: the P25 vocoder thread and `app::dmr_voice` have
  two copies of the same AGC today.

### UI

- **A modular layout.** System and site cards per protocol (P25: NAC/WACN/TSBK rate; DMR:
  colour code, network/site, message rate), call cards, recent calls, activity, settings.
- **No P25/DMR branches inside generic components.** Protocol-specific parts are small
  components chosen by the site's protocol. Today `site_card.js` and `radio.js` branch on
  `protocol`.
- **Andy's placement rules:**
  - the Now page is the at-a-glance summary;
  - message and event feeds go in the Diagnostics events box (one shared box for every
    protocol);
  - configuration goes in Settings;
  - the setup scan goes in the Systems page.
- **Keep the stack:** plain ES modules, embedded at compile time
  (`httpd/ui_assets.rs`), no build step and no CDN (the unit may have no internet).

### Professional organisation

- **The target layout from the code review**, adjusted:
  - hardware;
  - dsp (shared FIRs, demodulators: today `protocol::p25::c4fm` is shared with DMR through
    `pub(crate)`);
  - protocol::{p25, dmr};
  - trunking;
  - services (config, history, recordings, clock, event log);
  - api;
  - ui.
- **`main.rs` boot orchestration** goes into modules.
- **Cleanup:**
  - delete dead code (the ARM build's warnings list it);
  - delete retired scaffolding (seeding snapshots, legacy dashboard paths, sw_demod
    re-exports);
  - delete duplicate helpers (several `now_unix_ms`);
  - merge the two grant tallies.
- **Docs:** each module gets a short header saying what it owns. Update `doc/P25_API.md`
  (or replace it with an API reference covering both protocols) and the project inventory.

## How to work

1. **Analysis first, no code changes.** Inventory the current state against the requirements
   and write the design into this file's sibling, `DESIGN.md`:
   - the module map;
   - the config schema and the migration of each persisted file;
   - the call model;
   - the history schema;
   - the recording manager;
   - the API changes, with their consumers;
   - the UI structure;
   - the test strategy;
   - the stage plan.
   **Show Andy the design and wait for his approval before moving code.** He wants a
   professional result, and the big choices are his.
2. **Then stages, each one a commit that builds, passes the checks below and leaves both
   units working.** Behaviour-preserving moves come before behaviour changes. Don't mix a
   move and a rewrite in one commit.
3. **Checks for every stage:**
   - **Host tests:** `cargo test` from `p25-httpd/` (475+ today). The golden-vector emitters
     are `#[ignore]` since 076 phase 0, so a run no longer rewrites
     `maia-hdl/test/golden_vectors/`.
   - **ARM check:** the Windows host check skips the `#[cfg(target_os = "linux")]` code:

     ```
     V=/c/Users/Andy/Projects/MAIA_SDR/maia-sdr/.venv-hdl
     PATH="$V/Scripts:$V/Lib/site-packages/ziglang:$PATH" cargo-zigbuild check --target armv7-unknown-linux-gnueabihf.2.31
     ```

   - **DMR reference:** `DMR_CAPTURE_DIR=C:/Users/Andy/Projects/MAIA_SDR/maia-sdr/runs/dmr cargo test --release dmr::` must keep 24,984+ of 24,996 lines matching SDRTrunk. The follower test checks the 20:57 call.
   - **P25 replay corpus bench:** `fbench run rf.p25_corpus` (see `bench/` and memory).
     There must be no regression in TSBK CRC %, follow rate or vocoder errors.
   - **Live, on a unit:** Clay County P25 (CC 860.9625) and Clay Electric DMR (site
     `cec_gcs`, CC 454.36875). Check calls, audio, recordings and history on both.
4. **Record as you go:** a `CHANGELOG_FORK.md` entry per stage, the status log in
   `DESIGN.md`, and memory notes for anything a later session must not re-learn.

## Practical rules of this setup

- **Repo:** `C:\Users\Andy\Projects\MAIA_SDR\maia-sdr`, branch fishball-p25.
  - Commit locally. **Ask before any push.**
  - No `Co-Authored-By` lines.
  - Firmware is `C:\Users\Andy\Projects\Tezuka\tezuka_fw` (branch fishball-dev); its
    p25-httpd package rsyncs the main checkout.
- **Builds:**
  - A quick binary: `$V/Scripts/cargo-zigbuild.exe zigbuild --release --target armv7-unknown-linux-gnueabihf` with `CARGO_ZIGBUILD_ZIG_PATH` set to the venv's `ziglang/zig.exe`.
  - The SD image: `build_tezuka_p25_pretty.sh` (~30 min). Don't run cargo while it rsyncs.
  - FPGA bakes (`build_fpga_p25_pretty.sh`) are not expected for this change.
- **Units:**
  - **Unit A:** 192.168.120.50 over Ethernet, external antenna.
  - **Unit B:** 192.168.12.1 over USB. Its SD card was checked clean on 2026-10-01.
  - **SSH:** write the full literal command:
    `ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR root@<ip> '...'`.
  - **Deploying a binary:** `cat > /tmp/p25-httpd.new`; then S60 stop; then `cp` +
    `chmod +x`; then start. It lands in the RAM rootfs, so a reboot returns to the SD image.
  - **Other sessions:** run `ListAgents` and message them before you restart, deploy or
    retune a unit another session may be using.
- **Windows pitfalls:**
  - Never run `cargo fix`.
  - `sed -i` strips CRLF from `.bat` files.
  - A double backslash in a Bash heredoc arrives single. Put edit scripts in a file
    (Write tool), not in a heredoc.
- **SDRTrunk is the reference:**
  - P25 and DMR DSP/protocol constants came from SDRTrunk (`C:\Users\Andy\Projects\SDRTrunk\sdrtrunk`). Don't "clean them up" without evidence.
  - `tools/sdrtrunk_dmr_reference.py` decodes our captures with SDRTrunk itself.
- **Gateware:** don't change it in this refactor. The register map and DMA rings are in
  `doc/P25_ADDRESS_MAP.md`; traffic chain 1's IQ ring is wired since 075b.

## Done means

- A fresh unit with no config boots to an empty state. A scan finds Clay County P25 and
  Clay Electric DMR (plus the other local systems), and the user adds them in the UI.
- Both systems decode, follow calls, play live audio, record, and fill the history.
- Switching between them is one action.
- Existing units keep their sites, names, profiles, history and recordings through the
  migration.
- The tree follows the agreed module map with no dead code, every stage passes the checks,
  and the docs (API reference, inventory, 076 design and status log) match the code.
