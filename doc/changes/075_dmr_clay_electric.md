# 075 — DMR Tier III receiver for Clay Electric Cooperative

**Started:** 2026-09-30. **Branch:** fishball-dmr (worktree `MAIA_SDR/maia-sdr-dmr`, off
fishball-p25 at 074b). **Bake required:** no. The DDCs, the IQ hub and the 50 kSPS chains
already deliver what a software DMR receiver needs.

Goal: the board follows Clay Electric Cooperative's DMR system the way it follows Clay County
P25: the control channel, who talks on which talkgroup, the per-site history, and then audio.

## What the system is (corrected 2026-09-30)

ROADMAP and the earlier notes called it Capacity Plus. It is **DMR Tier III Standard**
(Radio Reference sid 11943, "DMR Tier 3 Standard", system ID 1). Andy's SDRTrunk logs from
the shop (2026-02-26 to 2026-03-21, `C:\Users\Andy\SDRTrunk\event_logs\*_45*_Hz_*`) confirm
it:

- **Green Cove Springs (Clay) site.** Control channel (TSCC) 454.36875 MHz, LCN 5, timeslot 1.
  Colour code 0, small network 0, site 2.
  - Messages seen: ALOHA (every other burst), SLC "Tier III control channel … registration
    required", C_BCAST vote-now and site announcements, P_PROTECT, P_CLEAR, call timers,
    registration accepts and grants.
- **Voice channel.** Grants go to **LCN 6 = 451.0875 MHz** (SDRTrunk CHANID 13 = TS1, 14 =
  TS2). The control repeater's TS2 also carries voice.
  - 454.5375 MHz took calls on 2026-03-16 morning. It is probably the old LCN 6 mapping:
    the forum lists LCN changes over time.
- **Neighbours** (C_BCAST vote-now): site 0 on LCN 1, site 1 on LCN 3, site 3 on LCN 7, site 7
  on LCN 15. Their frequencies are not in the playlist.
- **Talkgroups 87921–87926** (Lake City, Salt Springs, Palatka, Orange Park, Keystone Heights,
  Gainesville), clear voice. On 2026-03-16 up to ~160 calls an hour (19:00–21:00): mostly
  87924 Orange Park, 87922 Salt Springs and 87925 Keystone Heights.
- **SDRTrunk config.** The playlist's `<timeslot lsn=…>` holds the LCN, not an LSN
  (`DecodeConfigDMR`/`TimeslotFrequency`). LCN 5 → 454.36875 and LCN 6 → 451.0875 are right.
  The other Clay entries (LSN 19 → 454.59375, LSN 1 → 454.38125) are other sites' control
  channels, per Radio Reference.
- **SDRTrunk decode quality** with the Pluto at the shop: 101,217 messages passed, 3,321
  failed (96.8 %).

Radio Reference sites (frequencies, control channel first):

| Site | Frequencies (MHz) |
|------|-------------------|
| Green Cove Springs (Clay) | 454.36875 (CC), 451.0875 |
| Lake City (Columbia) | 451.050 (CC), 451.1375 (CC), 452.0125 |
| Brooker (Bradford) | 452.225 (CC), 452.400 |
| Reddick (Marion) | 451.1625 (CC), 451.2625 |
| Gainesville (Alachua) | 451.2125 (CC), 452.3625 |
| Hollister (Putnam) | 451.0625 (CC), 452.2375 |
| "Unknown" (Clay) | 454.59375 (CC) |
| "Highland ?" (Clay) | 454.38125 (CC) |

## Measured on unit A, 2026-09-30 evening

Unit A was retuned with `POST /api/tune {"radio_freq_hz":454368750,"center_mode":"lock",
"center_hz":452728125}`, 12M preset. Captures are in `maia-sdr/runs/dmr/` (gitignored).

- **454.36875 is strong.** About +50 dB over the wideband floor. The 50 kSPS control-chain
  IQ (`/api/control_iq_dump`) shows clean 4-level FSK at about ±1900 / ±600 Hz around a
  −530 Hz offset.
  - **2000 BS-data syncs in 60 s**, one per 30 ms burst: a continuous TSCC, data on both
    slots.
