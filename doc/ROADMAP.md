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
| 071 | Find local systems: sweep the band, build sites automatically | done (071a fixes, 071b C4FM, 071 finder) |
| 072 | Per-site activity history: radios, talkgroups, grants, encryption, airtime; graphs | done |
| 072b | Encrypted calls: follow them for their details (no audio) on a free chain | idea |
| 074 | Packet data (SNDCP) on the data channel; status-dibit fix for every frame | done |
| 074b | Packet data: keep it in the history; decode LRRP / ARS / TMS contents | next |
| — | Phase 2 TDMA voice | later, if a nearby system uses it (Clay grants none) |
| 076 | Restructure into a clean multi-band, multi-protocol scanner | design approved 2026-10-01; built as a fresh crate (`scanner/doc/DESIGN.md`) |
| 079 | General radio core: a polyphase channelizer in the PL, every demodulator in software | design approved 2026-10-03 (`doc/changes/079_general_radio_core.md`); step 1 (software LSM) next |
| — | Remote libiio control: detect it and share the radio | idea |
| — | Agent control: MCP server and prompt structure | idea |
| 075 | Clay Electric DMR (Tier III): software DMR receive, control channel, then voice | done (on fishball-p25) |
| — | Spectrum survey: identify everything on air | idea (Andy, 2026-10-01; unit A) |
| — | Real-time diagnostics: spectrum, waterfall, constellation and eye at 20+ Hz over WebSockets | idea |
| — | Handheld page: the radio's face in the browser | idea |
| — | Transcription (Whisper) and LLM summaries | idea (off-board) |

The review runs before 071 (started 2026-09-28), so new modules land in the right place. The
refactor itself should come after 072, once the site and history models exist.

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

Done (2026-09-28): the Activity page and `/api/activity/*` (not `/api/history/*`, which
already serves the event log and recordings). The limits are 365 days and 2 GB, oldest calls
first. Still open from the list below:

- the grant map and the planner counts still keep their own tallies;
- a call's time goes to its primary radio. Other speakers in the same call are counted as
  taking part, with no time.

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

## 072b — Encrypted calls: details without audio

Today an encrypted call is never followed, so all that is known of it comes from the control
channel:

- the radio granted;
- the grant time: from the grant to its last update.

The grant time includes hang time and any other radio that keyed up on the same grant. On
Clay's followed clear calls, decoded voice is about 0.7 of grant time (the Activity page shows
this ratio), so grant time overstates airtime by about 40 %.

Idea (from the operator): an option to follow encrypted calls for their details only, with no
audio and no recording. The voice channel gives:

- every radio that keys up (LDU1 link control), not only the one granted;
- each transmission's real length (LDUs);
- the algorithm and key ID (ESS in the HDU and LDU2), per call;
- the end (TDU / TDULC) rather than the grant-update timeout.

Cost: the chain is busy while it follows an encrypted call, and a clear call that starts
meanwhile could be missed. That is what the option must not do. The design:

- only a free chain follows an encrypted call; with two chains, only when the other is free
  too (or only chain 2);
- any clear grant pre-empts it at once (the follower already pre-empts, change 066), so a
  clear call loses only the retune (a few ms), not the call;
- the IMBE path stays off (no vocoder CPU, no recording);
- the history then stores measured voice time and every speaker for encrypted calls, marked
  as such.

Measure first: how often both chains are busy on Clay at peak, and how many clear calls would
have started during an encrypted follow.

## 074b — Packet data, next steps

074 decodes Clay's packet data (downlink only: radios transmit on the uplink). Next:

- Keep data records in the activity history (per radio and site), so Activity can show data
  per radio, hour and day, like calls.
- Decode the contents: LRRP (location requests and the reports the server forwards), ARS
  (registrations), TMS (text). SDRTrunk has decoders for each (`module/decode/ip/mototrbo`).
- Follow data channel grants (SNDCP data channel grant) to other data channels when Clay uses
  more than one (074 parks the idle chain on the announced channel only).

## 079 — General radio core

One bitstream for many radio functions, with lanes that are all alike. The PL channelizes the
receive window the way SDRTrunk does (25 kHz bins oversampled 2x, two bins joined per lane) and
sends each lane's 50 kSPS IQ to the PS, where LSM, C4FM and DMR all run in software. The gateware
LSM chains and DDCs go; lanes become a build parameter (8-16) instead of copies of a chain.
Design and steps: `doc/changes/079_general_radio_core.md`. Supersedes the chain-2 IQ tap (077).

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

1. **Analysis** (read-only): done 2026-09-28, `doc/CODE_REVIEW_2026_09_28.md`. It covers the module
   map, the findings by severity, a target layout and a staged plan.
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

Change 075, done and merged into fishball-p25: design, measurements and status in
`doc/changes/075_dmr_clay_electric.md`.

- The system is **DMR Tier III Standard** (Radio Reference sid 11943), not Capacity Plus. It
  has a real control channel (TSCC) whose grants carry a logical channel number (LCN). The
  SDRTrunk playlist's `lsn` attribute holds that LCN.
- Green Cove Springs (Clay) site: control channel 454.36875 MHz (LCN 5, TS1), voice on
  451.0875 MHz (LCN 6) and on the control repeater's TS2. Talkgroups 87921–87926 (Lake City,
  Salt Springs, Palatka, Orange Park, Keystone Heights, Gainesville), clear voice.
