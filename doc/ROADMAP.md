# Fishball Scanner — Roadmap and Working Notes

Started 2026-09-28 (after change 070). This is where ideas get written down before they
become numbered changes. Each numbered change still gets its CHANGELOG entry (and, when it
is large, a `doc/changes/NNN_*.md` design note).

## Direction

Today the board is a P25 Phase 1 trunking receiver. It is meant to become a full scanner
running on the Zynq in two forms:

- a **network edge device**: headless, on Ethernet, with the web UI and API;
- a **portable handheld** in the 3D-printed case, with its own display, LEDs and controls.

Both run the same software. P25 is the first protocol, not the only one, so new work should
keep the protocol-specific parts (framing, vocoder, trunking messages) apart from what every
scanner needs:

- sites and systems;
- channels and the receive window;
- calls, talkgroups and radios;
- profiles, history and recordings;
- the API.

## Proposed order

| # | Item | Status |
|---|------|--------|
| 071 | Find local systems: sweep the band, build sites automatically | next |
| 072 | Per-site activity history: radios, talkgroups, grants, encryption, airtime; graphs | planned |
| — | Code review and analysis of p25-httpd, then refactor into clean modules | planned (see below) |
| — | Remote libiio control: detect it and share the radio | idea |
| — | Agent control: MCP server and prompt structure | idea |
| — | Clay Electric DMR | idea (needs a DMR chain) |
| — | Handheld page: the radio's face in the browser | idea |
| — | Transcription (Whisper) and LLM summaries | idea (off-board) |

The review could run before 071 so new modules land in the right place. The refactor itself
should come after 072, once the site and history models exist. Andy decides the order.

## 071 — Find local systems

Goal: a radio with no site files populates itself, and more systems can be added later.

- Sweep the tunable range in steps of the widest validated window (16 MSPS: ±7.2 MHz usable,
  change 070). At each step, take the wideband FFT (`/api/spectrum_wide`) and list the
  carriers above the noise floor.
- Try the control-channel decoder on each carrier (the control DDC retunes in-window without
  moving the LO). Keep the ones that give TSBKs with a stable NAC.
- For each control channel found, read the site identity (WACN, system, RFSS, site, NAC), the
  IDEN bands, adjacent sites and secondary control channels. Listen long enough to collect
  grants for the channel list.
- Write a site file per control channel (overlay `/mnt/jffs2/p25-sites/<name>.json`), named
  from the identity until the operator labels it. The window planner (070) then places the LO
  from the grants.
- UI: a "Find systems" page with progress, systems found, signal level and decode quality,
  plus "Add" and "Listen".
- Seeds: the SDRTrunk playlist lists nearby systems worth confirming:
  - Alachua County Public Safety;
  - Florida Power and Light;
  - SLERS (P25);
  - Putnam County Public Safety;
  - Clay Electric (DMR).
- Open questions:
  - Frequency range: 764–776 and 851–869 MHz first (P25 700/800), then VHF/UHF?
  - Adjacent-site broadcasts may name sites too weak to decode here; list them as "heard of,
    not received"?

## 072 — Per-site activity history and graphs

Goal: know who talks, on what, and how much, per site.

- A small database per site on the SD card (SQLite): grants, calls, encryption flags, and the
  radio → talkgroup affiliations and grants seen.
- Totals and airtime in seconds per radio ID, per talkgroup, per site; which talkgroups each
  radio uses; encryption history per talkgroup.
- Graphs per hour and per day (calls, airtime, busiest talkgroups and radios, encrypted share).
- Retention limits like the recordings' (count and size), so the SD card cannot fill.
- `/api/history/*` queries for the UI, the MCP tools and exports (CSV).
- The in-memory grant map (`/api/grant_map`) and the planner's grant counts (070) should come
  from the same records once this exists.
- Prior design worth reusing: SDRTrunk `doc/design/006a_call_log_database.md` (call sessions
  and events).

## Code review and refactor

Many changes have added and removed code since the last review (`doc/CODE_REVIEW_2026_04_16.md`).
p25-httpd is about 55k lines of Rust. The largest files:

| File | Lines |
|------|-------|
| `src/hardware/fpga.rs` | 2434 |
| `src/main.rs` | 2069 |
| `src/app/grant_follower.rs` | 1717 |
| `src/app/imbe_forwarder.rs` | 1713 |
| `src/jmbe/mod.rs` | 1681 |
| `src/httpd/api/tuning.rs` | 1664 |

Known smells to check:

- Dated narrative comments ("2026-05-03 …") that describe history rather than intent; the
  history belongs in the CHANGELOG.
- Duplicate helpers: several `now_unix_ms()` copies, repeated site/preset lookups.
- Dead code that the compiler already flags on the target build: `sw_demod` re-exports,
  unused `fpga.rs` functions, `sites.rs` imports.
- Scaffolding whose purpose has ended, such as the seed-snapshot primitives and the legacy
  dashboard paths.
- `main.rs` doing boot orchestration inline.
- Two grant tallies (`grant_map` and `lo_plan`).

Method:

1. **Analysis** (read-only): module map and dependencies, findings by severity, and a proposed
   module layout written to `doc/CODE_REVIEW_<date>.md`.
2. **Target layout** for a multi-protocol scanner, for example:
   - hardware (AD9361/IIO, FPGA registers, DMA rings);
   - DSP;
   - `protocol::p25` (later `protocol::dmr`);
   - trunking (sites, channels, the call lifecycle, chains/lanes, routing and profiles);
   - services (settings, history, recordings, clock);
   - API (HTTP, WS, MCP);
   - UI.