- **The voice repeater is as strong when it keys up.** A 10-minute watch of the HDL wideband
  spectrum (`runs/dmr/watch_uhf.py`, 1 poll/s, 20:49–20:59 EDT) saw:
  - 451.0875 at +1–4 dB between calls, then **+48–50 dB from 20:58:05 to 20:59:06**: a
    conversation;
  - its start on the control repeater's TS2 (the 20:57:14 control capture holds 6 BS-voice
    syncs, the next one 1);
  - ten 60 s control captures (`cc_454368750_*.wav`, 50 kSPS stereo i16), all 1966–2000
    BS-data syncs.
- **Nothing else from Clay Electric is above the floor.**
  - 454.59375 (+1–5 dB) is weak.
  - 454.5375 and the other sites' channels are at the floor.
  - 454.38125 (+18–28 dB) sits 12.5 kHz from the CC, probably the CC's skirt.
  - 454.0194 MHz (+20–27 dB) is an unrelated steady carrier.
- **Board findings:**
  - The wideband IQ tap (`/api/wideband_iq_capture`) runs at the preset's AD9361 rate (12 MSPS
    here). `WIDEBAND_IQ_RATE_HZ` says 4 MSPS and the API reply says 8 MSPS. A "10 s" capture
    held 3.33 s.
  - The crystal-trim NCO shift is a fixed Hz value calibrated at 856 MHz (598 Hz).
    `/api/tune` does not scale it, so the UHF chain sits ~0.5 kHz off. DMR still decodes
    (the equaliser absorbs it), but it should scale with frequency.

## Design

Software receive on the PS, like 071b's C4FM. No gateware changes.

```text
IQ hub (50 kSPS, control or traffic DDC)
  └─ dsp front end (shared with protocol::p25::c4fm): half-band → 25 kSPS, LPF, RRC,
     differential demod
      └─ protocol::dmr::demod  — SDRTrunk DMRSoftSymbolProcessor: sync-driven timing,
         primary + lagging soft sync on the BS/MS/direct patterns, equaliser
          └─ protocol::dmr::framer — 288-bit bursts, CACH (TACT, short LC fragments), two
             timeslot streams, slot type (Golay 20,8), voice superframe A–F tracking
              ├─ data: BPTC(196,96) → CSBK / MBC / data header / full LC (CRC-CCITT masks,
              │        RS(12,9)); short LC (BPTC 68,36, CRC-8)
              └─ voice: embedded LC (BPTC 128,77, checksum 5); 3 × 72-bit AMBE+2 frames
                 per burst
  └─ protocol::dmr::tier3 — Tier III messages: ALOHA, C_GRANT family (TV/BTV/PV/PD grants),
     P_CLEAR, P_PROTECT, C_BCAST (announcements, vote-now, neighbours), MBC absolute channel
     parameters, ACKs, registration
```

- **Front end.** Move `Fir` and `DifferentialDemod` out of `protocol::p25::c4fm` into a shared
  `dsp::fsk4` module. The P25 demodulator keeps its sync and NID logic. The DMR symbol
  processor is a sibling, not a copy.
