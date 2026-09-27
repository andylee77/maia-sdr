# 057 — Call close (traffic-channel teardown), per-call counters, SD-card recordings

**Date:** 2026-09-27. **Branch:** fishball-p25. **BUILD_TAG:** `2026-09-27-call-close-sd-057`.
**Bake required:** NO — p25-httpd only. Builds on 054 (low-latency dibits, air-time gating),
055 and 056 (web UI, persisted settings).

The three follow-ups of 056 that Andy approved, with task 1 widened to a teardown audit:

1. **Call close.** The 10 s idle close was sized for 054's 3.4 s dibit blocks. Andy: "a lot
   of the settings I had on tear down were due to the channel not working and dropping
   out", so every teardown / re-follow knob was inventoried against its history and
   against SDRTrunk (source and measured), and the teardown was rebuilt to match or
   beat SDRTrunk without dropping replies.
2. **Per-call counters by call_id.** Every per-call IMBE / LDU / error / vocoder count was
   a global-counter delta. They are now attributed where each frame is decoded.
3. **Recordings on the SD card**, as an option, with per-store retention, and never
   blocking the decode path on the card's multi-second write stalls.

## Part 1 — Traffic-channel teardown

### 1.1 What closed a call before 057

One lifecycle (`app/grant_follower.rs`) owns the call; its `CallClose` also releases the
traffic chain (follower: `TrafficChain` Idle, TG 0, air-time `CallClose` cut; the LSM stays
enabled and parked on the frequency). While a call is open, the follower's **sticky lock**
rejects every grant for another talkgroup, so the close time is also how long the only
traffic chain is unavailable.

Close paths at `9004a2c`:

- **Timeout:** 10 s (`IDLE_TIMEOUT_MS`) after the newest keep-alive: an audio chunk of the
  call, an HDU, the opening grant, or a `GRP_VCH_GRNT_UPD` for the TG (any channel).
- **TgChange:** any followable primary grant, including a repeat of the same call's grant,
  pre-empts at once.
- **StreamLag** (boundary broadcast lag) and the synthetic open/close of not-followed grants.
- Terminators (TDULC TALK COMPLETE / CALL TERMINATION) were source stamps only; `SyncLost`
  was never emitted.

On the bench this held a 1.44 s PTT open for 12.8 s (056 §3.5), locking the only chain to
TG 300 for ~11 s after every conversation.

### 1.2 Knob inventory

Every knob that shapes teardown, chain release / parking and re-follow, with its origin and
whether its root cause is gone. "054 F4" = the 3.41 s dibit-block delivery that 054 removed
(the control ring too: TSBKs, grants and updates reached the PS in 3.4 s bursts).