3. **Refactor in behaviour-preserving steps**, each checked by the host tests and the replay
   corpus bench (`fbench.py run rf.p25_corpus`) before it is committed.

## Boot and board configuration

- Since 070, boot tunes to the active site's control channel and planned window, and the init
  script's `--control-freq` / `--rx-lo` / `--preset` are only a fallback.
- Where tuning happens at boot hardly matters. p25-httpd starts from S60 and programs the
  AD9361 and DDCs in well under a second; decoding cannot start before p25-httpd runs anyway.
  Reading the site file instead of arguments costs microseconds. Measure boot to first TSBK
  before optimizing anything.
- Configuration to review for an edge radio:
  - The init script still names Clay (only the fallback now).
  - `--lo-ppm 0` relies on the persisted auto-PPM file.
  - The HTTPS certificate SAN covers only 192.168.x.1, so the Ethernet address gets a name
    warning.
  - `/var/log/p25-httpd.log` stays empty at the default `warn` level.
  - The planner's grant counts are saved every 10 minutes and not at shutdown.
  - A binary copied to `/usr/bin` is lost on reboot (RAM rootfs). That is fine for tests;
    images are the release path.

## Remote libiio control (shared use of the board)

iiod still gives network libiio access, so a remote program (SDR++, GQRX, SDRTrunk) can
retune the AD9361 under p25-httpd. Ideas:

- **Detect it:** poll the AD9361 LO, sample rate and gain every second and compare them with
  what p25-httpd last set. Also watch iiod's client connections (port 30431) to show who is
  connected.
- **React:**
  - If the control channel is still inside the new window at a known preset rate, keep
    decoding: recompute the NCO offsets and mark the radio "shared".
  - If it is outside, or the sample rate is not one of our presets, pause following and show
    "Radio controlled remotely — site unavailable". Offer "Take back control", which
    re-applies the site plan.
- **Policy setting:** yield to remote clients, or keep exclusive control (re-apply the plan,
  or turn iiod off).
- The recentre task and the preset handler must not fight a remote client: while it is
  connected, they stay passive.

## Agent control (MCP) and prompt structure

Goal: the radio is fully controllable by LLM agents, the way these sessions drive it now.

- An MCP server in p25-httpd (Streamable HTTP transport, for example at `/mcp`), so any MCP
  client on the LAN talks to the radio directly, with a token for access.
- **Read tools:**
  - status and site health;
  - sites, profiles and the coverage plan;
  - recent calls with their audio and transcripts;
  - history queries (072);
  - the event log;
  - spectrum.
- **Write tools:**
  - switch site;
  - select or edit a profile;
  - follow or ignore a talkgroup;
  - recentre;
  - tune, preset and gain;
  - find systems (071).
- Notifications: "call on TG n", "site lost", "new system found" (from the events socket).
- **A prompt/skill document for agents** covering:
  - the radio's concepts (site, profile, chains and speakers, the window);
  - safe defaults: confirm retunes, do not switch sites while the operator listens unless
    asked;
  - the read/write split.
- The API is already self-describing (`/api/endpoints`, `doc/P25_API.md`); the MCP tools wrap
  it rather than duplicating logic.

## Transcription (Whisper) and LLM

The Zynq's Cortex-A9 cannot run Whisper usefully, so this lives off the board:

- A LAN service (whisper.cpp or faster-whisper on a PC with a GPU) takes finished recordings,
  pushed by the radio (webhook) or pulled over the API, and returns transcripts to the radio's
  history (072). Transcripts show in Recent calls and are searchable.
- LLM summaries, locations and incident grouping are designed in SDRTrunk
  `doc/design/006d_whisper_transcription.md` and `006e_llm_and_incidents.md`. The per-talkgroup
  prompt profiles and hallucination filtering there carry over.
- Signal classification on the FPGA itself is a separate track: `_shared/FPGA_ML_INFERENCE_GUIDE.md`.

## Clay Electric DMR

From the SDRTrunk playlist (system "Clay Electric Cooperative", alias list `CEC-DMR`, talkgroups
87921–87926: Lake City, Salt Springs, Palatka, Orange Park, Keystone Heights, Gainesville):

| Site | Frequencies (MHz) and logical slots |
|------|-------------------------------------|
| Clay | 454.36875 (LSN 5) with 451.0875 (LSN 6); 454.59375 (LSN 19); 454.38125 (LSN 1) |
| Alachua | 451.2125 (LSN 13), 452.3625 (LSN 14) |
| Marion | 451.1625 (LSN 11), 451.2625 (LSN 12) |

- The Clay site spans 451.0875–454.59375 MHz (3.5 MHz), which one 8M window covers. That was
  the problem with the USB Pluto's narrow rate; the board handles it with the 070 planner.
  Andy sent Radio Reference the missing frequency; check the playlist against RR before
  building the site.
- Needs a DMR receive chain: 4FSK at 4800 baud like C4FM, but two-slot TDMA with its own
  sync patterns and CACH. The existing C4FM demod and DDC are a start; the framing, trunking
  (Capacity Plus / Connect Plus style LSN maps) and the AMBE+2 vocoder are new. SDRTrunk's DMR
  decoder is the reference, as it was for P25.
- The protocol-agnostic call and site model from the refactor makes this far easier; do the
  refactor first.

## Handheld page

A browser view that looks like the future handheld in its 3D-printed case:

- the display (site, talkgroup name, source radio, signal bars, time);
- LEDs: control-channel lock, call, encrypted, recording;
- volume, and a channel/profile knob;
- scan, hold, skip, ignore and menu buttons.

It uses the same API as the other pages, so it doubles as a remote control and as the
prototype for the physical UI. It needs the case dimensions or renders to match the look.