- **Reference.** SDRTrunk `module/decode/dmr` (253 files, 39k lines; most of it Hytera,
  Capacity Plus/Max and data formats we don't need). Port the scalar paths only, names
  following the Java, as with P25.
  - SDRTrunk bug to not copy: `LCMessageFactory.java:134` treats `residual != 0` as valid.
- **Channel map.** Tier III grants carry an LCN (12-bit channel number) and a timeslot. The
  site file holds LCN → frequency, like SDRTrunk's timeslot map.
  - An absolute-frequency grant (MBC "absolute parameters") also works without the map.
  - Later: learn a missing LCN by watching which carrier keys up after its grant.
- **Band.** UHF 451–455 MHz and Clay P25 (851–861 MHz) cannot share one AD9361 window. A DMR
  site is a site like any other, chosen with `/api/site`. The board watches one or the other,
  not both. A scan or priority mode between sites is a separate idea.
- **CPU.** The software C4FM control demod costs 11.6 % of one core on unit A. A DMR control
  channel plus one voice repeater (both timeslots in one stream) should be ~25 %.

## Phases

| Phase | What | Checked by |
|---|---|---|
| 0 | Measure at the shop: CC level, sync rate, which channels key up | done (above); a capture of 451.0875 during a call is still to do |
| 1 | `protocol::dmr` decoder library, host only: demod, framer, CACH, slot type, BPTC, CRCs, CSBK/LC/SLC parsing, Tier III message types | the captured IQ: syncs found, CRC pass rate, message mix like SDRTrunk's March logs; unit tests from SDRTrunk vectors |
| 2 | Live control channel on the board: a DMR site type (protocol, colour code, LCN map), the decoder on the control IQ hub, events and grants in the API, the Systems/Activity pages | unit A on 454.36875: live CSBK rate and CRC %, talkgroups and radios in the history |
| 3 | Follow voice: on a grant, tune a traffic chain's NCO to the LCN's frequency; run the DMR demod on the traffic IQ; both timeslots, call lifecycle, late entry from the embedded LC | calls open and close in step with the CC; AMBE frames counted; raw frames recorded |
| 4 | AMBE+2 vocoder: port jmbe's AMBE codec (GPL-3.0 source, like the IMBE port) | known-good frames decoded by SDRTrunk's jmbe 1.0.9 on the PC as reference; Andy listens |
| 5 | Discovery: UHF bands in the system finder; recognise DMR carriers (BS sync) instead of "other" | sweep 450–455 MHz lists GCS as a DMR site |

## Decisions for Andy

1. **Refactor first or not.** ROADMAP says to do the multi-protocol refactor before DMR.
   Phase 1 is a pure library and does not depend on it. Phases 2–3 need sites, calls and
   history to accept a protocol other than P25.
   - Proposal: do phase 1 now. Then make the smallest protocol-agnostic changes phase 2 needs
     (a `protocol` field on sites; the history keyed as it is, by site/talkgroup/radio),
     instead of the full refactor first.
2. **AMBE+2 source.** The jmbe source on GitHub (DSheirer/jmbe, GPL-3.0) has the AMBE codec;
   the IMBE port came from the same place.
   - Note: AMBE+2 is a DVSI codec covered by patents, as IMBE is. The same considerations
     that applied to the IMBE port apply here.
3. **The radio is either/or.** While unit A follows Clay Electric it hears no Clay County P25.
   Unit B could take one of the two once it is back.

## Status log

- 2026-09-30: phase 0 measurements; plan written; worktree and branch created.
- 2026-09-30: **SDRTrunk reference harness**, `tools/sdrtrunk_dmr_reference.py` with
  `tools/sdrtrunk_dmr_harness/DmrWavHarness.java`. It runs SDRTrunk's own `DMRDecoder` over
  our 50 kSPS WAVs, with the classpath from Gradle `--offline` via an init script. Nothing in
  the SDRTrunk repo changes.
  - On the ten control captures it gives 24,996 messages, 8 failing CRC (99.97 %).
  - It decodes a whole call at 20:57 (TG 87925, radios 82321 and 81921): grants to LCN 6 TS2,
    LCN 5 TS2 and LCN 6 TS1, voice with full LC, PROTECT and CLEAR.
  - Our port's messages will use SDRTrunk's text so the two logs can be diffed.
- 2026-09-30: **phase 1, demod and framer** (`protocol::dmr::{filters, sync, demod, framer}`).
  - Per idle control minute: one coarse acquisition, then 1998–1999 fine syncs with no losses.
  - Equaliser balance 0.49 rad ≈ −375 Hz carrier offset (before 074c's LO-shift scaling).
  - CACH valid on 1998 of 1999 bursts (the first burst, before the timeslot is known, is
    the exception); TS1/TS2 999/999.
  - The 20:57 voice superframes are followed through bursts B–F with no sync losses.
  - Synthetic 4FSK tests: no dibit errors in 190 bursts, −700 Hz offset handled, no false
    syncs on noise.