| Knob (where) | Value | Why it was added (commit, doc) | Cause gone? | 057 decision |
|---|---|---|---|---|
| `IDLE_TIMEOUT_MS` (grant_follower) | 10 s | 2 s (894e84b) → 5→10 s (3a9ecbc, "chain settle on a fresh retune can take 7+ s"); UPD-only 3 s broke every call, so hybrid 10 s (fa246d3, "UPDs … go SILENT for ~3.3 s … CC decoder stalls during traffic") | **Yes.** The 3.3 s silence is the 3.41 s CC block; the "7 s settle" was late grant decode. 2026-05-03 SESSION_PICKUP: "drop to ~1.5 s once the stall is fixed" | **Changed:** end-of-transmission close + `call.hang_ms` fallback (persisted, default 3 s) |
| Keep-alives (grant_follower) | audio, HDU, grant, UPD for the TG | fa246d3, 3a9ecbc; 054 ignores other calls' air-time chunks | Stall reason yes | **Changed:** UPD must be for the call's channel; ignored after an end marker. Voice NIDs are not keep-alives (noise can fake one) |
| `LOS_TIMEOUT_MS` / `SyncLost` / `last_nid_at_ms` | 1.5 s, dead | 38442a7 added and removed it the same day ("sync_lost with only 360 ms of PCM") | Partly (grants 3.4 s late put the chain at the tail of transmissions) | **Kept dead.** SDRTrunk has no sync-loss teardown either; decode dropouts inside a live transmission (≤ 0.54 s, 3 % of transmissions, §1.4) are bridged by CC updates |
| `GRANT_DEDUP_MS` | 200 ms | 3a9ecbc ("ENC GRANT spam"), source-keyed 2026-04-30 | **Semantics changed by 054:** pre-054 one 3.4 s CC block arrived at once, so the window swallowed every repeat; now it is 200 ms of air | **Kept** + same-call refresh below |
| `classify_cc_arrival` (every grant = new call) | always pre-empt | operator, 2026-04-30 (fa246d3) | Policy | **Changed:** a repeat of the on-air call's grant is a refresh; the next talker's grant while this one talks is queued (§1.6) |
| Not-followed synthetic pair, same-freq not-followed close | — | 3a9ecbc, fa246d3 | Physical facts | Kept |
| Update fast path, `update_no_lock` (updates never acquire) | — | 2026-04-26 (backlog); fa246d3 ("TG 700 (encrypted) update pulled the chain off-Idle") | Backlog yes; no-lock no (updates carry no encryption flag) | **Kept**, plus re-follow of a call closed by timeout (§1.6) |
| Channel-reuse / same-freq-reuse teardown | — | 65a7fa0, b9cab02 | Physical fact | Kept |
| Encrypted teardown pause (`traffic_lsm_enable = 0`) | — | M2B 2026-05-02 | Mostly (054 gating) | **Kept, bug fixed:** a same-freq resume never re-enabled it (chain dead until a cross-freq retune); now it does |
| Sticky lock | while a call is open | SDRTrunk PR #2010, 65a7fa0 | Policy (one chain) | Kept; its length now follows the close (≈ voice + 2 s instead of voice + 11 s) |
| `traffic_lock_freq`, `follower_enabled` | off / on | diagnostics | n/a | Kept |
| `LastCallQuality::was_clean` (coast vs reset on cross-freq retunes) | IMBE ≥ 30, silent < 5 % | 03d6143, 9d15586, 38442a7 ("3.2–3.6 s t_first_ldu") | Largely (the 3.4 s was delivery) | **Kept**; inputs now the call's own counts by call_id (were global minus a per-HDU baseline). A/B coast vs reset is a follow-up |
| Seed tuple (PLL-only) | computed, ignored by `retune_traffic_chain` | 050/051 ("First-IMBE stayed at 3–3.5 s") | Yes | Left as is (dead, logs "retune seeded"); follow-up |
| Same-freq resume (no NCO write / reset) | — | b960e0e, c3222ab | Optimisation still valid | Kept + re-enable fix; NCO-skip ignores later `lo_shift` / `rx_lo` changes (follow-up) |
| CallClose release (live call only, chain parked) | — | c3222ab, 84ab51b | n/a | Kept |
| `SPEAKER_END_COOLDOWN_MS` (global) | 1.5 s | b9cab02 ("~7 closes per real call") | Mostly | Kept for the SpeakerEnd source stamp only; end detection uses a per-call once-per-transmission rule |
| SpeakerEnd validity gates (MOT_TC BY = CC src, CALL_TERM controller address) | — | b9cab02 | Partly | Kept for stamps; end detection needs only an RS-valid LC after voice |
| `CLOSING_DRAIN_MS` (recorder) | 2 s | 3a9ecbc | Yes for delivery; the pacer still lags up to ~1 s | Kept (only delays the WAV) |
| `FINALIZE_GRACE` (recorder safety net) | 15 s | 327a6b2 (above the 10 s idle) | n/a | **Changed to 45 s**, above the longest configurable close (30 s + 10 s) |
| `CLOSE_STATS_WAIT_MS` (recorder, 056 R1) | 1 s | c75f9d4 | n/a | **Removed** (Part 2) |
| grant_stats 2 s vocoder wait at every close | 2 s | b9cab02 | Mostly | **Removed** (Part 2). It also delayed the next CallOpen, and the task exited for good on a broadcast lag |
| `settle_dibits`, delivery modes, air-time cuts | 48 dibits, airtime | 84ab51b, c75f9d4 | n/a (correctness) | Kept |
| UI `VOICE_HOLD_MS` / `ACQUIRE_WINDOW_MS` | 1.5 s / 3 s | 056 | Presentation | Kept; phase is "hang" ("Ended") as soon as the end marker is in |

### 1.3 SDRTrunk teardown, from the source

Local fork `C:\Users\Andy\Projects\SDRTrunk\sdrtrunk` (upstream differences noted).