- Both GCS channels are received at about +50 dB at the shop (unit A, 2026-09-30). The other
  sites in the playlist are at the noise floor there.
- Software receive on the PS (SDRTrunk's DMR decoder ported to `protocol::dmr`); no gateware
  change. UHF and Clay County P25 (851–861 MHz) cannot share one AD9361 window, so a DMR site
  is chosen like any other site.

## Spectrum survey: identify everything on air

Andy, 2026-10-01: sweep everything the radio can tune and identify what is there, as a study.
Targets:

- other DMR systems and other digital voice systems;
- ATSC TV, ADS-B and NOAA weather radio;
- ISM sensors: weather stations and TPMS;
- strong nearby signal activity.

What exists:

- **Sweeping:** the HDL spectrometer sweeps 16 MHz windows: 70 MHz–6 GHz on unit A (AD9361), and
  325 MHz–3.8 GHz specified on unit B (AD9363).
- **Decoders on board:** P25 and DMR Tier III.
- **Captures:** wideband IQ goes to the SD card at up to 16 MSPS, for decoding on the PC:

  | Signal | Decoded or recognised by |
  |--------|--------------------------|
  | ADS-B | dump1090 |
  | Weather stations and TPMS | rtl_433 |
  | Pagers | multimon-ng |
  | DMR Tier II, Capacity Plus, NXDN | SDRTrunk |
  | ATSC | its pilot tone |

Plan:

1. A tool drives the API (tune + `spectrum_wide`) for a 24-hour occupancy map: frequency ×
   time, steady vs bursty, bandwidth. No firmware change.
2. Targeted captures of the interesting carriers, decoded on the PC, give an inventory of what is
   on air.
3. The classifiers worth having move on board as probes in the scan. 076's discovery has one
   probe per protocol for this.

Limits:

- Unit A's antenna is not flat across the range.
- One 16 MHz window at a time, so bursty signals (TPMS, ISM sensors) need long dwells.
- A survey takes the radio away from following calls. Run it on unit A when it is free.

## Real-time diagnostics plots (20+ Hz)

Goal: the Diagnostics view's spectrum, waterfall, constellation and eye plots update live over
WebSockets at 20 Hz or more, instead of today's slow polling.

Today:

- **Wideband spectrum:** Maia's HDL spectrometer is still in the bitstream (a 4096-bin FFT of
  the full AD9361 rate, averaged in hardware, read by DMA). `/api/spectrum_wide` reads one
  integration, and the page polls it once a second. What did not carry over from Maia is its
  real-time waterfall streaming (maia-httpd's WebSocket).
- **IQ:** `/ws/iq` streams the `pre_diff` tap (after rotate and AGC, before the slicer) by
  polling every 80 ms (~12 Hz). Constellation and eye are drawn from it
  (`doc/DASHBOARD_PLOTS.md`).
- **Narrowband spectrum:** `/api/spectrum` is computed on the PS.

Plan:

- **`/ws/spectrum`:** push every completed spectrometer integration as a binary frame. One
  byte per bin in dB is 4 KB per frame, so 25 Hz is ~100 KB/s.
  - Set `spec_num_integrations` for the rate: each FFT is 4096 / sample rate (0.34 ms at
    12 MSPS), so about 117 averaged FFTs give 25 Hz.
  - Draw spectrum and waterfall on a canvas, with the window, channels and live calls marked
    (from the 070 plan).
- **Constellation and eye at 20+ Hz:** push IQ when the DMA buffer is ready (or poll at 40 ms)
  as binary frames, per chain (control, traffic 1, traffic 2).
  - Draw with persistence ("phosphor") so the eye opens the way reference analyzers show it.
  - `doc/DASHBOARD_PLOTS.md` §7 lists better tap points (post-PLL).
- **Cost:** stream only while a page subscribes; binary frames and drawing in the browser keep
  the PS load low. The A9 ran at ~8 % / 19 % with both traffic chains (change 066). Measure it
  with the streams on.

## C4FM voice and vendor grants (after 071b)

- C4FM on traffic channels: run the software C4FM demodulator on chain 1's IQ (already in the
  IQ hub) when the site is C4FM, feeding the traffic decoder with the same air-time stamping as
  the HDL dibits. Chain 2 needs an IQ tap in the gateware (packer + DMA + DT node), or an HDL
  C4FM mode.
- Harris (MFID 0xA4) and Motorola (0x90) vendor TSBKs: SLERS and FPL send many Harris ones.
  Decode the voice-grant and patch ones so those systems can be followed.
- The software C4FM demodulator uses 12–17 % of one A9 core. NEON or fixed-point FIRs would cut
  that before a second instance runs for traffic.

## Handheld page

A browser view that looks like the future handheld in its 3D-printed case:

- the display (site, talkgroup name, source radio, signal bars, time);
- LEDs: control-channel lock, call, encrypted, recording;
- volume, and a channel/profile knob;
- scan, hold, skip, ignore and menu buttons.

It uses the same API as the other pages, so it doubles as a remote control and as the
prototype for the physical UI. It needs the case dimensions or renders to match the look.
