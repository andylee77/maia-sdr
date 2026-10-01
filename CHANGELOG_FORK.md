# Maia SDR -- Changelog (andylee77 fork)

Tracking log for the `andylee77/maia-sdr` fork.
Upstream: [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr) (originally [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr))

---

## [2026-09-30] Talkgroups are 32-bit in the call pipeline (075a)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-30-lo-scale-074c` (unchanged)
**Bake required:** NO.

Prerequisite for DMR Tier III, whose talkgroups are 24-bit (Clay Electric uses 87921-87926).
No behaviour change for P25.

- **Widened to u32:** call boundaries and audio chunks, the call lifecycle and its snapshot,
  grant stats, the history (SQLite rows, queries), recordings (entries, file-name parser),
  the forwarder's talkgroup atomics and encrypted set, lane policy, routing, monitor list,
  UI settings (names, groups, monitor and ignore lists, profiles), the `p25-json` types and
  the HTTP API (activity, talkgroups, encrypted list, audio WS meta).
- **Stays u16:** the P25 protocol layer (TSBK fields, `Talkgroup(u16)`, `GrantEvent`, the
  P25 grant map, `UnitObservation`). Values enter the shared pipeline with `u32::from`.
- **End marker:** the lifecycle's pending end-of-transmission marker was `(tg << 48) | ms`
  in an `AtomicU64`; a 24-bit TG and a unix ms no longer fit, so it is a small mutex
  (`ImbeForwarder::end_marker`).
- **Limits:** talkgroups are 1..=16777215 (`ui_settings::MAX_TG`); TG 0 is still rejected.
  The web UI's 65535 caps (monitor picker, talkgroup names, group / ignore lists) are now
  16777215.
- **Compatibility:** the saved settings JSON, the SD recording names and the history
  database are unchanged in format and load as before (tests below).

Tests: p25-httpd 335 (a 16-bit settings file loads unchanged; 24-bit settings validate;
recording names with TG > 65535; history rows old and 24-bit; end marker keeps a 24-bit TG).

---

## [2026-09-30] Crystal correction scales with the LO; wideband capture length (074c)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-30-lo-scale-074c`
**Bake required:** NO (p25-httpd).

Two findings from the DMR session's UHF captures on unit A:

- **LO shift.** `/api/tune` and site presets applied `current_lo_shift_hz` (598 Hz, measured
  at 856 MHz) unchanged at any frequency, so at 454 MHz the chain sat ~0.3 kHz off. The
  crystal error is a ratio (ppm): the shift now scales with the LO on every LO change
  (`tuning::scale_lo_shift`; 598 Hz at 856 MHz is 317 Hz at 454 MHz) and the scaled value is
  stored.
- **Wideband IQ capture.** The tap runs at the active AD9361 rate (12 MSPS on the 12M
  window), but the capture sized its file for 4 MSPS and the reply said 8 MSPS, so a
  "10 s" capture held 3.33 s. It now reads the current sample rate; the reply gives
  `rate_hz`. Checked on unit A: 1 s = 48,000,000 bytes at 12 MSPS.

Tests: p25-httpd 330 (the shift scales with the LO).

---

## [2026-09-30] Recent calls and the history survive a restart (074b)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-30-restart-safe-074b`
**Bake required:** NO (p25-httpd).

- **Symptom** (Andy, unit A): call details showed their stats, then lost them; Recent calls
  showed "0 calls" next to the recordings; the SD card had more calls than the history.
- **Causes.**
  - The call summaries behind Recent calls lived only in memory: every restart dropped them,
    leaving the recordings with no stats.
  - The history stores a call 15 s after it ends, every 30 s. A restart (SIGTERM, which
    p25-httpd did not handle) lost the calls of the last ~45 s, while their recordings were
    already on the card. Many deploys today, plus the 073a boot race, made the gap visible.
- **Fix.**
  - At start, the rings are refilled from the history (the newest 200 followed and 50
    encrypted / not-followed calls): stats, on-air and held-open times, close reason, chain,
    channel and radios come back (`grant_stats::backfill`).
  - On SIGTERM the history stores every finished call at once, then p25-httpd exits (at most
    3 s). Checked on unit A: a call that ended 8 s before a restart was in the history after
    it.
  - Call ids continue after the highest id in the history too, not only the SD recordings
    (encrypted calls are not recorded; a reused id would pair a recording with another call).
- Calls lost before this change stay missing from the history; their recordings remain.

Tests: p25-httpd 329 (stored calls refill the rings).

---

## [2026-09-30] Recordings keep their frequency and channel across a reboot (074a)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-30-rec-channel-074a`
**Bake required:** NO (p25-httpd).

- After a reboot, the recordings listed from the SD card had no frequency or channel: their
  file names carry time, id, talkgroup, radio and site only.
- The activity history now keeps each call's channel id (a `channel` column, added to existing
  databases at start). At boot each SD recording takes its frequency, channel and radios from
  its call in the history (`HistoryStore::recording_info`, which replaces `site_of_call`).
- Calls stored before this change have a frequency but no channel id.
- Packet data (074), asked by Andy: Clay does not send positions back out. All location
  packets seen were "triggered location start" requests to radios; the reports go radio to
  server on the uplink.

Tests: p25-httpd 328 (recording details by call id; an old database gains the column).

---

## [2026-09-30] Packet data on the data channel; status dibits one dibit late everywhere (074)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-30-packet-data-074`
**Bake required:** NO (p25-httpd and web UI).

- **Status dibits were placed one dibit early in every frame.** The body status dibits are
  every 36th dibit of the frame (frame dibits 35, 71, ...; body raw 14 + 36k); the decoder used
  13 + 36k since the first TSBK work (April). Each status crossing put one wrong dibit into the
  data, and the FEC hid it. Fixed in `types::is_body_status_dibit` (voice, TDU, PDU) and the
  TSDU deinterleaver. Unit A, Clay, same hour:

  | | before (13 + 36k) | after (14 + 36k) |
  |---|---|---|
  | TSBK CRC good (5 min each) | 97.07–97.74 % | 99.72 % (failures down tenfold) |
  | Vocoder errors per IMBE frame | 3.41 % (112 calls, 13,149 frames) | 0.96 % (12 calls, 2,718 frames) |
  | Packet data 3/4-rate blocks with 0 bit errors | 0 of 95 | 53 of 56, then 28 of 28 |
  | Packets passing CRC-32 | 5 of 28 | 15 of 17, then 8 of 8 |

  TDULC had already been found to need +14 (against SDRTrunk `.bits` files); it uses the shared
  helper again. Andy: "voice is coming through perfect".
- **Packet data (SNDCP), new.** Clay announces a data channel (SNDCP data channel
  announcement, about 40 a minute) and grants data on it.
  - A data-only decoder on each traffic chain reads what the call gate holds back (the chain
    between calls).
  - The idle last chain parks on the announced data channel (any voice grant still takes it).
  - `protocol::p25::pdu`: header (CRC-16 with one-bit correction), unconfirmed (1/2-rate) and
    confirmed (3/4-rate, CRC-9) blocks, the packet CRC-32, response packets, AMBTC / UMBTC by
    opcode, SNDCP data header, IPv4 / UDP and the Motorola service by port (LRRP, ARS, TMS,
    XCMP, ...). Ported from SDRTrunk plus the CRC-9 and CRC-32 checks it skips.
  - `/api/data`, a Packet data card on the Activity page, event-log category `data`. The same
    PDU read by both chains counts once.
- **Live on Clay** (about 10 minutes): about 140 PDUs, all network to radio:
  - location polls (LRRP, UDP 4001) from a server at 10.51.1.116 to radios at 10.71.x.x;
  - delivery receipts acknowledging radios' own uploads (which go out on the uplink we do not
    receive), retry requests and SNDCP context messages.
- **Not yet** (roadmap 074b): data is kept in memory only; LRRP / ARS / TMS contents are not
  decoded; other data channels than the announced one are not followed; Phase 2 TDMA (Clay
  grants none).

Tests: p25-httpd 327 (PDU end to end through the decoder, CRC reference values, 3/4-rate
trellis round trip with errors, packet assembly, data records and duplicates; status layout
tests on the frame schedule).

---

## [2026-09-30] Fix: after a reboot, calls, recordings and Activity were empty (073a)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-30-sd-boot-073a`
**Bake required:** YES for the boot-order part (tezuka_fw 93a0d75 and the fsck commit after
it); the p25-httpd part works on its own.

- **Symptom** (unit A after a reboot): Recent calls and Activity were empty; the card still
  held the history database and 2,280 recordings.
- **Cause.** `S91nfs-mount` mounts the SD card after `S60p25-httpd` has started. p25-httpd saw
  no card:
  - it opened the history in RAM (`/tmp`);
  - it listed no SD recordings;
  - it restarted call ids at 1, so new files repeated old ids.
- **Fix.**
  - `S60p25-httpd` mounts the card first (after `fsck.fat -a`); `S91nfs-mount` skips it when
    mounted.
  - p25-httpd waits up to 20 s for the card when one is present.
  - The boot index of the card allows 15 s instead of 3 s, and lists recordings oldest first by
    start time: after such a boot, ids are not in time order, and retention removes from the
    front.
- **Also found:** unit A's FAT was damaged by unclean power-offs ("deleting FAT entry beyond
  EOF"); the kernel set the card read-only. The image now runs `fsck.fat -a` before mounting,
  and `S60p25-httpd stop` unmounts the card.

Tests: p25-httpd 319 (index order by start time with a restarted id).

---

## [2026-09-28] Calls, recordings and history kept per site (073)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-28-site-scoped-073`
**Bake required:** NO (p25-httpd and web UI).

- **Symptom** (unit A, switching between Clay, Duval and FPL): the Activity page showed Clay's
  calls under Duval and FPL, and Jacksonville's under FPL.
- **Causes** (the history only):
  - Calls were filed under the site active when they were written, up to 45 s after they
    ended, not the site they were on.
  - The history task forgot a stored call after 30 min, but calls stay in the rings about
    1.5 h. After a switch, older Clay calls were stored again under the new site (151
    duplicates on A).
- **Every call carries its site.** The lifecycle stamps it on `CallOpen`
  (`lo_plan::active_site()`). Call summaries, recordings and the history keep it, so a call
  that ends after a switch stays with its site.
  - The history task remembers a stored call while it is in the rings.
- **Recordings.** The site is in the file name (`rec_…_from<src>.<site>.wav`) and read back
  when the SD card is indexed. Older files take their site from the history.
- **Recent calls** (Now page) lists the active site's calls and recordings by default.
  - A site picker shows another site's (still in the rings or on the SD card) or all of them.
    With all, each row names its site.
  - Names come from each call's own site. "Ignore TG" is offered only on the active site's
    calls; the call cards' "last call" is the active site's.
  - `/api/ui/calls?site=`, `/api/recordings?site=`.
- **Site switch.**
  - Until the new control channel is tuned (then 1 s, at most 8 s), grants are dropped: the old
    channel's would carry the new site's name.
  - The grant map (monitor roster) and the encrypted talkgroups are kept per site and swapped.
    A talkgroup encrypted on one system was blocked on every other.
  - The event log gets a "site switched" entry.
- **Checked and left as they are** (the whole radio by design, or live): radio gain, frequency
  correction, spectrum, Systems (a sweep covers every system), Diagnostics counters (since
  boot), clock and recording settings.