| | SDRTrunk | p25-httpd 057 |
|---|---|---|
| Call **event** end | First TDU, or TDULC with a valid LCW (`P25P1DecoderState.processTDU/processTDULC` → `processP1TrafficCallEnd`) | First LC-valid (RS) TDULC after the call's voice → `VoiceEnd`; UI shows "Ended" at once. Bare TDUs ignored (a talker change on this site, §1.4) |
| Traffic **channel** stop | Fade timer: 3000 ms (fork) / 1000 ms (upstream) after the last decoded message, reset by every message incl. hang TDULCs (`ChangeChannelTimeoutEvent`, `SingleChannelState`); immediately on a TDULC CALL TERMINATION with a system-controller address (`LCCallTermination.isNetworkCommandedTeardown`) | `end_grace_ms` (2 s) after the end marker, unless voice resumes; the next grant pre-empts at once |
| No-activity teardown | Same fade timer (traffic frames only) | `hang_ms` (3 s) without voice, HDU or a CC grant / update for the TG on its channel |
| Sync loss | No separate teardown (fade timer) | None (keep-alives bridge dropouts) |
| Grant update for an active channel | Extends the event only before voice; never keeps the channel alive | Keep-alive until the end marker |
| Grant / update for a channel not running | Starts a channel (no "recently torn down" guard) | Grant: follow. Update: only for the call just closed by timeout (§1.6) |
| Reply on the same channel | Same channel keeps running; new event per HDU | Reply grant pre-empts (new call) or, if queued during the talk, is applied at the hand-over |
| Max call length | None for P25 (45 s default is MPT1327's) | None |

### 1.4 SDRTrunk teardown, measured on this site

`tools/sdrtrunk_teardown_stats.py` (new) over the SDRTrunk logs of Clay County
(`C:\Users\Andy\SDRTrunk\event_logs`, 2026-04-15 … 05-03): 32 CC sessions (3.4 h), 420
traffic allocations, **719 transmissions** (11 488 LDUs), 332 `.mbe` files. The logs are
stamped to 1 s; the script rebuilds a 9600 bit/s clock per log from the framer's bit
counts (bit-exact within a log, ±30 ms CC vs traffic, checked against `.mbe` epoch-ms
frame times: first frame − log voice start p50 0.268 s, expected 0.2625 s).

| Measurement | n | p10 | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| First terminator after the last LDU | 718 | 0 | 0 | 0 | 0.18 | 5.09 |
| System channel hang: last LDU → carrier drop | 413 | 1.26 | 1.40 | 1.62 | 1.67 | 1.67 |
| Same-TG turnaround, same channel: last LDU → next voice | 301 | 0.24 | 0.87 | 1.50 | 1.94 | 5.45 (fade) |
| … with a CC grant for the reply | 170 | 0.33 | 0.83 | 1.55 | 1.85 | 2.25 |
| Grant → next voice, same channel | 172 | 0.17 | 0.20 | 0.36 | 1.34 | 1.49 |
| Grant for the reply − end of current talker (negative = queued) | 172 | −0.36 | 0.58 | 1.20 | 1.59 | 1.63 (min −7.98) |
| SDRTrunk call-event end − last LDU (grant-originated) | 242 | −0.01 | 0.04 | 0.08 | 0.21 | 0.63 |
| SDRTrunk channel stop − last decoded message (app log, ms stamps) | 222 | 0.011 | 0.044 | 0.079 | 0.112 | 3.10 |
| CC update gap for the channel, during voice | 4083 | 0.30 | 0.315 | 0.375 | 0.525 | 0.96 |
| CC update gap, during the hang | 1753 | 0.075 | 0.30 | 0.36 | 0.53 | 1.01 |
| Largest CC update gap per allocation | 273 | 0.34 | 0.40 | 0.53 | 0.75 | 1.01 |
| Last CC update − last LDU | 273 | 0.91 | 1.15 | 1.39 | 1.48 | 1.56 |
| Decode dropout inside a transmission | 25 | 0.18 | 0.18 | 0.35 | 0.50 | 0.54 |
| Voice per transmission | 719 | 0.98 | 1.88 | 5.30 | 28.4 | 51.2 |

All in seconds. What the numbers say:

- **Terminators:** the first frame after the last LDU is TALK COMPLETE (49.9 %), TDULC
  GROUP VOICE CHANNEL USER (39.8 %), CHANNEL UPDATE (6.5 %) or a bare TDU (2.9 %); only one
  transmission in 719 has none. Subscribers send TALK COMPLETE (95 %), consoles (1011–1014)
  never. There is **no CALL TERMINATION on voice channels** in normal operation: the carrier
  simply drops after ~28 CHANNEL USER TDULCs. So SDRTrunk's "network teardown" path never
  fires here, and a close rule keyed on CALL TERMINATION would never fire either.
- **Bare TDU is a talker change, not an end:** 78 % of same-TG turnarounds have a TDU in the
  gap, 0.225 s (p50) before the next HDU; only 6 of 413 final hangs contain one.
- **Replies:** 99.0 % of same-TG replies on the same channel start within 2 s of the last
  LDU (99.4 % of those with a grant; all within 2.25 s). Replies after the carrier drop need
  a new grant: 11.7 % within 2 s, 25.8 % within 3 s — holding longer buys little.
- **Queued grants:** 22 of 172 reply grants (12.8 %) came while the current talker was still
  transmitting, up to 8 s early (consoles). Hand-over then takes 45–195 ms, and 9 of 306 next
  transmissions start with an LDU (no HDU).
- **CC keep-alive:** the CC announces the channel every ~0.3 s through the voice and the
  hang, and stops ~0.3 s before the carrier drops. No live allocation went more than 0.41 s
  without either a traffic frame or a CC mention.
- **SDRTrunk's timing on this site:** call event ends at the terminator (+0.04 s); the
  channel stops 44 ms after the last decoded message, i.e. at the carrier drop, 1.26–1.67 s
  after the last LDU.

### 1.5 The 057 close rules

`app/grant_follower.rs` (lifecycle; pure `close_due` / `sweep`, host-tested):

1. **End of transmission → `call_end`.** The forwarder sends `CallBoundaryKind::VoiceEnd
   {call_id, air_ms, lc}` for the first RS-valid TDULC after voice of the call being decoded
   (`CallCounts::note_end_marker`: once per transmission, never before the call's first LDU,
   so the previous talker's hang TDULCs decoded under a new call are not its end). In
   airtime mode `call_id` / `air_ms` come from the air-time segment, so a late terminator
   of a pre-empted call never ends the next one. The call closes `end_grace_ms` later
   (default **2 s**) unless voice resumes:
   - two valid voice NIDs (HDU / LDU1 / LDU2) within 400 ms from the traffic-LSM heartbeat
     (`TrafficNidObserved { voice }`, read from the HDL in real time, ahead of the PS
     decode); one NID can be noise;
   - or a voice chunk of the call **aired** after the marker (chunks aired before it, still in
     the vocoder / pacer when the terminator was decoded, do not count).

   CC updates do not extend it (they continue through the hang). Resumed voice returns to
   rule 3.
2. **Next grant → `tg_change`**, at once, as before — except a repeat of the on-air call's
   own grant (refresh) and a queued grant (§1.6).
3. **No keep-alive → `timeout`:** `hang_ms` (default **3 s**) without voice of the call, an HDU,
   or a CC grant / update for the TG **on the call's channel**. The fallback when no
   terminator is decoded.

Both are persisted settings (`call.hang_ms` 1000..30000, `call.end_grace_ms` 0..10000) on the
Settings view ("Call close"), applied live on the next 100 ms tick.

Why these defaults:

- **2 s end grace.** The grace starts ~0.1–0.2 s after the last LDU (TDULC + delivery), so the
  call stays open until ~2.15 s after the last LDU: past the whole system hang (max 1.67 s)
  and past 99 % of same-TG reply turnarounds (so the sticky lock keeps the conversation, and
  the reply's grant pre-empts). Compared with SDRTrunk: its call event ends at the
  terminator (ours shows "Ended" then), and its channel stops at the carrier drop, ~0.5 s
  before our release at p50 — the difference is the reply hold a single-chain receiver
  needs. Releasing at the carrier drop (valid-NID silence after the marker) was considered
  and left as a follow-up (§1.9).
- **3 s timeout.** Three times the largest CC-update gap of a live channel (0.96 s during voice,
  1.01 s in a hang); equal to SDRTrunk's fork traffic timeout. It fires only when no
  terminator was decoded (1 transmission in 719) or both signals fade; SDRTrunk's CC decode
  dropouts reached 8 s, so a shorter value would close more calls during CC dropouts with no
  gain.

### 1.6 Same-call repeats, queued grants, re-follow

- **Same-call repeat → refresh** (`classify_cc_arrival`): a grant with the same TG, channel and
  source (or no source) while the call's transmission has not ended is the CC re-announcing
  it (SDRTrunk's same-call check). Pre-054 such repeats fell inside the 200 ms dedup by
  accident (3.4 s CC blocks); since 054 a repeat 0.3 s later would split one transmission
  into two calls. After the call's end marker the same unit keying again is a new call.
- **Queued grant** (`ActiveCall::queued`): the next talker's grant (same TG and channel,
  another source) while this call has voice and no end marker is held and applied at the
  hand-over, where the successor's call_id cut lands closest to the new transmission:
  - an HDU on the channel (seen in real time, so the HDU completes after the cut);
  - two voice NIDs after the end marker (next talker starting with an LDU);
  - the end grace or no-keep-alive timeout of the current call;
  - at the latest `QUEUED_GRANT_MAX_MS` = 10 s after the grant.

  Pre-057 the grant pre-empted at once: the rest of the current transmission (up to 8 s) went
  to the next call. Remaining cost: the follower updates `current_source` at the grant, so the
  current call's last frames carry the next talker's id in the forwarder context (recording
  `sources_observed`, vocoder AGC speaker reset, LDU1 cc_match); call identity and audio
  attribution are right.
- **Re-follow on a grant update** (`refollow_on_update`): updates still never acquire the
  chain (no encryption flag, TG 700 incident) — except for the (TG, channel) of the live call
  the lifecycle just closed by `timeout`, within 30 s, while the chain is idle: the CC still
  announcing it means it did not end (both signals faded). The update then goes through every
  gate as a source-less grant. SDRTrunk (re)starts a channel on any update.

### 1.7 Other teardown fixes

- **Encrypted pause never undone on the same frequency** (inventory latent bug 1): after an
  encrypted teardown paused the LSM (follower or `/api/encrypted_tgs`), a same-frequency
  grant took the NCO-skip resume path, which writes no register; the chain stayed dead until
  a cross-frequency retune. `ImbeForwarder::traffic_paused_by_teardown` now makes the resume
  re-enable it (with its 054 Resume cut); a retune clears the flag.
- **Updates keyed on the channel:** a `GRP_VCH_GRNT_UPD` for the TG on another channel (the
  TSBK's B slot, a patch) no longer keeps the call alive.
- **grant_stats:** no 2 s blocking wait at every close (it delayed the next call's CallOpen and
  shortened its `duration_ms`); `duration_ms` is the lifecycle's monotonic open time
  (`CallClose.open_ms`); the task no longer exits for good on a broadcast lag.
- **Recorder safety net** `FINALIZE_GRACE` 15 → 45 s (above the longest configurable close).

### 1.8 Expected effect on the bench replay (28 s loop)

| L + s | Air | 056 | 057 |
|---|---|---|---|
| 4.0–5.8 | 3436046, 81 IMBE, 857.9875 | voice | voice |
| ~5.9 | TALK COMPLETE | — | `close_via` "end", "Ended", `end_lc` talk_complete |
| ~6.5 | grant 1014 | pre-empt, `tg_change`, open ≈ 2.5 s | same |
| ~8.3 | 1014 ends, hang TDULCs | hang, closes at ~L+19 (`timeout`, 12.8 s) | "Ended", `end_lc` channel_user; closes ~L+10.4, `call_end`, open ≈ 3.9 s |
| 10.4–22.4 | channel hang ends ~L+9.7 | chain Active (sticky) until L+19 | chain Idle, free for any TG |
| 22.4 / ~25 | 3406028 then 1014, 858.4375 | `tg_change` ≈ 2.5 s; 1014 held until next loop (`tg_change` 7.3 s) | `tg_change` ≈ 2.5 s; 1014 `call_end` ≈ 3.9 s at ~L+28.5 |

Per call `imbe` 81 / 72 / 72 / 72 and `voice_ms` 1620 / 1440 / 1440 / 1440 unchanged.

### 1.9 Not verified without hardware / on air, and follow-ups

- The whole of 1.5–1.7 is host-tested only. Bench-verifiable: end marker timing and
  `call_end` on the replay, no missed transmissions over a soak, grant → first voice
  latency unchanged. **Only on air:** queued grants (the replay has none), same-call repeats,
  re-follow after a timeout close (CC + traffic fade), the encrypted-pause re-enable, CC
  decode dropouts.
- Follow-ups: release at the carrier drop (valid-NID silence after the marker, ~0.5 s sooner
  at p50) as an option; A/B the coast / reset retune gate; remove the dead seed code and
  `vocoder_reset_pending`; make the NCO-skip resume compare the live (`rx_lo`, `lo_shift`,
  preset) tuning; the follower should not stamp a queued grant's source on the live context.

## Part 2 — Per-call counters by call_id

New `app/call_counters.rs`: `CallCounterBook`, a bounded (512) map call_id → `CallCounts`
(HDU, LDU1/2, TDU, TDULC, framer-arm ×5, IMBE extracted / dropped, vocoder PCM / errors /
silent / encrypted), held by the `ImbeForwarder`:

- the voice handlers add framer-side counts for `eff_call_id()` — the air-time segment's call
  in airtime mode (054), the live call otherwise;
- `forward_frames` adds IMBE drops (vocoder queue full) for the batch's call;
- the vocoder thread adds PCM / silent / error / encrypted-skip counts for
  `ImbeBatch::call_id`, once per batch.

Readers look a call up by id whenever they like, so late-decoded frames of a call land in it
and nothing of the neighbours does:

- **grant_stats** (`/api/grant_decode_stats`, `/api/ui/calls`): the summary is pushed at the
  close with the counts so far; for 10 s after the close a 250 ms refresh adds frames of the
  call decoded later (the air-time tail) and bumps `GrantStatsRev`, which is part of
  `calls_rev`, so the page refetches.
- **recorder** (`/api/recordings`): read at finalise (close + 2 s drain).
- **follower** `LastCallQuality`: the closed call's own IMBE / silent counts.

The global counters (`/api/traffic` `imbe.*`, `/api/stats`, `/api/decoder_compare`,
`/api/system`, `/api/decoder_reset`, `/api/dibit_delivery`) are unchanged. In `legacy` /
`poll` delivery the attribution is by live call_id (pre-054 quality).

**Vocoder errors:** `vocoder_errors` was never incremented (always 0, `/api/pipeline` calls it
`errors_over_4bit`). It now counts frames whose IMBE FEC corrected more than 4 bits (JMBE
`error_count_total`, `vocoder::ERROR_FRAME_BITS`), globally and per call.

## Part 3 — Recordings on the SD card

New `audio/rec_storage.rs`.

- **Setting:** `recording.storage` "ram" (default, `/tmp/p25_recordings`, tmpfs) or "sd"
  (`/mnt/sd/p25_recordings`, the FAT32 SD partition). Retention per store:
  `recording.max_count` (RAM, 1..500), `recording.sd_max_count` (1..5000, default 2000) and
  `recording.sd_max_mb` (16..32768, default 2048 ≈ 36 h of voice). The Settings view has the
  selector, both limits and the card status; the Now card says where recordings go.
- **Nothing moves** when the setting changes: new recordings go to the new store, older ones
  stay listed and playable where they are, and each store's retention deletes only its own
  files (a lowered RAM limit can never delete SD recordings).
- **Stalls never reach the hot path.** The bench measured multi-second SD stalls (3.4 s write,
  6 s fsync). The recorder builds the WAV in memory, lists it at once (the entry keeps the
  bytes, `sd_pending: true`; `/api/recordings/{id}.wav` serves them) and queues the write. One
  OS thread (`p25-rec-writer`) writes `.<name>.part`, fsyncs, renames, then drops the RAM
  copy. SD deletions go through the same queue. Nothing waits on it: the recorder task, the
  vocoder, the pacer and the dibit readers never touch the card. A write in progress shows
  as `writing_for_ms`.
- **Card absent / read-only / full / failing.** The writer probes the card every 30 s and on a
  store change (`/proc/mounts` for `/mnt/sd` — without the card it is a plain directory on the
  root filesystem and is never written — read-only flag, statvfs free space, < 64 MB = full).
  While the card is not "ok", or more than 32 MB of writes are waiting (a stall far beyond the
  measured ones), new recordings are saved to RAM (`fallbacks_to_ram`, `sd_unavailable` in
  the recorder log). A write that fails is saved to RAM instead and the entry moves there.
  Status: `/api/ui/settings` `recording_storage.sd` and `/api/ui/state` `recording.sd_state`.
- **Restarts.** SD recordings survive: at start-up (before any task can open a call) the
  directory is listed (`index_sd`, bounded to 3 s) from the file names
  (`rec_<ms>_<id>_tg<tg>[_from<src>].wav`); duration comes from the size, other details
  (frequency, counters) are not kept. **Call ids continue after the highest id found**, so ids
  stay unique across restarts and the recording id / call_id link of `/api/ui/calls` holds.
  Leftover `.part` files are removed. RAM recordings are cleared at start-up as before.

## API and UI

- `/api/ui/state` `call`: `close_via` end|timeout, `close_window_ms`, `end_lc`; `phase` "hang" once
  the end marker is in. `recording`: `storage`, `sd_state`, `sd_count`, `ram_count`.
- `/api/ui/calls`: per-call counts by call_id, `close_reason` adds `call_end`, `recording.storage`.
- `/api/ui/settings`: `settings.call {hang_ms, end_grace_ms}`, `recording {storage, sd_max_count,
  sd_max_mb}`, `recording_storage {selected, active, ram {…}, sd {state, detail, ready, count,
  bytes, free_bytes, writes_ok, writes_failed, fallbacks_to_ram, last/max_write_ms, queue_jobs,
  queue_bytes, writing_for_ms, …}}` (the 056 keys `dir`, `tmpfs`, `count`, `free_bytes` stay).
- `/api/recordings`: both stores, `storage` per item, `sd_pending`; `?limit=`; `max` is the live
  RAM retention, plus `max_sd`, `max_sd_bytes`.
- `/ws/events` and `/api/log` (`voice`): `TRF_VOICE_END {call_id, tg, lc, air_ms}`; the page uses
  it as a kick. Recorder log: `call_closing` gains `open_ms`, `end_lc`; `call_saved` gains
  `storage`.
- UI: call card "Ended" badge and a countdown scaled to the pending rule; Settings "Call close"
  card and the storage / SD retention / SD status rows; calls list shows the close reason in
  words and the recording's store. No new modules (all ≤ 420 lines).

Details: [`P25_API.md`](../P25_API.md).

## Tests

`cargo test` (host): **229 passed**, 0 failed, 4 ignored (was 195). New or changed:

- `app/grant_follower_tests.rs` (+14): end marker then silence closes within the grace; a second
  marker does not restart it; voice aired after the marker, or a voice-NID pair, cancels it (a
  single NID does not); CC updates do not extend it; a reply granted in the grace pre-empts and
  the old call's late marker cannot end the new call; same-grant repeats refresh until the end;
  updates keep a silent call alive only on its channel; queued grant waits for the HDU hand-over,
  hands over on voice NIDs for an LDU-only start, at the end grace, or after 10 s; grants before
  any voice still pre-empt; re-follow on update only for a call closed by timeout; CallClose
  carries `open_ms` and the marker; defaults.
- `app/call_counters_tests.rs` (4), `app/grant_stats_tests.rs` (3): per-call counts not global
  deltas (the next call counting before the close does not leak), tail after the close picked up
  and bumps the rev, zero for calls without frames.
- `audio/rec_storage_tests.rs` (7): file-name parsing, per-store retention and the SD size cap,
  boot index (sorted, durations, duplicate ids, `.part` ignored), SD write then RAM copy dropped
  and queued delete, failed write falls back to RAM, **stalled writer**: submitting never waits,
  a full queue sends new recordings to RAM, the queue drains; unusable card reported without
  writing.
- `audio/recorder_tests.rs` (rewritten, 6; the 056 close-snapshot test is gone with the snapshot): WAV sizes, retention, per-call counters in the
  recording, SD recording listed at once and written off the task, SD selected but unusable →
  RAM.
- `services/ui_settings_tests.rs` (+3), `app/ui_state_tests.rs` (updated).

Linux type-check (`cargo-zigbuild check`, armv7) clean (pre-existing warnings only).

## Bench checklist

The replay (Clay County, CC 860.9625, TG 300); L = loop start. `open_ms` values assume the
defaults (end grace 2 s, hang 3 s).

1. **Call timeline** (`/api/ui/state` at 2 Hz for 2+ loops):
   - L+4.0 grant 857.9875 / 3436046: `phase` acquiring → voice (first voice 16–260 ms as before).
   - ~L+5.9: `phase` "hang", `close_via` "end", `end_lc` "talk_complete", `close_window_ms`
     2000, `close_in_ms` counting down from ≤ 2000.
   - ~L+6.5 grant 1014: new call_id; the 3436046 row `close_reason` "tg_change",
     `open_ms` ≈ 2.5 s.
   - ~L+8.4: `close_via` "end", `end_lc` "channel_user" (consoles send no TALK COMPLETE).
   - ~L+10.4: `call` null, `chain.state` "Idle"; the 1014 row `close_reason` "call_end",
     `open_ms` ≈ 3.7–4.1 s (was 12.8 s), `air_ms` ≈ 2.8 s.
   - L+22.4 3406028 on 858.4375: `tg_change` at the 1014 grant (~L+25), `open_ms` ≈ 2.5 s;
     1014: `call_end` at ~L+28.5, `open_ms` ≈ 3.7–4.1 s (was 7.3 s `tg_change`).
   - Between ~L+10.4 and L+22.4 and ~L+28.5 to L+32: no call, chain Idle.
2. **Teardown latency from the last voice:** `/api/log?category=voice&tail=1` `TRF_VOICE_END`
   entries ~0.1–0.25 s after the last LDU of each PTT (compare with `recorder`
   `call_closing` `close_at_ms` 2.0 s later); `tools/sdrtrunk_teardown_stats.py
   --event-logs NONE --p25-calls calls.jsonl --p25-log log.json` on dumps of `/api/ui/calls`
   and `/api/log` gives the distributions (teardown ≈ `open_ms − first_voice_ms − voice_ms`
   ≈ 2.1–2.3 s for `call_end`).
3. **No missed transmissions:** `fbench run rf.p25_replay` soak (64 loops): 297 IMBE / loop,
   HDU 4.0, 0 missed (baseline 0 / 258); grant → first voice 16–260 ms.
4. **Per-call counters:** `/api/ui/calls` `imbe` 81 / 72 / 72 / 72 and `ldu` 9 / 8 / 8 / 8;
   `/api/recordings` `imbe_extracted` 81 / 72 exactly, `ldu1_count + ldu2_count` 9 / 8;
   `vocoder_errors` near 0 on the cabled replay. `/api/traffic` `imbe.*` still global and
   monotonic (fbench reads them).
5. **Settings:** `PUT /api/ui/settings {"call":{"end_grace_ms":500}}` → the 1014 calls close
   ~0.6 s after their end (`open_ms` ≈ 2.4 s) and the replies still pre-empt only if they
   come before; `{"call":{"end_grace_ms":0}}` → close at the next tick after the marker.
   Restore 2000. Out-of-range values → 400.
6. **SD storage:**
   - `GET /api/ui/settings`: `recording_storage.sd.state` "ok", `free_bytes` ≈ 58 GB.
   - `PUT {"recording":{"storage":"sd"}}` → `active` "sd"; new items in `/api/recordings`
     have `storage` "sd" (briefly `sd_pending`), files appear in `/mnt/sd/p25_recordings`,
     `writes_ok` rises, `last_write_ms` / `max_write_ms` reported; playback works while
     pending and after.
   - **Stall tolerance:** during the soak with storage "sd", load the card, e.g.
     `dd if=/dev/zero of=/mnt/sd/stall.bin bs=1M count=2000 conv=fsync` (repeat), then delete
     the file. Expect `max_write_ms` in seconds and `queue_jobs` rising and draining, and
     **no change** in `rf.p25_replay` (297 IMBE / loop, 0 missed), in `/api/dibit_delivery`
     ages, or in live audio (`/ws/audio` underruns).
   - **Restart p25-httpd:** SD recordings listed again (`sd.indexed_at_boot` = count), the
     first new call_id is above the highest SD id, old ones play; RAM recordings are gone.
   - **Card problems:** `mount -o remount,ro /mnt/sd` → `sd.state` "read_only" within 30 s
     (immediately after re-selecting "sd"), new recordings go to RAM (`fallbacks_to_ram`,
     `sd_unavailable` in `/api/log?category=recorder`); remount rw → back to SD. Same with the
     card unmounted ("absent"). Fill check: `min_free` is 64 MB.
   - Retention: `{"recording":{"sd_max_count":5}}` → `evicted`, files deleted on the card;
     `{"recording":{"max_count":1}}` never deletes SD files.
7. **On air (not the replay):** queued grants (`/api/log` shows the reply's grant during the
   first talker; the first call keeps all its IMBE), no split calls from grant repeats, and
   after an encrypted teardown a same-frequency clear grant decodes (chain re-enabled).

## Bench verification (2026-09-27, unit A, cabled replay from B)

- **Close timing** matched 1.8, polled at 4 Hz over 2 loops:
  - every push-to-talk went acquiring → voice → hang, with `close_via` "end" and `end_lc`
    "talk_complete" (the first talker) or "channel_user" (the reply);
  - the reply closes `call_end` at `open_ms` 3752–3884 (was 12.8 s) and the first talker
    `tg_change` at 2439–2576;
  - the chain is Idle between exchanges;
  - per-call `imbe` / `ldu` are 81/72/72/72 and 9/8/8/8, with first voice 63–265 ms.
- **Soak with the SD card stalling** (`fbench run rf.p25_replay -p loops=64`, SD storage):
  - The card was kept busy by back-to-back 2 GB `dd … conv=fsync`, with single recording
    writes up to 6.1 s and 2–3 s common.
  - Decode score 100.0 % (297.0 / 297 IMBE per loop, HDU 4.0), 0 resyncs, traffic age p99
    189 ms.
  - 264 SD writes, 0 failed, 0 RAM fallbacks.
- **SD store:**
  - Files land in `/mnt/sd/p25_recordings` (9 ms per write when idle) and play back from the
    card.
  - After a restart, 288 SD recordings were indexed at boot and call ids continued above them.
  - A read-only remount during a replay reported state `read_only`, sent 3 recordings to RAM
    (`sd_unavailable`) and went back to SD after the rw remount. That replay still scored
    99.9 %.
  - `sd_max_count` 5 evicted 320 and left 5 files.
- **Found by the soak and fixed here: same-frequency resume after a pause.**
  - The Idle → Active same-frequency path ("M2B" coast) kept the traffic chain's AGC / PLL
    state. That state had been demodulating noise since the carrier dropped, and the soak
    log shows `pll preserved=8579`, the clamp.
  - With the old 10 s hang this path was rare. With 057 it runs whenever the next call for a
    channel comes after the 2 s grace.
  - A 15 s single-exchange window (every loop a same-frequency resume after ≈ 10.8 s parked,
    `-p clip_start_s=13 -p clip_seconds=15 -p loops=16`) recovered 68.7 % (105 / 153 IMBE
    per loop, HDU 1.37 / 2).
  - `resume_needs_reset()` now resets via the same retune path as a frequency change
    (NCO write + LSM reset + PLL seed) unless the chain carried voice within 1 s and its PLL
    is under half the clamp. After the fix: 100.0 % (153 / 153, HDU 2.0), with all 16
    resumes choosing "reset".

## Files

- New: `p25-httpd/src/app/call_counters.rs` (+ tests), `app/grant_stats_tests.rs`,
  `audio/rec_storage.rs` (+ tests), `tools/sdrtrunk_teardown_stats.py`.
- Changed: `app/grant_follower.rs` (+ tests), `app/imbe_forwarder.rs`, `app/grant_stats.rs`,
  `app/vocoder_task.rs`, `app/ui_state.rs` (+ tests), `audio/mod.rs`, `audio/recorder.rs`
  (+ tests), `services/ui_settings.rs` (+ tests), `protocol/p25/voice_frame.rs`,
  `jmbe/mod.rs`, `vocoder/mod.rs`, `httpd/mod.rs`, `httpd/api/{ui,history,system,talkgroups}.rs`,
  `httpd/ui/js/{api,format,store}.js`, `views/{now,settings}.js`,
  `components/{call_card,calls_list}.js`, `p25-json/src/ui.rs`, `main.rs` (wiring, SD index,
  BUILD_TAG), `doc/P25_API.md`, `tools/README.md`.
