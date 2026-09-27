# 056 — Web UI review and redesign

**Date:** 2026-09-26. **Branch:** fishball-p25. **BUILD_TAG:** `2026-09-26-web-ui-056`.
**Bake required:** NO — p25-httpd only. Builds on 054 (low-latency dibits, air-time gating)
and 055 (autoppm sign).

The request (Andy): check whether the 054/055 fixes also fixed the channel tracking and
what the web page shows, review and simplify the page, make saving recordings switchable,
and redo the presentation of P25 tracking and logging.

Short answer: 054 fixed the data. Per-call audio attribution is right now; every PTT's WAV
has exactly its frames on the bench. Most of what still looks wrong on the page is
presentation. The page mixes three different notions of "the current call", shows the
lifecycle's 10 s hang as an active call, and reads counters that never reset. A few real
bugs sit behind the page too; they are listed in 1.8 and fixed where small.

Part 1 is the review, Part 2 the design, Part 3 what was built.

## Part 1 — Review

### 1.1 Method

- **Live bench, read-only.** Unit A ran build `2026-09-26-dibit-lowlatency-airtime` on the
  28 s cabled replay of Clay County (NAC 8A1, CC 860.9625). The replay carries TG 300 PTTs
  on 857.9875 (3436046 then 1014) and on 858.4375 (3406028 then 1014), plus a carrier on
  858.4625. A 150 s capture polled GET endpoints at 0.5–15 s and tailed `/ws/events` and
  `/api/log`:
  - 0.5 s: `/api/stats`, `/api/traffic`, `/api/grants`.
  - 2 s: `/api/recordings`, `/api/grant_decode_stats`, `/api/dibit_delivery`, and others.
  - 15 s: identity, PPM, presets, pipeline, and others.
- **Code.** All of `dashboard.html` (5849 lines) and every handler it calls. Also the
  lifecycle, follower, grant_stats, recorder, forwarder and event log.
- **History.** `doc/DASHBOARD_CLEANUP.md` (the 20-item April catalogue), the 2026-04-2x
  diagnostics, and changes 036–054.
- **Offline harness.** Headless Chrome ran the legacy page against captured JSON. It
  reproduced the page's own JS errors and failed no request to the board.

### 1.2 Inventory of the pre-056 dashboard

One file, `p25-httpd/src/httpd/dashboard.html`: 180 lines of CSS, ~800 of markup, ~4800 of
JS, and 5 tabs. Poll timers run regardless of tab unless noted (`dashboard.html:5150-5167`).

| Tab / panel | Shows | Endpoint | Rust state behind it | Poll |
|---|---|---|---|---|
| Header | build, site selector, "Tracking/Searching" dot | `/api/system`, `/api/stats`, `/api/sites` | decoder `system`, `wacn.is_some()` | 2 s (`refresh`) |
| Radio · Board Info | build (again), uptime, wall clock, NAC/WACN, RX LO, BW, gain, RSSI, SR, DDC, "Audio WS clients", WS lag | `/api/stats`, `/api/system` | AD9361 sysfs, atomics | 2 s |
| Radio · tune widget | preset, radio freq + steps, centre auto/lock | `/api/presets`, POST `/api/preset`, POST `/api/tune` | `current_*` atomics | on load |
| Radio · PPM rows | shift, ppm, cal age, tracker estimate, auto + anchor, recalibrate | `/api/ppm`, POST `/api/ppm/auto`, `/api/ppm_calibrate` | autoppm atomics | 2 s |
| Radio · modulation, AGC/gain | selector + NID rates; AGC checkbox, gain dropdown | GET `/api/modulation?set=`, GET `/api/rx_gain?mode=&db=` | `active_modulation`, AD9361 | 3 s / 2 s |
| Radio · PS Cores | per-core %, 20 threads | `/api/ps_cores?top_n=20` (sleeps 250 ms server-side) | /proc | 2 s |
| Radio · Grant Follower | state, TG, freq, enc, grants seen, retunes, enc skipped, last DUID, last retune | `/api/traffic` | `TrafficChain`, forwarder atomics | 2 s |
| Radio · Traffic Channel | big parked freq, batch TG, DUID, NAC, offset, sync, PLL, AGC, modulation, IMBE queue, lock checkbox | `/api/traffic`, GET `/api/traffic?lock=` | `TrafficChain`, traffic LSM regs | 2 s |
| Radio · IMBE + Vocoder | "CURRENT CALL" TG/src/duration, HDU/LDU/TDU, IMBE ext/drop, PCM/err/silent; session totals | `/api/traffic` `imbe.current_call` | forwarder `call_baseline_*` (reset on each HDU) | 2 s |
| Radio · TG Monitor | roster checkboxes, apply | `/api/grant_map`, `/api/monitor` | `TrafficChain::grant_map`, `MonitorList` | 10 s |
| Radio · Frequency Map | empty (hard-coded plan removed, API path never written) | — | — | — |
| Radio · Active Grants | 0–1 row: channel, TG, source, freq, age | `/api/grants` | lifecycle `active_call_snapshot` | 2 s |
| Radio · Frequency Bands | IDEN table | `/api/bands` | decoder `bands` | 2 s |
| Radio · Recent Calls | 200 grant summaries joined to 40 WAVs by call_id; start, TG, src, freq, first IMBE, duration, LDU, IMBE, vocoder, AGC, player, Events | `/api/grant_decode_stats[?include_enc=1]` (147 KB), `/api/recordings` (23 KB) | `grant_stats` rings, `RecordingStore` | 5 s + `recording_saved` ws |
| Radio · Log Tail (opt-in) | text tail | `/api/log?since=` | `EventLog` | 1–5 s |
| Radio · Live Activity | ws feed with 25 type filters | `/ws/events` | decoder `event_tx`, forwarder | push |
| Logs | event log with 5 category checkboxes | `/api/log?since=&limit=200` | `EventLog` | 1 s (Logs tab) / 5 s |
| Plots | wideband / narrowband spectrum, constellation, eye, deviation, distribution | `/api/spectrum_wide`, `/api/spectrum`, `/ws/iq`, `/api/deviation`, `/api/distribution` | HDL spectrometer, IQ rings | 0.5–1 s (Plots only) |
| Debug | pipeline dump (opt-in), decoder matrix, identity, decode stats, HDL LSM, IRQ, NID ring, LSM dibit histogram | `/api/pipeline`, `/api/decoder_compare`, `/api/hdl_lsm`, `/api/irq_stats`, `/api/control_lsm_dibit_dump` | heartbeat, decoders | 2 s (Debug only) |
| API | endpoint catalogue | `/api/endpoints` | `ENDPOINT_CATALOGUE` | once |