- **Still open** (from the page survey):
  - the IDEN bands of a site file are not loaded on a switch (grants wait for the new control
    channel's band broadcast);
  - the site file's modulation is not applied (auto picks it);
  - event-log entries carry no site.

Tests: p25-httpd 319 (a call keeps its site across a switch; calls listed per site; site in
recording file names; the switch hold; a call's site from the history).

---

## [2026-09-28] Fix: one encrypted header silenced a traffic chain for good (072a)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-28-enc-latch-072a`
**Bake required:** NO (p25-httpd).

- **Symptom** (unit A, TG 301): most calls on chain 2 had no recording.
  - They were followed and their voice frames were extracted (call 312: 558 frames).
  - The vocoder skipped every frame as encrypted, so there was no audio and nothing to record.
  - From 16:51 local (20:51 UTC) on, every call chain 2 followed was skipped (TG 301 and
    TG 850 alike, 34 calls). Chain 1 and unit B were unaffected.
- **Cause.** An in-band encrypted HDU (a real one, or a corrupt one with a valid algorithm ID)
  marks its call encrypted. The air-time reader keeps that mark "within the call" by comparing
  call ids, but:
  - the call's own CallClose cut carries the closing call's id, so it never cleared the mark;
  - the next call's id arrives as a call-id-only cut, which kept the old mark;
  - so every later call on the chain inherited it.
  - The forwarder also copied the mark into the chain's live flag. That flag survives
    back-to-back calls on one chain, so the follower's next context carried it forward too.
- **Fix.**
  - `plan_chunk`: a new call id starts from the follower's own flag (the last full context,
    without in-band latches).
  - `latch_encrypted`: in air-time mode it latches only the segment. The reader keeps the mark
    for that call.
  - One bad header now costs at most one call.
- **Also.** Both units record to RAM (`storage: ram`), which keeps the newest 40 recordings and
  loses them on every restart. The SD store (2000 recordings / 2 GB) is ready on both but not
  selected.

Tests: p25-httpd 315 (the field sequence: latch, CallClose, call-id cut, follower context; it
fails on the old code).

---

## [2026-09-28] Activity history per site: radios, talkgroups, encryption, graphs (072)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-28-history-072`
**Bake required:** NO for the binary. The image build needs the tezuka_fw `p25-httpd.mk`
change (CC/AR for the bundled SQLite).

- **Activity page** (new tab) and `/api/activity/*`:
  - per site: 24 h, 7 days or 30 days;
  - totals and a stacked chart per hour or per day (local time);
  - talkgroups and radios by time;
  - drill-down: a radio's talkgroups and affiliations, a talkgroup's radios and its encryption
    history (first / last encrypted, last clear);
  - recent calls, and CSV export.
- **Voice and grant time are kept apart.** Voice is decoded on the voice channel (followed
  calls) and is measured. An encrypted or not-followed call has only its grant time: from the
  grant to its last update on the control channel.
  - Grant time includes hang time and any other radio that keyed up on the grant. It is
    credited to the radio granted.
  - `voice_per_grant` (decoded voice per grant second of the calls with voice) gives an
    estimate: on Clay it is about 0.72.
  - Follow-up idea 072b in the roadmap: follow encrypted calls for their details, with no
    audio.
- **Storage.** SQLite (rusqlite, bundled) on the SD card, or `/tmp` without one.
  - Every finished call is stored with its radios, 15 s after it ends, under the active site.
  - Hourly totals per talkgroup and per radio are kept in the same transaction.
  - Radio events are stored: accepted group affiliations, registrations and deregistrations,
    from the active control decoder only. Nothing is gathered during a sweep.
  - One transaction every 30 s: the WAL grows about 80 KB a minute on Clay (290 KB with a
    transaction per event).
  - Limits: 365 days, and 2 GB on the card (16 MB in RAM), oldest calls first. Clay uses about
    4 MB a day at 10k calls.
- **Speed.** A month of a busy site (300k calls, 900 radios, synthetic) on the host:
  talkgroups 0.27 s and radios 0.45 s with the hourly tables, against 73 s and 3 s from the
  calls. The 7-day and 30-day views reload every 2 and 5 minutes.
- Live on both units (Clay): the first 15 minutes on A stored 107 calls, 40 of them encrypted.
  Totals were 3m 18s of voice and 2m 28s of encrypted grant time (≈ 1m 46s of voice at 0.72),
  from 38 radios on 7 talkgroups.

Tests: p25-httpd 314 (store: idempotent inserts, voice vs grant totals, radios and their
talkgroups, affiliations merged, encryption history, local-day series, size trim; decoder:
radio events only when accepted and active).

---

## [2026-09-28] Find local systems: band sweep, control-channel probe, sites from results (071)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-28-find-systems-071`
**Bake required:** NO (p25-httpd and web UI).

- **Systems page** (new tab) and `POST /api/discovery/scan`: a sweep of the P25 700 / 800 /
  900 MHz bands (VHF and UHF optional).
  - The LO steps at 16 MSPS, four windows for the default bands.
  - The hardware spectrometer finds carriers present in nearly every frame. A control channel
    is continuous; a traffic channel is on only during calls.
  - Each carrier is probed with both control decoders (LSM and C4FM).
  - TSBKs plus a full identity make a site. The probe keeps its WACN, system, RFSS, site, NAC,
    IDEN bands, neighbours and secondary control channels, and which demodulator passes more
    CRCs.
  - Neighbours' control channels in the bands are probed too.
  - NIDs without a control-channel identity are listed as a busy P25 voice channel.
  - A silent steady carrier is "other", but only if it is still there afterwards; one that went
    away was a call that ended.
  - The radio then returns to the active site.
- **"Add"** (`POST /api/discovery/add`) writes a site file from a found site: exact control
  channel (the site's own announcement), secondary control channels, identity, IDEN bands,
  modulation. The window planner (070) learns its traffic channels from grants. "Listen"
  switches to a site that already has a file.
- **Radio lease** (`app::discovery::RadioLease`), the first piece of the review's tuning
  ownership (H2). While a sweep has the radio:
  - the follower ignores grants, and recentre, the PPM tracker and the site clock pause;
  - `/api/tune`, `/api/preset`, `/api/site`, `/api/site/recentre` and `/api/ppm_calibrate`
    answer 409.
- **Control modulation choice (071b follow-up).** An hour on unit B (weak Clay) switched 59
  times with the 5 s window. The auto choice now uses a 20 s window, a 25 % margin and 30 TSBKs
  minimum, and holds 60 s after a switch; a new control channel starts afresh.
- Live on unit A, three sweeps, ~2.3 min each, the same 12–14 sites every time:
  - Clay (8A0 1-1, LSM) and Duval (3BD 1-2), matched to their site files.
  - A second Jacksonville site: 3BD 1-3 on 852.7625, weak.
  - SLERS 141 19-19 on 770.20625 (C4FM).
  - PSIC St. Johns 292 1-15 on 774.65625 (C4FM), added as `psic_st_johns` and listened to.
  - An unknown system 4D6 1-2 on 853.3875 (LSM, weak).
  - FPL 00A sites 21, 111, 51, 87, 77, 39, 89 and 109 (C4FM; 89 and 109 found through
    neighbour lists).
  - Steady non-P25 carriers: 852.4376 and 857.139 MHz.
- An hour on each unit before this change (071b): A on FPL (C4FM active, 97.8 % CRC, no FPL
  voice) and B on Clay indoors (70.5 % CRC, 118 calls followed). Health 120/120 on both; the
  C4FM demodulator used 12.6 % / 16.4 % of a core.
  - B's memory grew 8.8 → 25 MB as its 16k-entry event log filled; A's log was a third full.
- Not yet: a sweep does not decode DMR / NXDN (listed as "other"). Traffic C4FM and the Harris
  vendor grants are still open (roadmap).

Tests: p25-httpd 305 (step plan covers the bands, continuous vs bursty carriers, DC and edge
exclusion, found site → site file, lease exclusivity; modulation hysteresis).

---

## [2026-09-28] C4FM control channels: SDRTrunk's C4FM demodulator in software (071b)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-28-c4fm-071b`
**Bake required:** NO (p25-httpd).

The HDL chain is an LSM (CQPSK) demodulator. It locks on C4FM too, but on unit A it passed only
42 % of FPL's TSBKs and 60–69 % of SLERS's. Those sites, like the other nearby C4FM systems
(Putnam, St. Johns, PSIC), need a real C4FM demodulator.

- **`protocol::p25::c4fm`:** a port of SDRTrunk's `P25P1DecoderC4FM` and
  `P25P1DemodulatorC4FM`.
  - Front end: half-band 50 → 25 kSPS, baseband low-pass (5.2 / 6.5 kHz), RRC, then the
    differential demodulator with the 8-tap MMSE interpolator.
  - Demodulator: soft sync detection (primary and half-symbol lagging), the timing optimiser,
    the equaliser (phase balance and gain from each sync), and NID validation through BCH
    before a correction is taken.
  - It feeds our framer the way SDRTrunk feeds its own: dibits, plus `sync_detected()`.
- **IQ hub** (`app::iq_hub`): one reader per DDC IQ ring (control and traffic chain 1, 50 kSPS)
  fans the samples out.
  - The narrowband spectrum and the IQ dumps read from it. Before, readers took each other's
    DMA sub-buffers.
  - `/api/traffic_iq_dump` works again.
- **Both control decoders run all the time;** only the active one publishes grants and TSBK
  events (`ControlChannelDecoder::active`).
  - Auto (the default) picks the decoder with more TSBK CRC passes over 5 s, and switches only
    for 20 % more.
  - `/api/modulation` separates the setting (`mode`) from the choice (`active`); the old auto
    mode latched on its first decision.
  - Radio → Modulation shows both CRC rates and the demodulator's CPU.
- Offline, on 20 s recordings from unit A:

  | Recording | TSBK CRC, HDL LSM | TSBK CRC, software C4FM |
  |-----------|-------------------|-------------------------|
  | FPL 936.25 MHz | 42 % | 99.4 % |
  | SLERS 770.20625 MHz | ~64 % | 95.6 % |
  | Clay 860.9625 MHz (LSM) | 99.5 % | 100 % |

- Live on unit A:
  - FPL: auto switched to C4FM within 5 s, 1079 / 1081 TSBKs (LSM 43 %).
  - SLERS: 99 % (LSM 69 %).
  - Clay: both decoders ~98 %, so auto keeps whichever is active.
  - The demodulator uses 12–17 % of one A9 core.
- **Not yet:**
  - C4FM on traffic channels. Voice still uses the HDL LSM chains. Chain 1's IQ is in the hub
    for it; chain 2 has no IQ tap.
  - Harris (MFID 0xA4) and Motorola (0x90) vendor TSBKs are counted but not decoded (SLERS and
    FPL send many Harris ones).
  - No FPL or SLERS voice was granted during the tests.

Tests: p25-httpd 301 (C4FM slicer and sync pattern, synthetic C4FM decode, IQ hub, auto choice
hysteresis); `c4fm_wav` (ignored) decodes an IQ recording given in `P25_C4FM_WAV`.

---

## [2026-09-28] Review fixes before 071: neighbour sites, TDMA bands, NAC relock, call handling, restarts (071a)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-28-review-fixes-071a`
**Bake required:** YES for the respawn loop (tezuka_fw `overlay_p25/etc/init.d/S60p25-httpd`);
the rest is p25-httpd.

Stage 0 of `doc/CODE_REVIEW_2026_09_28.md`.

- **Neighbour sites (Adjacent Status Broadcast, 0x3C)** decoded at SDRTrunk's offsets (they
  were 4 bits off: system, RFSS, site and channel all wrong), with the status flags and the
  service class. Kept per (system, RFSS, site) and shown in `/api/system` `neighbours` with
  their control-channel frequency.
  - Validated live on unit A. FPL (C4FM, 936.25 MHz, system 00A) lists 8 neighbours, all of
    them system 00A; 5 of their control channels are FPL frequencies in the SDRTrunk playlist:
    Bradford 937.650 and 935.500, Putnam 936.225, St. Johns 937.700 and 935.475.
  - SLERS (770.20625 MHz, system 141) lists 3 neighbours of system 141 at 769–770 MHz.
  - Clay does not broadcast 0x3C (single site). SDRTrunk's own logs only have CRC-failed ones,
    which is why they looked like nonsense.
- **TDMA bands:** IDEN_UPDATE_TDMA carries the timeslots per carrier (SDRTrunk `ChannelType`).
  - A TDMA channel number counts timeslots (base + spacing × floor(ch / slots)).
  - A grant on a TDMA band is not followed ("Phase 2 (TDMA)"; reason `phase2`), instead of
    tuning a wrong carrier. Clay defines a TDMA band but granted none in 720 grants.
- **NAC relock:** after a retune, dibits still buffered from the old channel could re-lock the
  NAC tracker to the old system. Every frame of the new one was then dropped as a NAC mismatch
  (seen live: back on Clay but locked to SLERS 0x0C5).
  - Eight consecutive valid NIDs of one other NAC now move the lock; scattered false decodes
    do not.
- **Call handling:**
  - The call-boundary channel holds 4096 events (was 64), and only voice NIDs go through it.
  - A lag keeps the open calls (it used to close every call).
  - A chain still locked to a talkgroup with no open call (lost CallClose) is released after
    two 5 s checks.
  - Clock steps (site time, NTP) wait up to 2 min for both chains to be idle.
  - The vocoder flushes per lifecycle call id, and its per-frame timings are capped at 3000;
    a one-talkgroup lane grew ~100 MB/day.
- **Robustness:**
  - The register mapping is shared in an `Arc`; it was cloned, and a dropped clone unmapped
    it under the other.
  - Interrupts use `notify_one` (a wakeup can no longer be lost).
  - A panic anywhere exits the daemon.
  - The init script now runs p25-httpd in a respawn loop (restart after 3 s; tested on A with
    `kill -9`: back in 3 s; stop and start clean).
- Live check on unit A (Clay, 12M, 10 min): health 59/60, TSBK 39.5/s at 98.9 % CRC, 160 calls,
  110 followed, 20826 IMBE, vocoder errors 3.3 % (0–4.4 % across today's runs). No stuck-lane
  release, lag or Phase 2 events.
- The replay bench could not run: unit B is not connected.

Tests: p25-httpd 297 (0x3C offsets, TDMA frequency and IDEN_UPDATE_TDMA, neighbours and TDMA
grant flag, NAC relock).

---

## [2026-09-28] Narrowest window per site; PPM calibration follows the site (070b)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-28-coverage-070b`
**Bake required:** NO (p25-httpd and web UI).

- **Narrowest window per site** (Radio → Coverage, `PUT /api/site/plan {"min_preset": "12M" | null}`):
  the planner never picks a narrower preset there, and the recentre task widens a window that
  is narrower. Unit A: Clay is set to 12M. Without it, once Clay's plan is learned (1000 grants)
  its never-granted 852.4385 MHz entry stops counting and the window would move back to 8M.
- **What 852.4385 MHz is:** a steady carrier about 30 dB above the noise, present in every
  wideband frame, with no P25 on it. The control decoder tuned there for 12 s saw 2 NIDs and no
  TSBKs; the LSM chain also decodes C4FM phase steps, so a P25 control channel of either kind
  would have shown up. It is not a Clay P25 channel (Clay's LCN 6 is 858.4375).
- **12M vs 8M on Clay:** no measurable decode difference (control CRC 98.9–99.1 % vs 98.8 %,
  similar vocoder error rates). The AGC runs ~2 dB lower at 12M because more strong carriers
  sit in the wider passband; the PS load is the same (the DDCs are in the FPGA).
- **Retune dropouts measured on unit A:** re-applying the preset, moving the LO 200 kHz, and
  12M → 8M → 12M kept TSBKs flowing (longest gap between TSBKs ≤ 170 ms, normal ~25 ms). A
  control-channel move to another frequency and back resumed decode 0.27 s after the request.
- **PPM calibration** now searches around the control channel tuned now, not the boot one
  (after a site switch it looked around the old site's control channel). Found by the
  2026-09-28 code review.

---

## [2026-09-28] Receive window planned per site, auto recentre, Jacksonville (070)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-28-coverage-070`
**Bake required:** NO (p25-httpd and web UI).

- **Window planner** (`services::lo_plan`): places the LO, and with preset `auto` picks the
  preset, so the window holds the control channel and as many of the site's channels as
  possible.
  - Channels are the site file's list plus every frequency granted on this site, weighted by
    grants.
  - After 1000 grants the plan counts as learned: a listed channel never granted weighs nothing.
    Clay's 852.4385 MHz entry (SDRTrunk "LCN 6", off the 6.25 kHz raster, no grants in 228 so
    far) is the only reason Clay needs 12M today.
  - Usable window ±0.45 × sample rate; no channel within 15 kHz of the LO (DC notch).
  - Grant counts and the auto switch persist per site in `/mnt/jffs2/p25-plans/<site>.json`.
- **Auto recentre** (`app::recentre_task`): when a better window exists (all channels, or 5 %
  more of the traffic), the site's auto switch is on, the LO is not locked, both chains are
  idle, 2 min after start and 10 min after the last move. Unit A moved itself from 8M
  LO 858.10 to 12M LO 856.70 two minutes after start (event log "recentre (auto)").
- **Not followed "outside the window"** (`out_of_band`): a grant beyond the usable window is not
  followed (it would alias) and is counted for the planner.
- **Radio → Coverage card:** the window, every channel drawn by use (red outside, grey never
  granted), what lies outside, the better window (dashed), Recentre, and Automatically when idle.
- **Site switch** plans the preset and LO (`preset: auto`), and the control decoder forgets
  the old system (`new_system`: NAC lock, identity, IDEN bands; also on `/api/tune` to another
  frequency). Before, the NAC tracker stayed locked to Clay's 0x8A1 and dropped every
  Jacksonville frame as a NAC mismatch.
- **Boot** tunes to the active site's control channel and planned window. The init script's
  `--control-freq` / `--rx-lo` / `--preset` are only the fallback; a radio left on Jacksonville
  used to boot on Clay's control channel.
- API: `GET/PUT /api/site/plan`, `POST /api/site/recentre`, `/api/preset` `auto`.
- Duval site file: identity observed on air (system 0x3BD, RFSS 1, site 2).

Live on unit A (external antenna, 2026-09-28):

| Window | CC offset | TSBK/s | CRC | Calls followed | IMBE | Vocoder errors |
|--------|-----------|--------|-----|----------------|------|----------------|
| Clay 8M, LO 858.10 | +2.86 MHz | 39.4 | 98.8 % | 20 | 2016 | 79 |
| Clay 12M, LO 858.10 | +2.86 MHz | 39.4 | 98.9 % | 15 | 1440 | 64 |
| Clay 16M, LO 858.10 | +2.86 MHz | 39.5 | 99.1 % | 32 | 4491 | 132 |
| Clay 8M, LO 857.46 | +3.50 MHz | 39.5 | 99.1 % | 22 | 3789 | 0 |
| Clay 16M, LO 855.96 | +5.00 MHz | 39.6 | 99.3 % | 34 | 4635 | 39 |
| Clay 16M, LO 854.46 | +6.50 MHz | 39.5 | 99.0 % | 30 | 4662 | 50 |
| Clay 16M, LO 853.46 | +7.50 MHz | 39.4 | 99.0 % | 30 | 4104 | 0 |
| Clay 12M planned, LO 856.70 (5 min) | +4.26 MHz | 39.5 | 99.1 % | 32 | 4572 | 71 |
| Duval 8M planned, LO 857.98 (8 min) | −2.49 MHz | 35.3 | 91.3 % | 134 | 23544 | 1486 |

Each Clay row is 3 min. Jacksonville is far from the shop (its control channel is ~23 dB
below Clay's). All 28 Duval channels sit inside the 8M window, and voice decoded on every
frequency granted, from 856.21 to 860.94 MHz (window edge +2.96 MHz). The two frequencies
without a followed call had only encrypted or chain-busy grants.

Tests: p25-httpd 292 (planner: Duval fits 8M, Clay needs 12M, busy channels win, the CC stays
inside, learned plan, persistence; `new_system`).

---

## [2026-09-28] Profiles per site: groups, speakers, monitor and ignore lists (069)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-28-profiles-069`
**Bake required:** NO (p25-httpd and web UI).

- **Profiles:** a profile is a named setup of the talkgroup groups, the speakers, the monitor
  list and the ignored talkgroups. Each site has its own profiles, and its own talkgroup and
  radio names (talkgroup numbers mean different things on different systems).
  - Now → Speakers has a profile picker; Settings → Profiles adds New (empty: follows
    everything), Copy (starts from the live one), Rename and Delete (a site keeps one).
  - Settings cards that belong to the profile or the site show which one in their title.
  - Picking a profile swaps the whole setup at once; a call the new setup does not follow is
    dropped (event log reason `profile`).
- **Site switch** (Radio page, `POST /api/site`) loads that site's names and the profile last
  used there; a site seen for the first time starts with an empty "Default" (follow
  everything, no names).
- **Migration:** the settings file from before 069 becomes the active site's "Default" profile
  at start-up (unit A: Clay County › Default with Primary / TAC / Hospital).
- API: `PUT /api/ui/settings` `{"profile": {"select": name}}`, `{"create": {name, copy}}`,
  `{"rename": {from, to}}`, `{"delete": name}`, alone in its patch. `GET` adds
  `profiles: {site, site_label, names, active}` and `settings.site` / `settings.sites`.

Tests: p25-httpd 287 (migration, profile actions and validation, site switch swaps names and
profiles, persistence and a hand-edited file). On unit A: the stored setup adopted as Clay
County › Default; create / select / rename / delete and a Clay → Duval → Clay switch
(`no_apply`) behave as above; Settings and Now render (headless Edge).

---

## [2026-09-28] Ignored talkgroups (068)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-28-ignore-list-068`
**Bake required:** NO (p25-httpd and web UI).

- **Settings → Ignored talkgroups:** talkgroups that are never followed — the opposite of
  the monitor list. Enter single talkgroups or ranges (`402, 700-710`); ✕ on a chip takes one
  off. Saved on the radio (`ignore_tgs` in `/api/ui/settings`).
  - The ignore list wins over the monitor list and the speaker groups.
  - A talkgroup on the air when it is added is dropped at once (its chain goes idle), not at
    its next grant.
  - Ignored grants still appear in Recent calls, not followed ("ignored").
- **Recent calls:** a call's details have "Ignore TG n" (or "Follow TG n again").
- `release_chains_on` (`/api/encrypted_tgs` and the ignore list share the chain release).

Tests: p25-httpd 279 (ignore list validation, routing precedence, live policy); 23 UI modules
pass `node --check`.

---

## [2026-09-27] Board clock from the site's time; site time on the Now page; live-audio normalization (067)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-27-site-clock-067`
**Bake required:** NO (p25-httpd, web UI, bench).

- **The radio clock can follow the control channel** (Settings → Clock, "Radio clock from").
  - **Control channel (site time), the default:** SYNC_BCST (TSBK 0x30) is now fully
    decoded: 7.5 ms micro-slots, the rollover lock and the local time offset. With locked
    micro-slots the time is exact to the decode delay. Otherwise the second comes from
    watching the minute roll over.
  - An unset clock and the first correction step the clock. After that, small differences
    are slewed (`adjtime`), so call times never jump backwards; only a clock more than 30 s
    off steps again.
  - **Internet time (NTP)** at start and hourly. NTP no longer blocks start-up (up to 15 s
    offline before).
  - **Manual:** set from a browser only.
- **Site card:** a ticking site time in the site's own time zone (when it announces one) and
  the radio clock's source, with how far it is from the site.
- **Clock card:** site time, quality (precise / to the second / to the minute), GPS lock,
  radio-vs-site offset. The browser's automatic "set from this browser" and the clock
  banner stay out of the way while the radio follows the site.
- **Volume normalization** for live audio (header "Level" toggle, per browser, on by
  default).
  - Each chain's stream is levelled from the first 20 ms of every transmission, instead of
    waiting for the vocoder's AGC to settle for a second.
  - Loud peaks are followed fast and quiet passages slowly. The gain holds through silence,
    and a soft limiter above −2 dBFS keeps boosted peaks clean. Recordings are unchanged.
- **Grant dedup survives a clock stepped back:** an entry "in the future" no longer
  suppresses the same grant until the clock catches up.
- **Bench:** `rf.p25_corpus` pins the DUT clock source to manual for a run (a replayed site's
  time is a different day per item) and restores it; `Http.put_json`.

Tests: p25-httpd 278 (site clock 6, SYNC_BCST fields, clock step/slew rule, dedup with a
clock stepped back); UI modules pass `node --check`; the leveler and the two-lane mixer run
under node 20.

---

## [2026-09-27] Two calls at once: the second traffic chain in p25-httpd (066)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-27-dual-chain-066` (shipped with 067)
**Bake required:** NO (uses core 0.3.0, change 064; one chain on older cores).

- **Chain 1 follows the left speaker's groups, chain 2 the right's**, so a TG 300 call no
  longer stops a TAC or hospital call. "Other" talkgroups on both speakers use either chain.
  Within a chain the pre-066 rules apply: sticky lock, end-marker pre-emption (059), group
  priority (063). A busy side does not borrow the other side's chain.
- **Hardware layer** gated on core ≥ 0.3.0 (older cores stall the CPU on chain-2
  addresses): `IpCore::lane`, `LaneRegs`, the chain-2 DMA ring, interrupt bit 8, and chain 2
  armed at boot. `/api/traffic2` shows chain 2's registers and call; bring-up retune and a
  ring probe that decodes its dibits.
- **Per chain:** decoder, IMBE forwarder, dibit reader, heartbeat, vocoder thread, pacer,
  recorder, grant-stats slot. One call lifecycle (one call-id space) with a slot per chain
  and cross-chain channel rules.
- **Live audio:** `/ws/audio?v=2` carries both chains (lane-tagged frames); the player mixes a
  ring per chain into the speakers. `/ws/audio` (chain 1) and `/api/audio?chain=` for the
  bench and external players.
- **UI:** a call card per chain on the Now page, "chain 2" in Recent calls, the chain count
  in the Speakers panel. `/api/ui/state` adds `calls[]` and `chains[]`.
- `--traffic-chains auto|1|2` (default auto).
- **Bench** (Mode B replay corpus, full): recovery of all clear transmissions 95.2 % →
  99.1 %; 11 of the 12 transmissions a single chain lost to an overlapping call are decoded
  in full; followable 99.3 % → 99.6 %, 0 missed. Chain 2 brought up on the control channel:
  NAC 0x8A1, 268 TSBKs decoded from its ring.

Doc: `doc/changes/066_dual_traffic_chain_ps.md`.

---

## [2026-09-27] Second traffic decode chain in the gateware, core 0.3.0 (064)

**Branch:** fishball-p25
**Bake required:** YES (core 0.3.0). **Tezuka rebuild required:** yes (device tree node
`p25-traffic2-lsm-dibit`, ring at 0x1D00_0000).

- **Chain 2** (`traffic2_*`): its own DDC, LSM chain (with the 059 hold), dibit packer and DMA
  master (`m_axi_traffic2_lsm_dibit` on HP1), interrupt bit 8, banks 9–11 at 0x120–0x168. A
  block-for-block copy of chain 1 without its diagnostic taps; everything resets idle.
- **Register map** is a strict superset of 0.2.0: the deployed p25-httpd runs unchanged.
  A PS must not read 0x120–0x168 on an older core (the bus stalls); change 066 gates it.
- **Timing met**, WNS +0.021 ns, WHS +0.018 ns. LUT 54 %, DSP 78 %, slices 90 %.
- **Bench:** Mode B replay corpus identical to 0.2.0 (99.3 % of SDRTrunk's IMBE frames,
  0 missed).

Doc: `doc/changes/064_second_traffic_chain.md`.

---

## [2026-09-27] Channel time for calls not followed; delete stored recordings; clearer call times (065)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-27-channel-time-065`
**Bake required:** NO (p25-httpd and web UI only).

- **Channel time for calls not followed** (encrypted, busy on another call, monitor list or
  speaker groups), as SDRTrunk lists encrypted calls.
  - The record stays open while the control channel announces the call (repeat grants,
    GRP_VCH_GRNT_UPD).
  - It closes at the last announcement once none came for `hang_ms`, or when its channel is
    granted to another call.
  - Recent calls shows that time ("channel time"); before, these calls had no duration.
- **Delete stored recordings:** Settings → Recording, "Delete SD recordings" / "Delete RAM
  recordings" (with a confirmation). Backed by `DELETE /api/recordings?store=sd|ram|all`,
  which removes the files and the list entries.
- **Call details list three times, labelled:**
  - Voice: the decoded audio, the same as the playback bar.
  - On air (control channel): grant to the last update.
  - Held open, or Channel time for a call not followed.

Tests: p25-httpd 247 passed; 21 UI modules pass `node --check`.

---

## [2026-09-27] Talkgroup groups, speakers on the Now page, priority pre-emption (063)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-27-tg-groups-063`
**Bake required:** NO (p25-httpd and web UI only).

- **Talkgroup groups** (Settings → Talkgroup groups), saved on the radio.
  - A group is a name and a list such as `300` or `301-310, 315`.
  - The list order is the priority (1 = highest).
- **Speakers panel** on the Now page. Each group goes on the left or the right speaker,
  or on neither (then it is not followed). Talkgroups in no group play on both speakers,
  either one, or are off. This replaces 062's per-browser speaker card; the volume slider
  stays per browser.
- **Priority:** with "A higher-priority group interrupts a lower one" on (the default),
  a grant of a higher group takes the traffic channel from a lower group's call. The log
  reason is `priority_preempt`.
  - A follow filter rejects talkgroups whose group is off, or ungrouped talkgroups with
    "other" off (`not_followed` = `speaker_off`).
  - With no groups defined, behaviour is unchanged.
- **Settings API:** new fields `tg_groups` and `speakers {left, right, other, preempt}`.
- **Store:** the web UI now keeps `/api/ui/settings` current and refetches it on
  `settings_rev`.

Tests: p25-httpd 244 passed; 21 UI modules pass `node --check`; list parsing and routing
helpers checked.

---

## [2026-09-27] Live audio: volume control and per-talkgroup left/right speaker (062)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-27-volume-speakers-062`
**Bake required:** NO (p25-httpd and web UI only).

- **Volume:** a slider next to Listen, 0–200 %, saved in this browser.
- **Speaker routing:** Settings → "Live audio (this browser)".
  - Set the talkgroups for the left speaker, the talkgroups for the right speaker, and where
    all other talkgroups play (both, left or right). For example: TG 300 left, everything
    else right.
  - The player outputs stereo and switches speaker per sample, so a talkgroup change inside
    the 150 ms buffer lands on the right side.
  - Both the AudioWorklet (https) and ScriptProcessor (http) paths do this.
- **`/ws/audio`:** a `{"type":"meta","tg","src","call_id"}` text frame now comes before the
  first audio frame of each talkgroup / call. Binary frames are unchanged, and clients ignore
  text types they do not know.

Tests: p25-httpd 241 passed; all 19 UI modules pass `node --check`; the worklet routes
left- then right-tagged audio to the matching channel.

---

## [2026-09-27] A call's source is the unit the grant was issued to (061)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-27-grant-source-061`
**Bake required:** NO (p25-httpd only).

- **The source is the grant's unit.** `grant_stats` no longer lets a later link-control ID
  replace a call's source; it only fills a source-less call. The lifecycle and the recorder
  already worked this way.
  - On the site, the link-control ID was wrong both times it differed from the grant:
    - responder 3400043 was heard, and the LC said 1014;
    - dispatch 1013 was heard, and the LC said 3402072.
  - It became the call's source, so the UI showed the wrong unit.
- **UI:** the call list and "Now on air" show only the grant's unit. A different
  link-control ID appears only in the call details ("Link control ID … differs from the
  grant"). This replaces 060's "grant N" label.
- **The LDU1 link-control vote ring resets per call, not only per talkgroup.** Since 057 every
  grant is a call, so back-to-back calls of one TG no longer share votes.

Tests: p25-httpd 241 passed.

---

## [2026-09-27] RX gain survives a restart; talker vs grant unit in the call list; no countdown bar (060)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-27-gain-persist-060`
**Bake required:** NO (p25-httpd only).

- **RX gain is persisted.** A successful `/api/rx_gain` change (the Radio view's AGC switch
  and gain selector) is saved in the UI settings (`radio.gain_mode`, `radio.manual_gain_db`)
  and applied at startup after `--hardwaregain`. Before, the operator's AGC choice was lost
  at every restart. On the site antenna the boot default (manual 60 dB) left the control
  channel under core 0.2.0's no-signal gate: 71 % TSBK decode with the PLL frozen, against
  83 % with `slow_attack` (73 dB).
- **Call list:** a second unit on a call was labelled "grant N" instead of "also N" (replaced
  by 061).
- **The hang-time countdown bar under "Now on air" is removed.** The "Closes in" figure
  stays.

Tests: p25-httpd 240 passed. On A: the AGC setting survived a p25-httpd restart.

---

## [2026-09-27] Retire the pre-056 dashboard (`/legacy`)

**Branch:** fishball-p25
**Bake required:** NO (p25-httpd only).

The web UI at `/` (056) replaced it. The following are removed:

- the `/legacy` route and the 259 KB embedded `dashboard.html`;
- the links to it in the UI;
- its panel map in `doc/P25_API.md` (still in git history).

No API endpoint changes.

---

## [2026-09-27] LSM PLL/timing hold on "no signal", core 0.2.0; sticky lock freed at end of transmission (059)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-27-signal-hold-preempt-059`
**Bake required:** YES (P25 core 0.2.0; the register map is unchanged). p25-httpd adapts to
0.1.0 or 0.2.0 at runtime.

Changes:

- **Gateware: the PLL no longer traps in carrier gaps.**
  - The decision-directed PLL kept updating on noise between transmissions and reached the
    π/3 clamp. That end is absorbing, so every later dibit came out a quadrant off: 0 IMBE
    on the traffic channel, and no grants on the control channel after a silence.
  - Both LSM chains now hold the PLL and the Gardner timing while the AGC idle gate
    reports no signal (new `LsmSignalHold`; hold after 4 gated symbols, release after 8
    ungated symbols).
  - The clamp drops to 0.65 rad, below π/4, so it is no longer absorbing near the
    operating point.
- **p25-httpd:**
  - reads the core version (`hardware/core_version.rs`);
  - runs the traffic PLL watchdog (new, `traffic_pll_watchdog.rs`) only on gateware
    without the hold;
  - checks `resume_needs_reset` against the running clamp;
  - reports `core_version`, `signal_hold` and `pll_clamp_q13` in `/api/traffic`.
- **Sticky lock:**
  - Another TG's grant may take the chain once the locked call's end marker has been
    pending 600 ms, instead of 2 s later. That is still before SDRTrunk frees its channel.
  - A clear grant rejected while the chain was busy is re-followed from its grant updates
    once the chain frees, within 2 s of the reject.
- **Board, Mode B corpus (42 scenes, 219 followable clear transmissions):** 92.4 % of
  SDRTrunk's IMBE frames with 21 missed (0.1.0 + watchdog) → 97.8 % with 4 missed (0.2.0)
  → 99.3 % with 0 missed (0.2.0 + sticky-lock changes). Scenes losing their first calls:
  10 → 0. The tone scene decodes 100 % with the watchdog off.
- Deployed to A's SD card by swapping the bitstream partition of BOOT.bin (same-length
  padding, no data checksum). Tezuka still needs `build.bat --p25` with the new XSA.

Tests: p25-httpd 239 passed. maia-hdl LSM suites passed except the test that already
failed at HEAD (`test_reset_in_restores_gain_to_init`).

---

## [2026-09-27] P25 replay validation corpus: `rf.p25_corpus`, SD relay, modes A/B/C (058)

**Branch:** fishball-p25
**Bake required:** NO (fbench and fbench-agent only).

- **`rf.p25_corpus`:** replays many different SDRTrunk recordings once each and scores
  every transmission against SDRTrunk's own `.mbe`, using p25-httpd's per-call `imbe`.
  - Mode A: A's wideband captures.
  - Mode B: 42 synthetic scenes (a CC recording plus the traffic recordings, aligned to the
    log clocks), 320 transmissions.
  - Mode C: traffic only.
- **fbench-agent `replay stream|check|verify`:** a 192 MiB SD-to-`iio_writedev` relay.
  It has streamed 24.6 GB passes with 0 underruns.
- **Scorer:**
  - survives an unset DUT clock and clock steps;
  - matches calls by TG, frequency and time (not by source);
  - classes a transmission as not followable only when the chain was genuinely busy.
- `tools/p25_corpus_index.py` builds the inventory and manifest.
- `tools/p25_lsm_hdl_replay.py` feeds recordings through a bit-true front end and the
  Amaranth `LsmDemod`, including carrier-gap scenarios.

Tests: bench host 272 passed.

---

## [2026-09-27] Call close at the end of transmission, per-call counters, SD-card recordings (057)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-27-call-close-sd-057`
**Bake required:** NO — p25-httpd only.

Traffic-channel teardown audit and rebuild against SDRTrunk (source and 719 measured
transmissions of the site, doc/changes/057), exact per-call counters, and recordings on the
SD card as an option.

Changes:

- **Call close.** A call closes 2 s after its end of transmission (first LC-valid TDULC after
  its voice; SDRTrunk ends its call event at the same frame) unless voice resumes, at once on
  the next grant, or after 3 s with no keep-alive. It was 10 s after the last CC update,
  sized for 054's 3.4 s dibit blocks: a 1.44 s PTT now closes after ~3.9 s instead of 12.8 s,
  and the single traffic chain is free ~9 s sooner. Both times are persisted settings
  (`call.end_grace_ms`, `call.hang_ms`, Settings view "Call close").
- **Grants:** a repeat of the on-air call's grant refreshes it instead of splitting the
  transmission; the next talker's grant that arrives while the current one still talks
  (12.8 % of same-channel grants on this site) waits for the hand-over instead of taking the
  rest of the transmission; a grant update re-follows a call closed by timeout.
- **Same-frequency resume resets a stale chain.** A parked chain demodulates noise once the
  carrier drops (its PLL reached the clamp), and with the prompt close every same-channel
  call after a pause resumed from that state: 68.7 % recovered on a bench window built for
  it, 100.0 % after the fix. The chain now coasts only if it carried voice within 1 s and its
  PLL is under half the clamp; otherwise it takes the reset retune.
- **Bench:** a 64-loop soak with the SD card stalling up to 6.1 s kept 100.0 % IMBE, 0 resyncs
  and 0 failed SD writes; the SD store was checked for re-index at boot, read-only fallback and
  retention.
- **Fixes:** a same-frequency grant after an encrypted teardown left the traffic LSM disabled;
  CC updates for the TG on another channel kept a call alive; grant_stats blocked 2 s at every
  close and stopped for good on a broadcast lag.
- **Per-call counters by call_id** (IMBE, LDU, HDU, TDU, drops, vocoder PCM / silent / errors /
  encrypted), attributed where each frame is decoded; `/api/ui/calls`, `/api/grant_decode_stats`
  and `/api/recordings` report exact numbers. Global counters unchanged. `vocoder_errors`
  (never counted before) = frames whose IMBE FEC corrected more than 4 bits.
- **Recordings on the SD card** (`recording.storage` "sd", `/mnt/sd/p25_recordings`), with count
  and size retention per store. Writes happen on a separate thread (the card stalls for
  seconds), recordings play from RAM until written, and an absent / read-only / full card
  falls back to RAM with the reason in `/api/ui/settings`. Existing recordings do not move.
  SD recordings are listed again after a restart, and call ids continue after them.
- `tools/sdrtrunk_teardown_stats.py`: SDRTrunk teardown / turnaround distributions from its
  event logs, and the same figures from p25-httpd `/api/ui/calls` / `/api/log` dumps.

Tests: 229 passed (was 195). Not yet run on the board.

---

## [2026-09-26] Web UI review and redesign; persisted recording / alias / monitor settings (056)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-26-web-ui-056`
**Bake required:** NO — p25-httpd only.

Review of the old dashboard against the live 054 bench replay, plus a new UI
(doc/changes/056). After 054 the call data is right: every WAV holds exactly its PTT's
frames. What still looked wrong was mostly presentation:

- The page mixed three "current calls" (lifecycle, follower, per-HDU vocoder baselines).
- It showed the lifecycle's 10 s hang as an active call.
- It never cleared the CURRENT CALL box, and rendered call duration as the open time.

Changes:

- New web UI at `/`: native ES modules under `p25-httpd/src/httpd/ui/` (22 files, each
  ≤ 231 lines), embedded at compile time, versioned URLs (BUILD_TAG + content hash), no CDN.
  - Views: Now (current call with acquiring / voice / hang phase and close countdown, site
    health, recording switch, recent calls with on-demand playback and a reason when there
    is no audio), Radio, Diagnostics, Settings.
  - Works on phones (bottom tab bar, single column).
  - The old dashboard stays at `/legacy`.
- `GET /api/ui/state` (~1 KB, 1 Hz) and `GET /api/ui/calls` (joined by call_id, refetched
  on `calls_rev`), built from the call lifecycle only. `/ws/events` is used as a push kick.
- Recording on/off and retention (`PUT /api/ui/settings`), persisted to
  `/mnt/jffs2/p25-ui-settings.json` and honoured by the recorder; lowering retention
  deletes the oldest WAVs at once.
- TG and radio-unit aliases and the monitor list are persisted and restored at boot.
  `PUT /api/aliases` now reaches both control decoders; it only reached the C4FM decoder,
  so aliases never showed on LSM sites.
- Fixes:
  - `/api/recordings` per-call counters are snapshotted at the close; they used to include
    the next call's frames (72-frame PTTs reported 144–153).
  - `/api/log` gains `tail=1` (a first read returned boot-time entries), `from_ms` /
    `to_ms` and `tsbk=0`.
  - `/api/pipeline` grant ring capacity (said 20, is 200).
  - The control spectrum is centred on the live tuned frequency.
  - Air-time cut ordering (a 054 bug found by the bench soak). A cut is never placed
    before the previously recorded one (`cuts_reordered` counts the raises). A grant
    hold's estimate could land 2 dibits after the TG change / CallOpen recorded after it,
    which gated a new call's first transmission, about 1 call in 150.
  - `/ws/audio` listener count (the old `audio_ws_clients` counts 2 internal receivers).
- 44 new host tests (192 total), including asset tests: every `index.html` reference and
  JS import resolves, no orphan files, no network loads, module size cap.
- Existing endpoints and fields are unchanged; the `bench/fbench` endpoints are
  untouched.

---

## [2026-09-26] Low-latency dibit delivery + air-time traffic gating (F4); autoppm sign fix (055)

**Branch:** fishball-p25
**BUILD_TAG:** `2026-09-26-dibit-lowlatency-airtime`
**Bake required:** NO — p25-httpd only.

Fixes finding F4 (doc/changes/054): dibits reached the PS in 3.41 s sub-buffer blocks and
the traffic gate was applied at delivery time.

- Position-based reader for both dibit rings: absolute byte position, safe end = previous
  poll's next-address − 256 B, sub-buffer invalidate + copy every ~40 ms (IRQ as wake
  hint), lap guard with EventLog resyncs (jump / stall / base change / `last_buffer`
  phase mismatch / overrun). Dibit age at delivery drops from 0–3.41 s to ≈ 0.05–0.2 s (host DMA
  model: mean 114 ms, max 195 ms); the control channel's grants arrive correspondingly
  earlier.
- Production clock from next-address readings (± a few ms once converged) and air-time
  epochs on the traffic ring: retune / NCO / LSM reset / pause-resume (IpCore hooks), TG
  changes, CallOpen (call_id only), CallClose and grant holds are cut at their production
  index; each chunk is split and decoded under the context in effect on the air (framer
  reset at discontinuities and gate flips, pre-settle dibits discarded and counted). IMBE
  batches carry the epoch's TG / call_id / encryption and an air-time `captured_at`;
  the recorder routes such chunks by call_id. CallClose no longer drops the closing
  call's in-flight tail; a new call never receives the previous call's dibits.
- Gating fix: the follower no longer releases the chain on the CallClose of a
  not-followed grant's synthetic call or of a preempted predecessor (it zeroed the TG of
  the call being followed on every rejected grant).
- `GET/POST /api/dibit_delivery` (age percentiles + histogram, clock uncertainty,
  resyncs, epoch splits, discards, recent cuts; runtime mode switch) and
  `--dibit-delivery airtime|poll|legacy`, `--dibit-poll-ms` for bench A/B without
  reflashing. 42 new host tests.
- Same build also carries the autoppm sign fix (doc/changes/055): `pll_dbg` reads NCO
  minus signal, so the corrected shift is `shift − residual`. `POST /api/ppm_calibrate`
  now lands within 5 Hz of the hand trim; the old sign put it 165 Hz off.
- Bench result: a cabled two-board replay of site capture 1777801424, scored against
  SDRTrunk's live decode of the same air. Traffic IMBE frames recovered: 57.6 % on the
  deployed build, **99.9 %** on this one (33/33 LDUs, 4/4 HDUs per loop). Traffic dibit
  age p99 is 189 ms. Full table in doc/changes/054.

---

## [2026-09-26] Hardware validation bench: `fbench` CLI, on-board agent, `hwval` bitstream

**Branch:** fishball-p25
**Bake required:** Tier 0 tests NO (run on the current P25 image); Tier 1 needs the new
`hwval` bitstream (`./build_fpga_hwval_pretty.sh`) plus the Tezuka SD dual-image layout.

A two-Fishball, cabled, attenuated bench suite that a Claude session drives from the CLI
(JSON output, exit codes, run dirs with `result.json` + `FINDINGS.md`). Design contract:
`doc/HW_VALIDATION_SUITE.md`; evidence and audit results: `doc/changes/053`.

- Audit results that change the evidence record: the forensics
  `gap_dibits=14336, overflows=1` was a host-poll artifact (ring loss was never
  measured); the production overflow flag is cleared by the `last_buffer` read; no ring
  has lap detection; dibits reach the PS in 3.41 s blocks and the traffic TG gate is
  applied at delivery time (cross-call bleed / tail truncation candidate); the P25
  image's `maia-sdr.ko` is a stale leftover not built by the P25 defconfig.
- `hwval` gateware (`maia-hdl/hwval_hdl/`): ring v2 (FIFO, store-and-forward, committed
  counter, flush-bounded latency, protect mode, lap-safe reader protocol), an
  instrumented replica of the production ring, AXI memory testers on HP0/HP3, clock
  census, ingest monitor with per-sample PRBS BER, CTRL_OUT event recorder, an
  always-responding register bridge with snapshot CDC.
- `bench/agent/` (`fbench-agent`, static ARM musl binary) and `bench/fbench/` (host CLI,
  Tier 0 + Tier 1 tests, analysis, safety interlocks for the PGA-102+ TX outputs).

---

## [2026-06-09] Live-glitch validation plan + bench TX replay tool

**Branch:** fishball-p25
**Bake required:** NO — docs + host tool only.

Fresh-eyes review session. Field facts reframed the decoding
roadblock: the PS software demod run live produced the SAME glitches
as the HDL chain, while clean offline decoding has only ever come from
libiio-path captures — the custom wideband DMA ring has never saved
stable traffic. Demod math (both sides) exonerated; prime suspect is
the DMA-ring transport (`DmaStreamRingWrite` → maia-kmod → PS
readers), which carries both the wideband IQ and the HDL dibits.

- New `doc/LIVE_GLITCH_VALIDATION_PLAN.md`: staged bisect with
  pass/fail gates. Stage A = decisive libiio-vs-DMA-ring A/B on the
  same air (no new build needed); Stage B = ring-layer bisect;
  Stages C/D/F (lifecycle, loops, audio) only if transport is
  exonerated. Production target: HDL control + traffic.
- New `tools/p25_bench_tx_replay.py`: replay wideband site captures
  through a second PlutoSDR (cabled + attenuated) so the full chain
  sees identical, ground-truth-known RF every run — deterministic
  glitch reproduction + per-build regression scoring.
- Also corrected the evidence record: the 2026-05-03 "30.9 %
  HDL-vs-SW divergence" compare was a chance-level artifact (78 %
  non-contiguous dibit loss fed to a contiguity-assuming aligner).

---

## [2026-05-03] On-device forensics + SDRTrunk halfband DDC port

**Branch:** fishball-p25
**BUILD_TAG:** `2026-05-03-on-device-forensics`
**Bake required:** NO — pure PS change.

Track-2 HDL-vs-SW forensics infrastructure. Two parallel deliverables:

**On-device forensics ring** (PS):

- New `app/forensics.rs`: lock-free dibit ring + auto-trigger on
  CallOpen / finalise on CallClose. Wideband IQ capture fires in
  parallel. Output dir per call under `/tmp/p25_forensics/run_<...>/`.
- Eliminates host-side polling losses (prior tool dropped ~78 % of
  dibits to HTTP latency spikes in the 426 ms ring window).
- API: `POST /api/forensics_arm` (with `auto_rearm`,
  `follow_encrypted` flags), `POST /api/forensics_disarm`,
  `GET /api/forensics_status`.
- `follow_encrypted=1` flag bypasses the grant_follower's encrypted
  rejection so encrypted calls are also captured for diff testing
  (audio garbled, dibits intact).
- Host companion: `tools/p25_forensics_pull.py` arms the device,
  watches status, scp's each new run dir + matching wideband.cs16.

**SDRTrunk halfband DDC port** (offline SW oracle):

- New `sw_demod/halfband_ddc.rs`: faithful Rust port of SDRTrunk's
  `HalfBandTunerChannelSource` filter chain (heterodyne mixer +
  power-of-2 halfband cascade, SDRTrunk's exact 11/15/23/63-tap
  Hamming/Blackman halfband filters).
- Wired as `SOFTDEC_HALFBAND=1` in `software_decode_tests.rs` (both
  single-freq and full-chain tests).
- Validated bit-exact with SDRTrunk's `.bits` reference on the
  2026-05-03 my_captures wideband: 98.7 % / 98.9 % alignment probe,
  100 % per-window agreement after the AGC/PLL settling transient.

Full detail in `doc/changes/052_on_device_forensics.md`.

---

## [2026-05-03] Seeds live + LoS + recording_saved + traffic IQ relinked

**Branch:** fishball-p25
**BUILD_TAG:** `2026-05-03-seeds-live`
**Bake required:** NO — pure PS change against the
`2026-05-03-seeding-bake` HDL.

Five PS-side changes that turn the bank-8 seed registers from 050 into
a live warm-start path, plus operator-UX fixes that landed alongside.
Full detail in `doc/changes/051_seeds_live.md`.

**Seeds live:**

- New `p25-httpd/src/app/seed_snapshot.rs` (192 lines + 3 unit tests):
  rolling-window median snapshot of converged AGC / Costas PLL /
  Gardner timing values, captured from the control chain only on
  ticks where `nid_event && nid_valid && sync_distance == 0 &&
  !bch_busy`. Commits after `MIN_CLEAN_SAMPLES = 6` clean snapshots
  (~1 s of clean signal).
- `IpCore::write_traffic_seeds(agc, pll, timing)` — writes all three
  bank-8 seed registers with a CDC read-back fence (essential
  because seeds and the reset Wpulse cross independent RegisterCDC
  instances).
- `retune_traffic_chain` extended with an `Option<(u32, i16, i32)>`
  seeds parameter. Grant follower path passes `Some(...)` from the
  shared snapshot; manual `/api/traffic` passes `None`.
- New `/api/system.converged_seeds` field surfaces the current seed
  status (armed/warmup, values, sample count, age).

**Loss-of-sync detector:**

- New `CallBoundaryKind::TrafficNidObserved` broadcast from the
  traffic-LSM heartbeat on every `nid_event` strobe.
- New `CloseReason::SyncLost` + `ActiveCall.last_nid_at_ms`. Tick
  closes with `SyncLost` when `now - last_nid_at_ms > 1500 ms`
  (~8 missed LDUs). Sits alongside the existing 10 s `Timeout`
  backstop. `tracing::info!` log line on each LoS close.

**recording_saved ws-event push:**

- `recorder::finalize` now broadcasts a `recording_saved` event with
  the full `RecordingEntry` payload after the WAV is written.
  Dashboard intercepts the event and triggers an immediate
  `refreshRecordings()` — eliminates the ~4 s gap between call close
  and Recent Calls row update (operator-flagged 2026-04-30).

**Plots page:**

- Narrowband spectrum dropdown label is chain-aware: ±31.25 kHz
  (control DDC at 62.5 kSPS) vs ±25 kHz (post-2026-05-03 dual-DDC
  traffic chain at 50 kSPS).
- `/api/spectrum?chain=traffic` was returning empty (stub from the
  2026-05-02 M2A delete that was never updated when the 2026-05-03
  dual-DDC pivot added the new traffic IQ ring). Wired up:
  `IpCore::traffic_iq_dma` + `read_traffic_iq_buffers()` +
  `DmaChannel::TrafficIq` + `/api/spectrum` chain=traffic dispatch.
  Tezuka DT already had the `p25-traffic-iq` carve-out (32 KB
  buffers @ 0x1c000000) so no kmod changes needed.

**Verification:** `cargo check` clean (179 pre-existing warnings,
zero new errors). `cargo test seed_snapshot::tests` passes 3/3.
On-target verification pending Tezuka cross-build + flash.

**Files touched:**

- `p25-httpd/src/app/seed_snapshot.rs` (NEW)
- `p25-httpd/src/app/mod.rs`
- `p25-httpd/src/app/grant_follower.rs`
- `p25-httpd/src/audio/mod.rs`
- `p25-httpd/src/audio/recorder.rs`
- `p25-httpd/src/main.rs`
- `p25-httpd/src/hardware/fpga.rs`
- `p25-httpd/src/httpd/mod.rs`
- `p25-httpd/src/httpd/api/system.rs`
- `p25-httpd/src/httpd/api/traffic.rs`
- `p25-httpd/src/httpd/api/debug.rs`
- `p25-httpd/src/httpd/dashboard.html`
- `doc/changes/051_seeds_live.md`

---

## [2026-05-03] Seeding bake — AGC / PLL / Gardner timing seed register bank

**Branch:** fishball-p25
**BUILD_TAG:** `2026-05-03-seeding-bake`
**Bake required:** YES — HDL `lsm_timing_interp.py` adds `timing_seed_in`;
new register bank 8 at 0x100 in `p25_top.py`; bitstream + SVD + svd2rust
regen all required.

Cold-start First-IMBE on a traffic-chain retune is 3–5 s today; the
2026-05-02 offline SW sweep showed the bit-exact decode reaches SDRTrunk's
85–90 % rate while the on-target HDL chain delivers ~57 %, gap dominated
by the per-retune acquisition transient. Dedicated seed register bank
gives the PS a write-once / pulse-many path to warm-start AGC, Costas
PLL, and Gardner timing on every retune.

The 047 bake added AGC + PLL `seed_in` ports to the demod modules but
the corresponding CSR fields were silently dropped during the
2026-05-03 dual-DDC pivot. This bake restores them in a dedicated bank
(cleaner than wedging seeds into bank 6's control / agc_config words)
and adds the new Gardner timing seed.

**HDL — seed input + register bank:**

- `LsmTimingInterp` (`p25_hdl/lsm_timing_interp.py`):
  - New `timing_seed_in` input (signed 18-bit Q5.12, matches
    `sample_point`).
  - Reset override now loads `Mux(seed != 0, seed, sample_point_init)`
    into `sample_point`. Mirrors the AGC/PLL warm-start pattern.
- `LsmDemodLoop` + `LsmDemod`: forward `timing_seed_in` through to the
  `LsmTimingInterp` submodule.
- `p25_top.py` — new register bank 8 at 0x100 (`lsm_seed_registers`):
  - `lsm_agc_seed[19:0]`            @ 0x100  (Q9.11 unsigned)
  - `lsm_pll_seed[15:0]`            @ 0x104  (Q2.13 signed)
  - `lsm_timing_seed[17:0]`         @ 0x108  (Q5.12 signed)
  - `traffic_lsm_agc_seed[19:0]`    @ 0x10C
  - `traffic_lsm_pll_seed[15:0]`    @ 0x110
  - `traffic_lsm_timing_seed[17:0]` @ 0x114
  - `lsm_seed_registers_cdc` (s_axi_lite → sync) + bank decode at
    `addr_bank == 0b1000`.
  - Wire all six register fields into `lsm_demod` /
    `traffic_lsm_demod` `agc_seed_in` / `pll_seed_in` /
    `timing_seed_in` ports.
- Address-map block comment updated; bank 8 is no longer "vacant".

**PS — out of scope this bake (next change):**

Seeds remain at zero (cold-start fallback) until the heartbeat snapshot
is added. Per the 2026-05-03 session memo, empirical seed targets:

- PLL bias: -13 ± 4 Hz uniform across active voice channels — single
  global cached value works for first PS revision.
- AGC: needs PTT-time sampling (whole-window medians corrupted by
  between-PTT noise).
- Gardner timing: separate sweep deferred; `timing_seed = 0` ships in
  this bake (HDL falls back to cold-start init when seed is zero).

**CDC ordering — read-back fence required on PS:**

The seed registers and the `lsm_reset` / `traffic_lsm_reset` Wpulse
cross **independent** RegisterCDC instances (bank 8 vs bank 5/6). PS
retune sequence MUST round-trip a read after the seed writes to fence
the CDC before pulsing reset, otherwise the reset can fire before the
seed values cross and the demod will latch the previous value.

**Tests:**

`maia-hdl/test/test_lsm_timing_interp.py` — three new cases for the
seed reset behaviour (zero → cold-start init, non-zero → override,
negative-value sign round-trip). All 7 tests in the file pass.

**Files touched:**

- `maia-hdl/p25_hdl/lsm_timing_interp.py`
- `maia-hdl/p25_hdl/lsm_demod_loop.py`
- `maia-hdl/p25_hdl/lsm_demod.py`
- `maia-hdl/p25_hdl/p25_top.py`
- `maia-hdl/test/test_lsm_timing_interp.py`
- `p25-httpd/src/main.rs` (BUILD_TAG bump only)
- `doc/changes/050_seeding_bake.md`

---

## [2026-04-29] PS perf + scanner pivot — `/api/ps_cores`, `/ws/audio` close-detect, JMBE recurrence, AGC dbg fix

**Branch:** fishball-p25
**BUILD_TAG:** `2026-04-29-jmbe-cos-recurrence`
**Bake required:** YES — HDL `lsm_agc.py` reset block update + bitstream rebuild for `gain_dbg` mirror.

Strategic pivot mid-session: dropped multi-traffic-channel goal, refocused
on a highly optimized single-chain scanner. See
`doc/changes/049_ps_perf_and_scanner_pivot.md` for the full session arc
and `doc/diagnostics/2026-04-29/SESSION_LOG.md` for the chronological log.

**PS — new endpoint + jitter fixes:**

- `GET /api/ps_cores?interval_ms=&top_n=` — per-core CPU% + per-thread CPU%
  over a configurable window (default 250 ms). Two `/proc` reads, no state
  plumbing. Dashboard System Health gains a "PS Cores" card (2 s poll, only
  on Radio tab) showing per-core busy bars + top threads.
- `/ws/audio` handler now drives socket recv concurrently with audio
  broadcast. Old loop blocked on `rx.recv().await` and only noticed dead
  sockets when the next chunk failed to send — caused ghost subscribers
  during idle (operator observation: dashboard reported 3 audio WS
  clients with one real listener).

**JMBE optimization — cos/sin recurrence on the synthesis hot path:**

- Restructured `get_voiced` from outer-n / inner-li (~16 k libm `cos`
  calls per 20 ms frame) to outer-li / inner-n with the trig recurrence
  `cos(θ + Δ) = cos(θ)·cos(Δ) − sin(θ)·sin(Δ)`. Linear-phase branches
  (Algs #131/#132/#133) now use the recurrence, ~10× speedup on the
  inner loop. Quadratic-phase branch (Alg #136, ~14 % of harmonics)
  retained direct cos for now.
- New `accumulate_linear_window` helper does 2 transcendentals per
  harmonic-term (initial `sin_cos` for base + step) plus 4-mul-1-add-1-sub
  per sample.
- Hoisted `synthesis_window(n)` and `synthesis_window(n - SPF)` table
  lookups outside the harmonic loop into precomputed 160-element arrays.
- Bit-equivalence reference test (`test_synthesis_signature_stable`)
  added with values captured from real IMBE frames pulled from
  `/api/imbe_dump` during a live TG 301 call. Tolerance: 1 % relative
  on RMS/peak, 2 % on per-sample, 2e-5 absolute floor — accommodates
  ULP drift over 160-sample recurrence iterations.

Expected on-target: vocoder thread drops from ~100 % of one core during
a call to ~25-30 %, freeing core 1 for channelizer/scanner experiments.

**HDL — AGC seed diagnostic fix:**

- `maia-hdl/p25_hdl/lsm_agc.py:725-727` — reset block previously clobbered
  `gain_dbg.eq(0)` in the same cycle it loaded the gain register from
  `seed_in`, so the PS-side seed-load diagnostic structurally always read
  0 (pre/post drift = -agc_seed_written every retune). Data path was fine
  the whole time — only the readback was broken. Reset block now mirrors
  the seeded value into `gain_dbg` (Q9.7 truncation of seed, or
  GAIN_INIT >> 4 when seed=0). After flash, `traffic_agc_post_reset`
  matches `agc_seed_written` and `agc_drift` becomes a real diagnostic.

**Tooling — settle measurement:**

- `tools/p25_settle_measure.py` — anchors on `/api/log` retune/nco_skip
  events with seed diagnostic, times to next `voice` TRF_HDU/LDU1/LDU2
  event. Heartbeat mode polls `/api/traffic_lsm_dibit_dump` for
  sync.hits + nid_attempts deltas to isolate which stage owns the
  settle budget. Setup flags `--lock-freq` (chain park) and `--agc-off`
  (disable per-symbol AGC), both with auto-restore on exit. CSV row
  per retune + summary stats (median / p90 first-frame latency, cache
  hit rate, drift sanity).

**Findings worth documenting:**

- 3.4 s cold settle floor on retunes (n=33, median=3276 ms, p90=3444 ms).
  Bimodal: same-chain follow-on = 50 ms, cold = 3.4 s. Frame-sync
  acquisition (2-3 LDU periods × 1.35 s) is the dominant component;
  PLL + AGC seeding only saves ~500 ms of front porch.
- mosquitto + api_controller from legacy Tezuka DATV stack were the
  startup-CPU spike for ~5 min post-boot. Removed in `tezuka_fw` build
  via new `post-build-p25.sh`. Chronic <1 % overhead, but exposed as
  unnecessary for p25 builds.
- Vocoder pegging core 1 was normal-mode JMBE cost on no-NEON-tuned
  decode, not a starvation symptom — core 0 was 96 % idle during the
  same window. Recurrence shipped above addresses it.

## [2026-04-26] not_followed grants no longer preempt the active session

**Branch:** fishball-p25
**BUILD_TAG:** `2026-04-26-not-followed-no-preempt`
**Bake required:** NO -- PS only.

Post-flash bug observed: TG 300 (clear) on channel 1193 active, then a
TG 402 (encrypted) primary GRANT arrives also on channel 1193. The
session-lifecycle refactor's preempt-on-TG-change rule fired, closed
the TG 300 session, and opened TG 402 with `not_followed=encrypted`.
But the follower DID NOT retune the chain (per not_followed). The
chain stayed on 858.4625, kept decoding the continued TG 300
transmissions, and the new lifecycle attributed 459 IMBE / 73440 PCM
samples / 3 distinct sources to the encrypted TG 402 entry. User
heard the audio play live (vocoder produced PCM because the encrypted
atomic was never set — grant-was-not-followed shortcut bypassed it),
but no recording landed because the session was tagged encrypted.

Fix: `CcGrantArrival` handler now treats `not_followed=Some(...)`
as "ignore for active session" — the chain isn't retuning, so the
lifecycle shouldn't change either. To preserve the operator's
"show every CC GRANT in Recent Calls" rule, a synthetic
`CallOpen+CallClose` pair fires for the not-followed grant so
`grant_stats` still records it (recorder filters not_followed
CallOpens, so no recording).

Also: discovered that Buildroot caches per-package build state across
runs. After a build failure at `target-finalize`, a subsequent run
sees `p25-httpd` as already-installed and skips it, producing a
firmware image with the *previous* run's binary. To pick up
p25-httpd source changes after a partial build, run
`make p25-httpd-dirclean` (or equivalent) before re-running the
build. This is now in
[Build cache gotcha memory](memory/feedback_buildroot_pkg_cache.md).

---

## [2026-04-26] Session lifecycle refactor — capture-time routing + GRANT-driven open/close + multi-speaker bundling

**Branch:** fishball-p25
**BUILD_TAG:** `2026-04-26-session-lifecycle-refactor`
**Bake required:** NO -- PS only.

Post-flash diagnostics on `2026-04-26-audio-driven-speakerend` showed
`recorder_chunks_dropped_no_active = 1980` over a 30-min run — about 40 s
of audio that played live but never landed in any recording (capture
rate 18 % of vocoder output). Operator-clarified spec:

- Primary `GRP_VCH_GRANT` opens a session.
- First audio block starts the audio; last block ends it.
- Grant tolerates audio gaps; closes only on TG change or 5 s timeout.
- TDULC terminators (MOT, CALL_TERM) and bare TDU never close — they fire
  multiple times per multi-PTT grant and dispatchers don't emit MOT at
  all, so terminator-driven close fragments real continuous sessions.

Implementation:

- `AudioChunk.captured_at_ms` stamped at LDU dispatch in
  `ImbeForwarder::forward_frames`. Plumbed through
  `(tg, src, call_id, captured_at_ms, frames)` tuple → vocoder →
  broadcast → recorder. Decouples routing from vocoder/queue lag.
- Recorder routes by `captured_at_ms ∈ [active.open_at_ms,
  active.close_at_ms or u64::MAX]`. Late chunks for closed sessions
  still land in the right WAV regardless of vocoder lag.
- Closing-state drain owned by recorder: `CallClose` stamps
  `close_at_ms`, periodic tick finalises after `CLOSING_DRAIN_MS = 2 s`.
  Replaced the inline `audio_rx` drain loop.
- `grant_follower` rewritten:
  - Constants reduced to `HARD_TIMEOUT_MS = 5_000` and `TIMEOUT_TICK_MS = 100`.
  - `ArrivalDisposition` ∈ `{Bundle, Ignore, TgChange}`.
  - `CloseReason` ∈ `{Timeout, TgChange, StreamLag}` (was 5).
  - `ActiveCall.sources_observed: Vec<u32>` accumulates every distinct
    SRC seen (CC GRANT, LDU1 LC, TDULC MOT BY:).
  - `SpeakerEnd { kind }` boundaries are source-stamp-only.
  - HDU never splits.
- `audio::TerminatorKind` enum (`BareTdu | MotTalkComplete | CallTermination`)
  added so emission sites tag which terminator fired (lifecycle ignores
  the kind for now; available for future heuristics).
- Lag instrumentation: `RecordingEntry.max_chunk_lag_ms` /
  `mean_chunk_lag_ms` per recording (`now_ms - chunk.captured_at_ms`).
- `RecordingEntry.sources_observed` and `GrantDecodeSummary.sources_observed`
  surfaced through APIs; dashboard renders comma-separated list when
  multiple speakers landed in one bundled grant.

Validation: 64 Rust tests green; cargo check clean.

doc/changes/048_session_lifecycle_refactor.md.

---

## [2026-04-26] Traffic LSM PLL + AGC seeding from control chain

**Branch:** fishball-p25
**BUILD_TAG:** `2026-04-26-traffic-pll-agc-seeding`
**Bake required:** YES -- p25_top register-bank field positions changed.

First step of the channelizer redesign (Option D from
`doc/diagnostics/2026-04-25/CHANNELIZER_REDESIGN.md`). The on-target
problem was 700-3400 ms between traffic-chain retune and first IMBE,
mostly Costas PLL cold-acquire on a freshly-tuned channel. SDRTrunk
hits ~0 ms because per-channel decoders stay always-locked. We can't
match that with a single FPGA chain, but the operator's PPM-sweep
evidence shows the converged Costas value is the same on every channel
(crystal trim is the dominant carrier-error source). So we copy the
control chain's already-converged PLL accumulator + AGC gain into the
traffic chain on every retune.

HDL: `LsmPllUpdate` / `LsmPllUpdateLinearised` / `LsmAgc` gain
`seed_in` ports that load on `reset_in` (zero = legacy cold start).
`LsmDemodLoop` + `LsmDemod` plumb the seeds through. `p25_top.py` adds
two RW fields: `traffic_pll_seed[20:5]` on `traffic_lsm_control`
(Q2.13 signed) and `traffic_agc_seed[31:16]` on `traffic_lsm_agc_config`
(Q9.7 unsigned, FPGA pads to Q9.11 internally). Control chain unchanged
-- it stays cold-start so it tracks whatever's actually on the control
frequency.

PS: `fpga.rs::set_traffic_lsm_seeds` writes both fields; `retune_traffic_chain`
reads `lsm_debug.pll_dbg` + `lsm_agc_debug.agc_gain_dbg`, calls the
setter, then pulses `traffic_lsm_reset` (which latches the seeds in
the same cycle as the reset). Q9.7-truncated AGC seed is fine -- AGC
re-corrects within ~10 symbols, well inside the < 50 ms target.

Validation: 30 HDL tests + 64 Rust tests green, P25Core elaborates +
SVD round-trips. Field validation = `first_imbe_ms` per call after
flash.

See `doc/changes/047_traffic_pll_agc_seeding.md`.

---

## [2026-04-25] Phase 2c-2h -- unified call lifecycle

**Branch:** fishball-p25
**BUILD_TAG:** `2026-04-25-phase2c-2h-call-lifecycle`
**Commits (6):** `8e98d5b` (2e), `894e84b` (2c+2d), `b960e0e` (2f), `d32928e` (2j defer), `170e2af` (2g), `c47c07f` (2h), plus this wrap-up.
**Bake required:** no -- p25-httpd source only. Tezuka rebuild + flash.

Six-phase arc that consolidates the P25 call-lifecycle authority into a single `app::grant_follower` module owning routing, lifecycle, and chain control, and replaces the recorder's tg+source heuristic with explicit `call_id` propagation through the audio path.

### Phase 2e -- delete decoder grant HashMap

Drops `ControlChannelDecoder.grants: HashMap<u16, GrantInfo>`, `expire_grants(...)`, and `take_other_grants_for_talkgroup(...)`. The dashboard's Active Grants panel had a 30 s zombie-grant problem because grants only expired on a 30 s sweep; calls that had ended (TDU / chain Idle) sat in the panel for tens of seconds. Reroute `/api/grants`, `/api/stats.active_grants`, and `/api/decoder_compare` to read from a new `ActiveCallShared` mirror populated by the call_tracker authority. Empty when idle, single entry when active. Deletes the two `expire_grants(30)` periodic spawn tasks in `main.rs` and the `dec.grants.retain(...)` block in the follower's Idle/timeout handler. Removes `PreservedGrantFields` (no remaining caller). Three control_channel grant tests retired -- equivalent semantics now live in the lifecycle layer.

### Phase 2c -- TrafficManager timer retirement

Pre-2c the TrafficManager and CallTracker were parallel lifecycle judges with different timeouts (TM 2 s + post-TDU 2 s; tracker 10 s) and different TDU-handling rules. Drops `call_timeout_ms`, `post_tdu_hold_ms`, `post_tdu_hold_until` fields and `note_activity` / `tdu_received` / `check_timeouts` / `post_tdu_hold_remaining_ms` methods. `force_idle` becomes the only release path -- driven by the grant follower's `CallTrackerEvent::CallClose` subscription. The 200 ms `timeout_tick` poller in the follower is replaced with a `tracker_rx.recv()` arm. TDU/TDU_LC NIDs from the LSM heartbeat no longer dispatch to TM; lifecycle TDUs flow exclusively through the LSM voice handler -> CallBoundary::SpeakerEnd -> CallTracker.

### Phase 2d -- shrink call_tracker timeout

Tighten `CALL_TIMEOUT_MS` from 10000 to 2000 to match SDRTrunk's `STALE_EVENT_THRESHOLD_MS = 2000`. Recent Calls panel updates within 2 s of call end instead of 10 s. Affects only the no-explicit-close fallback; CC-driven and TDU-driven closes still fire promptly via CallBoundary.

### Phase 2f -- TrafficChain NCO write-skip

When the chain releases (Idle) and the next grant lands on a frequency that happens to equal the currently-loaded NCO word, skip the FPGA write and promote directly to Active. Common pattern on busy sites with a small voice-channel pool: back-to-back calls on the same TG (or different TGs that the trunking system reassigns to the same freq) reuse the chain's existing tuning. Adds `nco_skips: u64` counter surfaced via `/api/traffic`.

### Phase 2j -- LDU1 LC parser fix DEFERRED

Originally scoped as mandatory before Phase 2g per `CALL_LIFECYCLE_IMPLEMENTATION.md`. Operator decision 2026-04-25: skip in this session. Code review of `parse_ldu1_source` / `parse_ldu1_lcw` / `hamming10_correct` / `rs_24_12_13` didn't surface an obvious bit-offset / hexbit-position bug. Without an on-target LDU1 capture where SDRTrunk decoded a clean `FM:<rid>`, blind fixes risk regression. Operator's working hypothesis: traffic decode failures (garbage RIDs, intermittent gaps) more likely caused by upstream PLL offset > 250 Hz than by a parser bug -- field measurement against SDRTrunk on the same site planned next. Phase 2g design adjusted to keep the deferral safe: the proposed `observed_sources: Vec<u32>` field is NOT introduced; existing `actual_speaker: Option<u32>` continues to capture LDU1 LC voted consensus internally. CC `GRP_VCH_GRANT.SRC` stays authoritative.

### Phase 2g -- module reorg into clear layer boundaries

Operator intent: file structure should reflect the three architectural layers -- ControlChannel (CC dibits -> grant events), TrafficChain (traffic dibits -> frame events), GrantFollower (owns the active grant: routing + lifecycle + IMBE forwarder atomics + recorder coordination + source tracking).

Renames:

- `protocol/p25/traffic_manager.rs` -> `traffic_chain.rs`. Struct `TrafficManager` -> `TrafficChain`.
- `protocol/p25/traffic_manager_tests.rs` -> `traffic_chain_tests.rs`.
- `app/follower.rs` + `app/call_tracker.rs` -> `app/grant_follower.rs`. Single file owning grant following + lifecycle authority. Cross-platform types (`CallTrackerEvent`, `CloseReason`, `OpenReason`, `SourceUpdateVia`, `ActiveCallSnapshot`, `ActiveCallShared`, `new_event_tx`, `new_active_call_shared`) live at the top with no cfg gating. `spawn_call_lifecycle` (renamed from `spawn_call_tracker`) is portable. `spawn_grant_follower` (renamed from `spawn_traffic_grant_follower`) is `cfg(target_os = "linux")` -- lives in a `mod routing` block inside `grant_follower.rs` and is re-exported at module level. File-level `#![cfg(target_os = "linux")]` removed; cfg now applied per-item so the lifecycle types stay portable.

Behavior preserved: `spawn_call_lifecycle` body unchanged from prior `spawn_call_tracker`. `spawn_grant_follower` body unchanged from prior `spawn_traffic_grant_follower`. TrafficChain method signatures and field shapes unchanged from the post-2c-2f TrafficManager. `/api/traffic` JSON shape unchanged.

### Phase 2h -- call_id through the audio path

The recorder used to route AudioChunks by tg+source heuristic with a "source-match gate" guarding against in-flight PCM from a just-closed call landing in the new call's WAV. Phase 2h replaces the heuristic with explicit `call_id` propagation: every IMBE batch is stamped with the GrantFollower call_id active at submit time, the vocoder propagates it onto every emitted AudioChunk, and the recorder routes by `chunk.call_id` directly. Cross-call PCM bleed becomes structurally impossible.

- `audio/mod.rs`: `AudioChunk` gains `call_id: u64`.
- `app/imbe_forwarder.rs`: new `current_call_id: AtomicU64` field. `forward_frames` reads it and includes in the `imbe_tx` tuple, which becomes `(tg, src, call_id, frames)`.
- `app/vocoder_task.rs`: receives 4-tuple, stamps `batch_call_id` onto every emitted AudioChunk.
- `app/grant_follower.rs`: `mirror_active` now also writes `forwarder.current_call_id` (the active call's id, or 0 when idle) on every state mutation.
- `audio/recorder.rs`: replaces tg-mismatch + source-match gates with `chunk.call_id != 0 && chunk.call_id != c.call_id` drop. Trailing-PCM drain in CallClose matches by `call_id` too.

### Notes

- BCH t=4 tightening (HDL `lsm_nid_bch_fec.py`) was already in the Phase 2b checkpoint commit `b9cab02`; surfaces here only because the bake includes that bitstream.
- `cargo check` + `cargo test` green on Windows host between every phase (64 tests pass).
- Linux build / on-target observation deferred to operator's `build.bat --p25` + flash step.

---

## [2026-04-18] Phase 10.6 -- post-LSM matched-filter IQ taps + bank widening

**Branch:** fishball-p25
**BUILD_TAG:** `2026-04-18-phase10.6-post-lsm-iq`
**Commits (2):** `1bf4fc7` (HDL + PAC), `30f18b8` (PS + dashboard)
**Bake required:** yes — bitstream rebuild + Tezuka firmware rebuild. Tezuka DT change lives in `tezuka_fw` `fishball-dev` commit `629def8`.

Adds two new DMA rings to the P25 core that tap the matched-filter output of each LSM chain (post-RRC, 31.25 kSPS, before timing-recovery / PLL rotation). Dashboard eye plot defaults to this new "matched-filter" source, giving clean decision-crossing eye geometry instead of the raw post-DDC sinusoid.

### HDL (`1bf4fc7`)

- **Address space widened** `axi4_awidth` 7 → 8 bits; bank select `[5:3]` → `[6:3]` (3-bit → 4-bit, 8 → 16 banks max). All existing bank selects rewritten as 4-bit constants; Maia core layout unchanged.
- **Two new IQ taps** sourced from `lsm_rrc.re_out / im_out` (control) and `traffic_lsm_rrc.re_out / im_out` (traffic). New `IQPacker` + `DmaStreamRingWrite` instances, 256 KB rings (8 × 32 KB sub-buffers) at physical addresses `0x1D00_0000` / `0x1E00_0000`.
- **Two new register banks** at bank-base `0x100` / `0x120`: `lsm_iq_{dma_status, dma_control, next_address}` + traffic-side mirror. Matches existing `iq_registers` layout byte-for-byte so the PS reader logic reuses without changes.
- **Two new interrupt bits** (`lsm_iq_dma`, `traffic_lsm_iq_dma`) in the control `interrupts` register with `PulseSynchronizer` CDC into the s_axi_lite domain, same pattern as the existing six DMA IRQ chains.
- **SVD + PAC regen**: `p25.svd` via `generate_p25_svd.py`; `p25-httpd/p25-pac/src/lib.rs` via svd2rust 0.33.5 with the `unknown_lints` + `mismatched_lifetime_syntaxes` preamble preserved.

### PS + dashboard (`30f18b8`)

- **`fpga.rs`**: new `RxBuffer` fields `lsm_iq_dma` + `traffic_lsm_iq_dma` opened via the UIO devices `p25-lsm-iq` / `p25-traffic-lsm-iq`. Matching `LsmIq` + `TrafficLsmIq` `DmaChannel` variants, dispatch arms, reader methods (`read_lsm_iq_buffers`, `read_traffic_lsm_iq_buffers`), and enable setters. Both rings armed at boot from `main.rs`.
- **`/ws/iq` gets a `source=post_ddc|post_lsm` query param**. `post_ddc` is the default (backwards-compatible). Hello frame now echoes both `source` and the actual `sample_rate_hz` (62500 for post_ddc, 31250 for post_lsm) so clients can size ring buffers correctly.
- **Dashboard eye plot**: new "Src" dropdown defaulting to Post-LSM (MF). `IqStream.connect()` takes an optional 4th `source` argument (existing spectrum Live-IQ callers unchanged). `onEyeFrame` pulls `EYE.sps` from the hello-announced rate so source switches are drift-proof.

### Deferred to the next bake

Four items were planned for this bake but carved out to keep the payload observation-only (zero risk of detuning SDRTrunk-matched constants):

- Runtime-writable TED gain / PLL loop BW (needs `LsmDemod` internal refactor + individual A/B validation per param).
- Runtime-writable DC blocker alpha / AGC attack rate (smaller scope).
- Signal-quality telemetry register (DC offset leaky-integrator readback, RMS min/max window, `stats_reset` W1P).
- Pre-DDC wideband IQ tap at 8 MSPS (separate project, distinct DMA question).

Bank widening opened the address-space room for all of these — they're additive-only to the new 16-bank layout.

---

## [2026-04-17] Stage 1 + Stage 2 + Stage 3 -- bake-blocking fixes + API-first refactor + dead-code sweep

**Branch:** fishball-p25
**BUILD_TAG:** `2026-04-17-stage3-fix-linux-build`
**Commits (5):** `db59a01` (Stages 1+2), `cad25b8` (Stage 3 first pass), `d403375` (Linux hotfix), `6fc2554` (dead-code part 1), `31a7e80` (dead-code part 2)
**Rebuild scope:** p25-httpd only (no bitstream change)

Multi-stage cleanup session grounded in the 2026-04-16 code review. Net +1,500 / -1,400 lines; zero behavioural change on the radio. Linux build verified via Tezuka.

### Stage 1 -- bake-blocking fixes

- **Dashboard XSS**: new `escHtml()` helper; 5 `innerHTML` interpolation sites (grants alias, event timestamp/type/summary/alias) now escape properly. LAN-exposed dashboard no longer accepts raw HTML from the alias PUT endpoint.
- **AGC readback parity**: `*_lsm_control_readback()` returns 4-tuple with `agc_enabled`; boot-time tracing + `/api/{control,traffic}_lsm_control` JSON updated.
- **`tools/p25_check.py`**: fixed `NameError` on `lsm_running` / `hdl_pct` in the acceptance summary; phase-tag regex loosened from `phase[67]` to `phase\d+|p\d+prep` so Phase 10+ is recognised.
- **`build_fpga.bat`**: IP hint clarified — `192.168.2.1` (RNDIS-USB, primary) with `192.168.120.50` (Ethernet) noted as alternative.
- **`CLAUDE.md` + `README.md`**: fixed references pointing to `doc/DEVPLAN.md` / `doc/BUILD_FPGA.md` (actual location: repo root).
- **`p25-pac` lints**: added `#![allow(unknown_lints)]` + `#![allow(mismatched_lifetime_syntaxes)]` at the svd2rust-generated crate root. Correct fix for auto-generated code.

### Stage 2 -- API-first refactor (Android-app prep)

- **Dashboard HTML extraction**: `DASHBOARD_HTML` moved from 2,781-line inline constant to sibling `src/httpd/dashboard.html` via `include_str!`. `httpd/mod.rs` 6,725 → 3,945 lines.
- **Handler split into 9 consumer-facing modules** under `src/httpd/api/`: `system`, `radio`, `traffic`, `talkgroups`, `history`, `tuning`, `chain`, `debug`, `ws`. Each maps to a logical Android-app screen. `httpd/mod.rs` 3,945 → 320 lines.
- **New `/api/sys_health`** endpoint: loadavg, daemon RSS, thread count, free memory. Cheap to poll from a mobile client; lets headless consumers distinguish "board alive but CPU-starved" from "board alive and healthy" without SSH.
- **WebSocket hardening**:
  - `/ws/events`: on broadcast-channel `Lagged`, sends synthetic `{"event_type":"ws_lag"}` control frame instead of closing the connection. Clients stay connected across slow-consumer events.
  - `/ws/audio`: on `Lagged`, sends `{"type":"lag","skipped":N}` text control frame so clients can flush their jitter buffer.
  - Dashboard `/ws/events` reconnect: flat 3 s → exponential backoff (1 s → 15 s ceiling, resets on first healthy message).
- **`doc/P25_API.md`** catalogue expanded from 20 to all 42 endpoints, grouped by `api/` module.
- **`doc/API_CONSUMERS.md`** created: governance contract ("one API, all consumers equal, no private backchannels"). Rules for adding endpoints + adding consumers.

### Stage 3 -- section-by-section cleanup (-868 lines of dead code)

Linux-truth-verified via Tezuka build. Windows cargo check flagged 79 dead-code warnings; Linux showed 34 real ones — the 45-warning gap was all `#[cfg(target_os = "linux")]`-gated false positives.

**Deleted this session:**

- **`src/vocoder/mod.rs`** (592 → 141 lines, -76%): removed the mbelib C-FFI `ImbeDecoder` fallback wrapper. JMBE pure-Rust (`vocoder::JmbeDecoder` wrapping `jmbe::ImbeDecoder`) has been the sole live vocoder for weeks. Kept `mbelib-sys` crate only for its `SAMPLES_PER_FRAME = 160` constant.
- **`src/jmbe/mod.rs`**: 3 dead helpers (`LOG_2` const, `requires_adaptive_smoothing`, `WhiteNoiseGenerator::next_buffer`).
- **`src/fpga.rs`**: 8 dead register-readback methods kept "for future symmetry" (`traffic_last_buffer`, `traffic_next_address`, `traffic_iq_*`, `pulse_lsm_reset`, `waiter_iq_dma`, `waiter_traffic_iq_dma`).
- **`src/p25/control_channel.rs`**: `process_directed_tsdu` (~150 lines, retired Phase 6F.9 soft-sync TSDU entry point).
- **`src/p25/fec.rs`**: `GolayDecoder` struct + `decode_nid` wrapper + the earlier `decode`/`syndrome`/`parity_of_bit` helpers. The live NID decoder is `lsm::nid_fec::decode_nid` (direct); 4 tests rewritten to use it.
- **`src/p25/tsbk.rs`**: 7 parser-side items (`data_access_control`, `micro_slots`, `protected`, `crc`, `sign_extend`, `crc16_ccitt`, `channel_uplink_frequency`) + tests. Also 3 unused service-options constants (`DUPLEX_FLAG`, `SESSION_MODE_FLAG`, `PRIORITY_MASK`).
- **Small items**: `AudioChunk.seq` field, `EventLog::len()`, `MonitorList::priority_of()`, `GrantEvent.timestamp` field.

**Linux hotfix (`d403375`)**: Windows `cargo fix` renamed `grant_event_rx` → `_grant_event_rx` and stripped `mut`. The binding is consumed via `.recv()` inside a `cfg(target_os = "linux")` block — Linux build broke. Restored with `#[allow(unused_variables, unused_mut)]` and a comment warning future cargo-fix passes off the rewrite.

**Docstring pass**: `httpd/mod.rs` architecture overview + `api/mod.rs` handler-addition checklist + 10–20-line consumer-facing docstrings on all 9 `api/*.rs` submodules.

**Should-fix items applied**:

- Recording-list fingerprint by `(id, started_unix_ms)` instead of `(id, size_bytes)` — in-progress recordings no longer tear down `<audio>` elements mid-playback every 5 s.
- `tools/p25_nid_analyze.py`: opaque `(1<<63)-1 | (1<<63)` → explicit `0xFFFF_FFFF_FFFF_FFFF`.
- `/api/spectrum` catalogue entry now documents the `?fft=` param.

### Verification status

- Windows `cargo check`: clean compile, ~24 remaining warnings (all cfg-gated false positives).
- Linux `cargo check` via Tezuka build: clean compile, ~24 remaining dead-code warnings all from the small-items deferred list.
- **On-target**: pending. Next bake should verify XSS fix + AGC readback + `/api/sys_health` + WebSocket Lagged handling end-to-end.

---

## [2026-04-16] Phase 10 follow-up 2 -- tab-gated polling + audio re-prime + C4FM dashboard retirement

**Branch:** fishball-p25
**BUILD_TAG:** `2026-04-16-p10prep-tab-gating-audio-reprime-c4fm-retire`
**Rebuild scope:** p25-httpd only (no bitstream change)

Three UI-layer fixes motivated by on-target observation that the
live `/ws/audio` stream stuttered mid-call while saved WAV playback
sounded clean. Root cause: dashboard polling was starving the
WebSocket audio sender on the Zynq-7020 ARM. `refresh()` was
hitting 10 HTTP endpoints every 2 s unconditionally, plus
constellation + spectrum + log-tail pollers running regardless of
which tab was visible.

### Tab-gated dashboard polling

- **`refresh()` split by tab**: the 10 `fetchJson` calls are now
  grouped into an always-on block (`/api/system`, `/api/stats`),
  a Debug-tab-only block (`/api/decoder_compare`, `/api/hdl_lsm`,
  `/api/irq_stats`, `/api/control_lsm_dibit_dump`), and a
  Radio-tab-only block (`/api/traffic`, `/api/grants`,
  `/api/bands`). On Radio, that drops the per-cycle fetch count
  from 10 to 5. On Debug, same 6. On Logs / API tabs, only 2.
- **`refreshRecordings`, `refreshModulation`, `refreshMonitorTgs`**
  early-return when `activeTab() !== 'radio'`. Their target cards
  are Radio-tab exclusive.
- **`logPoll` cadence** switches between 1 s (when Logs tab is
  active so the tail reads live) and 5 s (elsewhere, just keeping
  the unread-count badge current).
- **`scheduleSpectrum` / `scheduleConstellation` timers** now
  stop entirely on switch-away from Debug (previously their
  bodies had a `pane.style.display === 'none'` early-return that
  never matched the `classList.toggle('active')` tab model, so
  the 1 Hz /api/spectrum + /api/constellation fetches kept
  running on every tab).
- **`switchTab`** now kicks the Radio-tab refreshers (`refresh`,
  `refreshRecordings`, `refreshModulation`, `refreshMonitorTgs`)
  on entry so the user sees fresh data immediately instead of
  waiting for the next 2 s tick.
- **`/api/grant_map` + `/api/monitor`** only fetched on Radio.

### AudioWorklet re-prime on sustained underrun

Previously the worklet absorbed every underrun as a single silence
sample and kept playing as soon as any data arrived. When
/ws/audio delivery stalled for ~100 ms (dashboard polling
starvation, LDU spacing jitter, etc.), the worklet would play
micro-bursts of audio interleaved with silence -- the
"stuttering 4x/sec" mode the user heard mid-call.

Fix in both the `P25AudioProcessor` worklet path and the
`AUDIO_SPN` ScriptProcessorNode fallback: after
`UNDERRUN_REPRIME = 4800` consecutive per-sample underruns
(~100 ms at a typical 48 kHz AudioContext), re-enter the
`priming` state and hold silence until the ring refills to
`PREFILL = 4320` samples. Resumes playback cleanly from a full
buffer instead of stuttering the next burst.

### C4FM dashboard retirement

The HDL LSM chain decodes both C4FM and LSM sites (validated on
Clay County NAC 0x8A1, Duval County 0x3BA, and FP&L's C4FM site).
The PS C4FM pipeline is dormant everywhere. Dashboard content
retired:

- **"PS C4FM Dibit Stream" card** on Debug tab (with histogram /
  sync correlator / raw-DUID sections). The companion
  `/api/dibit_dump` fetch is gone from `refresh()`.
- **"PS C4FM" column** in the Decoder Comparison Matrix. Matrix
  is now 2-column (PS LSM framer | PL HDL LSM gateware).
- **C4FM HDL dibit/overflow rows** in the Decode Stats card
  (kept the hidden `#dibits` / `#overflow` elements so the
  existing refresh() logic doesn't error out, marked for deletion
  in a future cleanup).

Backend code (PS C4FM decoder, HDL c4fm_demod, C4FM dibit DMA
ring) is **unchanged**. Per memory
`project_c4fm_stack_cleanup_todo` the full retirement waits for
LSM-decodes-C4FM confirmation on ≥3 sites. This is dashboard-only.

### Deploy

No HDL change, no PAC change. Re-flash p25-httpd binary only.

---

## [2026-04-16] Phase 10 follow-up -- WAV Range support + TG Monitor UI

**Branch:** fishball-p25
**BUILD_TAG:** `2026-04-16-p10prep-agc-gate-wavrange-tgmonitor-ui`
**Rebuild scope:** p25-httpd only (no bitstream change)

Two UI-layer fixes surfaced after the AGC-gate bake landed:

1. **WAV recording playback stuttered in `<audio>` element** -- downloaded
   files played fine, but clicking Play in the dashboard would start,
   stop, restart repeatedly. Root cause: `get_recording_file` returned
   `200 OK` with the full body and no `Accept-Ranges` header; HTML5
   `<audio>` issues `Range: bytes=0-` probes to test for seek support
   and was re-interpreting each re-sent full body as a stream restart.
   Fix in [httpd/mod.rs:504-640](p25-httpd/src/httpd/mod.rs#L504-L640):
   parse `Range: bytes=<start>-<end?>` request header, emit
   `206 Partial Content` with `Content-Range` / `Content-Length` /
   `Accept-Ranges: bytes` for valid ranges, fall back to full `200 OK`
   for unranged or malformed requests. Single-range only (no
   multipart) since that covers every browser we care about.

2. **TG selector missing from the dashboard.** The prior change added
   `/api/grant_map` + relied on the existing `/api/monitor` for the
   scanner-mode gate, but never added a dashboard widget. Added a
   new "TG Monitor" card in the Radio tab above Frequency Map:
   checkbox grid populated from `/api/grant_map` (every TG seen on
   this site, with clear/encrypted grant counts shown), Apply /
   Clear all / Refresh roster buttons, "hide encrypted-only TGs"
   toggle. State machine separates `active` (what the follower
   currently filters on) from `staged` (what the user has checked);
   shows an "unsaved changes" hint when they diverge. Apply posts
   a JSON body to `PUT /api/monitor` and syncs both states from the
   server response. Auto-refreshes the roster every 10 s so newly-
   discovered TGs show up without a page reload.

### BUILD_TAG

Bumped to `2026-04-16-p10prep-agc-gate-wavrange-tgmonitor-ui`.

### Deploy note

No HDL change and no PAC change -- **re-flash p25-httpd binary
only**. The bitstream from the prior Phase 10 bake stays in place.

---

## [2026-04-16] Phase 10 -- LSM AGC noise-floor gate + traffic-chain API parity + grant map

**Branch:** fishball-p25
**Related:** `doc/changes/045_phase10_agc_gate_and_parity.md`
**BUILD_TAG:** `2026-04-16-p10prep-agc-gate-traffic-parity-grantmap`

Root-cause fix for the "first call splits into 3+ files" traffic-
chain acquisition failure seen on Clay County NAC 0x8A1. Live
idle `/api/constellation` on the traffic chain showed
`p50(|IQ|) = 0.33`, 23% of samples with `|IQ| < 0.2`, PLL hunting
at −0.27 rad. Gain sweep (40/50/60/70 dB front-end) showed the
traffic chain only converged at 70 dB -- not because it was
starved for signal (86 dB RSSI) but because the idle-channel
noise was dragging the AGC gain register down via SDRTrunk's
fast-attack / slow-release asymmetric clamp, and it couldn't
climb back.

### HDL change (baked)

- **`maia-hdl/p25_hdl/lsm_agc.py`**: added a
  `mag_update_threshold` kwarg (default 1024 raw Q1.15 =
  -30 dBFS). In the `DIV_INIT` state, inputs with
  `mag < threshold` skip the gain-update step entirely and
  proceed straight to `APPLY` with the current gain. Kills the
  noise-chase trap while preserving the asymmetric clamp for
  real fading events. Threshold = 0 restores the exact
  SDRTrunk-identical behaviour. New 16-bit `gate_dbg` counter
  (internal signal, not yet exposed via CSR).
- **`maia-hdl/p25_hdl/lsm_demod_loop.py`** + **`lsm_demod.py`**:
  plumbed the threshold kwarg and the `agc_gate_dbg` tap
  upward through the submodule hierarchy so a later bake can
  land CSR exposure per chain.
- **`maia-hdl/test/test_lsm_agc.py`**: three new test cases
  covering the gated-hold path, the `threshold=0` SDRTrunk
  fallback, and constructor argument validation. All 11 AGC
  tests + 12 in related `test_lsm_demod_loop` /
  `test_lsm_demod` / `test_p25ddc` pass.

### Rust change (ships in same bake)

- **`p25-httpd/src/httpd/mod.rs`** -- new endpoints, all
  behind `#[cfg(target_os = "linux")]` where they touch
  hardware:
  - `GET /api/traffic_lsm_dibit_dump` -- twin of
    `/api/control_lsm_dibit_dump`
  - `GET /api/traffic_iq_capture` -- twin of
    `/api/control_iq_capture`
  - `GET /api/traffic_iq_capture_aligned` -- twin of
    `/api/control_iq_capture_aligned`
  - `GET /api/traffic_lsm_control?dc_block=0|1&agc=0|1` --
    twin of `/api/control_lsm_control`, **plus** a new `agc`
    toggle matching the new HDL capability.
  - `GET/PUT /api/rx_gain?db=<N>` -- standalone AD9361
    hardwaregain knob (range -3..76 dB). Previously the only
    way to change gain was `/api/reinit`, which rewrites
    every front-end field.
  - `GET /api/grant_map` -- accumulated
    `HashMap<(tg, freq_hz), GrantMapEntry>` with count,
    encrypted-count, first-seen, last-seen. Plus a
    `frequencies` roll-up sorted by activity so future LO
    auto-center logic has the input it needs.
- **`p25-httpd/src/p25/traffic_manager.rs`** -- new
  `GrantMapEntry` struct + `grant_map` field on
  `TrafficManager` + `tally_grant` method. Hooked in
  `main.rs:2040` right after the raw grant-receipt log entry,
  **before** the encryption and monitor-list gates, so every
  observed grant is recorded regardless of follow decision.
- **Scanner-mode decision**: the existing
  `/api/monitor` + `monitor::MonitorList` already implement
  priority-ordered TG allow-list gating in the grant
  pipeline. No new endpoint added; the draft
  `/api/monitor_tgs` was dropped before commit.
- **BUILD_TAG** bumped to
  `2026-04-16-p10prep-agc-gate-traffic-parity-grantmap`.

### Expected post-flash behaviour

- Idle `p50(|IQ|)` on traffic chain shifts up to match the
  last real-signal operating point instead of parking at the
  noise-spike floor.
- First-call acquisition lands inside the 180 ms LDU budget
  instead of taking 2-5 s of AGC unwind. Recording files
  stop fragmenting at call start.
- Traffic-side `sync_near_misses / sync_hits` ratio drops
  (fewer noise-sync pickups). Noise-corrupted NIDs stop
  being "corrected" toward the all-ones TDU_LC codeword, so
  `tdu_lc` count should drop toward the real ~1 per-call
  rate.

### Things explicitly not in this change

- `agc_gate_dbg` stays an internal HDL signal; CSR exposure
  was deferred to avoid SVD/PAC regen scope creep.
- LO auto-center algorithm: data plumbing (`/api/grant_map`)
  landed, but no endpoint that acts on the data yet.
- Frontend work (scanner-mode picker, traffic-chain debug
  panels, gain slider) will ship in a follow-up PR after
  on-target validation.

---

## [2026-04-15] Phase 8C.1 -- Control-side `lsm_ctrl_dom` revert + TSBK CRC regression diagnosis

**Branch:** fishball-p25
**Related:** `doc/changes/038_phase8_runtime_reset.md`, this session's investigation log

Investigation session chasing a PS LSM TSBK CRC pass-rate regression
(~18-25% vs Phase 6F.9's documented 91.7%) that was unmasked by the
Phase 9 iq_lsm_decoder retirement removing the dashboard's dual-decoder
union safety net.

### Phase 8C.1 HDL revert (baked + flashed + tested)

- **`maia-hdl/p25_hdl/p25_top.py`**: reverted the Phase 8C
  `DomainRenamer({'sync': 'lsm_ctrl_dom'})` wrap around the
  control-side `lsm_demod` submodule. The `lsm_ctrl_dom`
  `ClockDomain` declaration, its clock/reset comb assignments,
  and the `lsm_ctrl_renamer` helper are all removed. The
  traffic-side `lsm_traffic_dom` wrap **is kept** because
  Phase 8B's per-call retune flow depends on
  `traffic_lsm_enable` toggling to clear non-`reset_less`
  state between calls.
- **Phase 8A `reset_in` plumbing is unchanged** -- the W1P
  `lsm_control.lsm_reset` field and the `self.lsm_demod.reset_in`
  wiring still work; the override block inside LsmPllUpdate /
  LsmTimingInterp / etc. just runs in the global `sync` domain
  now instead of `lsm_ctrl_dom`.
- Vivado timing dramatically improved: **WNS −0.688 ns → −0.093 ns**
  (6 failing endpoints instead of 95, all PS7 CDC false-positives
  in `axi_ad9361/up_axi` and `sys_rstgen`). But the TSBK CRC
  regression **did not improve** post-flash -- still at ~18% pass.
  The DomainRenamer wrap was hurting Vivado's placer badly but
  wasn't the cause of the framer-level regression.

### Diagnosis: TSBK CRC regression is symbol-timing drift, not a bug

Bit-exact offline replay via `tools/p25_decode_capture.py` (pure
Python reimplementation of the same framer/deinterleave/trellis/
CRC pipeline) demonstrated:

- **Sync dibits**: perfect match (distance 0, rust and python
  agree bit-for-bit).
- **NID BCH**: rust and python produce identical NAC/DUID from
  the same nid_bits word.
- **TSDU deinterleave** (status-dibit strip + trellis dibit
  extraction): rust and python produce bit-identical 98-dibit
  output.
- **Viterbi trellis decode**: rust and python produce bit-
  identical 12-byte TSBK output, matching final-state metrics.
- **CRC check**: python and rust **both** fail the CRC on
  failing captures -- computed CRC doesn't match the message
  CRC field either with plain or xored convention.

**Conclusion**: the PS framer pipeline is bit-exact with the
reference and has no bug. The trellis INPUT dibits have
enough bit errors to kick the Viterbi onto wrong (internally
self-consistent) paths that then fail CRC validation.

### Root cause (on-target live readings): Gardner TED sample-point drift

- `/api/hdl_lsm` 1-second window shows `sp_dbg` swinging by
  ~6000 Q4.12 ULPs (**~1.6 dibit-periods**) every second while
  the PLL is sitting at its lock point. That's 5-10x more
  sample-point movement than expected for a locked Gardner.
- Per-block TSBK CRC pass rates monotonically degrade with
  block position: **TSBK1 33%, TSBK2 26%, TSBK3 17%** -- classic
  signature of symbol-timing walk accumulating across the ~35 ms
  TSDU span. Sync is at the start (clean), and the walk gets
  worse as you move into the body.
- NID BCH pass rate (~80% cumulative) matches the Phase 6G.1
  cold-boot probe baseline of 75-80% from t=0. The LSM chain
  itself is producing Phase-6G.1-consistent output at the NID
  level. It's the TSDU BODY dibits that accumulate more errors
  as Gardner drifts across the 100+ symbol span.

### Reassessment of the Phase 6F.9 "91.7%" baseline

After seeing the diagnostic data, the Phase 6F.9 doc 029
measurement of 91.7% TSBK pass rate is probably not reproducible
today, and may have never been a stable steady-state:

- Phase 6G.1 (which came AFTER 6F.9) documented 75-80% NID valid
  from t=0 in the cold-boot probe. That's what we're seeing now.
- 6F.9's measurements were taken at "specific post-flash intervals"
  (3-5 minutes of uptime) and may have captured a favorable
  environmental moment.
- The Gardner TED sample-point hunting was likely always present,
  just not diagnosed because the 1-second window view in
  `HdlLsmRuntime` didn't exist until Phase 6F.2.
- SDRTrunk's 20+ TSBKs/s baseline on the same RF is achieved via
  a completely different symbol-timing architecture (stock C4FM
  Costas loop tuned for LSM vs. our custom Gardner TED).

### Phase 8C.1 BUILD_TAG

`2026-04-15-phase8c.1-revert-control-lsm-domain-wrap`

### Phase 10 follow-up scope (deferred for next session)

1. **Gardner TED tuning** -- reduce loop gain to stop
   sample-point hunting, or add proportional + integral
   control if it's currently proportional-only.
2. **Capture a sp_dbg time series** (longer than 1 sec) to
   characterize the drift pattern exactly.
3. **Compare against SDRTrunk** on the same RF to rule out
   environmental change (AD9361 LO drift, antenna, RF noise).
4. **End-of-call TDU_LC burst** (carried over from Phase 8
   session): TDU_LC-as-terminator or shorter
   `call_timeout_ms`.
5. **Duplicate same-TSBK `[TSBK2]` emission** in control_channel.rs
   decoder dispatch (seen in user's earlier activity logs).
6. **Optionally retire Phase 7A.2 traffic LSM chain + PS C4FM
   decoder** if we commit to LSM-only permanently and want to
   free resources that might be indirectly affecting control-side
   place-and-route quality.

---

## [2026-04-15] Phase 9 -- Retire the Phase 6D software LSM pipeline

**Branch:** fishball-p25
**Related:** `doc/changes/039_phase9_retire_phase6d_iq_lsm.md`

Phase-out of PS-side code that was duplicating functionality the PL
(FPGA gateware) already provides. The Phase 6D pure-Rust software
LSM pipeline was the "algorithmic development + validation
reference" before Phase 6E ported the full LSM demod into Amaranth;
since Phase 6E.9 landed the HDL LSM chain in production (Oct 2025)
the software pipeline has been dead weight. Phase 9 formally
retires it.

**What went away:**

- **Phase 6D LSM IQ reader tokio task** (200 lines, read iq_dma →
  full software LSM demod → soft-sync → directed TSDU dispatch).
  Gone.
- **`iq_lsm_decoder` + `lsm_stats` Arc constructions** in `main.rs`;
  AppState fields dropped.
- **`iq_dma` HDL ring**: still in the bitstream (dormant dead code),
  disabled at boot. Can be revived by a future phase for baseband
  capture or a new in-PL DSP block tapping post-DDC IQ.
- **`/api/lsm` endpoint + `get_lsm` handler**.
- **Dashboard "LSM Decoder (Phase 6D)" + "Top NACs (LSM)" cards**.
- **`/api/decoder_compare` → `ps_iq_lsm` + `ps_phase6d` sections**.
  The response is now a 3-column matrix: `ps_c4fm` (dormant
  fallback), `ps_lsm` (PS framer on PL HDL dibits — production),
  and `pl_hdl` (FPGA gateware heartbeat).

**What got better as a side effect:**

- **Active Grants panel stale-age bug fixed**. `iq_lsm_decoder`
  had no `expire_grants` loop, so its grants accumulated forever
  and leaked into the `/api/grants` union, showing entries with
  186 s / 368 s ages even though the control channel was
  refreshing them every 2-3 s. Removing the union = single
  decoder read = single expire loop = no more stuck entries.
- **PL HDL column on the Decoder Comparison matrix filled in**
  with derivable aliases (Sync hits / NID attempts / NID BCH
  failures / NID decoded OK / Total dibits — every row that has
  a meaningful PL equivalent now shows a number). Rows that are
  fundamentally PS-only (TSBK framing, active grants, bands) get
  clearly-labelled tags instead of bare `--`.
- **ARM CPU drops** — the software demod is no longer running on
  every iq_dma wake.

**What stayed** (deliberate non-goals):

- PS C4FM `decoder` (framer on HDL c4fm_dibit_dma) — kept as the
  only path to decode TSBKs on C4FM sites; labelled "dormant on
  LSM sites" in the dashboard.
- Traffic C4FM dibit reader task — kept because it still pets
  the TrafficManager's inactivity timer.
- libiio ADI IIO DMA chain — orthogonal to iq_dma; kept on the
  bitstream per the "Keep libiio path on Fishball" memory.
- `p25-httpd/src/lsm/` module — kept in-tree with
  `#![allow(dead_code)]` because `nid_fec::T_MAX_ERRORS` and
  `nid_fec::encode_nid` are still referenced by `/api/bch_t` and
  the HDL test bench, the full `LsmPipeline` serves as a
  readable algorithmic reference for the HDL port, and
  `golden_dump` still feeds the HDL test fixture generator.

Also updated: `doc/P25_API.md` (endpoint catalogue + examples +
dashboard panels table), `tools/p25_check.py` (ps_iq_lsm section
replaced with pl_hdl section, /api/lsm section removed),
`tools/p25_status_and_next_step.py` (decoder_compare rendering +
endpoint list). BUILD_TAG →
`2026-04-15-phase9-retire-phase6d-iq-lsm`. `cargo check` clean
(warnings 132 → 75).

Phase 10 follow-ups: end-of-call TDU_LC burst fix
(TDU_LC-as-terminator or shorter inactivity timeout), optional
HDL retirement of `iq_packer` / `iq_dma`.

---

## [2026-04-15] Phase 8 -- LSM runtime reset + clean re-lock on retune

**Branch:** fishball-p25
**Related:** `doc/changes/037_phase8_hdl_lsm_review.md`,
`doc/changes/038_phase8_runtime_reset.md`

Root-cause fix for the Phase 7 traffic-audio quality problem
(1-in-20 calls intelligible on Clay County LSM). Three sub-phases
in one session:

- **Phase 8A:** HDL runtime reset plumbing. New `reset_in` port
  cascades from `LsmDemod` through `LsmDemodLoop`,
  `LsmPllUpdate` (both linearised + CORDIC), `LsmTimingInterp`,
  `LsmDiffDemodSlicer`, `LsmSyncNidExtract`, `LsmNidBchFec`, and
  `LsmNidPipeline`, clearing all persistent state to init on a
  1-cycle pulse. Two new W1P register fields
  (`lsm_control.lsm_reset`, `traffic_lsm_control.traffic_lsm_reset`)
  wired into both LsmDemod instances. SVD + PAC regenerated. 4
  new unit tests (linearised + CORDIC reset clears pll_reg,
  CORDIC post-reset trajectory bit-exact to cold-start, end-to-
  end `LsmDemod` reset clears `pll_dbg` + `sample_point_dbg`).

- **Phase 8B:** PS integration. New `fpga.rs` helpers
  `pulse_traffic_lsm_reset`, `retune_traffic_chain` (atomic
  freeze-reset-thaw), `pause_traffic_chain`. Follower task in
  `main.rs` uses `retune_traffic_chain` on every grant retune
  and `pause_traffic_chain` on Idle/timeout + encryption
  tear-down. Boot init flipped: `traffic_lsm_enable` starts OFF
  and is enabled per-call. BUILD_TAG ->
  `2026-04-15-phase8-lsm-runtime-reset`.

- **Phase 8C:** Local clock domains. Both `LsmDemod` instances
  moved into per-chain `lsm_ctrl_dom` / `lsm_traffic_dom` local
  clock domains with reset wired to `~lsm_enable` /
  `~traffic_lsm_enable`. Disabling the chain now forces a
  synchronous reset of all non-`reset_less` pipeline + FSM
  state; the `reset_less=True` accumulators still need the 8A
  explicit reset pulse, which `retune_traffic_chain` fires AFTER
  the re-enable so it lands while the domain is active. Phase
  7G channelizer groundwork.

Verification: all 30 reset-relevant LSM HDL tests pass,
elaboration + `cargo check` clean. On-target Vivado bake +
firmware rebuild in flight.

---

## [2026-04-12] Phase 7D + 7B + 7E -- JMBE vocoder, monitor list, audio streaming

**Branch:** fishball-p25
**Related:** `doc/changes/036_phase7d_vocoder_and_7b7e_audio.md`

Three phases landed in a single work session:

- **Phase 7D:** IMBE vocoder — pure Rust port of JMBE (DSheirer/jmbe)
  replaces mbelib as the primary decoder. ~2500 LOC covering the full
  MBE synthesis pipeline with spectral enhancement and adaptive
  smoothing. mbelib retained as vendored fallback in `mbelib-sys/`.

- **Phase 7B:** Event-driven grant follower replaces 50ms polling.
  Typed `P25Event::Grant` mpsc channel from decoder to follower.
  New `MonitorList` + `/api/monitor` endpoint for pinning TGs.

- **Phase 7E:** Audio output — `GET /api/audio?format=wav` for VLC,
  `WS /ws/audio` for browser, `GET /api/audio_test` for offline QA.
  Broadcast channel from vocoder task to HTTP/WebSocket clients.

Also: TG encryption history, sticky encryption flag, grant store
cleanup on Idle, traffic DUID events in activity feed, source ID
in grant events, activity filter checkboxes, XSA files now tracked.

---

## [2026-04-11] Phase 7C -- LDU sync + IMBE frame extraction (focused, PS Rust only)

**Branch:** fishball-p25
**Related:** `doc/changes/035_phase7c_ldu_imbe_extraction.md`

Wires the new traffic-side LSM dibit DMA ring (from Phase 7A.2)
into a fourth `ControlChannelDecoder` instance that runs the same
state machine as the control side, with **new LDU1/LDU2/HDU/TDU/TDU_LC
dispatch arms** that extract raw 144-bit IMBE voice frames at the
SDRTrunk-documented bit positions. The frames flow through a new
`VoiceHandler` trait to an `ImbeCounter` that will become the
Phase 7D vocoder feed.

**Key new modules:**

- `p25/voice_frame.rs` -- `ImbeFrameRaw` (18-byte raw IMBE frame),
  `IMBE_FRAME_BIT_POSITIONS` (`[0, 144, 328, 512, 696, 880, 1064,
  1248, 1424]` from SDRTrunk `LDUMessage.java:32-40`),
  `extract_imbe_frames()` with status-dibit strip + bit-pack +
  9-position extraction. 5 unit tests.

**Critical type corrections:**

- `p25/types.rs` `length_dibits` corrected for HDU (324 -> 339),
  TDU (0 -> 15), LDU1 (792 -> 807), LDU2 (792 -> 807), TDU_LC
  (168 -> 159). The previous values were never validated because
  Phase 6 only handled TSDU. Cross-checked against the SDRTrunk
  `P25P1DataUnitID.java` table via the new
  `body_status_count_matches_sdrtrunk_table` test. Also added
  the universal `is_body_status_dibit(body_pos)` helper validated
  by the `body_status_pattern_matches_tsdu` test.

**Encryption flag plumbed end-to-end from the control channel:**

- `p25/tsbk.rs` `TsbkMessage::GroupVoiceChannelGrant` now exposes
  `service_options: u8` (was previously dropped at decode). New
  `pub mod service_options` with `ENCRYPTION_FLAG = 0x40` and
  `EMERGENCY_FLAG = 0x80` constants verbatim from SDRTrunk
  `ServiceOptions.java:27-30`.
- `p25/control_channel.rs` `GrantInfo` gained `encrypted: bool`
  and `emergency: bool` fields, populated from the grant TSBK.
  `take_other_grants_for_talkgroup` (Phase 6G.1 source preservation)
  extended to also preserve encrypted + emergency across
  `GroupVoiceChannelGrantUpdate` refreshes via a new
  `PreservedGrantFields` struct.
- `p25-json::ChannelGrant` gained `encrypted` + `emergency` fields
  with `#[serde(default)]` for forward-compat.
- `httpd/mod.rs` `/api/grants` surfaces both flags per grant, and
  `/api/traffic` adds a top-level `current_call_encrypted` field
  read from the lsm_decoder grant store for the currently-locked
  TG. Phase 7D vocoder will gate on this -- saving the ~600 lines
  of HDU payload parsing (trellis + RS(36,20,17)) we'd otherwise
  need to extract the encryption flag from the voice channel
  itself.

**Decoder voice dispatch:**

- `p25/control_channel.rs` new `VoiceHandler` trait with
  default-no-op methods for `on_ldu1`, `on_ldu2`, `on_hdu`,
  `on_tdu`, `on_tdu_lc`. New `voice_handler:
  Option<Arc<dyn VoiceHandler + Send + Sync>>` field on
  `ControlChannelDecoder` with `set_voice_handler()` setter. New
  `ldu1_count` / `ldu2_count` / `hdu_count` / `tdu_count` /
  `tdu_lc_count` cumulative counters. New LDU1/LDU2/HDU/TDU/TDU_LC
  dispatch arms in `process_dibit` (the existing `match duid`
  block was previously a no-op `_ => true` for non-TSDU).

**main.rs wiring:**

- New `ImbeCounter` struct with `AtomicU64` counters implementing
  `VoiceHandler`. Atomic counters because the handler is called
  synchronously from inside the dibit decoder task and a Mutex
  would deadlock with the existing tokio dibit reader's stats
  lock.
- New `traffic_lsm_decoder` ControlChannelDecoder instance with
  the `ImbeCounter` installed via `set_voice_handler`.
- New traffic LSM dibit reader task spawned in cfg(linux) block,
  mirror of the existing control-side `lsm_dibit_decoder` task.
- New `traffic_lsm_dibit_waiter` in the IRQ-handler-waiter cluster.
- AppState extended with `traffic_lsm_decoder` + `imbe_counter`.
- BUILD_TAG bumped to `2026-04-11-phase7c-ldu-imbe-extraction`.

**httpd/mod.rs API extension:**

- `/api/traffic` JSON gained `phase: "7C"`, `current_call_encrypted`,
  `imbe` block (atomic counter snapshot from ImbeCounter), and
  `traffic_lsm_decoder` block (decoder-internal sync_hits +
  per-DUID counters). The two should track 1:1 -- any divergence
  indicates an extraction failure.

**What's deferred** (per the focused scope):

- HDU payload parsing (algorithm ID, key ID, source RadioID, MFID,
  MI) -- the encryption flag we'd get from this is redundant with
  the control channel grant, and the late-entry case is rare
  enough to defer to 7C.2.
- TDU_LC LC payload parsing -- end-of-call metadata, cosmetic.
- LDU1 LC + LDU2 ESS payload parsing -- redundant with the
  control channel grant.
- Trellis + RS(36,20,17) + RS(24,12,13) + RS(24,16,9) decoders --
  not needed for any of the focused 7C scope.
- The vocoder itself -- Phase 7D.
- RTP audio output -- Phase 7E.

**Verification: ✅ PASSED on-target on 2026-04-11.** All 7
acceptance criteria from `doc/changes/035` validated on the
combined Phase 7A.1 + 7A.2 + 7C flash. The headline check
`imbe_frames_extracted == (ldu1_count + ldu2_count) * 9`
holds exactly on every sample across multiple call boundaries
(verified at 990 / 1116 / 1458 frames). The
`traffic_lsm_decoder` framer-internal counters match the
`ImbeCounter` voice handler atomics bit-for-bit on hdu / ldu1
/ ldu2 / tdu / tdu_lc -- proving the dispatch chain has zero
races and zero dropped events. Encryption flag plumbing
confirmed: TG 402 on Clay County reads `encrypted: true` from
`/api/grants` (it's a genuinely encrypted talkgroup). All 62
host tests pass. See doc 035 "On-target verification" appendix
for the full verification log + the TDU_LC dispatch skew
bonus observation that's tagged for Phase 7B/7D-prep
investigation.

---

## [2026-04-11] Phase 7A.2 -- LSM demod chain on traffic side + HDU/TDU/LDU dispatch

**Branch:** fishball-p25
**Related:** `doc/changes/034_phase7a2_lsm_traffic_chain_and_tdu_hdu.md`

Mirrors Phase 6E.9 on the traffic side: a parallel LSM demod chain
sits beside the existing C4FM traffic chain on the traffic DDC
output, identical to the control-side LSM chain. The new chain
produces NID events (NAC + DUID + BCH validity + sync distance)
which the PS-side heartbeat task polls at 16 ms cadence and
dispatches by DUID to the appropriate TrafficManager handler:

| DUID | Name | Dispatch |
|------|------|----------|
| `0x0` | HDU (Header) | `hdu_received(now, nac)` |
| `0x3` | TDU | `tdu_received(now, nac, false)` |
| `0x5` | LDU1 | `ldu_received(now, nac, false)` |
| `0xA` | LDU2 | `ldu_received(now, nac, true)` |
| `0xF` | TDU_LC | `tdu_received(now, nac, true)` |

This is the FPGA prerequisite for HDU + TDU detection on followed
voice channels. With TDU detection in place, the TrafficManager
gains a **2 s post-TDU hold window** (matches SDRTrunk PR #2010 /
commit `1b3ce431` `STALE_EVENT_THRESHOLD_MS = 2000`) so that PTT
releases between speakers in a multi-speaker conversation reuse
the same slot instead of fragmenting into separate calls. Phase
7C will tap the new `traffic_lsm_dibit_dma` ring in parallel for
IMBE frame extraction, and Phase 7D will add the IMBE -> PCM
vocoder.

**HDL changes:**

- `maia-hdl/p25_hdl/config.py`: new
  `traffic_lsm_dibit_dma_address = 0x1B00_0000` constant +
  validate() assertion.
- `maia-hdl/p25_hdl/p25_top.py`: new constructor instantiations
  (`traffic_lsm_decimator`, `traffic_lsm_lpf`, `traffic_lsm_rrc`,
  `traffic_lsm_demod`, `traffic_lsm_dibit_packer`,
  `traffic_lsm_dibit_dma`), new `traffic_lsm` register bank at
  offset 0xC0 (bank 6) with bit-identical layout to the
  control-side `lsm` bank, new `m_axi_traffic_lsm_dibit` AXI
  master, new `interrupts.traffic_lsm_dibit_dma` field, register
  crossbar update for `addr_bank == 0b110`. The
  `elaborate()` chain wiring mirrors lines 664-781 of the
  control-side LSM chain exactly, just fed by `traffic_ddc.re_out`
  instead of `ddc.re_out`.
- `maia-hdl/projects/fishball7020_p25/system_bd.tcl`: new
  `ad_mem_hp1_interconnect` line for
  `p25_core/m_axi_traffic_lsm_dibit`.
- `maia-hdl/ip/p25-core/default/p25_core.v`: regenerated from
  Amaranth (54888 lines, +16K from Phase 7A.1).

**PS Rust changes:**

- `p25-httpd/p25-pac/p25.svd` + `src/lib.rs`: regenerated via
  `svd2rust` to expose the new `traffic_lsm_*` register accessors.
- `p25-httpd/src/p25/traffic_manager.rs`: new fields
  (`post_tdu_hold_until`, `last_duid`, `last_nac`, `hdus_seen`,
  `tdus_seen`, `ldus_seen`), new methods (`hdu_received`,
  `tdu_received`, `ldu_received`, `post_tdu_hold_remaining_ms`),
  modified `note_activity()` (now also clears the post-TDU hold),
  modified `check_timeouts()` (honours the post-TDU hold window
  with priority over the call_timeout_ms fallback). The
  Phase 7A.1 Acquiring auto-promote bug fix is still present and
  carries over.
- `p25-httpd/src/fpga.rs`: new
  `set_traffic_lsm_enable/dibit_dma_enable/dc_block_enable`
  helpers, `traffic_lsm_control_readback`, `traffic_lsm_status`,
  `traffic_lsm_nid`, `traffic_lsm_drop_count`,
  `traffic_lsm_dibit_last_buffer/next_address`,
  `traffic_lsm_debug`, new `traffic_lsm_dibit_dma: RxBuffer`
  field opened from UIO device `p25-traffic-lsm-dibit`,
  new `DmaChannel::TrafficLsmDibit` variant + branch in
  `read_dma_buffers`, new `notify_traffic_lsm_dibit_dma` /
  `waiter_traffic_lsm_dibit_dma` for the IRQ source, plus IRQ
  counter and log line wiring.
- `p25-httpd/src/main.rs`: extended `IrqStats` with
  `traffic_lsm_dibit` field, new traffic LSM chain init at
  startup (enable + dibit DMA + DC blocker, with readback
  verification), new traffic LSM heartbeat task (polls
  `traffic_lsm_status` at 16 ms cadence, dispatches NID events by
  DUID), BUILD_TAG bumped to
  `2026-04-11-phase7a2-traffic-lsm-chain-and-tdu-hdu`.
- `p25-httpd/src/httpd/mod.rs`: extended `/api/traffic` snapshot
  with `last_duid` / `last_duid_hex` / `last_duid_label` /
  `last_nac` / `last_nac_hex` / `hdus_seen` / `ldus_seen` /
  `tdus_seen` / `post_tdu_hold_remaining_ms` / `traffic_lsm_chain`
  (full new register bank readback) / `irq.traffic_lsm_dibit_total`.

**Tezuka side (separate repo):**

- New device-tree carve-out for
  `p25_traffic_lsm_dibit_dma@1b000000` so the rxbuffer kernel
  module exposes a `p25-traffic-lsm-dibit` UIO device. Mirrors
  the existing `p25_lsm_dibit_dma@1a000000` carve-out.

**Documentation:**

- `doc/changes/034_phase7a2_lsm_traffic_chain_and_tdu_hdu.md`
  (new).
- `doc/P25_API.md` -- new "Phase 7A.2 additions" section under
  `/api/traffic`.
- `doc/P25_ADDRESS_MAP.md` -- new bank 6 detail section, new
  IRQ table row, new HP1 master row, new DDR carve-out row.
- `tools/p25_status_and_next_step.py` -- Phase 7A.2 ROADMAP
  entry's check() now actually verifies
  `/api/traffic.traffic_lsm_chain.enabled == true` instead of
  always returning false.

**Verification: ✅ PASSED on-target on 2026-04-11.** Combined
with Phase 7A.1 sticky-lock fix + Phase 7C IMBE extraction on
the same flash:

1. ✅ `/api/traffic.traffic_lsm_chain.enabled == true` --
   chain is alive, `nid_valid=true`, `n_errors=0`,
   `drop_count=0`, `dibit_overflow=false`. The new HDL chain
   is producing clean NIDs in steady state.
2. ✅ NID events arrive at the heartbeat dispatcher --
   `last_duid_label` rotates through HDU/LDU1/LDU2/TDU as
   calls land.
3. ✅ `hdus_seen + ldus_seen + tdus_seen` grew steadily during
   verification (8 / 159 / 347 over the verification window).
4. ✅ TDU release is sub-second -- captured indirectly via
   the sticky-lock test which observed `Active` state held
   throughout multiple calls without retunes during the 12 s
   window. The 2 s post-TDU hold semantics correctly bridge
   short PTT release gaps.
5. ✅ `tools/p25_sticky_lock_test.py` reported
   `delta_retunes = 0` (zero retunes over 12 s on a busy
   active call). This validates BOTH Phase 7A.1 sticky-lock
   AND the deferred Acquiring auto-promote fix from Round 3
   in doc/changes/033.
6. ✅ NID CRC pass rate is healthy: `n_errors=0` on most
   snapshots, the BCH FEC is correcting cleanly. (Per-block
   pass rate isn't directly exposed by the heartbeat snapshot
   but the consistent `nid_valid=true` + `n_errors=0` is the
   functional equivalent.)

**Bonus observation flagged for follow-up:** the dibit
decoder counters showed `tdu_lc=349` vs `(ldu1+ldu2)=162`
(2.15:1 ratio) over the verification window, with `sync_hits=559`
vs `near_misses=141189` (252:1). Either the BCH decoder has a
bias toward decoding uncorrectable input as DUID 0xF, or the
sync threshold for the traffic LSM chain needs to be raised
(the control side has runtime-tunable threshold via
`?sync_tune` -- the traffic side could use the same knob).
**Not a Phase 7A.2 bug** -- Phase 7C's IMBE math passes exactly
on every LDU dispatch, which proves the LDU framing is
correct. The over-counted TDU_LCs are dispatch noise that
downstream consumers can discard. Captured in the new
`feedback_p25_traffic_lsm_dispatch_skew.md` memory for the
next session.

---

## [2026-04-11] Phase 7A.1 -- Traffic-channel grant follower scaffold + sticky-lock policy

**Branch:** fishball-p25
**Related:** `doc/changes/033_phase7a1_traffic_scaffold_wire_up.md`

First step of Phase 7 (voice channel follow + audio out). Wires
the **already-existing** C4FM traffic-channel scaffold (HDL chain
from doc 007 + `fpga.rs` traffic helpers + `p25/traffic_manager.rs`
state machine, all sitting dormant since Phase 4) into the live
`p25-httpd` process so that:

- The traffic DDC is configured at startup (the existing
  `configure_ddc()` only set up the control DDC; the new
  `configure_traffic_ddc()` mirrors the same decimation /
  operations / bypass writes against the `traffic_*` register bank,
  no coefficient loading because the FIR ROM is shared at the HDL
  level).
- A new traffic dibit reader task drains the `traffic_dma` ring
  on every IRQ, builds a per-dibit histogram in `TrafficStats`,
  and pets the `TrafficManager` activity timer.
- A new traffic grant follower task polls
  `lsm_decoder.grants` at 50 ms cadence and retunes the traffic
  DDC to follow active calls.
- A new `/api/traffic` endpoint surfaces TrafficManager state,
  the dibit histogram, the traffic_dma IRQ counter, and four
  manual-control query params (`?reset_stats=1`,
  `?follower=on/off`, `?retune_hz=N`, `?demod_enable=0/1`)
  processed in fixed order.

**Sticky-lock policy from SDRTrunk upstream PR #2010 (commit `1b3ce431`):**
The initial naive newest-by-timestamp follower thrashed the
singleton DDC across multiple simultaneously-active TGs (~15+
retunes/sec observed on Clay County). Replaced with sticky-lock:
TG-based call identity (matching SDRTrunk's
`isSameCallCheckingToOnly()`), 2 s stale eviction threshold
(matching `STALE_EVENT_THRESHOLD_MS = 2000`), and a polling-task
gate that only accepts new TGs when state is Idle. Same-TG
different-frequency falls through to retune (handles network
channel reassignment mid-call). Plus an `Acquiring -> Active`
auto-promote in `handle_grant` that fixes a compound bug where
the 200 ms `acquire_timeout_ms` was hard-timing-out every call
because no real sync detector exists yet (Phase 7C).

**No FPGA bake required.** The traffic chain has been in the
HDL since Phase 4 (doc 007) and is already in the bitstream
from `tezuka_fw@08f7607`. Only `p25-httpd` needed a Tezuka
rebuild + flash to pick up the new endpoint and tasks.

Files touched:

- `p25-httpd/src/fpga.rs` -- new `configure_traffic_ddc()`.
- `p25-httpd/src/p25/traffic_manager.rs` -- removed
  `#![allow(dead_code)]`, added metrics fields and accessors,
  TG-based call identity in `handle_grant`, 2 s
  `call_timeout_ms`, `Acquiring -> Active` auto-promote.
- `p25-httpd/src/main.rs` -- new `TrafficStats`, two new tokio
  tasks (dibit reader + grant follower), traffic DDC startup
  configuration, BUILD_TAG bump to
  `2026-04-11-phase7a1-traffic-scaffold-wire-up`.
- `p25-httpd/src/httpd/mod.rs` -- extended `AppState`, new
  `/api/traffic` endpoint with four manual-control query params.
- `doc/P25_API.md` -- documented `/api/traffic` + bumped route
  count to 21.
- `doc/changes/033_phase7a1_traffic_scaffold_wire_up.md` -- new
  doc with the discovery, design decisions, the sticky-lock
  derivation from SDRTrunk PR #2010, the Acquiring bug story,
  and the on-target verification appendix.
- `tools/p25_status_and_next_step.py` -- new Phase 7A.1 ROADMAP
  entry plus restaged Phase 7A.2 -> 7H entries; also fixed two
  pre-existing brittle build-tag-string checks (Phase 6F
  source-preservation and Phase 6G.1 DC blocker) by replacing
  them with functional checks against `/api/grants[].source`
  and `/api/lsm_control.lsm_dc_block_enable`.
- `tools/p25_sticky_lock_test.py` -- new verification script
  that polls `/api/traffic` until an active call is seen, then
  takes a 12-sample burst to confirm `retunes` stays flat.

**Verification:** Round 1 (scaffold) verified on hardware -- all
seven acceptance criteria from doc 033 pass. Round 2 (sticky
lock) verified the TG pin holds. **Round 3 (Acquiring auto-promote
fix) ✅ PASSED on 2026-04-11** on the combined Phase 7A.1 + 7A.2 +
7C flash: `tools/p25_sticky_lock_test.py` reported
`delta_retunes = 0` over a 12 s window on a busy active call,
state remained `Active` throughout, single TG locked
(`unique talkgroups = [402]`). The compound bug fix is now
fully validated end-to-end on hardware. See doc 033 "Round 3"
appendix for the full verification log.

---

## [2026-04-11] Phase 6 closeout -- LSM trunking control channel COMPLETE

**Branch:** fishball-p25
**Related:** `doc/changes/032_phase6_closeout.md`

Phase 6 (the multi-month port of an LSM Simulcast P25 control
channel decoder onto the Fishball Z7020) is **DONE**. The Clay
County NAC 0x8A1 control channel is decoded end-to-end on the
FPGA + ARM PS at ~76-80 % steady-state TSBK CRC pass with ~88 %
of CRC-OK blocks dispatching as structured TsbkMessage events,
TG dedup + source-RadioId preservation in the active grants
table, and a runtime DC blocker A/B knob.

This commit closes out the phase with three small additions
and a documentation sweep:

### New endpoint: `/api/lsm_control` (Phase 6G.2)

`p25-httpd/src/httpd/mod.rs` adds a new GET handler that
reads back all three `lsm_control` register bits
(`lsm_enable`, `lsm_dibit_dma_enable`, `lsm_dc_block_enable`)
and exposes a `?dc_block=0|1` query-param shortcut for
toggling the DC blocker without ssh + devmem. The handler
takes the `ip_core` lock once and does the optional write +
the readback under it so a write+read sequence is atomic.
Closes the doc 031 verification gap that previously required
shell access on the board for runtime A/B testing.

The two other lsm_control bits are intentionally read-only
from this endpoint -- flipping them at runtime would tear
down the radio for no debugging benefit, and the devmem
escape hatch is still there.

### Documentation sweep

- New `doc/changes/032_phase6_closeout.md` -- canonical
  "Phase 6 is done, here's what shipped, here's what was
  consciously deferred, here's Phase 7" reference. Includes
  the full sub-phase rollup (6A through 6G.2), the deferred-list
  with decision references, the final commit chain, and a
  Phase 7A-7E sketch for the next session.
- `DEVPLAN.md` -- updated implementation order section to
  reflect Phase 6 completion (was stale past Phase 5 since
  the original C4FM-only redirect). Now shows Phase 6 sub-phases
  6A-6G.2 marked done with brief descriptions, and Phase 10
  added as the explicit Phase 7 voice-channel-follow next
  step with sub-phases 7A-7E.
- `doc/P25_API.md` -- new `/api/lsm_control` section, updated
  route count from 19 to 20, removed `/api/lsm_control` from
  the "endpoints we don't have" table.
- `tools/p25_status_and_next_step.py` -- ROADMAP[] entry for
  Phase 6G.2 now probes the live `/api/lsm_control` endpoint
  to verify the new binary is on the box. New
  `render_lsm_control` section in the snapshot output.

### Memory

- New `project_phase7_entry_point.md` memory replaces the
  obsolete `project_phase6e_entry_point.md` and
  `project_phase6f_entry_point.md` files (both were full of
  historical Phase 6F.x debug detail that lives in the change
  docs now). The new memory is forward-looking: where Phase 6
  ended, what the Phase 7A-7E plan is, and the recommended
  fresh-session entry point.

### Status

Source ships in this commit. **One more Tezuka rebuild + flash
is needed** to get this binary onto the board (no FPGA bake --
the bitstream from `tezuka_fw@08f7607` is unchanged). After
flashing, `/api/system.build` will report the new
`phase6-closeout` tag and
`tools/p25_status_and_next_step.py` will advance the
next-step pointer to Phase 7A.

---

## [2026-04-11] Phase 6G.1 -- HDL DC blocker on the LSM IQ input

**Branch:** fishball-p25
**Related:** `doc/changes/031_phase6g1_hdl_dc_blocker.md`

First commit of the doc 030 PL port roadmap. Adds a pair of one-pole
leaky-integrator DC blockers (one each for I and Q) at the very
front of `LsmDemod`, runtime bypassable through a new
`lsm_control.lsm_dc_block_enable` register bit. The slicer was
running on a slightly DC-biased input, which gave it a 60/40
inner/outer dibit ratio for the first 2-3 minutes after PLL start
until the loop slowly absorbed the bias on its own. With the
front-end DC blocker enabled the slicer never sees the bias in
the first place and the loop should lock immediately from cold boot.

### HDL changes

- New `LsmDcBlocker` Elaboratable in `maia-hdl/p25_hdl/lsm_dc_blocker.py`.
  One-pole leaky integrator with `alpha = 1 - 2^-7` (~39 Hz cutoff at
  31.25 kSPS, ~4 ms time constant). Pure shifts and adds, no DSPs,
  no BRAM. Saturated signed-16 output. Runtime bypass via `enable_in`.
  ~6 LUTs per instance.
- `LsmDemod` instantiates two of them at the front, with a new
  `dc_block_enable` top-level input. The blockers add one cycle of
  latency on the IQ path, which is invisible to `LsmTimingInterp`.
- `p25_top.py` adds a new `lsm_dc_block_enable` field at
  `lsm_control[2]`, default 0 (matches the existing `lsm_enable`
  convention), wired through to `LsmDemod.dc_block_enable`.

### PS changes

- `p25-pac` SVD updated; PAC regenerated with `svd2rust 0.33.5`.
- `fpga.rs`: new `set_lsm_dc_block_enable(bool)` helper;
  `lsm_control_readback()` extended to return the new bit.
- `main.rs`: control DDC startup now calls
  `set_lsm_dc_block_enable(true)` alongside the existing
  `set_lsm_enable(true)` / `set_lsm_dibit_dma_enable(true)`. The
  startup readback log line includes the new bit, and a
  `tracing::warn!` fires if the readback comes back false (with
  the explicit warning that the PLL acquisition transient will be
  2-3 minutes instead of a few seconds, so this isn't a silent
  regression).

### Tests

- New `test/test_lsm_dc_blocker.py`: 4 unit tests (step response
  bit-exact against a Python reference + decay below 0.5% of input;
  passband 1 kHz unattenuated; bypass passes DC through unchanged;
  strobe lockstep with input).
- New `test_dc_blocker_absorbs_constant_iq_bias` regression in
  `test/test_lsm_demod.py`: drives `LsmDemod` with the synthetic
  golden + a 6%-of-fullscale DC bias on both I and Q; verifies the
  dibit pass-through still produces a sensible dibit count.
- All 9 LSM demod / DC blocker tests pass; `cargo check` on
  `p25-httpd` is clean.

### Doc

- `doc/changes/031_phase6g1_hdl_dc_blocker.md` -- full design rationale,
  fixed-point format, on-target verification plan.
- `doc/P25_ADDRESS_MAP.md` -- documents the new `lsm_dc_block_enable`
  field at `lsm_control[2]`.

### Status

HDL + PS source ships in this commit. **Next:** rebuild bitstream
via `build_fpga.bat --p25`, commit the binary artefact separately
(per the build/commit-sequencing rule), then on-target A/B
verification (blocker on vs off) to confirm the cold-boot lock
time drops from 2-3 minutes to a few seconds.

---

## [2026-04-11] Phase 6F.11 -- PS at 100% (5 new opcode parsers + API merge)

**Branch:** fishball-p25
**Related:** `doc/changes/030_phase6f11_ps_complete_and_pl_port_roadmap.md`

Finishes the PS Rust side of the Fishball P25 LSM control channel
decoder. Adds the top 5 unparsed opcodes from the 6F.10
verification, extends `SystemIdentity` with the new state fields,
and unions both decoders' state in the dashboard handlers so the
operator sees the best of both pipelines while the per-pipeline
diagnostic split stays intact in `/api/decoder_compare`.

### Five new opcode parsers

All using SDRTrunk-style absolute-bit-position layouts via the
existing `TsbkBlock::bits()` helper:

| Opcode | Name | What it brings |
|---|---|---|
| 0x05 | UU_ANS_REQ | Private call paging (target + source radio IDs) |
| 0x09 | TELE_INT_VCH_GRNT_UPDT | Telephone interconnect grant update |
| 0x16 | SNDCP_DCH_ANN_EX | SNDCP packet-data channels (DL + UL) |
| 0x30 | TDMA_SYNC_BCST | System date/time + microslot rollover |
| 0x39 | SEC_CCH_BROADCST | Backup primary control channels A/B |

### Extended `SystemIdentity`

New optional fields populated by the new parsers: `secondary_cch_a/b`,
`sndcp_downlink/uplink_channel`, `last_sync_clock`. `p25-json::SystemInfo`
grows matching `Option<String>` fields with `serde` skip-if-none.

### API-level merge of both decoders

`/api/system`, `/api/grants`, `/api/bands` now read BOTH
`lsm_decoder` and `iq_lsm_decoder` and union the state:

- `/api/system` picks the most-populated value per field via
  `pick(a, b) = a.or(b)`
- `/api/grants` unions grants by channel, picks the YOUNGER on
  duplicates
- `/api/bands` unions frequency band entries by identifier

`/api/decoder_compare` is INTENTIONALLY unchanged to keep the
per-pipeline diagnostic A/B comparison from 6F.4-6F.10. See doc 030
for the design discussion.

### Final on-target numbers (92 s post PLL lock)

| Pipeline | TSDU/s | Block/s | CRC OK/s | Pass% |
|---|---:|---:|---:|---:|
| `ps_lsm` | 10.17 | 30.50 | **25.04** | 82.1% |
| `ps_iq_lsm` | 10.23 | 30.70 | **16.55** | 53.9% |
| **COMBINED** | — | **61.20** | **41.59** | — |

- **87.6 % opcode coverage** of CRC-OK blocks (2021/2308 parsed)
- **Top 9 opcodes all `parsed: yes`** in `/api/tsbk_opcodes`
- **3 simultaneous active grants decoded** (TG 300/433/402)
- **`bands_known = 6`** (FDMA + TDMA via merge)

### PS side is feature-complete

Doc 030 captures what "PS at 100 %" means concretely + the PL port
roadmap for Phase 6G:

1. **HDL DC blocker** (top priority) -- shrinks PLL acquisition
   transient from 2-3 min to seconds, fixes the 60/40 inner/outer
   slicer ratio that costs us ~70 % of syncs during transients
2. **(possibly) soft sync correlator into PL HDL** -- moderate
   value, would let us retire the parallel `iq_lsm_decoder` pipeline
3. **Multi-channel decode for trunking failover** -- only if
   we have a real failover need
4. **TSBK status/Viterbi feed** -- defer indefinitely, not
   CPU-limited

What stays in PS forever: BCH NID FEC, Trellis Viterbi, TSBK CRC,
all opcode parsers, dashboard / WebSocket / API. What we won't do:
port BCH FEC to HDL (already there + PS port is faster), port
opcode parsers to HDL (high-level state machine work), kill the
parallel-decoder architecture as a "cleanup" (lose diagnostic
A/B value the 6F.4-6F.10 saga depended on).

Tests: cargo test = 52 green.

Build tag: 2026-04-11-phase6f.11-five-new-opcode-parsers-and-api-merge

---

## [2026-04-11] Phase 6F.5 → 6F.9 throughput breakthrough (PS LSM decoder)

**Branch:** fishball-p25
**Related:** `doc/changes/029_phase6f5_through_6f9_throughput_breakthrough.md`

Five flash cycles of throughput tuning + diagnostic infrastructure
that took the PS LSM decoder from 2.3 useful messages/sec to a
steady-state ~14.8 parsed messages/sec across 8 opcode types.
Both stretch targets (30 TSBK/sec, 10 msg/sec) MET.

### Final on-target numbers (130s steady-state, post PLL lock)

| Pipeline | TSDU/s | Block attempts/s | CRC OK/s | Pass% |
|---|---:|---:|---:|---:|
| **ps_lsm** (HDL slicer + dibit hard sync) | **12.4** | **37.2** | **34.1** | **91.7%** |
| **ps_iq_lsm** (raw IQ + soft sync, NEW) | **11.6** | **21.5** | **19.8** | **92.3%** |
| **COMBINED** | **24.0** | **58.7** | **53.9** | 92% |

### What landed in each phase

- **6F.5 -- TDMA IDEN_UPDATE offset fix.** SDRTrunk's
  `FrequencyBandUpdateTDMA.getTransmitOffset()` multiplies by
  `getChannelSpacing()`, NOT by 250 kHz like FDMA/VUHF. We were
  reporting -780 MHz on Clay County's TDMA bands instead of the
  SDRTrunk-correct -39 MHz. One-line fix in
  `decode_iden_update_tdma`.
- **6F.6 -- sync distance histogram.** New
  `sync_distance_hist[25]` field on `ControlChannelDecoder` that
  buckets every observed sync distance. Exposed via
  `/api/lsm_dibit_dump`. The diagnostic that revealed the PLL
  acquisition transient was distorting all the early throughput
  measurements.
- **6F.7 -- runtime tunable threshold + sweep tool.** New
  `RUNTIME_SYNC_THRESHOLD: AtomicU32`, two new GET endpoints
  (`/api/sync_tune?threshold=N` and `/api/decoder_reset`), new
  `tools/p25_sync_sweep.py` automated threshold sweep tool. All
  endpoints accept GET-with-query-params so they work from a
  plain browser bar / curl.
- **6F.8 -- decoder_reset bug fix.** New
  `ControlChannelDecoder::reset_diagnostics()` method that clears
  EVERY per-run counter / histogram in one place. The 6F.7
  handler had missed `sync_hits`, `total_dibits`, `dibit_hist`,
  `recent_dibits`, and `raw_duid_hist`, which made the sweep tool
  conflate lifetime average with per-window throughput.
- **6F.9 -- IQ-LSM parallel decoder.** New
  `process_directed_tsdu()` method on `ControlChannelDecoder`
  that skips the Hunting state machine and runs NID + multi-block
  TSBK decode directly on a caller-supplied dibit buffer. New
  `iq_lsm_decoder` field on `AppState`, fed by Phase 6D's
  `LsmPipeline` running on raw IQ -- soft sync events from
  `find_sync_events_soft` get dispatched into the directed-decode
  path with a 400-dibit cross-batch carry-over so events near a
  batch boundary still find their full 336-dibit body. Third
  parallel TSBK pipeline alongside the legacy C4FM and HDL LSM
  decoders. Robust against future HDL slicer regressions.

### Diagnostic infrastructure additions

New / updated REST endpoints:

- `GET /api/sync_tune` -- read current threshold + cumulative
  histogram
- `GET /api/sync_tune?threshold=N` -- write new threshold
- `GET /api/decoder_reset` -- clear all counters for clean
  measurement window
- `POST /api/decoder_reset` -- HTTP-method-correct alias
- `PUT /api/sync_tune?threshold=N` -- HTTP-method-correct alias
- `GET /api/decoder_compare` now includes `ps_iq_lsm` slice
- `GET /api/lsm_dibit_dump` now includes `sync.distance_hist[25]`

New tools:

- `tools/p25_sync_sweep.py` -- walks a list of thresholds with
  reset between each, prints comparison table
- `tools/p25_check_phase6f4.py` -- now displays "IQ-LSM decoder"
  section + "Sync distance histogram" section

### Lessons learned (saved to memory)

The biggest single throughput improvement wasn't any of the
threshold tuning or parser fixes -- it was waiting for the LSM
PLL to fully converge. On the Fishball P25 LSM signal the PLL
takes 2-3 minutes after a flash to settle, and during that
transient the slicer produces a 60/40 inner/outer dibit ratio
with ~5 bit errors per sync window. Every measurement in the
first 90 seconds shows ~22% CRC pass rate which **looks like** a
fundamental signal-quality ceiling but is actually just PLL
hunting noise.

I burned three flash cycles tuning sync threshold trying to
"fix" what was just transient noise. The right thing was to
wait, not flash. Saved as
`feedback_pll_acquisition_transient` memory.

### Tests

`cargo test p25::` = 28 green at every phase. Full crate = 52
green. No new tests because all the changes are diagnostic
infrastructure or parallel pipelines that share the existing
tested parser code.

### Open follow-ups still on the queue (none are blockers)

- iq_lsm cross-batch defer (6F.10) -- push `ps_iq_lsm` from 1.86
  blocks/TSDU to 3.0
- Bump `max_recent` 100 → 1000 + fix verification script
  `messages/sec` calculation
- Add parsers for SNDCP_DCH_ANN_EX (0x16), TDMA_SYNC_BCST (0x30),
  SEC_CCH_BROADCST (0x39), UU_ANS_REQ (0x05),
  TELE_INT_V_CH_GRANT_UPDT (0x09) -- biggest unparsed buckets,
  ~30 lines each
- Merge dashboard system identity from BOTH lsm decoders
- HDL DC blocker (long-term DEVPLAN item)

---

## [2026-04-11] Phase 6F.3 multi-block TSBK2 / TSBK3 support (PS LSM decoder)

**Branch:** fishball-p25
**Related:** `doc/changes/027_phase6f3_multi_block_tsbk.md`

Adds end-to-end multi-block TSBK support to the PS LSM software
decoder. Phase 6F.2j (doc 026) shipped a working TSBK1 reader that
populates the System Identity card, but it stopped after one block
per TSDU and dropped ~2/3 of on-air TSBK content because most TSDUs
on the Clay County test target are TSBK1+TSBK2+TSBK3 multi-block
frames.

This phase generalises the deinterleaver and the
`ReadingDataUnit` arm of the state machine to handle 1, 2, or 3
TSBK blocks per TSDU. After each successful block decode the
state machine inspects the `LB` (last block) header bit and
either extends `du_expected_len` to the next block boundary
(231 dibits for TSBK2, 303 for TSBK3) or returns to Hunting.
Trellis or CRC failure on any block also returns to Hunting,
matching SDRTrunk's framer behavior.

Expected impact on the Clay County test target after on-target
verification: roughly **3x more TSBK messages decoded per second**,
and the previously stuck `bands_known` and `active_grants` counters
should start populating now that `IDEN_UPDATE` and
`GRP_VOICE_CHAN_GRANT` TSBKs riding in TSBK2/TSBK3 slots are
finally being read.

### Code

- `p25-httpd/src/p25/fec.rs` -- `TsduDeinterleaver` rewritten
  with `body_dibits_for_blocks(num_blocks) -> Option<usize>` and
  `deinterleave_multi(tsdu_dibits, num_blocks) -> Vec<u8>`. Status
  positions are pre-computed for the period-36 schedule (max 9
  positions for TSBK3). The trellis test helper was also lifted
  out of `mod tests` to module level (`trellis_encode_block` +
  new `trellis_encode_bytes` wrapper) so cross-module e2e tests
  can build real TSBK frames.
- `p25-httpd/src/p25/control_channel.rs` -- new field
  `tsdu_blocks_decoded`, renamed `process_tsdu` →
  `process_tsdu_block` returning `bool` (`true` = done, `false` =
  need more dibits), state machine inspects the return value to
  decide whether to extend or transition to Hunting. Aligned
  capture finalization moved into `finalize_capture(...)` helper.

### Tests

Two new e2e tests in `control_channel.rs::tests`:

- `test_multi_block_tsbk_e2e` -- builds a real
  `TSBK1=NET_STS_BCST(LB=0)` + `TSBK2=RFSS_STS_BCST(LB=1)` body,
  verifies both blocks decode and dispatch their messages.
- `test_single_block_tsbk_terminates_on_lb1` -- regression guard
  that confirms `LB=1` on TSBK1 correctly stops without
  consuming dibits from the next sync window.

Two new fec.rs tests for the multi-block deinterleaver:

- `test_tsdu_deinterleave_two_blocks` (231 raw → 196 trellis)
- `test_tsdu_deinterleave_three_blocks` (303 raw → 294 trellis)
- `test_body_dibits_for_blocks_table`

`cargo test p25::` runs 28 tests, all green. Full crate test
suite (52 tests) green.

---

## [2026-04-10] Phase 6E.10 PS-side scaffolding + iq/dibit packer overflow HDL hotfix

**Branch:** fishball-p25
**Related:** Tezuka `doc/changes/004_p25_lsm_dibit_dma_reserved_memory.md`

Bring-up companion to the Phase 6E.10 Vivado bake below. Adds the
p25-httpd PS-side accessors and tasks that let Linux userspace
actually exercise the new HDL LSM chain on hardware, the Tezuka DT
carve-out that makes the new `lsm_dibit_dma` ring visible as
`/dev/p25-lsm-dibit`, and a long-deferred Phase 6C hotfix to the
iq/dibit packer `overflow` semantics that was silently wrecking the
Phase 6D Rust LSM pipeline on every boot.

See `doc/changes/020_iq_dibit_packer_overflow_pulse.md` for the
overflow hotfix investigation + fix, and the existing `018`/`019`
docs for the Phase 6E.9/6E.10 HDL context.

### p25-httpd PS-side scaffolding for the HDL LSM chain

- `p25-httpd/src/fpga.rs`: `IpCore` grows a fourth `lsm_dibit_dma`
  `RxBuffer` opened against the new `/dev/p25-lsm-dibit` chardev and
  a full set of `lsm_*` register accessors against the regenerated
  PAC -- `set_lsm_enable`, `set_lsm_dibit_dma_enable`, `lsm_status`
  (returning a new `LsmStatusSnapshot` struct that captures all 7
  fields of the register in one bus read so the PS sees a coherent
  per-NID-event picture), `lsm_nid`, `lsm_drop_count`,
  `lsm_dibit_last_buffer`, `lsm_dibit_next_address`, `lsm_debug`,
  `read_lsm_dibit_buffers`. `DmaChannel` gains an `LsmDibit` variant
  wired through the existing `read_dma_buffers` helper.
- `InterruptHandler` gains `notify_lsm_dibit_dma` + matching
  `waiter_lsm_dibit_dma()`, and the IRQ fanout loop decodes the new
  bit 3 (`interrupts.lsm_dibit_dma`) alongside the existing three.
  NID events themselves are deliberately NOT IRQ-driven -- the 60 Hz
  polling loop below catches every ~14 ms NID with plenty of headroom.
- `p25-httpd/src/main.rs`:
  - Boot sequence enables the HDL LSM chain alongside C4FM + iq_dma:
    `ip_core.set_lsm_enable(true)` +
    `ip_core.set_lsm_dibit_dma_enable(true)`.
  - New "HDL LSM dibit reader" task drains the `lsm_dibit_dma` ring
    on every IRQ and histograms the dibit distribution for bring-up
    sanity. It deliberately does NOT feed the dibits into the Phase
    2A C4FM control-channel decoder -- LSM has different symbol-phase
    timing so cross-feeding would corrupt the working decoder's state.
    A dedicated LSM TSBK decoder is a Phase 6F follow-up.
  - New "HDL LSM NID poller" task polls `lsm_status.nid_event` at
    60 Hz (16 ms tick). On each fired event it reads the coherent
    snapshot (lsm_status + lsm_nid + lsm_drop_count + lsm_debug in
    one pass under the mutex), logs NAC/DUID/`n_errors`/
    `sync_distance`/`drop_count`/`pll_dbg`/`sample_point_dbg`
    throttled to 5 Hz (always logs the first 10 events), and warns
    on `lsm_dibit_overflow` latches or `drop_count` bumps.
  - The Phase 6D Rust LSM path keeps running in parallel as an
    independent sanity check. Both pipelines consume the same
    control DDC output and should emit identical NID streams on the
    same RF feed -- useful A/B during bring-up. Retiring the Phase
    6D PS-side path is a Phase 6F decision once both converge.
  - Phase 6 stats task is extended to also log
    `lsm_dibit_last_buffer` and `lsm_dibit_next_address`.

### Tezuka DT carve-out for `/dev/p25-lsm-dibit`

Added to `board/tezuka/fishball7020/dts/fishball-p25.dtsi` alongside
the existing dibit / traffic / iq entries (kernel-side counterpart
in `andylee77/tezuka_fw`):

```dts
p25_lsm_dibit_dma: p25-lsm-dibit-dma@1a000000 {
    no-map;
    reg = <0x1a000000 0x8000>;
    label = "p25_lsm_dibit_dma";
};

p25-lsm-dibit {
    compatible = "maia-sdr,rxbuffer";
    memory-region = <&p25_lsm_dibit_dma>;
    buffer-size = <0x1000>;
};
```

32 KB region, 8 x 4 KB sub-buffers -- mechanically identical to the
existing C4FM `p25_dibit_dma` ring, just at a new base. Mirrors the
FPGA-side `lsm_dibit_dma_address = 0x1A00_0000` from
`maia-hdl/p25_hdl/config.py`. Tezuka commit lands separately in
`andylee77/tezuka_fw/doc/changes/004_p25_lsm_dibit_dma_reserved_memory.md`.

### Phase 6C iq/dibit packer overflow HDL hotfix (doc 020)

Long-deferred bug from doc 014 follow-ups: `iq_dma` overflow latch
fired on every sub-buffer on hardware, causing
`p25-httpd/src/main.rs` to call `pipeline.reset()` on the Phase 6D
Rust LSM pipeline every ~128 ms, which wiped accumulated streaming
FIR delay lines / /2 decimator phase / Gardner TED history / Costas
PLL accumulator / sync-detector state before the pipeline had any
chance to converge. On-target symptom: `overflow_resets` ticking up
monotonically at ~7.6 Hz regardless of signal strength, and the
Phase 6D Rust LSM pipeline never locking onto the Clay County
control channel despite the Python reference doing so cleanly on
the same wav capture.

**Root cause.** `maia-hdl/p25_hdl/iq_packer.py` (and
`dibit_packer.py` -- same pattern) drove `self.overflow.eq(1)` on
the trigger condition but never cleared it anywhere else, so once
latched the signal was stuck high forever. The `maia_hdl.register`
`Rsticky` wrapper implements read-clear as `sticky := input` (not
`sticky := 0`), because the intended semantics are "one-cycle pulse
in, accumulated + clear-on-read sticky out". With a stuck-high
input, reads "clear" the accumulator by replacing it with the
current input value (still 1), and the next cycle's
`sticky := sticky | input` re-accumulates it immediately. The PS
never sees the bit clear. The class docstring even documented this
backwards (*"never self-clears in the gateware; the AXI Rsticky
layer is the only clear path"*).

**Fix.** Add a default `m.d.sync += self.overflow.eq(0)` at the top
of `elaborate()` in both packers. Amaranth's last-assignment-wins
means the conditional `self.overflow.eq(1)` inside the trigger
branch still fires, but now only for one cycle; the Rsticky wrapper
accumulates the pulse and clears it correctly on PS read.

`dibit_packer` had the same latent bug but it had never triggered
in practice: at 4800 sym/s on a 1.7 GB/s HP1 budget, the trigger
condition (previous word still waiting for `stream_ready` when the
next word arrives) effectively never fires. Fixed anyway for
consistency.

### Overflow regression guard + test rewrite

Added `test_overflow_is_pulse_not_latched` to both
`test_iq_packer.py` and `test_dibit_packer.py`. The new test fires
enough strobes under back-pressure to guarantee at least one
overflow trigger, samples `overflow` every cycle during the burst,
then verifies: (1) overflow actually fired at least once, (2) it
fell to 0 within one tick after the strobe burst stopped, (3) it
stayed at 0 for 8 ticks after back-pressure was released. Fail
mode: cycle-high counter bumps past a small threshold with an
explicit "this is the Phase 6C latched-level bug (see doc 020)"
assertion message.

The old `test_overflow_sticky` in both files actively asserted the
*wrong* behaviour (*"overflow should be sticky in gateware"*) and
therefore could never have caught the bug via pure regression
testing. Rewrote both `test_overflow_flag` tests to sample cycle
by cycle instead of only at the end of the burst, so a 1-cycle
pulse still passes.

### PS-side `main.rs` companion fix (defence in depth)

Even with the HDL fix, the old PS-side behaviour was wrong: it
called `pipeline.reset()` on any overflow bit, which throws away
legitimate LSM lock state. The right reaction is log + count + keep
running, because doc 014 proved the sample math shows no actual
data loss when the bit fires. A genuine back-pressure event that
caused actual sample loss would need a higher-layer detection
(gap in sample timestamps or backward jump in the FPGA's AW
address counter), not inference from the Rsticky.

### Regenerated HDL artefacts

Re-ran `./build_hdl.bat --p25 --verilog-only` to roll the packer
fix into `p25_core.v`. `p25.svd` and `p25-pac/src/lib.rs` are
regenerated but their content is byte-identical to the post-6E.10
version -- this is a gate-level internal change inside the packer's
`elaborate()` that does not touch any register layout.

### Verification

- `python -m unittest test.test_iq_packer test.test_dibit_packer`
  -- 13/13 pass.
- `python -m unittest test.test_iq_packer test.test_dibit_packer
  test.test_c4fm_demod test.test_symbol_timing` -- 25/25 pass, no
  regressions in the wider p25_hdl suite.
- Host-side `cargo check` on the p25-httpd workspace -- clean (90
  pre-existing warnings, no errors).
- ARM cross-check `cargo check --target
  armv7-unknown-linux-gnueabihf` -- clean (27 pre-existing warnings,
  no errors). New `fpga.rs` + `main.rs` paths compile under
  `cfg(target_os = "linux")`.
- First Vivado bake (bitstream A, pre-fix) completed cleanly at
  `maia-hdl/projects/fishball7020_p25/fishball_p25.sdk/system_top.xsa`
  -- kept as a baseline for comparison, not intended for flashing.
- Second Vivado bake (bitstream B, with fix) running in background.

### What gets flashed

On-target testing should use **bitstream B** (post-fix) plus the
p25-httpd binary from this commit plus the Tezuka firmware that
picks up `004_p25_lsm_dibit_dma_reserved_memory.md`. With all three
in place:

- `overflow_resets` in `/api/lsm` should stay at 0 for the first
  minute of uptime and climb only on actual HP1 stalls (vs the
  ~7.6 Hz boot-constant of the old bitstream).
- The Phase 6D Rust LSM pipeline should start accumulating
  `hard_events` / `soft_events` within seconds of enable as the
  streaming state is no longer being wiped.
- The Phase 6E.9 HDL LSM path should start logging NID events at
  the expected ~14 ms cadence with `nac=0x8A1`, `valid=true`,
  `n_errors<=11`, `drop_count=0`, and `lsm_dibit_overflow` not
  latching.
- Both decoders should agree on the same NIDs on the same RF
  capture, cross-validating the HDL port of 6E.0-6E.9 against the
  Phase 6D Rust reference.

---

## [2026-04-10] Phase 6E.10: Vivado bake artefacts (regen Verilog + PAC, wire `m_axi_lsm_dibit` through TCL)

**Branch:** fishball-p25

Phase 6E.10 closes out the **HDL side** of Phase 6E. The 6E.9 source already
wired `LsmDemod` into `P25Core` at the Amaranth level; this sub-phase
regenerates the binary HDL artefacts that Vivado actually consumes
(`p25_core.v`, `p25.svd`, `p25-pac/src/lib.rs`) and adds the two TCL
one-liners that surface the new `m_axi_lsm_dibit` AXI master to the IP
packager and the block-design HP1 SmartConnect. After this commit, running
`build_fpga.bat --p25` produces a bitstream that contains the full LSM HDL
chain plus its parallel dibit DMA ring.

See `doc/changes/019_phase6e10_vivado_bake.md` for the full design log.

### Regenerated HDL artefacts

`build_hdl.bat --p25 --verilog-only` (Docker, ~1 min wall clock) refreshed
all three downstream artefacts from the 6E.9 Amaranth source:

- **`maia-hdl/ip/p25-core/default/p25_core.v`** -- 770 KB / 21,896 lines
  (pre-6E.7) -> 1.30 MB / 37,420 lines. The ~70 % growth is dominated by
  `LsmDemod` and its 11 child Elaboratables, plus the second
  `DibitPacker`/`DmaStreamRingWrite` pair for `lsm_dibit_dma`, plus the
  new `lsm` register bank with its `RegisterCDC`.
- **`p25-httpd/p25-pac/p25.svd`** -- 17 KB / 21 registers (pre-6E.9) ->
  22 KB / 27 registers. Adds the six `lsm_*` registers from the new bank 5
  (`lsm_control`, `lsm_status`, `lsm_nid`, `lsm_drop_count`,
  `lsm_dibit_next`, `lsm_debug`) at offsets `0xa0..0xb4`. The first 21
  registers are byte-for-byte unchanged so existing PAC consumers are
  unaffected.
- **`p25-httpd/p25-pac/src/lib.rs`** -- auto-regenerated by `svd2rust 0.33.5`
  inside the Docker `build_hdl.sh` pass. Compiles cleanly under
  `cargo check` (33 lifetime-elision warnings, all in the new `lsm_*`
  accessors, matching the same pre-existing svd2rust pattern that already
  affects every other bank in the file).

### TCL wiring for `m_axi_lsm_dibit`

- **`maia-hdl/projects/fishball7020_p25/system_bd.tcl`** -- one extra
  `ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 p25_core/m_axi_lsm_dibit`
  call alongside the existing three masters. `ad_mem_hp1_interconnect` is
  idempotent, so the new master simply becomes a fourth slave port on the
  same HP1-bound SmartConnect that already hosts `m_axi_dibit`,
  `m_axi_traffic`, and `m_axi_iq`. HP1 budget at ~1.7 GB/s absorbs the
  additional ~1.28 KB/s from the LSM dibit ring at well below 0.001 %
  utilisation.
- **`maia-hdl/ip/p25-core/package_ip.tcl`** -- one extra
  `ipx::associate_bus_interfaces -busif m_axi_lsm_dibit -clock clk` so the
  Vivado IP packager places the new master in the 100 MHz core clock
  domain (`maia_sdr_clk/clk_out1`). Without this line the IP packager
  would emit the master as an unclocked port and the block design would
  refuse to auto-connect it.

### Verification

- Docker regen: 27 expected registers in the SVD dump, all at the offsets
  the address-map doc predicted; `p25_core.v` lands at the expected size.
- `rg -c m_axi_lsm_dibit p25_core.v` returns 190 (top-level port + AXI
  channel signals through several wrapper levels).
- `cd p25-httpd/p25-pac && cargo check` finishes in 0.77 s with 33
  pre-existing-pattern warnings, no errors.
- `cd p25-httpd && cargo check --workspace` finishes in 9.42 s with 90
  pre-existing warnings, no errors -- existing C4FM/iq_dma/Phase 6D LSM
  PAC consumers all compile unchanged against the regenerated PAC.
- Amaranth HDL test suites (49/49 LSM HDL + 17/17 older P25 HDL) not
  re-run: no `.py` source touched in this sub-phase, so re-running them
  would be a no-op against the same 6E.9 source the doc 018 verified.

### What 6E.10 does *not* cover (user-side hand-off)

Three remaining items live on the user's side because they need
Vivado 2023.2 + the Fishball Z7020 hardware:

1. `build_fpga.bat --p25` -- the actual Vivado IP packaging + synth + PAR
   that produces the `.xsa`. Expected wall-clock time is ~20 % longer
   than the pre-6E.9 baseline because the Verilog is ~70 % larger. Per
   the build/commit sequencing rule, the resulting `.xsa` should land in
   a separate "shipping artefact" commit before it goes into a Tezuka
   firmware image.
2. Tezuka device-tree carve-out for `p25_lsm_dibit_dma@1a000000`
   (32 KB region, 32 KB alignment) alongside the existing `iq_dma` /
   `traffic_dma` / `dibit_dma` reserved-memory entries. Mechanically
   identical to the Phase 6C `iq_dma` carve-out, just at a new base.
3. On-target validation against Clay County NAC 0x8A1: with
   `lsm_control.{lsm_enable, lsm_dibit_dma_enable} = 1`, point the
   control DDC at 860.9625 MHz and confirm `lsm_status.nid_event` is
   firing at the expected ~14 ms cadence with `nid_valid == 1`,
   `n_errors <= 11`, `lsm_nid.nac == 0x8A1`, `drop_count == 0`, and
   `lsm_dibit_overflow` not latching.

The "AGC deferred to 6E.6.5" caveat from doc 015 is still in force; the
Clay County signal is strong enough that SDRTrunk decodes it without
explicit AGC, so on-target smoke against this specific site should still
pass even though `LsmDemod` does not yet have a runtime AGC.

### Phase ladder status (post-6E.10)

- 6A: Python LSM demod -- DONE
- 6B: NID BCH FEC -- DONE
- 6C: IQ DMA path in FPGA gateware -- DONE
- 6D: Rust LSM port to PS -- DONE
- 6E.0-6E.6: HDL front end + demod loop -- DONE
- 6E.6.5: AGC in HDL -- deferred follow-up
- 6E.7: BCH FEC in HDL -- DONE
- 6E.8: LsmDemod top-level (sync detect + NID pipeline) -- DONE
- 6E.8.5: Soft sync detector -- deferred follow-up
- 6E.9: wire LsmDemod into p25_top.py alongside C4FM -- DONE
- **6E.10: regen `p25_core.v` + Vivado wiring -- DONE on the HDL side (THIS commit)**
  - Vivado synth via `build_fpga.bat --p25` -- pending user action
  - Tezuka DT carve-out for `lsm_dibit_dma` -- pending user action
  - On-target validation against NAC 0x8A1 -- pending user action

Phase 6F (PS-side LSM HDL consumer + dashboard wiring) is the natural
follow-up once the on-target smoke passes.

---

## [2026-04-10] Phase 6E.9: Wire LsmDemod into p25_top.py alongside C4FM

**Branch:** fishball-p25

Phase 6E.9 is complete. The standalone `LsmDemod` Elaboratable from
Phase 6E.8 is now plumbed into `P25Core` so the top-level FPGA design
runs the C4FM and LSM demod chains in parallel on the control channel.
After this sub-phase the only Phase 6E HDL work remaining is the Vivado
bake (6E.10). See `doc/changes/018_phase6e9_lsm_top_integration.md` for
the full design log.

### New top-level pipeline

The control DDC output (62.5 kSPS, 16-bit signed I+Q) now drives both
chains in parallel:

```text
control DDC -> /2 decimator -> 83-tap LPF -> 105-tap RRC -> LsmDemod
                                                              |
                                                              +--> lsm_dibit_packer -> lsm_dibit_dma
                                                              +--> NID event registers
```

The C4FM chain (`C4FMDemod` -> `SymbolTimingRecovery` -> `dibit_packer`
-> `dibit_dma`) is unchanged. The traffic channel stays C4FM-only --
adding LSM there is a follow-up phase.

### New DDR carve-out + AXI master

`P25Config.lsm_dibit_dma_address = 0x1A00_0000`, 32 KB total ring
(8 sub-buffers x 4 KB), naturally aligned. New `m_axi_lsm_dibit` AXI
master added to `P25Core.ports()` for Vivado IP packaging in 6E.10.
This is a deliberately parallel ring (rather than muxing the existing
`dibit_dma`) so the PS can drain both rings simultaneously and A/B C4FM
vs LSM on the same RF capture without disturbing either chain. Cost is
~1.28 KB/s on HP1 -- well below 0.001 % of HP1 budget.

### New `lsm` AXI register bank (bank 5, byte base `0x7C46_00A0`)

Six registers in an 8-slot bank (3-bit reg field), 2 slots free for
future expansion:

- **`lsm_control`** -- `lsm_enable` (RW, master enable for the entire
  LSM chain; gates the strobe at the front of `LsmDecimator2` so all
  downstream blocks go quiescent when 0) + `lsm_dibit_dma_enable` (RW,
  enables the lsm_dibit_dma ring's AW channel).
- **`lsm_status`** -- `bch_busy` (R), `in_nid_window` (R, useful as a
  "have lock" indicator), `nid_event` (Rsticky, latches each
  `nid_event_strobe`, clears on read), `nid_valid` (R, latched), and
  the latched 7-bit fields `n_errors` and `sync_distance`. Also packs
  the Rsticky `lsm_dibit_overflow` flag.
- **`lsm_nid`** -- latched `nac` (12 bits) + `duid` (4 bits) for the
  most recent BCH-decoded NID event.
- **`lsm_drop_count`** -- 16-bit saturating count of NIDs the sync
  detector emitted while `bch_busy` was high (should always read 0 in
  normal operation), plus the LSM dibit DMA `last_buffer` index.
- **`lsm_dibit_next`** -- AW write address inside the LSM dibit ring
  (debug only).
- **`lsm_debug`** -- snapshots of `pll_dbg` (signed Q2.13) and the top
  16 bits of `sample_point_dbg` (signed Q4.10), useful for dashboard
  Costas-loop and Gardner-timing traces.

The five "latched" NID-event fields (`nid_valid`, `n_errors`,
`sync_distance`, `nac`, `duid`) live in local `Signal()`s that are
updated on each `nid_event_strobe` pulse, so the PS sees a coherent
snapshot per event. The `nid_event` Rsticky bit tells the PS *which*
snapshot is current; reading `lsm_status` clears it.

### New IRQ bit

Bank 0 `interrupts` register grows a fourth Rsticky bit at offset 3:
`lsm_dibit_dma`, fed by `lsm_dibit_dma.interrupt`. NID events themselves
are PS-polled via `lsm_status.nid_event` rather than IRQ-driven, because
at one NID per ~14 ms a 60 Hz dashboard poll catches every event without
burning IRQ overhead.

### Verification

- `P25Core(P25Config())` constructs cleanly, generates a ~22 KB SVD
  (up from ~17 KB) covering the new bank, and elaborates to ~1.5 MB
  Verilog (up from ~1.27 MB).
- 49/49 LSM HDL tests pass in ~135 s (no regressions vs Phase 6E.8;
  the same 2 slow BCH sweeps are still gated behind
  `MAIA_HDL_SLOW_TESTS=1`).
- 17/17 older P25 HDL tests pass (`test_c4fm_demod`, `test_dibit_packer`,
  `test_symbol_timing`).

No new test files in 6E.9 -- the building blocks are exhaustively
tested in their own benches, and the integration is purely top-level
wiring covered by the elaboration smoke test.

### Resource estimate (Z7020, post-6E.9)

| Component                                     | DSP48  | BRAM18 | LUT     | FF    |
|-----------------------------------------------|--------|--------|---------|-------|
| C4FM chain (control + traffic, unchanged)     | ~10    | 0      | ~2000   | ~1500 |
| Maia DDC + DMA infra (unchanged)              | ~30    | ~10    | ~5000   | ~2500 |
| LSM front end (decimator + LPF + RRC)         | ~2     | 0      | ~150    | ~250  |
| LSM demod (`LsmDemod`)                        | ~30    | 2      | ~3940   | ~1730 |
| LSM dibit packer + DMA                        | 0      | 0      | ~100    | ~50   |
| LSM register bank                             | 0      | 0      | ~80     | ~150  |
| **6E.9 grand total**                          | **~72**| **~12**| **~11270**| **~6180** |

That's ~33 % DSP48, ~9 % BRAM18, ~21 % LUT, ~6 % FF on Z7020 -- comfortable
margins for the Vivado bake in 6E.10 even after PAR overhead.

---

## [2026-04-10] Phase 6E.8: LSM Demod Top-Level (sync detect + NID pipeline)

**Branch:** fishball-p25

Phase 6E.8 is complete. The complete LSM demod chain (IQ -> dibits ->
sync detect -> NID extract -> BCH decode -> NAC/DUID) now lives behind
a single top-level Elaboratable, `LsmDemod`. After this sub-phase the
only HDL work remaining for Phase 6E is wiring `LsmDemod` into
`p25_top.py` (6E.9) and the Vivado bake (6E.10). See
`doc/changes/017_phase6e8_lsm_demod_top.md` for the full design log.

### New modules

- **`maia-hdl/p25_hdl/lsm_sync_nid_extract.py`** -- 48-bit hard sync
  detector + status-skipping NID extractor. Streaming HDL equivalent
  of `find_sync_events_hard()` + `extract_nid_skipping_status()` from
  `p25-httpd/src/lsm/sync.rs`. State machine: IDLE shifts dibits into
  a 48-bit register, gates the threshold check on a fill counter
  (no false-trigger before the register has 24 dibits), checks
  `popcount(reg ^ FRAME_SYNC_DIBIT_PATTERN) <= 4`, then enters
  COLLECT_NID for 33 dibits, skipping index 11 (the status dibit) and
  packing the remaining 32 dibits into a 64-bit NID word MSB-first.
  EMIT pulses `nid_strobe` for one cycle and clears the sync register
  to suppress re-trigger. Hard detector only -- the Rust soft detector
  is deferred to an optional 6E.8.5 sub-phase because it would need a
  CORDIC `atan2` tap on the differential demod output.

- **`maia-hdl/p25_hdl/lsm_nid_pipeline.py`** -- thin wrapper that
  chains `LsmSyncNidExtract` + `LsmNidBchFec` and owns the start/done
  handshake plus a 16-bit saturating NID-drop counter. The handshake
  feeds `nid_strobe` to `bch.start` gated by `~bch.busy`; if a NID
  arrives mid-decode it's silently dropped and the counter is bumped
  (NIDs are spaced ~14 ms apart and BCH takes ~656 us, so this should
  never fire on real RF). Extracted from `LsmDemod` for testability:
  the entire control logic of 6E.8 lives here, in one self-contained
  Elaboratable that can be integration-tested without dragging the
  IQ-to-dibit demod loop into the test bench.

- **`maia-hdl/p25_hdl/lsm_demod.py`** -- top-level `LsmDemod`.
  Instantiates `LsmDemodLoop` + `LsmNidPipeline` side by side, passes
  the dibit stream straight through to the existing `dibit_dma` path
  so the existing dibit consumer keeps working unchanged, and surfaces
  the new NID event outputs (`nid_event_strobe`, `nac_out`, `duid_out`,
  `n_errors_out`, `valid_out`, `sync_distance_out`, `in_nid_window`,
  `bch_busy`, `nid_drop_count`) plus the existing `pll_dbg` /
  `sample_point_dbg` debug taps from the demod loop.

### Tests

- **`maia-hdl/test/test_lsm_sync_nid_extract.py`** -- 6 standalone
  tests for the sync detector + NID extractor (no BCH cost):
  clean sync + clean NID for NAC=0x8A1/DUID=7, 1-dibit error in the
  sync pattern (mirrors the Rust 1-error tolerance test),
  status-dibit-skip equivalence, back-to-back sync events, no
  false-trigger before the 24-dibit register fills, and
  `in_nid_window` waveform tracking. Total runtime <0.5 s.

- **`maia-hdl/test/test_lsm_nid_pipeline.py`** -- 1 integration
  test: drives a constructed sync + clean NID dibit stream through
  `LsmNidPipeline` and verifies one `nid_event_strobe` fires with
  the right (NAC, DUID, n_errors=0, valid=1, sync_distance=0), and
  `nid_drop_count` stays at 0. ~13 s sim time (one BCH decode at the
  decoder's 65,538-cycle serial sweep).

- **`maia-hdl/test/test_lsm_demod.py`** -- 1 wiring test: drives
  the existing `demod_loop_synthetic.json` IQ golden through
  `LsmDemod` and verifies the dibit pass-through still produces ~254
  dibits AND the NID pipeline stays quiescent (no `bch_busy`, no
  `in_nid_window`, no `nid_event_strobe`, `nid_drop_count == 0`)
  because the synthetic golden has no sync pattern. The standalone
  per-dibit accuracy vs truth is covered by `test_lsm_demod_loop`
  and is not re-checked here. ~1.7 s sim time.

### Test results

```text
$ python -m unittest test.test_lsm_decimator test.test_lsm_fir \
    test.test_lsm_timing_interp test.test_lsm_diff_demod_slicer \
    test.test_lsm_gardner_ted test.test_lsm_pll_update \
    test.test_lsm_pll_rotate test.test_lsm_demod_loop \
    test.test_lsm_nid_bch_fec test.test_lsm_sync_nid_extract \
    test.test_lsm_nid_pipeline test.test_lsm_demod
...
Ran 49 tests in 172.729s
OK (skipped=2)
```

Up from 41/41 in 6E.7. Two skips are the slow-mode BCH sweeps from
6E.7 still gated behind `MAIA_HDL_SLOW_TESTS=1`. All previous LSM
tests still pass unchanged.

### Resource estimate (Z7020)

| Component | DSP48 | BRAM18 | LUT | FF |
|---|---|---|---|---|
| LsmDemodLoop (6E.6d) | ~30 | 2 | ~3500 | ~1500 |
| LsmSyncNidExtract (6E.8a) | 0 | 0 | ~50 | ~130 |
| LsmNidBchFec (6E.7) | 0 | 0 | ~340 | ~50 |
| LsmNidPipeline + LsmDemod glue | 0 | 0 | ~40 | ~50 |
| **LsmDemod total** | **~30** | **2** | **~3930** | **~1730** |

~14% of Z7020 DSP48, 1.4% of BRAM18, ~7% of LUT/FF for one full LSM
channel. Plenty of room for the existing C4FM chain, the Phase 6C
IQ DMA, the Maia SDR base platform, and any future AGC follow-up.

### Out of scope (deferred)

- AGC in HDL (still 6E.6.5)
- Soft sync detector (new 6E.8.5 -- only if hard detector under-
  performs on real RF)
- `p25_top.py` integration (6E.9)
- Vivado bitstream bake + on-target validation (6E.10)

### Status

Phase 6E.8 ends at "the entire LSM demod chain is one Elaboratable,
the dibit pass-through is verified against the synthetic golden, the
sync detect + NID extract + BCH chain is verified end-to-end on a
constructed clean stream, all 49 LSM HDL tests pass". Phase 6E.9 --
wiring `LsmDemod` into `p25_top.py` alongside the existing C4FM chain
and surfacing the new NID event outputs as AXI registers -- is up
next.

---

## [2026-04-10] Phase 6E.7: NID BCH(63,16,11) FEC in HDL

**Branch:** fishball-p25

Phase 6E.7 is complete. The NID BCH(63,16,11) maximum-likelihood
decoder is now in PL fabric, finishing the Phase 6E LSM
synchronisation chain in hardware. The remaining 6E sub-phases are
the top-level `LsmDemod` wrapper (6E.8), the `p25_top.py` integration
(6E.9), and the Vivado bake (6E.10). See
`doc/changes/016_phase6e7_bch_fec.md` for the full design log.

### Architectural deviation from doc 015 (with user approval)

The original Phase 6E plan called for a **65,536-entry codebook in
BRAM** (~4.2 Mbit ~= 84% of Z7020 BRAM) plus a popcount tree and a
running min. That works in software (Phase 6D's `nid_fec.rs` keeps a
`[u64; 65536]` static codebook) but is unworkably tight on the Z7020
fabric -- it leaves almost no headroom for the existing C4FM chain,
the rest of the LSM front end, or future expansion.

`LsmNidBchFec` instead **computes each codeword on the fly** from a
16-bit counter and the constant 16x48-bit generator matrix:

```text
parity = XOR over { GEN[i] : data[15-i] == 1 }   # 48-bit
cw     = (data << 48) | parity                   # 64-bit
diff   = cw ^ received_nid_latched               # 64-bit
dist   = popcount(diff)                          # 7-bit
```

Per-cycle update of the running min over 65,536 sweep cycles. Same
algorithm, same correction strength (t=11, identical to a
Berlekamp-Massey decoder within the unique-decoding sphere), same
cycle budget (~656 us per decode at 100 MHz, well under the ~14 ms
NID rate). The trade is ~1% of Z7020 LUT/FF for 0 BRAM and 0 DSP.

### Modules

- **`maia-hdl/p25_hdl/lsm_nid_bch_fec.py`** -- new
  `LsmNidBchFec` Amaranth module. Inputs: `start`, `received_nid[64]`.
  Outputs: `done` (1-cycle pulse), `nac_out[12]`, `duid_out[4]`,
  `n_errors_out[7]`, `valid_out`, `busy`. State machine: IDLE -> SWEEP
  -> IDLE. Uses a 17-bit counter so bit 16 cleanly signals "swept all
  65,536 codewords". The combinational parity tree is pure LUT
  (sparse generator means each output bit is the XOR of ~8 data bits
  -- ~2 LUT levels). Includes a software `encode_nid()` reference
  used by the test bench.

### Tests

- **`maia-hdl/test/test_lsm_nid_bch_fec.py`** -- 7 new tests
  (5 default + 2 slow-mode opt-in):

  - `test_encoder_reference_matches_sdrtrunk_vector` -- locks the
    generator matrix and bit ordering against the SDRTrunk-published
    golden `encode_nid(1, 0) == 0x00103185B7E9E224`. Pure-software,
    runs in milliseconds. Catches any future drift.
  - `test_encoder_data_field_layout` -- pure-software check that the
    16 data bits land in bits 48..63 of the codeword.
  - `test_clean_codeword_and_done_strobe` -- single clean decode for
    NAC=0x8A1 / DUID=7. Captures the `done` pulse width (must be
    exactly 1 cycle) and the `busy` waveform across the full sweep
    in the same simulation to avoid spinning a second `Simulator`.
  - `test_single_bit_error_sample_positions` -- flips one bit at
    each of {0, 15, 16, 47} (data MSB, data/parity boundary, parity
    LSB+1) and verifies the decoder corrects each. Also exercises
    the start-after-done path by reusing one DUT across decodes.
  - `test_error_correction_at_t1_t6_t11` -- three corrupted
    codewords with deterministic xorshift bit patterns at the easy
    edge (t=1), mid-range (t=6), and the corner of the
    unique-decoding sphere (t=11). PRNG seed
    `0xDEAD_BEEF_CAFE_BABE` matches the Rust
    `error_correction_sweep_up_to_t11` test for cross-debugging.

  Default suite: ~7 HDL decodes, ~90 seconds total. The two
  slow-mode tests (`test_error_correction_sweep_up_to_t11` with
  55 decodes, `test_all_64_single_bit_positions` with 64 decodes)
  are gated by `@unittest.skipUnless(MAIA_HDL_SLOW_TESTS=1)` --
  ~25 min combined sim time, run before bake or on CI.

### Test results

```text
$ python -m unittest test.test_lsm_decimator test.test_lsm_fir \
    test.test_lsm_timing_interp test.test_lsm_diff_demod_slicer \
    test.test_lsm_gardner_ted test.test_lsm_pll_update \
    test.test_lsm_pll_rotate test.test_lsm_demod_loop \
    test.test_lsm_nid_bch_fec
...
Ran 41 tests in 195.545s
OK (skipped=2)
```

Up from 34/34 LSM HDL tests in Phase 6E.0-6E.6. Two skips are the
slow-mode opt-in BCH sweeps. All previous LSM tests still pass
unchanged.

### Resource estimate (Z7020)

| Resource | LsmNidBchFec |
|---|---|
| BRAM18 | **0** |
| DSP48E1 | **0** |
| LUT (parity tree + XOR + popcount + compare) | ~340 |
| FF (counter + best_dist + best_data + outputs) | ~50 |

Total `LsmNidBchFec` cost: <1% of Z7020. Combined with the Phase
6E.0-6E.6 LSM front end + demod loop (~30 DSP48 + 2 BRAM18) the
full LSM chain is comfortably under 15% of Z7020 DSP and ~1.5% of
BRAM, leaving plenty of room for the existing C4FM chain and future
work.

### Out of scope (deferred)

- AGC in HDL (still 6E.6.5)
- Top-level `LsmDemod` wrapper (6E.8)
- Wiring into `p25_top.py` (6E.9)
- Vivado bitstream bake + on-target validation (6E.10)

### Status

Phase 6E.7 ends at "all 41 LSM HDL tests pass, BCH decoder is
bit-exact with the Rust ML decoder within the unique-decoding sphere,
zero BRAM and zero DSP cost". Phase 6E.8 -- top-level `LsmDemod`
Amaranth module that assembles the front end + demod loop + BCH FEC
behind a single Elaboratable -- is up next.

---

## [2026-04-09] Phase 6D: Rust LSM Demod + NID FEC Port to p25-httpd

**Branch:** fishball-p25

Phase 6D is complete (Rust port compiles, all unit tests pass, ARM
cross-build clean; on-target validation queued behind the next Tezuka
firmware rebuild). Mechanical port of the validated Phase 6A/6B Python
prototype into the embedded p25-httpd Rust workspace, file-by-file with a
one-to-one mapping between Python stages and Rust files. The new pipeline
runs in parallel with the existing C4FM dibit reader, consuming the
Phase 6C `iq_dma` ring directly. See `doc/changes/014_phase6d_lsm_rust_port.md`
for the full write-up.

- **`p25-httpd/src/lsm/`** -- new module, ~1580 lines of Rust + tests:
  - `nid_fec.rs` (~280 lines) -- BCH(63,16,11) encoder + ML codebook
    decoder. Generator matrix copied verbatim from
    `BCH_63_16_23_P25_Test.java` (octal literals → `u64`). Codebook is
    `OnceLock<Box<[u64; 65536]>>`, built lazily on first decode call
    (~512 KB resident, <10 ms build on Cortex-A9).
  - `filters.rs` (~310 lines) -- frozen LPF (83 taps Parks-McClellan) and
    RRC (105 taps closed-form) `const [f32; N]` arrays designed at
    31.25 kSPS via the existing `tools/p25_lsm_demod.py`. Plus
    `apply_real_fir_complex` (batch), `StreamingFir` (with `(taps.len()-1)`
    history for boundary-transient-free chunking), `decimate_by_2` (batch),
    and `StreamingDecimator2` (phase-tracking across odd-length chunks).
  - `demod.rs` (~330 lines) -- verbatim port of `demod_lsm()` and the
    SDRTrunk `P25P1DemodulatorLSM.process()` it descends from. Variable
    names match the Java source. AGC + PLL + Gardner TED + slicer.
    `DemodState` is exposed so the streaming variant
    `demod_lsm_with_state` can preserve loop state across iq_dma
    sub-buffer boundaries.
  - `sync.rs` (~400 lines) -- hard + soft sync detectors. Hard is the
    sliding 48-bit Hamming-distance correlator (`SYNC_THRESHOLD = 4`).
    Soft is the port of `P25P1SoftSyncDetectorScalar` correlating
    against the 24 ideal `±3π/4` sync phases (`SYNC_SCORE_THRESHOLD =
    60.0`). Both share the same status-dibit-aware NID extractor that
    skips the 33-dibit-window position 11.
  - `ring.rs` (~110 lines) -- iq_dma sub-buffer `&[u8]` to
    `Vec<Complex32>` adapter. Decodes the FPGA's
    `{im[1], re[1], im[0], re[0]}` 64-bit word as four little-endian
    `i16` samples, normalises to ±1.0.
  - `mod.rs` (~150 lines) -- module root, local
    `Complex32 { re: f32, im: f32 }` POD type (avoids new `num-complex`
    runtime dep), `LsmPipeline` orchestrator that owns the streaming
    decimator + LPF + RRC + demod state and exposes `process_iq()` /
    `reset()`.
- **`p25-httpd/src/fpga.rs`** -- `IpCore` gains `iq_dma: RxBuffer`,
  `iq_last_addr: Option<u32>`, `set_iq_dma_enable`, `iq_last_buffer`,
  `iq_overflow`, `iq_next_address`, `read_iq_buffers`. New `DmaChannel::Iq`
  arm in `read_dma_buffers`. `InterruptHandler` gains `notify_iq_dma`,
  `waiter_iq_dma`, and the IRQ-loop iq branch (bit 2 of `interrupts`).
- **`p25-httpd/src/main.rs`** -- `mod lsm`, `ip_core.set_iq_dma_enable(true)`
  at startup, third tokio task that runs the LSM pipeline on iq_dma
  wakeups. Snapshots `read_iq_buffers()` + `iq_overflow()` under the lock,
  drops the lock before CPU work, runs the streaming pipeline, and logs
  per-IRQ NID stats (hard/soft sync counts, cumulative top-3 NACs). The
  task resets all streaming state if the gateware overflow latch fires.
  Independent of the existing dibit reader -- both pull from the same
  control DDC output via separate ring DMAs and separate PS state.
- **`tezuka_fw/board/tezuka/fishball7020/dts/fishball-p25.dtsi`** --
  third `reserved-memory` entry `p25_iq_dma: p25-iq-dma@19000000`
  (256 KB, `no-map`) and a matching `p25-iq` rxbuffer node with
  `buffer-size = <0x8000>` (32 KB sub-buffer × 8 = 256 KB ring, matches
  FPGA `iq_dma_num_buffers_log2 = 3`).
- **Verification on Windows host:** **17/17 lsm unit tests pass**
  (`cargo test lsm::`). The encoder produces SDRTrunk's golden vector
  bit-for-bit (`encode_nid(1, 0) == 0x00103185B7E9E224`). The BCH decoder
  recovers all 550 corrupted codewords across the 1..=11 error sweep
  (50 trials per error level). The streaming FIR matches the batch FIR
  to <1e-5 absolute on a 400-sample frequency-sweep input chunked at
  position 137. The streaming /2 decimator preserves the even-grid phase
  across 7+8+8 odd-length chunks. Both sync detectors find a clean sync
  in a synthetic dibit stream and the BCH decoder recovers the embedded
  NAC/DUID with zero errors. The status dibit at NID-window index 11 is
  proven to be skipped (two streams differing only in that dibit produce
  identical extracted NIDs).
- **ARM cross-build clean:** `cargo check --target armv7-unknown-linux-gnueabihf`
  produces 27 warnings (all dead-code on existing modules) and zero
  errors. No new runtime dependencies (`pm-remez` and `num-complex` not
  pulled in).
- **Pending verification:** Tezuka firmware rebuild (`build.bat --p25`
  inside the Tezuka Docker container) to consume the Phase 6C XSA + the
  new lsm module, SD-card flash, on-target smoke test (LSM reader task
  starts, iq_dma wakeups arrive at ~7.6 Hz, NAC=0x8A1 dominates the
  cumulative histogram at a per-second rate comparable to SDRTrunk on
  the same antenna).

Phase 6D ends at "Rust port compiles, all unit tests pass, ARM
cross-build clean". Phase 6E -- HDL port of the streaming filters and
the demod loop into Amaranth, replacing the C4FM-only path -- is the
next step on the ladder.

---

## [2026-04-09] Phase 6C: P25 Post-DDC IQ Ring DMA in FPGA Gateware

**Branch:** fishball-p25

Phase 6C is complete (gateware logic + Verilog/SVD/PAC regen). Adds a third
ring DMA inside the P25 IP core that streams the control DDC's post-decimation
IQ output (62.5 kSPS, 16-bit signed I/Q, two samples per 64-bit word) to a
reserved DDR carve-out at `0x1900_0000`. This is the bridge that lets Phase
6D's Rust port of the validated Python LSM demod consume live antenna data
without disturbing the existing dibit pipeline. See
`doc/changes/013_phase6c_iq_dma.md` for the full write-up and
`doc/P25_ADDRESS_MAP.md` for the canonical address-space tables.

- **`maia-hdl/p25_hdl/iq_packer.py`** -- new ~120-line `IQPacker` Amaranth
  module. Buffers two consecutive `(re, im)` pairs into a 64-bit AXI4-Stream
  word `{im[1], re[1], im[0], re[0]}` (sample 0 in low half). Mirrors
  `DibitPacker`'s handshake + sticky-overflow conventions exactly.
- **`maia-hdl/p25_hdl/p25_top.py`** -- third tap of the control DDC output
  (alongside `c4fm_demod` and `symbol_timing`); new `iq_dma` instance of
  `DmaStreamRingWrite` exposing `m_axi_iq`; new 5th register bank `iq` at
  byte offset `0x80` containing `iq_dma_status`/`iq_dma_control`/
  `iq_next_address`. The bank decoder was widened from `address[3:5]`
  (4 banks) to `address[3:6]` (8 banks max) -- no `axi4_awidth` change
  needed, the existing 7-bit word address has plenty of headroom.
- **`maia-hdl/p25_hdl/config.py`** -- new `iq_dma_*` fields with the full
  bandwidth math (250 KB/s, 256 KB ring = 8 x 32 KB sub-buffers, ~128 ms
  per sub-buffer interrupt, ~1 s of IQ in flight) and an alignment assert
  in `validate()`.
- **`maia-hdl/ip/p25-core/package_ip.tcl`** -- one new
  `ipx::associate_bus_interfaces -busif m_axi_iq -clock clk` line.
- **`maia-hdl/projects/fishball7020_p25/system_bd.tcl`** -- one new
  `ad_mem_hp1_interconnect` line. SmartConnect on HP1 now arbitrates
  three masters (`m_axi_dibit` + `m_axi_traffic` + `m_axi_iq`); HP1 budget
  at ~1.7 GB/s absorbs the new ~250 KB/s consumer with ~0.015% utilisation.
- **`maia-hdl/test/test_iq_packer.py`** -- new pure-Python pysim test (uses
  `amaranth.sim.Simulator`, mirrors `test_dibit_packer.py`). 7 tests
  covering single/multi-pair packing, two's complement extremes, no-strobe
  quiescence, backpressure handshake, and the sticky overflow flag. All 7
  pass on the Windows host.
- **`maia-hdl/test_cocotb/iq_packer/`** -- new cocotb scaffold (Makefile,
  verilog.py, tb.v, test_iq_packer.py) ready to run in WSL Ubuntu or
  Docker as a CI step.
- **`doc/P25_ADDRESS_MAP.md`** -- new canonical address-map document.
  Single source of truth for DDR carve-outs, AXI-Lite register banks, and
  IRQ assignments. Per the doc-as-we-go discipline, written *before* the
  wiring code so the address-map decisions were committed in writing
  before they hardened.
- **Verification on Windows host:** 7/7 pysim tests pass. Full P25Core
  Amaranth elaboration succeeds. `build_hdl.sh --verilog-only --p25` (run
  in the project's Python 3.11 Docker container) regenerates `p25_core.v`
  (22046 lines, all 20 `m_axi_iq_*` ports declared), `p25.svd` (with the
  three new registers at `0x80`/`0x84`/`0x88`), and `p25-pac/src/lib.rs`
  via `svd2rust v0.33.5` cleanly. Phase 6D's Rust port can `use p25_pac::iq`
  immediately.
- **Pending verification:** Vivado bitstream synth (`build_fpga.bat --p25`,
  ~30-60 min on host Vivado 2023.2) and on-hardware smoke test (load
  bitstream, devmem to enable `iq_dma_control`, mmap `0x1900_0000`, dump
  sub-buffers, feed to `tools/p25_lsm_demod.py`, check NAC accuracy).

Phase 6C ends at "gateware logic verified, Verilog regenerates, register
PAC regenerates". Phase 6D -- Rust port of the LSM demod to the Cortex-A9,
fed by this new IQ ring -- is the next step on the ladder.

---

## [2026-04-09] Phase 6B: P25 NID BCH(63,16,11) FEC Validated Against SDRTrunk

**Branch:** fishball-p25

Phase 6B is complete. NID forward error correction is now in the validated
Python reference, and on the better-signal test recording our prototype
produces an exact sync count match to SDRTrunk (313/313) with 100% NAC
accuracy after FEC. See `doc/changes/012_p25_nid_bch_fec.md` for the full
write-up.

- **`tools/p25_nid_fec.py`** -- new ~280-line standalone module. Encoder
  is verbatim from SDRTrunk's `BCH_63_16_23_P25_Test.java` (16-row
  generator matrix in octal + 5-line systematic encoding loop). Decoder
  uses maximum-likelihood nearest-neighbour search across the 65,536-entry
  codebook -- mathematically identical to BCH decoding within the unique-
  decoding sphere, ~30 lines vs ~600 for a Berlekamp-Massey + Chien search
  port from the Linux-derived `BCH.java` base class.
- **Encoder bit-perfect**: `encode_nid(NAC=1, DUID=0)` produces
  `0x00103185B7E9E224` exactly, matching SDRTrunk's documented test vector
  in `BCH_63_16_23_P25_Test.java:38`.
- **Decoder bit-perfect**: synthetic 1-11 bit error injection at all
  positions, 100 trials each: 1100/1100 corrected. At 12 errors,
  200/200 declared uncorrectable -- exactly the (63,16,d=23) bound.
- **`tools/p25_lsm_demod.py`** -- BCH FEC integrated into both sync
  detector paths. `SyncEvent` now carries `nid_raw`, `nac_fec`, `duid_fec`,
  `fec_errors` (-1 if uncorrectable). New "after BCH(63,16,11) FEC" section
  in the report shows correctable count, bit-error histogram, and the
  "NAC among correctable" metric (the right way to measure FEC quality
  while ignoring false sync hits).
- **End-to-end validation**:
  - 175119 wav (better signal): 313/313 syncs, 313/313 correctable,
    313/313 NAC=0x8A1 after FEC. **Exact match to SDRTrunk truth log.**
  - 163748 wav (noisier): 339/335 syncs, 330/339 correctable, 330/330
    NAC=0x8A1 *among correctable*. The 9 uncorrectable events are
    dominated by 4 false sync hits beyond truth + 5 PLL-slip events;
    not a FEC bug.
- **Pyradio evaluation**: started this session by reading the user's
  pre-existing pyradio P25 port at `~/Downloads/sdrtrunk-master/docs/pyradio/`
  to assess whether to merge it. Two findings: (1) pyradio's LSM demod
  loop is algorithmically equivalent to ours but uses `MAX_PLL = π`
  instead of SDRTrunk's `π/3` (deviation comment cites Pluto crystal
  offset); ours is more faithful. (2) **pyradio's `decode_p25_nid`
  uses RS(24,12,13) over GF(2^6), NOT the BCH(63,16,11) that SDRTrunk
  actually implements**, has zero unit tests for the FEC, and the
  pyradio author's own skeleton at `p25_pure_python.py` mislabels which
  code goes where. Per the project mandate ("if pyradio diverges from
  SDRTrunk, prefer SDRTrunk"), this session ports BCH directly from
  SDRTrunk Java rather than trusting pyradio's RS substitute.
- **TSBK parser deferred**: cherry-picking pyradio's TSBK parser would
  require ~1000 lines of trellis decoder + deinterleaver + CRC + opcode
  dispatch + an event/identifier framework. Significantly bigger lift
  than the FEC was. Recommend a dedicated future session.

## [2026-04-09] Phase 6: P25 LSM Demodulator -- Validated Python Reference

**Branch:** fishball-p25

The Fishball P25 target site is **LSM Simulcast**, not C4FM. All P25 systems
within RF range of the user's location are LSM. The existing C4FM-only Phase 1
gateware cannot decode LSM regardless of how we tune the existing slicer --
LSM is pulse-shaped CQPSK with data in carrier *phase*, requiring an RRC
matched filter and a decision-directed PLL that the current architecture
lacks. See `doc/changes/011_p25_lsm_python_reference.md` for the full
diagnosis and the validated Python reference port.

This change establishes the project's new direction:

- **Read SDRTrunk's LSM source line by line** (`P25P1DecoderLSM.java`,
  `P25P1DemodulatorLSM.java`, `Dibit.java`, `P25P1MessageFramer.java`,
  `P25P1SoftSyncDetector.java`, `BCH_63_16_23_P25.java`) and document
  every constant and DSP block.
- **`tools/p25_lsm_demod.py`** -- new self-contained ~900-line Python port
  of the full LSM chain: half-band decimation -> Parks-McClellan baseband
  LPF -> RRC matched filter -> demod loop with AGC + PLL + Gardner TED +
  atan2 slicer -> hard + soft sync detectors -> status-aware NID extractor
  (skip dibit 11). Variable names mirror the Java source for direct diff.
  Includes a 6-panel Matplotlib `--plot` dashboard for visual diagnosis.
- **Validated bit-exact against SDRTrunk** on a 27-second .wav recording
  the user captured via their Pluto + SDRTrunk + custom pluto_server.py
  bridge: 339 sync events vs 335 in SDRTrunk's truth log (101.2% recall),
  91% at perfect Hamming distance 0, NAC = 0x8A1 in 93.5% of detections,
  DUID = 0x7 in 95.9% of detections. Symbol rate within 0.006% of nominal.
- **`tools/monitor_p25_decoder.py`** -- new tool for live polling of the
  on-target Fishball `/api/stats` and `/api/dibit_dump` endpoints into
  JSONL snapshots. Used during the failed slow-convergence-tracking
  detour but kept as the standard "watch the decoder over time" utility.
- **`maia-hdl/p25_hdl/p25_top.py`** -- corrected the docstring's wrong
  claim that the existing slicer is "unified for C4FM and LSM". It is
  not. Documented the architectural delta (RRC + PLL missing) and the
  recommended fix path (PS Rust -> HDL).
- **DEVPLAN.md** -- new Phase 6 with a 5-step ladder (6A Python reference
  done, 6B BCH FEC, 6C IQ DMA in FPGA, 6D Rust on PS, 6E HDL/PL final).
  Each step locks in a fixed reference for the next, so each one has at
  most one degree of freedom and a known-good target.

The validated Python reference unblocks the rest of the project. Phase 6B
through 6E now become mechanical port-and-test exercises with bit-exact
targets, instead of "design and pray."

## [2026-04-09] Phase 5 (final): P25 Decoder Observability + Init Hardening

**Branch:** fishball-p25

A bundle of small but high-leverage diagnostic-infrastructure changes that
turned the P25 decoder from a black box into something we could actually
debug from a browser. See `doc/changes/010_p25_decoder_observability.md`
for the per-item rationale.

The headline finding: `S60p25-httpd` was launching the daemon via
`start-stop-daemon -b`, which detaches the process and closes its standard
streams under busybox. **Every `tracing::info!` we have ever written has
been going straight to /dev/null.** Fixed by wrapping the binary in
`sh -c 'exec ... >> /var/log/p25-httpd.log 2>&1'`. The same bug exists in
`S60maia-httpd` (Maia's init script) and is worth fixing in `fishball-dev`
in a follow-up.

Other items in this bundle:

- **Explicit `EnvFilter` setup** in `main.rs` so the default tracing
  filter is `info,p25_httpd=info` and not whatever `fmt::init()`'s
  undocumented default is.
- **`/api/stats` exposes AD9361 RX gain + RSSI.** No more "ssh in and
  cat sysfs" while debugging.
- **`/api/dibit_dump` exposes inner/outer histogram percentages and a
  raw on-air DUID histogram.** The DUID histogram in particular was
  the diagnostic that broke open the LSM debug session -- it showed a
  near-uniform spread across all 16 nibble values (TSDU at 4-5%
  instead of expected ~100%), immediately implicating the demodulator
  architecture.
- **`SYNC_THRESHOLD` widened from 4 to 10** with a long comment
  explaining why and when it should drop back. Temporary diagnostic
  measure -- not a fix.
- **Periodic `expire_grants(30)`** task in `main.rs` so the dashboard's
  Active Grants count doesn't grow forever once decoding works.
- **NID DUID hardcode hack** in `decode_nid` to flush out downstream
  bugs faster while the demod is still broken. Diagnostic raw_duid
  histogram preserves the actual on-air values so we can observe the
  bit-error pattern. Replaced by proper BCH(64,16) FEC in change 011's
  follow-up work (Phase 6B).
- **Category-1 cleanups**: drop unused imports (`put`, `TsbkMessage`),
  module-level `#![allow(dead_code)]` for traffic-following placeholder
  code, remove dead `tracing::debug!` in `fpga.rs`, fold the NCO
  frequency into the existing `configure_ddc` info log.

## [2026-04-09] Phase 5 (cont.): Build Verilog Staleness Detection

**Branch:** fishball-p25

Added automatic staleness detection for Amaranth-generated IP Verilog in
`build_fpga.bat`. Previously the script only checked whether `p25_core.v` /
`maia_sdr.v` existed, so any edit to `p25_hdl/*.py` or `maia_hdl/*.py` after
the first build would silently bake pre-edit logic into new bitstreams that
looked fresh by mtime. See `doc/changes/009_build_verilog_staleness.md`.

This was discovered while debugging why the P25 dibit slicer fix (symbol-rate
differential, commits 3f1bff7 and 94faae9) wasn't affecting the on-target
FPGA behaviour. The bitstream was built 15 minutes *after* the fix was
committed, but Vivado had picked up the old `p25_core.v` generated 90 minutes
*before* the fix. The symbol-rate slicer logic never made it into the
bitstream, and the on-target dibit histogram exactly matched the pre-fix
failure mode fingerprint recorded in the `p25_top.py` docstring.

- **New:** `tools/check_verilog_stale.ps1` -- PowerShell helper that compares
  generated `.v` mtime against the maximum mtime of `*.py` files under one or
  more source directories. Outputs `MISSING`, `STALE`, or `FRESH`.
- **Changed:** `build_fpga.bat` Step 2 now calls this helper for both the
  Maia SDR and P25 IP Verilog. P25 checks against both `p25_hdl` and
  `maia_hdl` (since `p25_top.py` imports DDC, registers, DMA, and CDC from
  `maia_hdl`). On `STALE` or `MISSING`, it automatically invokes
  `build_hdl.bat --verilog-only [--p25]` in Docker before running Vivado.
- **No API change:** users still run `build_fpga.bat --p25` as the single
  command. The `--verilog-only` flag on `build_hdl.bat` remains as an
  internal mechanism and is no longer user-facing.

---

## [2026-04-09] Phase 5: Build Pipeline, Register CDC, Ring DMA, First Hardware Boot

**Branch:** fishball-p25

First successful hardware boot of the P25 FPGA bitstream on the Fishball Z7020,
followed by a same-day refactor of the DMA path from one-shot to a ring buffer
architecture. Also fixed multiple build pipeline issues from the standalone
repo migration, corrected the SVD register map, added DDC FIR coefficient
initialization, and fixed a register clock-domain-crossing bug.

### Build Pipeline Fixes

- Fixed P25 Verilog generation to use Docker (ext4 filesystem avoids NTFS pip issues)
- Fixed CMD escaping in `build_fpga.bat` for Docker invocation
- Added missing ADI library builds: `util_clkdiv`, `util_rfifo`, `util_wfifo`
- Fixed stale TCL/Makefile paths left over from standalone repo migration
- Added `.gitattributes` enforcing LF line endings on `.sh` files
- Added skip-if-built logic for incremental ADI library builds (and ADI library packages)
- `build_hdl.sh` now auto-generates `p25.svd` when `--p25` is set and regenerates `p25-pac/src/lib.rs` via a downloaded `svd2rust` binary, so SVD/PAC stay in sync with Amaranth HDL without manual steps

### Tezuka Firmware Fixes

- Added XSA cache invalidation in `build.sh` (detects when source XSA is newer than cached package and forces a package rebuild)
- Added p25-httpd / maia-httpd source change detection for auto-rebuild
- Removed phantom AXI UART Lite from `fishball-p25.dtsi` (was at 0x42C00000, inherited from the Maia DTSI but not present in the P25 FPGA design -- caused a kernel panic in `uartlite_probe`)

### Register Map Fix

- SVD register offsets were wrong: FPGA uses bank select bits [4:3] of word address giving byte offsets 0x00/0x20/0x40/0x60, but SVD had 0x00/0x08/0x20/0x30
- Fixed `RegisterMap` in `p25_top.py`, regenerated SVD and PAC

### Register Clock-Domain-Crossing Fix

- `demod_registers` and `traffic_registers` were instantiated in the `s_axi_lite` clock domain (100 MHz) but their Wpulse outputs fed modules running in the `sync` domain (62.5 MHz)
- The unsynchronized Wpulse edges were missed ~38% of the time due to clock skew, causing the DMA start signal to be silently dropped
- Wrapped both with `RegisterCDC`, matching the existing `sdr_registers_cdc` pattern already used for the DDC bank

### DDC FIR Coefficient Initialization

- Added 3-stage FIR filter coefficient loading to p25-httpd (`fpga.rs`, `configure_ddc()`)
- Previously only NCO frequency and enable bits were programmed
- P25 channel filter: 128x decimation (16x4x2), 8 MSPS -> 62.5 kSPS (13 samples/symbol)
  - Stage 1: 48 taps, 200 kHz cutoff, /16
  - Stage 2: 32 taps, 50 kHz cutoff, /4
  - Stage 3: 64 taps, 8 kHz cutoff, /2
- Coefficients: Kaiser window (designed via `scipy.signal.firwin`), 18-bit quantized, >137 dB stopband

### Ring-Buffer DMA Rework

- Replaced one-shot `DmaStreamWrite` with a new `DmaStreamRingWrite` module (ported from the never-merged upstream IQ stream branch)
- 8 sub-buffers x 4 KB = 32 KB ring per DMA channel
- Sub-buffer completion raises an interrupt on the AXI B-channel write response
- New `demod_status.last_buffer` field (3 bits) tracks which sub-buffer was just written
- Continuous operation -- no PS-side restart per buffer
- `demod_control` / `traffic_demod_control` simplified: start/stop Wpulses removed, only `demod_enable` (RW level) remains
- `demod_status` / `traffic_demod_status` grow the `last_buffer` field; all other bank offsets unchanged (`ddc_frequency`=0x2C, `ddc_control`=0x30, `demod_status`=0x40, etc.)
- SVD and PAC regenerated

### DMA Address Layout

- FPGA hardcodes physical buffer addresses `0x17000000` (dibit) and `0x18000000` (traffic)
- Reduced from 1 MB each to 32 KB each to match the new ring size
- Device tree updated: `reg = <0x17000000 0x8000>` and `<0x18000000 0x8000>`

### Hardware Test Results

- Board booted with P25 bitstream, FPGA registers accessible
- Product ID: 0x70323566 ("p25f")
- AD9361 configured: 858.1 MHz center / 8 MSPS
- Dibit counter incrementing (DSP chain active)
- Web UI served on port 8080
- SDRTrunk confirmed P25 signal at 860.9625 MHz (NAC:2209, WACN:781824, System:2208)

### Unified C4FM/LSM Differential Demod

- Local control channel turned out to be LSM (CQPSK), not C4FM. The original FM-only cross-product discriminator could not produce LSM dibits 1 and 3
- Refactored `C4FMDemod` to compute the full complex differential product `z[n] * conj(z[n-1])` (4 DSP48E1 multiplies instead of 2). Outputs `diff_re_out` and `diff_im_out`; `disc_out` retained as a legacy alias for `diff_im_out`
- Refactored `SymbolTimingRecovery` to take both `diff_re_in` and `diff_im_in` and use a sign-bit slicer that maps the four quadrants of the differential plane to the four P25 dibit values, matching SDRTrunk's `P25P1DemodulatorLSM.toDibit` exactly. Works for both C4FM and LSM with no path divergence
- `p25_top.py` wires the new IQ pair through both control and traffic chains
- Updated `test_c4fm_demod.py` and `test_symbol_timing.py` to drive and verify the new I/Q differential interface; all 16 P25 HDL tests pass

### p25-httpd Diagnostic Logging

Added structured tracing and a `/api/dibit_dump` endpoint to make hardware bring-up debuggable from the web UI without devmem on the target.

- `target=p25_irq` -- IRQ arrival counter (first 10, then every 64th)
- `target=p25_reader` -- per-wakeup buffer count, byte total, dibit histogram
- `target=p25_stats` -- FPGA register state (`dibit_count`, `overflow`, `last_buffer`, `next_addr`) every 2 s
- `target=p25_decoder` -- periodic dibit histogram + sync correlator stats, near-sync events (Hamming distance ≤ 12), state transitions, NID Golay decode results
- `GET /api/dibit_dump` -- JSON: `total_dibits`, per-value histogram with percentages, sync correlator stats (`hits`, `near_misses`, `best_distance`), and the last 2048 dibits packed as hex
- Dashboard: two new cards ("Dibit Histogram", "Sync Correlator") that poll `/api/dibit_dump` and surface the histogram + best Hamming distance live

---

## [2026-04-08] P25 Migration into maia-sdr

**Branch:** fishball-p25

Migrated the P25 trunking radio from the standalone `fishball-p25` repo into
the maia-sdr tree. The standalone approach failed due to relative path breakage,
IIO DMA not routed, and DTS/bitstream mismatches.

### P25 FPGA Gateware (Phases 0-1)

- `maia-hdl/p25_hdl/` -- Amaranth HDL modules: C4FM demod, Gardner symbol timing, dibit packer
- `maia-hdl/ip/p25-core/` -- Vivado IP packaging (TCL + constraints)
- `maia-hdl/projects/fishball7020_p25/` -- Vivado project (block design, constraints, top-level wrapper)
- `maia-hdl/test/` -- 14 Amaranth simulation tests (C4FM, symbol timing, dibit packer)
- `maia-hdl/generate_p25_svd.py` -- SVD generation for P25 register PAC
- Dual DDC + demod chains: control channel + traffic channel with independent NCO
- Spectrometer + recorder removed (unused in P25), freeing ~16 DSP48, ~5K LUT, ~12 BRAM
- Resource usage: 36 DSP48 (16%), 15.6K LUT (29%), 16 BRAM (11%) on Z7020

### P25 Firmware (Phases 2-3)

- `p25-httpd/` -- Rust workspace with `p25-json` and `p25-pac` sub-crates
- Control channel decoder: frame sync, NID extraction, TSDU de-interleave, TSBK parser
- FEC: Golay(23,12), trellis Viterbi, CRC-16-CCITT
- 6 TSBK opcodes: GRP_V_CH_GRANT, GRP_V_CH_GRANT_UPDT, IDEN_UP, NET_STS_BCST, RFSS_STS_BCST, ADJ_STS_BCST
- Traffic manager: grant lifecycle, NCO word calculation, timeout management
- Web dashboard: REST API (6 endpoints), WebSocket events, embedded SPA
- 16 Rust unit tests passing

### Build Infrastructure

- `build_fpga.bat --p25` builds P25 FPGA (fishball7020_p25)
- P25 Tezuka config: `fishball_p25_7020_defconfig`

See `doc/changes/003_p25_scaffolding.md` through `doc/changes/007_p25_traffic_channel.md` for details.

---

## [2026-04-07] Upstream Sync

**Branch:** fishball-dev

Rebased `fishball-dev` onto `upstream/main`, picking up 10 new commits from
F5OEO: Vivado 2023 env, WASM broadcast channel, interpolation coefficients,
overclock simplification, FIFO redesign, clock simplification, x32 interpolator,
RX2 fix, DAC FIR support, RX with FIFO.

Also analyzed the upstream `refactor` branch (53 commits, 18K lines, not yet
merged to main) -- includes board-based project layout, common TCL library,
DVB-S2 receiver, IQ burst capture, frequency sweeper, multi-board sync.

See `doc/changes/002_upstream_sync.md` for full analysis.

---

## [2026-03-08] Project Setup

**Branch:** fishball-dev

### Fork & Repository

- Forked `F5OEO/maia-sdr` -> `andylee77/maia-sdr`
- Created `fishball-dev` branch for Fishball Z7020 development
- Set up upstream tracking: `upstream` -> `F5OEO/maia-sdr`, `origin` -> `andylee77/maia-sdr`

### Build Infrastructure (Initial)

- In-repo build scripts (see `doc/changes/001_build_scripts.md`):
  - `build_hdl.bat/.sh` -- Amaranth -> Verilog + SVD via Docker
  - `build_fpga.bat` -- Full Vivado FPGA synthesis (Windows native)
  - `sim_hdl.bat/.sh` -- HDL simulation via Docker (Amaranth + cocotb)
  - `clean.bat` -- Clean all build artifacts
- Docker volume `maia-hdl-build` for persistent Python venv cache
- Tezuka firmware pointed to this fork's `fishball-dev` branch

---

## Pending / Future

- [ ] Phase 5 (remaining): Live control channel decode, traffic following test, SD card clean boot
- [ ] Voice frame extraction (LDU1/LDU2 -> IMBE frames)
- [ ] Audio codec (mbelib, codec2, or DVSI)
- [ ] Document IQ streaming patches to maia-httpd
- [ ] FPGA ring buffer implementation for high-bandwidth IQ
