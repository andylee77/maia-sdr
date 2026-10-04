# Fishball Scanner — Roadmap and Working Notes

This is where ideas get written down before they become numbered changes. Each numbered change
gets its CHANGELOG entry and a `doc/changes/NNN_*.md`.

## Direction

Today the board is a P25 and DMR trunking scanner with an ATSC TV mode. It is meant to become a
full scanner running on the Zynq in two forms:

- a **network edge device**: headless, on Ethernet, with the web UI and API;
- a **portable handheld** in the 3D-printed case, with its own display, LEDs and controls.

Both run the same software. P25 is the first protocol, not the only one, so new work should
keep the protocol-specific parts (framing, vocoder, trunking messages) apart from what every
scanner needs:

- sites and systems;
- channels and the receive window;
- calls, talkgroups and radios;
- aliases, history and recordings;
- the API.

## Proposed order

| # | Item | Status |
|---|------|--------|
| 071 | Find local systems: sweep the band, build sites automatically | done (071a fixes, 071b C4FM, 071 finder) |
| 072 | Per-site activity history: radios, talkgroups, grants, encryption, airtime; graphs | done |
| 072b | Encrypted calls: follow them for their details (no audio) on a free lane | idea |
| 074 | Packet data (SNDCP) on the data channel; status-dibit fix for every frame | done |
| 074b | Packet data: keep it in the history; decode LRRP / ARS / TMS contents | parked (DESIGN D11) |
| — | Phase 2 TDMA voice | later, if a nearby system uses it (Clay grants none) |
| 076 | Restructure into a clean multi-band, multi-protocol scanner | done: the scanner, on both units' images since 2026-10-01 (`scanner/doc/DESIGN.md`) |
| 079 | General radio core: lanes' IQ from the PL, every demodulator in software | steps 1, 3a and 4 done (unit A's image); bake A (082) next; the channelizer (3b) waits for a mode that needs more lanes (`doc/changes/079_general_radio_core.md`) |
| — | Remote libiio control: detect it and share the radio | idea |
| — | Agent control: MCP server and prompt structure | idea |
| 075 | Clay Electric DMR (Tier III): software DMR receive, control channel, then voice | done (on fishball-p25) |
| — | Spectrum survey: identify everything on air | partly done: ATSC (080, 081), the live window's survey; ISM bands in data mode |
| — | Data mode: ISM devices (315, 345, 433.92, 902-928 MHz) found, decoded, captured and streamed | study, Andy's decisions taken (2026-10-04; `scanner/doc/DATA_MODE.md`) |
| — | Gateware for every mode: what the next HDL work should add so a new mode needs no bake | studied in `doc/changes/079_general_radio_core.md`, "Every mode's needs": bakes A-D, A approved (Andy, 2026-10-04) |
| — | Cleanup: retire what the scanner and the radio core replaced | under way (`doc/CLEANUP_INVENTORY.md`) |
| — | Real-time diagnostics: spectrum, waterfall, constellation and eye at 20+ Hz over WebSockets | idea |
| — | Handheld page: the radio's face in the browser | idea |
| — | Transcription (Whisper) and LLM summaries | idea (off-board) |

## 071 — Find local systems

Done: 071's finder, then the scanner's systems scan (076). Systems in the SDRTrunk playlist still
worth confirming:

- Alachua County Public Safety;
- Florida Power and Light;
- SLERS (P25);
- Putnam County Public Safety.

## 072 — Per-site activity history and graphs

Done: the Activity page and `/api/v1/activity/*`, from the history on the SD card (365 days and
2 GB, oldest calls first). Still open: a call's time goes to its primary radio; other speakers in
the same call are counted as taking part, with no time.

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

Cost: the lane is busy while it follows an encrypted call, and a clear call that starts
meanwhile could be missed. That is what the option must not do. The design:

- only a free lane follows an encrypted call, and only while another lane is free too;
- any clear grant pre-empts it at once (the follower already pre-empts, change 066), so a
  clear call loses only the retune (a few ms), not the call;
- the IMBE path stays off (no vocoder CPU, no recording);
- the history then stores measured voice time and every speaker for encrypted calls, marked
  as such.

Measure first: how many clear calls would have started during an encrypted follow. On Clay, clear
grants overlapped as two calls 0.6 % of a day and as three 0.008 % (unit A, 2026-10-04).

## 074b — Packet data, next steps (parked, DESIGN D11)

074 decodes Clay's packet data (downlink only: radios transmit on the uplink). Next:

- Keep data records in the activity history (per radio and site), so Activity can show data
  per radio, hour and day, like calls.
- Decode the contents: LRRP (location requests and the reports the server forwards), ARS
  (registrations), TMS (text). SDRTrunk has decoders for each (`module/decode/ip/mototrbo`).
- Follow data channel grants (SNDCP data channel grant) to other data channels when Clay uses
  more than one.

## 079 — General radio core

One bitstream for many radio functions. The PL gives each lane's IQ to the PS, where LSM, C4FM,
DMR and 8-VSB run in software. Steps 1, 3a and 4 are done: the radio core 1.0.0 has three DDC
lanes in one tagged lane ring, the spectrometer and the capture ring, and runs on unit A's image.
Design and steps: `doc/changes/079_general_radio_core.md`. It superseded the chain-2 IQ tap
(077).

## Gateware for every mode

Andy, 2026-10-04: the gateware should serve every mode (scanner, ATSC TV, data), so that a new
mode is a software change. 079's "Every mode's needs" studies the eleven items (what changes in
the HDL and the PS, the cost, the timing risk, the evidence still needed) and groups them into
bakes:

- **A:** the timing fix, a register map that does not move with the lane count, and items 3-7;
  approved, next.
- **B:** more lanes, when a mode needs them.
- **C:** data mode in the PL.
- **D:** 8-VSB in the PL.

The DDC lanes stay beside a later channelizer.

## Code review and refactor

Done: the 2026-09-28 review led to the fresh `scanner/` crate (076).

## Boot and board configuration

- Boot tunes to the live site's control channel and planned window from the scanner's
  configuration (`/mnt/jffs2/scanner/`); `S60scanner` passes only the listen address and TLS.
- The scanner writes its learned state (crystal, band plans, grant counts) on a site switch and
  at shutdown.
- To check for an edge radio: the HTTPS certificate (`S50p25-httpd-certificates`) covered only
  192.168.x.1 when last looked at, so the Ethernet address would get a name warning.
- A binary copied to `/usr/bin` is lost on reboot (RAM rootfs). That is fine for tests; images
  are the release path.

## Remote libiio control (shared use of the board)

iiod still gives network libiio access, so a remote program (SDR++, GQRX, SDRTrunk) can
retune the AD9361 under the scanner. Ideas:

- **Detect it:** poll the AD9361 LO, sample rate and gain every second and compare them with
  what the scanner last set. Also watch iiod's client connections (port 30431) to show who is
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

- An MCP server in the scanner (Streamable HTTP transport, for example at `/mcp`), so any MCP
  client on the LAN talks to the radio directly, with a token for access.
- **Read tools:**
  - status and site health;
  - systems, sites, aliases and the coverage plan;
  - recent calls with their audio and transcripts;
  - history queries (072);
  - the event log;
  - spectrum.
- **Write tools:**
  - switch site;
  - hold a talkgroup, or edit its alias;
  - follow or ignore a talkgroup;
  - recentre;
  - tune, preset and gain;
  - find systems (071).
- Notifications: "call on TG n", "site lost", "new system found" (from the events socket).
- **A prompt/skill document for agents** covering:
  - the radio's concepts (system, site, aliases, lanes and speakers, the window, the mode);
  - safe defaults: confirm retunes, do not switch sites while the operator listens unless
    asked;
  - the read/write split.
- The API is already self-describing (`GET /api/v1/routes`, `scanner/doc/API.md`); the MCP tools
  wrap it rather than duplicating logic.

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
- **ATSC TV on board** (changes 080 and 081, ATSC mode): the TV channel finder reads RF 4-36 and
  says which channels carry 8-VSB (by its pilot), a signal without it (ATSC 3.0) or nothing, with
  each channel's carrier to noise and spectrum. Each 8-VSB channel strong enough is then decoded
  from a 0.5 s capture to its station's PSIP: TSID and virtual channels (numbers and names).
- **Captures:** the radio core's capture ring holds 0.26 s of raw IQ at 16 MSPS. The scanner
  reads it for ATSC naming; an API route for raw IQ is data mode's (`scanner/doc/DATA_MODE.md`
  §6). Captures would be decoded on the PC:

  | Signal | Decoded or recognised by |
  |--------|--------------------------|
  | ADS-B | dump1090 |
  | Weather stations and TPMS | rtl_433 |
  | Pagers | multimon-ng |
  | DMR Tier II, Capacity Plus, NXDN | SDRTrunk |

Plan:

1. A tool drives the API (tune + `/api/v1/spectrum`) for a 24-hour occupancy map: frequency ×
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

- **Wideband spectrum:** the radio core's spectrometer (a 4096-bin FFT of the full AD9361 rate,
  averaged in hardware, read by DMA). `/ws/live` pushes each frame to a page that subscribes.
- **IQ:** no stream yet. Every demodulator runs on the PS, so any point in a lane's receiver can
  be tapped.

Plan:

- **`/ws/spectrum`:** push every completed spectrometer integration as a binary frame. One
  byte per bin in dB is 4 KB per frame, so 25 Hz is ~100 KB/s.
  - Set `spec_num_integrations` for the rate: each FFT is 4096 / sample rate (0.34 ms at
    12 MSPS), so about 117 averaged FFTs give 25 Hz.
  - Draw spectrum and waterfall on a canvas, with the window, channels and live calls marked
    (from the 070 plan).
- **Constellation and eye at 20+ Hz:** push a lane's symbols from its software receiver as
  binary frames, per lane.
  - Draw with persistence ("phosphor") so the eye opens the way reference analyzers show it.
- **Cost:** stream only while a page subscribes; binary frames and drawing in the browser keep
  the PS load low. The scanner uses 15-17 % of one A9 core (079). Measure it with the streams on.

## C4FM voice and vendor grants

- C4FM on traffic channels: done in 079 step 4 (every lane demodulates the site's modulation in
  software).
- Harris (MFID 0xA4) and Motorola (0x90) vendor TSBKs: SLERS and FPL send many Harris ones.
  Decode the voice-grant and patch ones so those systems can be followed.
- The software C4FM demodulator uses 4.3 % of one A9 core since 079's NEON FIRs.

## Handheld page

A browser view that looks like the future handheld in its 3D-printed case:

- the display (site, talkgroup name, source radio, signal bars, time);
- LEDs: control-channel lock, call, encrypted, recording;
- volume, and a channel/profile knob;
- scan, hold, skip, ignore and menu buttons.

It uses the same API as the other pages, so it doubles as a remote control and as the
prototype for the physical UI. It needs the case dimensions or renders to match the look. The
board and case it would mirror: `doc/PANEL_ADDON_BOARD.md`.