Load on the Radio tab: about 40 KB/s of JSON the board serialises and the browser parses.
The Recent Calls poll alone is 170 KB every 5 s, whether or not anything changed.

### 1.3 The core problem: three "current calls"

The page reads three independent authorities and labels all of them "the call".

1. **Lifecycle** (`app/grant_follower.rs` `spawn_call_lifecycle`). This is the call
   identity everything else keys on: call_id, recordings, grant_stats. One call per
   primary grant ("grant = call"). It closes on a new grant (`tg_change`) or after
   `IDLE_TIMEOUT_MS` = 10 s with no audio, HDU or CC update (`Timeout`). It feeds
   `/api/grants` and `/api/stats.active_grants`.
2. **Follower** (`TrafficChain` + forwarder atomics `current_talkgroup`, `current_source`,
   `current_frequency_hz`, `call_encrypted`). This is what the chain is tuned to. It goes
   Idle only on the lifecycle's CallClose. It feeds `/api/traffic`
   `state` / `current_talkgroup` / `current_frequency_hz` / `current_call_encrypted`.
3. **Per-PTT vocoder baselines** (`ImbeForwarder::call_baseline_*`, snapshotted in
   `on_hdu`). These feed `/api/traffic` `imbe.current_call`, which the "CURRENT CALL" box
   renders. The baselines are never cleared at call end, so `duration_ms` keeps growing
   after the call (`api/traffic.rs:460-491`).

What that looked like live (capture timeline, 2026-09-26, `t` in seconds):

| t | Grant Follower card | Active Grants | CURRENT CALL box (`dashboard.html:1957`) |
|---|---|---|---|
| 0.1 | Active, TG 300, 857.9875 | TG 300 / 1014 / age 7 s | "TG 300 · src 1014 · 7.1 s", counts 72 IMBE (voice ended 5.6 s earlier) |
| 5.7 | Idle | none | "TG ? · 12.7 s", still green, still 72 IMBE (no call at all) |
| 8.5 | Active, TG 300, 858.4375 | TG 300 / 3406028 | "TG 300 · src 3406028 · 15.5 s", 72 IMBE: the previous PTT's numbers under the new call |
| 9.0 | same | same | "… · 0.3 s", 18 IMBE (the new HDU reset the baselines) |
| 18.5–33.6 | Active for 11 s after the last voice frame (t = 22.5) | TG 300 / 1014, age counting | counting |

- **HDU always reads 0** in the box. The baseline is taken in `on_hdu` after the HDU is
  counted.
- The **"Active" state and the Active Grants row persist for the whole 10 s hang**. This
  is by design (the lifecycle keeps the call for late UPDs / audio). The page does not
  show it as hang; it looks like a stuck call.
- On this loop, the second 1014 PTT on 858.4375 is shown as active for 5.5 s after its
  last voice, until the next loop's grant pre-empts it.

Verdict: tracking is **stale-by-design** (10 s hang) plus **wrong presentation**. The
CURRENT CALL box mixes baselines and has an idle state that never clears.

### 1.4 Findings — call and channel tracking

| # | Finding | Evidence | Class |
|---|---|---|---|
| T1 | Hang time shown as an active call: "Active", age counting, green, up to 10 s after the last voice | timeline above; `grant_follower.rs` `IDLE_TIMEOUT_MS`; `dashboard.html:1904-1931` | presentation of a by-design state |
| T2 | CURRENT CALL box shows a PTT that ended long ago, "TG ?" while idle, and the previous PTT's counts on the next call | `dashboard.html:1956-1976`; `api/traffic.rs:443-491` | presentation (stale counters) |
| T3 | "Duration" in Recent Calls is the lifecycle's open time. A 1.44 s PTT shows as 12.9 s (timeout close) or 7.3 s (pre-empted during hang); `air_duration_ms` 2.3–2.8 s | `/api/grant_decode_stats` capture; `dashboard.html:4991` | presentation (wrong field) |
| T4 | Start times rendered from a board clock that was never set (1970): "Started 20:33:34" looks real but is 01:33 UTC on 1 Jan 1970. The page only sets the clock when someone loads it (POST `/api/set_time` on every load) | `/api/stats.wall_clock` `1970-01-01 01:33:51`; recording names `rec_5614677_…` | presentation / ops |
| T5 | Every PTT is its own call, including the dispatcher's reply on the same grant channel. 4 calls per 28 s loop, as designed (2026-04-30 "grant = call") | grant_stats: 72 / 81 / 72 / 72 IMBE per loop = SDRTrunk's 297 | correct |
| T6 | Grant Follower "Grants Seen" counts every grant TSBK decode (3 per TSDU); `grants_seen_new` is the de-duplicated one | `api/traffic.rs:676-682` | presentation |
| T7 | Channel id format differs: `/api/grants` and recordings say `1189`, ws / log say `0-1189` | captures | cosmetic |
| T8 | Legacy **Debug tab throws** `TypeError` on entry. `scheduleSpectrum()` reads `#spec_rate`, retired with the spectrum card. The rest of the tab-switch hooks do not run (e.g. `plotStop` when leaving Plots) | harness: `dashboard.html:3739` via `:3947` | legacy bug (left as is) |

What 054 fixed that the page did show wrongly before:

- Grants now reach the follower 0.1–0.2 s after air, not up to 3.4 s. First-voice
  latency on the bench is 16–256 ms (was 3–3.5 s cold).
- The chain is no longer released by a rejected grant's synthetic CallClose, so a
  followed call no longer drops to Idle mid-call on a busy site (054 §4).
- Audio is attributed by air time. The bench shows every WAV at exactly its PTT's frames
  (1440 ms = 72 frames, 1620 ms = 81), and no cross-call bleed. The "(no audio)" rows
  and the orphan recordings from April were symptoms of the 3.4 s delivery.

### 1.5 Findings — recordings

| # | Finding | Evidence | Class |
|---|---|---|---|
| R1 | Per-recording counters in `/api/recordings` are global-counter deltas taken at FINALISE, 2 s after the close. By then the next call's frames are counted too: 72-frame PTTs report `imbe_extracted` 144 / 153, LDU 8 / 17 | `/api/recordings` capture (recs 247–256) vs grant_stats 72 / 81; `recorder.rs` `finalize()` | real bug, **fixed** (R1 fix: snapshot at the close) |
| R2 | Recording is always on, a fixed ring of 40 in tmpfs. No switch, no retention setting, lost on reboot | `recorder.rs` `MAX_RECORDINGS` | missing feature, **added** |
| R3 | The page pulls both rings in full every 5 s (170 KB) and rebuilds the table with `innerHTML` whenever the fingerprint changes (then tries to restore a playing `<audio>`) | `dashboard.html:4772-5061` | presentation / load |
| R4 | "Events" per call downloads the whole 16384-entry log (`/api/log?limit=16384`, ~2 MB) to filter one call client-side | `dashboard.html:4733` | load |
| R5 | No reason shown for a missing WAV beyond "(no audio)" / "(no IMBE)" / "(encrypted)": saving in progress, rotated out and recording off all look the same | `dashboard.html:4944-4965` | presentation |

### 1.6 Findings — logging

| # | Finding | Evidence | Class |
|---|---|---|---|
| L1 | The `grant` category is mostly the control channel's TSBK mirror (ACK_RESP, U_DE_REG_ACK, GRP_VCH_GRNT_UPD…), one line per decoded TSBK: 4055 of 6503 ring entries. The follower's own grant decisions are 509 | log capture; `tsbk_handlers.rs:228-237` | naming / volume (tools depend on it: `tools/p25_audit_capture.py`) |
| L2 | A first read `/api/log?limit=N` returns the OLDEST N entries (boot time). The Logs tab then sets `lastSeq = d.last_seq` and jumps to the end, so it shows boot entries and skips everything between | `EventLog::recent_since`; `dashboard.html:3104-3111` | real bug in the reader semantics, **fixed** (`?tail=1`) |
| L3 | Logs tab category checkboxes use `imbe`; the category was renamed `voice` on 2026-04-25. `recorder` and `duid` have no checkbox and are always shown | `dashboard.html:952-956, 3043-3050` | presentation |
| L4 | Live Activity filters use `GRP_GRANT`, `GRANT_UPD`, `NET_STS`… but the events are `GRP_VCH_GRANT`, `GRP_VCH_GRNT_UPD`…, so grants never appear in the feed | `dashboard.html:906-913`; `tsbk_handlers.rs:295-666`; ws capture | presentation |
| L5 | Recorder lines are just `call_saved` / `call_finalise`, with the meaning in fields | log capture | presentation |
| L6 | `vocoder call_start` / `call_end` fire only on a TG change, so on a one-TG site the vocoder category is mostly `agc_reset` | log capture | noted |

### 1.7 Findings — counters and labels

- **"Audio WS clients"** is `audio_tx.receiver_count()`, which includes the recorder and
  the call lifecycle. It reads 2 with nobody listening (`api/radio.rs:226`). Fixed:
  `/api/ui/state` `audio.listeners` is counted in the ws handler, and the doc comment of
  the old field now says what it is.
- **"Messages"** on Debug is `recent_messages.len()`, capped at 1000; it always reads 1000.
  `dibit_count` / `overflow` / `dma_next_address` in `/api/stats` are retired and always 0.
- **Phase labels** in the UI ("Phase 7C", `trf.modulation` sentence), the build shown
  twice, the IMBE+Vocoder "ACTIVE (N samples)" that never returns to idle: all in
  DASHBOARD_CLEANUP.md §1–§6, still present.
- `/api/pipeline` reported the grant ring capacity as a hard-coded 20 (it is 200).
  **Fixed.**
- `/api/spectrum?chain=control` centred on the BOOT control frequency, stale after
  `POST /api/tune`. **Fixed.**
- `/api/presets` says every preset outputs 62.5 kSPS; `/api/stats` computes 50 kSPS
  (`8 MSPS / 160`). `/api/spectrum` and the IQ-dump WAVs hard-code 62.5 kSPS. **Follow-up.**
- The aliases dialog wrote the C4FM decoder only (`api/talkgroups.rs` pre-056
  `put_aliases`). The LSM decoder, the active one on every site, never had aliases, so
  they never appeared anywhere. `P25_API.md` also claimed they were persisted in
  `~/.config`. They were not persisted at all. **Fixed.**
- About 1000 lines of legacy JS (IQ streams, live spectrum mode, eye, constellation for
  the Debug tab) reference elements that no longer exist.

### 1.8 What 054 fixed, what remains, what is presentation-only

| Issue | Status after 054 | 056 |
|---|---|---|
| Grants and audio 0–3.4 s late; tails cut; cross-call bleed; "(no audio)" rows; orphan WAVs | **fixed by 054** (99.9 % of SDRTrunk's frames, exact WAVs) | — |
| Followed call dropped on a rejected grant's synthetic close | **fixed by 054** | — |
| Hang shown as an active call (T1) | remains, by design | presentation: phase `voice` / `hang` / `acquiring` + close countdown |
| CURRENT CALL box stale / mixed (T2) | remains | presentation: the new UI uses the lifecycle only; `/api/traffic` unchanged for tools |
| Duration = open time (T3) | remains | presentation: voice = WAV length or IMBE × 20 ms; open time and air time in details |
| 1970 clock (T4) | remains | UI shows board-relative ages when the clock is invalid, and sets it only when wrong |
| Per-recording counters double-count (R1) | remains | **backend fix** |
| No recording switch / retention (R2) | — | **backend + UI** |
| Log first read shows boot (L2) | remains | **backend** `?tail=1`, `?tsbk=0`, `?from_ms/to_ms` |
| Aliases on the wrong decoder, not persisted | remains | **backend fix** + persistence |
| TSBK mirror in `grant` (L1) | remains | UI groups them as "CC messages" (off by default, filtered server-side); category kept for tools |
| Live Activity / Logs filter names (L3, L4), Debug-tab TypeError (T8), dead JS | remain | legacy page kept as is at `/legacy`; the new UI does not have these panels |
| Presets / spectrum / IQ-dump sample rate (62.5 vs 50 kSPS) | remains | follow-up |
| autoppm Stage A uses `boot_control_freq` (wrong after `/api/tune`) | remains | follow-up |

### 1.9 Settings: what is configurable and where it lives

| Setting | How | Persisted? |
|---|---|---|
| Active site | POST `/api/site` | yes, `/mnt/jffs2/p25-sites/active` |
| PPM calibration | `/api/ppm_calibrate`, PUT `/api/ppm` | yes, `/mnt/jffs2/p25-ppm-cal.json` |
| Auto-PPM on/off, anchor | POST `/api/ppm/auto` | no (process lifetime, default on / ±50 Hz) |
| Preset, tune, centre lock | POST `/api/preset`, `/api/tune` | no (boot from CLI / site preset) |
| RX gain / AGC mode, modulation | `/api/rx_gain`, `/api/modulation` | no |
| TG aliases | PUT `/api/aliases` | **was no** (and wrong decoder) → now yes |
| Monitor list | `/api/monitor` | **was no** → now yes |
| Encrypted TG lockout | learned; PUT `/api/encrypted_tgs` | no (by choice, see follow-ups) |
| Follower on/off, diagnostic lock | GET `/api/traffic?follower=&lock=` | no |
| Dibit delivery mode / poll | POST `/api/dibit_delivery`, CLI | no |
| Log verbosity | env `P25_LOG_VERBOSE` | env |
| Recording on/off, retention | — | **missing** → new, persisted |
| Radio-unit (source) names | — | **missing** → new, persisted |
| Theme, tab | browser localStorage | browser |

Also missing and not built: saving recordings to the SD card (62 GB free on unit A), and
per-TG record/priority rules. Both are follow-ups.

## Part 2 — Design

### 2.1 Goals

- One screen answers "what is on the air now, and what just happened": control-channel
  health, the current call with its real state (acquiring / voice / hang), recent calls with
  playback, and a reason whenever there is no audio.
- One authoritative state source, the call lifecycle. The page renders documents built
  on the board and does not reconstruct call state from half a dozen endpoints.
- RF and diagnostics stay available, but off the main path and polled only while open.
- Works on a phone: bottom tab bar, single column, no horizontal scroll, touch-sized
  controls.
- Self-contained: no CDN; the radio may have no internet.

### 2.2 Information architecture

| View | Contents |
|---|---|
| **Now** (`#now`, default) | *Now on air*: TG (+ name), source (+ name, other sources), frequency and channel, elapsed, voice received, phase badge (Acquiring / Voice / Hang), Encrypted and Rec badges, hang countdown bar. When idle: "No active call", where the traffic chain is parked, the last call and how long ago. *Site*: name, NAC / WACN / SYS / RFSS, health (decoding / silent / searching), CC frequency, TSBK/s, CRC-ok %, last TSBK age, modulation, traffic chain. *Recording*: on/off switch, retention and count, live listeners. *Recent calls*: newest first; time (wall clock when valid, else age), TG, source(s), frequency, voice length, first-voice latency; play (on demand), download, details (call id, open time + close reason, CC air time, IMBE / LDU, vocoder errors / silent, file). Filter by TG / source / name; "Not followed" chip includes encrypted / sticky / monitor-rejected grants. |
| **Radio** (`#radio`) | Control channel (tuned freq, LO, NCO, BW, SR, DDC; tune with steps; LO centre mode; preset; site), gain (AGC switch, manual gain, RSSI), frequency correction (applied shift / ppm, calibration age, tracker estimate, auto-track + anchor, recalibrate), modulation, spectrum (wideband HDL FFT with CC and traffic markers, control or traffic DDC). Every change asks for confirmation. |
| **Diagnostics** (`#diag`) | Decode chain (control / traffic framer, IMBE queue, vocoder, audio lag, recorder), traffic chain live (state, parked, DUID / NAC, PLL, AGC, sync distance), dibit delivery per ring (054: age p50 / p99 / max, clock uncertainty, resyncs, phase mismatches, reseeds, cuts, fed / gated / pre-settle), board health, event log, raw JSON links, link to `/legacy`. |
| **Settings** (`#settings`) | Recording (on/off, keep N, storage, free space), talkgroup names, radio names, monitor list (roster from `/api/grant_map`), encrypted TGs (remove / clear), clock (board vs browser, set, auto-set when wrong), this browser (theme, live push, show not-followed), about (build, settings file, load note, last save error). |

Left out on purpose: the HDL NID ring, IRQ counters, LSM dibit histograms, decoder matrix,
constellation / eye / deviation plots, and PS cores. They are bring-up tools. The legacy
page keeps them at `/legacy`, and all their endpoints are unchanged.

### 2.3 One state source

```text
lifecycle (grant_follower) ── ActiveCallSnapshot (mirrored every 100 ms) ─┐
grant_stats rings (clear 200 + enc/not-followed 50) ──────────────────────┤
RecordingStore (+ RecordingPolicy skipped ids) ───────────────────────────┤── app::ui_state (pure, host-tested)
ui_settings (aliases, recording) ─────────────────────────────────────────┤        │
control decoder (identity, TSBK counters) + RateWindow ───────────────────┘        ▼
                                              GET /api/ui/state (~1.1 KB)   GET /api/ui/calls (~24 KB / 40 calls)
```

- `GET /api/ui/state` is polled at 1 Hz, or 5 s while the tab is hidden. It returns site,
  call, chain, recording, audio, `calls_rev`, `settings_rev` and `log_last_seq`; types are
  in `p25-json/src/ui.rs`.
- `GET /api/ui/calls?limit=&nf=` is refetched only when `calls_rev` changes, or every
  30 s so ages stay fresh. `calls_rev` changes when either grant ring or the recordings
  ring changes.
- **Push.** `/ws/events` already exists. The page opens it only while visible (a
  preference) and uses it as a kick: `GRP_VCH_GRANT`, `TRF_HDU`, `TRF_TDULC_CALL_TERM`
  and `recording_saved` trigger an immediate state poll, so a new call appears within
  ~0.2 s. No new socket type was needed; the data still comes from the two documents.
  A dedicated `/ws/ui` that pushes state on every `CallTrackerEvent` is a follow-up if the
  events firehose (~3.5 KB/s on this bench) matters on phones.
- **Times.** All times are the board clock. The page keeps the offset from `now_unix_ms`
  and computes ages against it, so a 1970 board clock still gives correct "12 s ago".
- **Load.** The Now view moves about 1.1 KB/s, plus ~24 KB per call-list change (a few
  KB/s on this very busy loop). The legacy Radio tab moved about 40 KB/s.

### 2.4 Call phases

From `app/ui_state.rs`:

```text
voice      last voice chunk ≤ 1.5 s ago
acquiring  granted, no voice yet, ≤ 3 s since the grant
hang       otherwise; close_in_ms = 10 s − (now − max(last audio/HDU, last CC grant/UPD, start))
```

- **Voice** is counted in the lifecycle from audio chunks attributed to the call (air-time
  call_id in 054's airtime mode), at 20 ms per chunk. The lifecycle mirrors this every
  100 ms (`ActiveCallSnapshot` `voice_frames`, `first_voice_unix_ms`,
  `last_voice_unix_ms`, `last_activity_unix_ms`).
- The page re-derives the phase every 250 ms from those timestamps. Voice therefore
  turns to hang on time between polls, and the countdown runs smoothly.

### 2.5 Recording policy and settings persistence

- `services/ui_settings.rs`: one JSON document, `/mnt/jffs2/p25-ui-settings.json`. It
  follows the autoppm / sites pattern: atomic tmp + rename, and `P25_UI_SETTINGS_FILE`
  overrides the path. It holds:

  ```text
  { recording: {enabled, max_count},
    tg_aliases: {tg: name},
    unit_aliases: {rid: name},
    monitor_tgs: [tg...] }
  ```

  - Missing or unknown fields default. Bad values are clamped on load (retention 1..500,
    names trimmed to 48 chars, TG 0 and non-24-bit radio ids dropped).
  - A corrupt file loads defaults and is replaced on the next save.
- `PUT /api/ui/settings` takes a partial patch. It is validated (`deny_unknown_fields`;
  400 changes nothing), applied live, persisted, and logged to the EventLog (`system`).
  The response says whether it was persisted and carries the save error if not.
- The recorder reads `RecordingPolicy` (atomics):
  - **Off:** a new followed call opens no WAV. Its id is remembered, so the call list says
    "recording off" rather than "missing", and its chunks are counted as
    `chunks_not_recorded`. A recording already in progress completes.
  - **Retention:** applied at every finalise. Lowering it deletes the oldest WAVs at once
    (`enforce_retention`).
- `PUT /api/aliases` and `/api/monitor` (PUT and `?add=` / `?remove=`) now go through the
  same store, so they persist too. Aliases are applied to both control decoders; ws events
  carry `talkgroup_alias` again. At boot the stored aliases and monitor list are applied
  before the decoders start.

### 2.6 Front-end structure

The monolithic `dashboard.html` is replaced by native ES modules. There is no bundler, no
build step and no CDN:

```text
p25-httpd/src/httpd/ui/
  index.html                    thin shell (header, tab bar, #view)
  css/base.css                  tokens, dark/light themes, layout, phone tab bar
  css/components.css            cards, badges, switches, call card, call rows, log, spectrum
  js/main.js                    boot, router (#now #radio #diag #settings), header, clock banner
  js/api.js                     every endpoint the UI uses + /ws/events client (+ UiState shape)
  js/store.js                   the single state object, polling, events kick, prefs
  js/format.js, js/dom.js       formatting (board-clock ages), safe DOM builder, keyed lists
  js/audio/player.js, sources.js  live audio (AudioWorklet + Worker, ScriptProcessor on http)
  js/views/{now,radio,diagnostics,settings}.js
  js/components/{site_card,call_card,calls_list,log_view,spectrum,kv_table,alias_editor,monitor_picker}.js
```

- **Size.** 22 files, 38–231 lines each (2815 in total, vs 5849). The tests cap modules
  at 420 lines.
- **Serving** (`httpd/ui_assets.rs`, `api/ui.rs`):
  - Every file is `include_str!`-ed, so the binary is still the whole deployment.
  - `/` renders `index.html` with URLs `ui/<version>/…`, where `<version>` is BUILD_TAG
    plus an FNV hash of all assets. A rebuild without a tag bump still busts caches.
  - Imports are relative, so every module inherits the version.
  - The current version is served `immutable` with an ETag; any other version is served
    `no-cache`; the index is served `no-cache`. JS is served as `text/javascript`.
- **Tests** (`httpd/ui_assets_tests.rs`, `api/ui_tests.rs`). They fail `cargo test` when:
  - an `index.html` reference or a JS import (static, re-export or dynamic) does not
    resolve to an embedded asset;
  - an embedded file is unreachable from the index;
  - anything loads from the network;
  - a module grows past 420 lines;
  - the handlers stop returning the right status, type or cache headers (200 / 304 / 404).
- **Safety.** All data is rendered with `textContent`, never `innerHTML`, so names from
  the radio or settings cannot inject markup.

### 2.7 Phone

- Below 640 px the tab bar moves to the bottom, with safe-area insets.
- The Now view stacks call, site / recording, then calls. Call rows become two-line cards
  with the player on its own line.
- The log drops the category column. Verified at 390 × 844 with no horizontal overflow.

## Part 3 — What was built

### 3.1 Backend

- `p25-json/src/ui.rs` (new): `UiState`, `UiSite`, `UiCall`, `UiChain`,
  `UiRecordingStatus`, `UiAudio`, `UiCalls`, `UiCallSummary`, `UiRecordingRef`.
- `app/ui_state.rs` (new): call phase, `build_call`, `site_health`, `RateWindow` (TSBK/s,
  CRC-ok % over ~10 s, reset-safe), `calls_rev`, `build_calls` (join by call_id, orphans
  kept, `audio_status`).
- `services/ui_settings.rs` (new): `UiSettings`, `SettingsPatch`, `apply_patch`,
  `parse_settings`, `write_atomic`, `RecordingPolicy`, `SettingsStore`.
- `httpd/api/ui.rs` (new): `/`, `/ui/{version}/{*path}`, `/api/ui/state`,
  `/api/ui/calls`, `/api/ui/settings` (GET / PUT); `apply_settings_patch` is shared with
  the legacy alias / monitor endpoints.
- `httpd/ui_assets.rs` (new): embedded asset table, content types, version, index
  rendering.
- `httpd/mod.rs`:
  - routes: `/` → new UI, `/legacy` → old dashboard;
  - `AppState` gains `ui_settings`, `audio_ws_listeners` and `ui_cc_rate`.
- `app/grant_follower.rs`: the lifecycle counts voice chunks per call and mirrors
  voice / activity times into `ActiveCallSnapshot` every tick. `IDLE_TIMEOUT_MS` is now
  public, and `ActiveCall::open` replaces three struct literals.
- `audio/recorder.rs`:
  - `RecordingPolicy` (on/off, retention) is honoured;
  - `enforce_retention`;
  - close-time counter snapshot (R1);
  - diag counters `calls_skipped_disabled`, `chunks_not_recorded`.
- `services/event_log.rs`: `LogQuery` / `EventLog::query` (tail, time window, TSBK
  filter). `/api/log` gains `tail=1`, `from_ms` / `to_ms` and `tsbk=0`; old parameters
  behave as before.
- `httpd/api/talkgroups.rs`: aliases and the monitor list go through the settings store
  (persisted); aliases are applied to both decoders.
- `httpd/api/ws.rs`: `/ws/audio` listener count.
- `httpd/api/system.rs`: `/api/pipeline` grant ring capacity; `fs_usage` crate-visible;
  catalogue entries for the new routes.
- `httpd/api/debug.rs`: the control spectrum is centred on the live tuned frequency.
- `main.rs`: settings loaded at boot (aliases into both decoders, monitor list), policy to
  the recorder, BUILD_TAG `2026-09-26-web-ui-056`.

Unchanged: every existing endpoint and field (tools and `bench/fbench` read
`/api/decoder_compare`, `/api/stats`, `/api/traffic`, `/api/system`, `/api/decoder_reset`
and `/api/dibit_delivery`), the lifecycle's close rules, follower gating, and 054's
delivery and epochs.

### 3.2 Tests

`cargo test` (host): **192 passed**, 0 failed, 4 ignored (was 148). The 44 new tests:

- `services/ui_settings_tests.rs` (11): defaults, range checks, unknown fields rejected,
  alias trimming / removal / caps, monitor dedup in priority order, tolerant parse of
  partial / bad files, persist + reload round trip (no stray tmp file), a rejected patch
  writes nothing, corrupt-file fallback and replacement, in-memory store, bounded
  skipped ids.
- `app/ui_state_tests.rs` (9): clock validity, phase transitions, hang countdown and
  aliases, site health, TSBK rate window (spacing, sliding, reset, no-block window), join
  by call_id with WAV-length voice and "saving", every `audio_status` reason,
  not-followed toggle + orphan recordings + limit, `calls_rev`.
- `app/grant_follower_tests.rs` (4): voice accounting of the open call, an air-time tail
  of the previous call not counted, HDU is keep-alive but not voice, not-followed grants
  never become the current call.
- `audio/recorder_tests.rs` (3): close-time snapshot rule, eviction deletes WAVs,
  `enforce_retention`.
- `services/event_log_tests.rs` (4): old reads unchanged, tail, TSBK exclusion, time
  window.
- `httpd/ui_assets_tests.rs` (9) and `httpd/api/ui_tests.rs` (4): the asset checks listed
  in 2.6.

**Linux type-check:** `cargo-zigbuild check --target armv7-unknown-linux-gnueabihf.2.31`
is clean; warnings are the pre-existing ones. cargo-zigbuild needs the venv's `ziglang`
directory on PATH as well as `.venv-hdl\Scripts`. Its `python3 -m ziglang` fallback picks
up the system Python, which has no ziglang.

**Page:**

- Headless Chrome (CDP) ran every view on desktop (1360 px) and phone (390 × 844, mobile
  emulation), dark and light. The data came from a harness that serves the UI files and
  answers `/api/*` in one of two ways:
  - from captured JSON;
  - by forwarding GETs to unit A. PUT / POST are answered locally and never forwarded.
- No JS exceptions; `scrollWidth` equals the viewport on the phone.
- Against the live replay:
  - the call card went Voice → Hang with the countdown;
  - 40 call rows rendered, with recording ids equal to call ids;
  - a recording played (81 frames = 1.62 s);
  - the wideband spectrum marked the CC and TG 300;
  - the recording switch, radio names and monitor list sent the expected PUT bodies.

### 3.3 Not verified without hardware

- `/api/ui/state` on the board: the lifecycle's voice counting, the TSBK rate window and
  the listener count. The page was verified against a harness that synthesises the
  document from the legacy endpoints, not against the Rust builder.
- Persistence on JFFS2: write, reboot, reload. Behaviour when `/mnt/jffs2` is read-only
  or full is covered only on the host.
- Recording off / on and retention changes on the live recorder, and that the in-progress
  WAV completes.
- `/ws/events` kicks and `/ws/audio` playback through the new page. The audio code is
  the legacy design, ported; not heard.
- CPU cost of the 1 Hz state poll per client (expected small: a few short locks, ~1 KB).
- The R1 fix: expect `/api/recordings` `imbe_extracted` 72 / 81 on the bench, not
  144 / 153.

### 3.4 Bench checks after deploying

The loop, with L = loop start. Timings are from the 2026-09-26 capture:

| L + s | Air | Expected `/api/ui/state` |
|---|---|---|
| 4.0 | grant 857.9875, TG 300, src 3436046 | `call.call_id` = N, `tg` 300, `source` 3436046, `freq_hz` 857987500, `channel` "1117", `phase` "acquiring", then "voice" within ~0.3 s (first voice 16–260 ms) |
| 4–6.5 | voice | `phase` "voice", `voice_ms` climbs to ~1620 |
| ~6.5 | grant, same freq, src 1014 | new `call_id` N+1, `source` 1014, `voice_ms` restarts, `calls_rev` changes (N closed `tg_change`), a row for N with `audio_status` "saving" then "recorded" ~2 s later, `recording.id` = N, `voice_ms` 1620 |
| ~8–9.5 | voice ends | `phase` "hang" about 1.5 s after the last voice, `close_in_ms` counting down from ≈ 9 s (the CC keeps sending UPDs ~1 s) |
| ~19 | idle close | `call` null; the row for N+1 has `close_reason` "timeout", `voice_ms` 1440, `open_ms` ≈ 12.8 s, `air_ms` ≈ 2.8 s; `chain.state` "Idle", `parked_freq_hz` 857987500 |
| 22.4 | grant 858.4375, src 3406028 | new call, `freq_hz` 858437500, `channel` "1189" |
| ~25 | src 1014 | new call |
| ~26.5–32 | hang | `phase` "hang" until the next loop's 857.9875 grant pre-empts it at L+32; `close_reason` "tg_change", `open_ms` ≈ 7.3 s |

Also check:

1. **Site** (`/api/ui/state`): `site.health` "ok", `tsbk_per_s` ≈ 38.8, `tsbk_ok_pct`
   ≈ 97.7 (054's numbers), `last_tsbk_age_ms` < 500, `chain.delivery_mode` "airtime".
2. **Calls** (`/api/ui/calls`): four calls per loop, `voice_ms` 1620 / 1440 / 1440 / 1440,
   `imbe` 81 / 72 / 72 / 72, `first_voice_ms` < 300.
3. **Per-recording counters** (R1): `/api/recordings` `imbe_extracted` should equal the
   frames (72 / 81), not 144 / 153.
4. **Recording off:**
   - `PUT /api/ui/settings {"recording":{"enabled":false}}` → `recording.enabled` false;
     `call.recording` false on the next call.
   - New rows show `audio_status` "not_recorded".
   - `GET /api/log?category=recorder&tail=1&limit=10` shows `call_open_skipped`
     `reason=recording_disabled`.
   - `/api/recordings` count stays put.
   - Switch it back on afterwards.
5. **Retention:** `{"recording":{"max_count":5}}` → response `evicted` 35 and
   `/api/recordings` count 5. Restore to 40.
6. **Persistence:** restart p25-httpd → `GET /api/ui/settings` `load_note` "loaded
   /mnt/jffs2/p25-ui-settings.json" with the saved values.
7. **Aliases:** `PUT /api/aliases {"300":"Clay EMS"}` → ws `GRP_VCH_GRANT` events carry
   `talkgroup_alias` "Clay EMS", and `/api/ui/state` `call.tg_alias` too.
8. **Listeners:** Listen on one browser → `audio.listeners` 1 (the old `/api/stats`
   `audio_ws_clients` reads 3).
9. **Log:** `GET /api/log?tail=1&limit=5&tsbk=0` returns the 5 newest non-TSBK entries.
10. **Serving:**
    - `GET /` → HTML referencing `ui/2026-09-26-web-ui-056.<hash>/js/main.js`;
    - that URL → `text/javascript` with `Cache-Control: public, max-age=31536000,
      immutable`;
    - `/legacy` → the old page.
11. **Fallback:** if anything is off, the old page is at `/legacy`, and every endpoint it
    uses is unchanged.

### 3.5 Bench verification (2026-09-26, unit A, cabled replay from B)

Everything in 3.4 was checked on the board. The results:

- **Call timeline** (`/api/ui/state` polled at 2 Hz for two loops) matches the table
  above:
  - every transmission went `acquiring` → `voice` (≤ 0.5 s) → `hang`, with
    `close_in_ms` counting down from ≈ 9 s, and then to `call` null / chain `Idle`;
  - `site.health` "ok", `tsbk_per_s` 38–39, `tsbk_ok_pct` 96–98.
- **Calls list** (`/api/ui/calls`): `voice_ms` 1620 / 1440 / 1440 / 1440 and `imbe` 81 / 72 /
  72 / 72, exactly SDRTrunk's `.mbe` counts. Close reasons are "timeout" (`open_ms`
  ≈ 12.8 s) and "tg_change" (≈ 2.5 / 7.3 s), and each recording is linked by call_id.
- **R1 fixed:** `/api/recordings` `imbe_extracted` reads 72 / 81 (was 144 / 153).
- **Recording off:** new rows show `not_recorded` while `imbe` is still 72 / 81; the log
  shows `call_open_skipped reason=recording_disabled`. Switched back on.
- **Retention:** `max_count` 5 → `evicted` 35, and the count reads 5.
- **Persistence:** after `max_count` 39 and a restart, the setting read 39 with
  `load_note` "loaded /mnt/jffs2/p25-ui-settings.json". It was restored to 40.
- **Aliases** show on the call and in the calls list, and were cleared afterwards.
- **Serving:** `/` loads `ui/2026-09-26-web-ui-056.543a4815/…`, and the JS comes back as
  `text/javascript` with `public, max-age=31536000, immutable` and an ETag. `/` itself is
  `no-cache`, and `/legacy` serves the old 259 KB page.
- **Decode regression** (`fbench run rf.p25_replay`, scored against SDRTrunk's `.mbe`):

  | Build | Loops | IMBE recovered | Missed transmissions |
  |---|---|---|---|
  | 054 | 10 | 100.0 % | 0 / 40 |
  | 056 as delivered | 8 + 64, plus 3 while probing | 100.0 % / 99.6 % | 2 / ~300 |
  | 056 + cut-order fix | 64 | 100.0 % (297.0 / loop, HDU 4.0) | 0 / 258 |

  On the fixed build `cuts_reordered` counted 141 of 839 cuts: the jitter inversion is
  common, and the misses only happened when it put a gate-closing cut last.

**Air-time cut reordering (054 bug, fixed here).** Both misses were the first
transmission after a cross-frequency retune: once from hang, once from idle. The soak
monitor captured `/api/dibit_delivery` `recent_cuts` at the second one (call 234):

- `grant_hold` (TG 0, old call 233) was recorded first, seq 757, index 8845327.
- `retune` came next, seq 758, index 8845292 (the low edge, by design).
- `tg_change` and `call_open` 234 followed, seq 759/760, index 8845325.

The hold's midpoint estimate landed 2 dibits after the cuts recorded after it. Cuts are
applied in index order, so the gate closed (TG 0) after the new call opened it, and the
whole 1.4 s transmission was gated until the next grant 2.5 s later.

Every production caller stamps a cut with "now", so record order is program order.
`push_cut` now keeps each cut's index ≥ the previous cut's, counted in the new
`cuts_reordered` counter. The floor resets on mode hand-over and resync. A regression
test replays the exact sequence: `estimate_jitter_never_reorders_cuts_against_record_order`.

## Follow-ups (not done here)

1. **Shorter close.** Now that dibits arrive in ~0.2 s, the "CC decoder stalls during
   traffic" rationale for the 10 s idle close is probably gone: 2026-05-03 SESSION_PICKUP
   said to drop it to ~1.5 s once the stall was fixed. Measure UPD gaps with 054, then
   consider closing on TDULC call-termination plus UPD silence. The UI already shows hang
   honestly; this would make calls end when the air does.
2. **Exact per-call counters.** `ImbeBatch` carries call_id in airtime mode, so count
   IMBE / LDU / vocoder errors by call_id instead of global-counter deltas (both
   grant_stats and the recorder). R1 is a fix, not the cure.
3. **Recordings to SD** (`/mnt/sd`, 62 GB free) as an option, with an age / size cap and
   file names that survive restarts (call ids restart at 1).
4. **Sample rate.** `/api/spectrum`, `/api/control_iq_dump` and `/api/traffic_iq_dump`
   hard-code 62.5 kSPS while `/api/stats` says the DDC outputs 50 kSPS (8 MSPS / 160);
   `/api/presets` says 62.5. Verify on the bench with a carrier at a known offset, then
   fix the constants or the doc.
5. **autoppm and retunes.** Stage A and the persisted file use `boot_control_freq`, which
   is wrong after `POST /api/tune`; use `current_control_freq`.
6. **Category split.** Split the TSBK mirror out of the `grant` log category (e.g. `cc`)
   together with `tools/p25_audit_capture.py`, which reads `category=grant` for it.
7. **`/ws/ui` push.** Send the state on every `CallTrackerEvent` (and 1 Hz), replacing
   the events kick and the 1 Hz poll on phones.
8. **Remove `/legacy`** once the new page has run for a while, with the ~1000 lines of
   dead JS and the Debug-tab TypeError (T8). The Plots views (constellation, eye,
   deviation) could move into Diagnostics first if they are still used.
9. **Encrypted lockout.** `encrypted_tg_history` is learned forever, per process. Consider
   persisting a manual list separately from the learned one, and expiring learned
   entries.
10. **Settings writes.** `SettingsStore::update` writes the file under a `std` lock from an
    async handler. That is fine for a few-hundred-byte JSON; move it to
    `spawn_blocking` if the settings ever grow.
