# P25 HTTPD API Reference

Canonical reference for every HTTP / WebSocket endpoint exposed by
`p25-httpd` running on the Fishball Z7020. Source of truth is
[`p25-httpd/src/httpd/mod.rs`](../p25-httpd/src/httpd/mod.rs); typed
JSON shapes are in [`p25-httpd/p25-json/src/lib.rs`](../p25-httpd/p25-json/src/lib.rs).

**Default target**: `http://192.168.2.1:8080` (wired Ethernet
direct-connect). All endpoints accept GET unless otherwise noted.
JSON throughout. No auth — the radio is on a private subnet.

---

## Quick start

```bash
# One-shot status snapshot:
python tools/p25_status_and_next_step.py

# Or hit a single endpoint by hand:
curl -s http://192.168.2.1:8080/api/system | python -m json.tool
```

---

## Endpoint catalogue

**Authoritative runtime source:** `GET /api/endpoints` returns the live
in-code catalogue. The daemon itself is the source of truth; this
document reflects HEAD at the time of the last Stage 2 refactor
(2026-04-17). Endpoints below are grouped by the
[`p25-httpd/src/httpd/api/`](../p25-httpd/src/httpd/api/) module layout
introduced in Stage 2 — the groupings mirror the consumer-facing
categories an Android app (or any headless consumer) would reach for
together.

Phase 9 retirement (2026-04-15): `/api/lsm` (Phase 6D software LSM
pipeline stats) was removed; `/api/hdl_lsm` is the PL-side runtime
endpoint now. `/api/decoder_compare` dropped `ps_iq_lsm` and
`ps_phase6d`; it is now a 3-column matrix (`ps_c4fm`, `ps_lsm`,
`pl_hdl`). See
[`doc/changes/039_phase9_retire_phase6d_iq_lsm.md`](changes/039_phase9_retire_phase6d_iq_lsm.md).

### `api/system` — identity + health + self-describe

| Path | Method | Returns | Purpose |
|---|---|---|---|
| `/` | GET | HTML | Web UI (change 056): shell page, `Cache-Control: no-cache`; asset URLs are `ui/<BUILD_TAG>.<hash>/...` |
| `/ui/{version}/{*path}` | GET | CSS / JS | Change 056 embedded UI assets (`httpd/ui/`). JS as `text/javascript`. Current version `immutable` + ETag, any other version `no-cache`, unknown path 404 |
| `/api/system` | GET | `SystemInfo` | System identity: NAC, WACN, RFSS, site, control channel, secondary CCH, SNDCP channels, system clock, build tag. Change 071a: `control_channel_hz`, `secondary_cch_a_hz` / `_b_hz`, and `neighbours: [{system_id (hex), rfss_id, site_id, lra, channel, freq_hz (its control channel), flags (conventional / failure / valid / active), services, age_ms, count}]` from Adjacent Status Broadcasts (cleared on a site switch or retune) |
| `/api/sys_health` | GET | JSON | **Stage 2** — process + kernel health: loadavg, daemon RSS, thread count, free memory. Cheap to poll from a mobile client |
| `/api/endpoints` | GET | JSON | Self-describing endpoint list (authoritative — the live `ENDPOINT_CATALOGUE`) |
| `/api/set_time` | POST | JSON | `?unix_ms=<i64>` — push browser/client wall-clock to the board. For RNDIS-USB or air-gapped setups where NTP is unreachable. Change 067: with the clock source `site` the control channel's time wins again within seconds |

### `api/radio` — live radio state (primary-view endpoints)

| Path | Method | Returns | Purpose |
|---|---|---|---|
| `/api/stats` | GET | `DecoderStats` | Decoder + AD9361 + FPGA counters: dibit count, overflow flag, AGC gain, RSSI, RX LO, RF bandwidth, sampling freq, gain mode, DDC geometry, wall clock |
| `/api/grants` | GET | `Vec<ChannelGrant>` | Active voice grants (talkgroup-deduped; `encrypted` + `in_encrypted_history` badges) |
| `/api/bands` | GET | `Vec<BandInfo>` | Frequency band table from `IDEN_UPDATE*` opcodes. Change 071a: `slots` (1 = FDMA, 2/4 = TDMA; a TDMA band's channel numbers count timeslots) |
| `/api/hdl_lsm` | GET | JSON | HDL LSM chain runtime: cumulative, live, last NID, 32-entry NID ring |
| `/api/irq_stats` | GET | JSON | Per-IRQ wait counts and average wait (all 6 DMAs) |
| `/api/decoder_compare` | GET | JSON | 3-column matrix: `ps_c4fm`, `ps_lsm`, `pl_hdl` |

### `api/traffic` — current-call view

| Path | Method | Returns | Purpose |
|---|---|---|---|
| `/api/traffic` | GET | JSON | Traffic-follower state + manual knobs: `?follower=on/off`, `?reset_stats=1`, `?retune_hz=<i64>` (routes through full chain), `?demod_enable=0/1` |
| `/api/traffic2` | GET | JSON | Change 066: second traffic chain (core 0.3.0) registers (`nid_nac`, `pll_dbg`, AGC, NCO, ring position, `irq_total`), `in_use`, and `follower` (chain 2's call) when the follower runs it. Bring-up: `?freq_hz=<Hz>` or `?retune_hz=<i64>` (retune with reset, enable), `?enable=0\|1`, `?probe_ms=<ms>` (reads the chain-2 ring and decodes it: dibit histogram, frame syncs, TSBK opcodes). 409 without the chain; the controls need `force=1` while the follower uses chain 2. |
| `/api/imbe_dump` | GET | JSON | Raw IMBE frame ring (diagnostic; vocoder input) |
| `/api/audio` | GET | WAV | Streaming vocoder PCM as an open-ended WAV file (8 kHz 16-bit mono). Change 066: one traffic chain, `?chain=1\|2` (default 1) |
| `/api/audio_test` | GET | JSON | One-shot vocoder test-tone ring dump |

### `api/talkgroups` — per-TG metadata

| Path | Method | Returns | Purpose |
|---|---|---|---|
| `/api/aliases` | GET, PUT | `AliasMap` | Talkgroup-id → display-name map. Change 056: persisted (`/mnt/jffs2/p25-ui-settings.json` `tg_aliases`) and applied to both control decoders (it used to reach only the C4FM decoder, so aliases never showed on LSM sites) |
| `/api/monitor` | GET, PUT | JSON | Monitor list. `?add=N` / `?remove=N` / PUT body `{"talkgroups":[...]}`. When non-empty, grant follower ignores TGs not in the list. Change 056: persisted (`monitor_tgs`) and restored at boot |
| `/api/encrypted_tgs` | GET, PUT | JSON | Persistent encryption blocklist. `?add=N` / `?remove=N` / `?clear=1`. Any TG ever seen encrypted is eagerly added |
| `/api/grant_map` | GET | JSON | Accumulated per-`(tg, freq)` grant map with first/last-seen and encrypted counters; frequency roll-up for LO-centering decisions |

### `api/history` — time-series + retention

| Path | Method | Returns | Purpose |
|---|---|---|---|
| `/api/log` | GET | JSON | Event-log ring. `?since=<seq>` — entries after seq, OLDEST first (`&limit=N`, default 200, max 16384); `?category=grant\|traffic\|voice\|vocoder\|system\|recorder\|duid`. Change 056: `?tail=1` — the NEWEST `limit` matches (a plain first read returns boot-time entries); `?from_ms=&to_ms=` — wall-clock window on `timestamp_ms`; `?tsbk=0` — drop the control-channel TSBK mirror lines (`grant` entries with `fields.event_type`). Filters apply before `limit`. Response `{last_seq, count, entries[]}` |
| `/api/recordings` | GET | JSON | Recording list, newest first (one row per call; `id` = lifecycle call_id). Change 057: per-recording counters (`imbe_extracted`, `imbe_dropped`, `hdu_count`, `ldu*_count`, `tdu*_count`, `vocoder_*`) are this call's own frames, counted by call_id where each frame is decoded (056 took global-counter deltas at the close; pre-056 at finalise, which included the next call's frames). Both stores are listed: `storage` `ram`\|`sd` per item, `sd_pending: true` while the SD write is queued (the WAV is already playable). `?limit=N` (optional). Top level: `count`, `total`, `max` (live RAM retention), `max_sd`, `max_sd_bytes`, `storage` (store for new recordings), `items` |
| `/api/recordings` | DELETE | JSON | Change 065: `?store=sd\|ram\|all` deletes every recording of that store (files and list entries; a recording still being written is not listed yet and is kept). Returns `{ok, deleted}` |
| `/api/recordings/{id}` | GET | WAV | Download a recorded call by id (`.wav` suffix optional; HTTP Range supported). Change 057: served from either store; a recording still waiting for the SD writer is served from its RAM copy |
| `/api/recent_tsbks` | GET | JSON | Newest 50 TSBKs as `{age_secs, block, summary}` |
| `/api/tsbk_opcodes` | GET | JSON | Per-opcode + per-block-position histogram with parsed/unparsed flag + MFID breakdown |

### `api/ui` — web UI documents (change 056)

One document per question, built on the board from the call lifecycle (the single
call-identity authority), the grant-stats rings, the recordings ring and the persisted UI
settings. Types: [`p25-json/src/ui.rs`](../p25-httpd/p25-json/src/ui.rs). Design and field
semantics: [`changes/056`](changes/056_web_ui_review_and_redesign.md).

| Path | Method | Returns | Purpose |
|---|---|---|---|
| `/api/ui/state` | GET | `UiState` | ~1 KB, for a 1 Hz poll. `now_unix_ms` + `clock_valid` (board clock; compute ages against it), `site` (identity, `cc_freq_hz`, `modulation`, `acquired`, `last_tsbk_age_ms`, `tsbk_per_s` and `tsbk_ok_pct` over ~10 s, `health` ok\|stale\|searching), `call` (null when idle; `call_id`, `tg` / `tg_alias`, `source` / `source_alias`, `sources`, `freq_hz`, `channel`, `encrypted`, `started_unix_ms`, `elapsed_ms`, `phase` acquiring\|voice\|hang, `voice_ms`, `first_voice_unix_ms`, `last_voice_unix_ms`, `close_in_ms`, change 057: `close_via` end\|timeout (which close is pending: end-of-transmission grace, not extended by CC updates, or no keep-alive), `close_window_ms` (that rule's full length, for a countdown), `end_lc` (terminator LC, e.g. `talk_complete`, `channel_user`); `phase` is `hang` once `close_via` is `end`; `recording`), `chain` (`state`, `parked_freq_hz`, `follower_enabled`, `lock_freq`, `delivery_mode`), `recording` (`enabled`, `max_count`, `count`, change 057: `storage` ram\|sd selected for new recordings, `sd_state` when the SD store is selected or holds recordings, `sd_count`, `ram_count`), `audio` (`listeners` = browsers on `/ws/audio`, `lag_total`), `calls_rev`, `settings_rev`, `log_last_seq`. Change 066: `calls` (the open call of every traffic chain, same shape as `call` plus `chain` 1\|2) and `chains` (every running chain: `chain`'s fields plus `number` and `tg`); `call` / `chain` stay chain 1's |
| `/api/ui/calls` | GET | `UiCalls` | Recent calls newest first, grant summaries joined with recordings by call_id. `?limit=N` (default 40, max 250), `?nf=0` hides encrypted / not-followed grants. Per item: `voice_ms` (WAV length, else IMBE × 20 ms), `open_ms` (lifecycle open time: voice plus the time until the close — change 057: end grace or the reply's grant, typically 2–4 s for a 1.4 s PTT; pre-057 up to 10 s more), `air_ms`, `first_voice_ms`, `imbe`, `ldu`, `vocoder_errors` (change 057: frames whose IMBE FEC corrected more than 4 bits; was never counted) / `_silent`, `not_followed`, `close_reason` call_end\|tg_change\|timeout\|stream_lag, `recording` {`id`, `url`, `duration_ms`, `size_bytes`, `filename`, `storage`} and `audio_status` recorded\|saving\|not_recorded\|evicted\|no_voice\|encrypted\|not_followed\|missing. Change 057: `imbe` / `ldu` / `vocoder_*` are the call's own counts by call_id; frames of the call decoded after its close (air-time tail) are added within ~0.25 s and bump `calls_rev`. Refetch when `/api/ui/state` `calls_rev` changes |
| `/api/ui/settings` | GET | JSON | `{settings: {recording: {enabled, max_count, storage, sd_max_count, sd_max_mb}, call: {hang_ms, end_grace_ms}, radio: {gain_mode, manual_gain_db}, tg_aliases, unit_aliases, monitor_tgs, tg_groups: [{name, tgs}] (change 063; list order = priority), speakers: {left, right (group names), other both\|left\|right\|off, preempt}, clock: {source site\|ntp\|manual} (change 067), ignore_tgs (change 068: never followed, wins over monitor_tgs and the groups; sorted), site, sites: {<site>: {tg_aliases, unit_aliases, profiles: [{name, tg_groups, speakers, monitor_tgs, ignore_tgs}], active_profile}} (change 069: the live fields above are the active site's names and its active profile, kept in step)}, profiles: {site, site_label, names, active} (change 069), rev, file, load_note, last_save_error, recording_storage, limits, encrypted_tgs}`. Change 057 `recording_storage`: `dir` / `tmpfs` / `count` / `free_bytes` (pre-057 keys, now describing where the NEXT recording goes), `selected` ram\|sd, `active` ram\|sd (SD selected but unusable → ram), `ram` {`dir`, `count`, `bytes`, `free_bytes`}, `sd` {`dir`, `state` unknown\|ok\|absent\|read_only\|full\|error, `detail`, `ready` ("ok" or why new recordings go to RAM), `count`, `bytes`, `total_bytes`, `free_bytes`, `writes_ok`, `writes_failed`, `fallbacks_to_ram`, `deletes`, `last_write_ms`, `max_write_ms`, `queue_jobs`, `queue_bytes`, `writing_for_ms` (age of the write in progress: a stall shows here), `writer_running`, `last_error`, `last_probe_unix_ms`, `indexed_at_boot`, `index_note`}, `moves_on_change` false. `limits` adds `sd_max_count_max`, `sd_max_mb_min` / `_max`, `hang_ms_min` / `_max`, `end_grace_ms_max` |
| `/api/ui/settings` | PUT | JSON | Partial patch, any subset of `settings`; maps / lists replace. Validated (`recording.max_count` 1..500, change 057: `recording.storage` "ram"\|"sd", `recording.sd_max_count` 1..5000, `recording.sd_max_mb` 16..32768, `call.hang_ms` 1000..30000, `call.end_grace_ms` 0..10000; names ≤ 48 chars, TG ≠ 0, radio id 1..2^24−1, unknown fields rejected → 400, nothing changes). Change 069: `{"profile": {"select": name}}` \| `{"profile": {"create": {"name", "copy": bool}}}` \| `{"profile": {"rename": {"from", "to"}}}` \| `{"profile": {"delete": name}}` act on the active site's profiles and must be alone in the patch (names ≤ 32 chars, unique ignoring case, ≤ 32 per site, the last one cannot be deleted); a switch drops a call the new setup does not follow. `switch_site` (used by `POST /api/site`) loads that site's names and profile. Applied live (recorder policy, retention enforced at once, call-close timing on the next lifecycle tick, SD re-probed when the store changes to SD, aliases to both decoders, monitor list) and persisted atomically to `/mnt/jffs2/p25-ui-settings.json` (`P25_UI_SETTINGS_FILE` overrides). Changing `storage` moves nothing: new recordings go to the new store, existing ones stay listed and playable where they are, each store's retention deletes only its own files. Response `{ok, persisted, save_error, evicted, ...GET body}` |

Recording off: the recorder opens no WAV for new followed calls (a recording in progress
completes); the call list shows them as `not_recorded`; the recorder log says
`call_open_skipped` `reason=recording_disabled`.

### `api/tuning` — runtime knobs

| Path | Method | Returns | Purpose |
|---|---|---|---|
| `/api/presets` | GET | JSON | List DDC presets: `presets[]` with `name`, `sample_rate_hz`, `rf_bandwidth_hz`, `decim[3]`, `nco_half_window_hz`, `rejection_25k_db`; plus `current`, `default`, `center_locked` |
| `/api/preset` | POST | JSON | Apply a preset. Body `{preset, center_freq_hz?, gain_mode?, gain_db?}`. Slow path: AD9361 resettle + DDC coefficient reload. Change 070: without `center_freq_hz` the LO goes where the window planner puts it for this preset (control channel inside, most channel weight covered, the covered set centred, no channel within 15 kHz of the LO); `preset: "auto"` also lets the planner pick the preset (narrowest of 8M / 12M / 16M covering every channel). 409 for "auto" without an active site |
| `/api/sites` | GET | JSON | `{ok, active, sites: [{name, label, active}]}` |
| `/api/site` | POST | JSON | `?name=<site>[&no_apply=true]`: make the site active (persisted), set its control channel, change 069: load its names and profile, change 070: count grants for it. The Radio page then posts `/api/preset` `{"preset": "auto"}` |
| `/api/site/plan` | GET | JSON | Change 070: `{ok, plan: {site, auto, locked, preset, sample_rate_hz, lo_hz (as the DDC sees it: crystal trim removed), low_hz, high_hz (usable window: ±0.45 × sample rate), control_hz, min_preset (070b), channels: [{freq_hz, grants, listed (in the site file), weight, covered}], covered_weight, total_weight, best: {preset, sample_rate_hz, lo_hz, usable_half_hz, covered_weight, total_weight}, better (the best window is worth a move), last_recentre_unix_ms}}`. Channel weight: grants seen, at least 1 for a listed channel. 409 without an active site |
| `/api/site/plan` | PUT | JSON | Change 070: `{"auto": bool, "min_preset": "8M"\|"12M"\|"16M"\|null}` (either): recentre automatically; 070b: the narrowest preset the planner may pick here (null = narrowest that fits). Saved per site in `/mnt/jffs2/p25-plans/<site>.json` with the grant counts (`P25_PLANS_DIR` overrides). 400 for another preset |
| `/api/site/recentre` | POST | JSON | Change 070: move to `plan.best` now (a preset apply; a call on the air is cut). The recentre task does the same by itself when `auto`, not locked, `better`, both chains idle, 2 min after start and 10 min after the last move |
| `/api/tune` | POST | JSON | Scanner retune. Body `{radio_freq_hz, center_mode?}`. Auto recenters LO only when window exceeded; Lock returns 409 if outside window |
| `/api/rx_gain` | GET, PUT | JSON | AD9361 RX gain + AGC mode. `?db=<-3..76>` sets manual hardwaregain; `?mode=manual\|slow_attack\|fast_attack\|hybrid` sets `gain_control_mode`. Both params can be combined; mode applied first. A successful change is saved in the UI settings (`radio.gain_mode` / `radio.manual_gain_db`, response `persisted`) and applied at the next start after `--hardwaregain` (change 060) |
| `/api/modulation` | GET, PUT | JSON | Change 071b: both control decoders run (HDL LSM, software C4FM); the active one publishes grants and TSBK events. PUT `?set=auto\|c4fm\|lsm` sets `mode` (0 auto / 1 C4FM / 2 LSM; auto picks by TSBK CRC passes over 5 s, switching only for 20 % more). GET: `mode`, `active` / `label`, `tsbk_ok` / `tsbk_fail` / `nid_decoded_ok` / `tsdu_ok` per decoder, `c4fm_software` {`cpu_pct` (of one core), `chunks`, `lagged`, `resets`, `pll_rad_per_symbol`, `iq_samples`} |
| `/api/bch_t` | GET, PUT | JSON | Runtime BCH-t correction cap per decoder. `?side=control\|traffic&t=<0..11>` |
| `/api/sync_tune` | GET, PUT | JSON | Runtime sync-detector Hamming-distance threshold |
| `/api/decoder_reset` | GET, POST | JSON | Zero the decoder counters (clean post-flash measurements) |

### `api/chain` — HDL chain internals (control + traffic symmetry)

| Path | Method | Returns | Purpose |
|---|---|---|---|
| `/api/dibit_dump` | GET | JSON | Raw C4FM dibit DMA ring (inner/outer ratio, raw_duid histogram) |
| `/api/control_lsm_dibit_dump` | GET | JSON | Raw control-chain LSM dibit DMA ring |
| `/api/traffic_lsm_dibit_dump` | GET | JSON | Raw traffic-chain LSM dibit DMA ring (symmetric to control side) |
| `/api/control_dibit_capture` | GET | JSON | Rolling dibit snapshot from control-chain LSM decoder (post-demod, up to 2048 dibits) |
| `/api/traffic_dibit_capture` | GET | JSON | Same shape, traffic-chain LSM decoder |
| `/api/control_dibit_capture_aligned` | GET | JSON | Next-sync-aligned capture with sync + NID + TSDU body + BCH result |
| `/api/traffic_dibit_capture_aligned` | GET | JSON | Same alignment scheme, traffic side |
| `/api/control_iq_dump` | GET | WAV (audio/wav) | Post-DDC complex IQ samples as a WAV file (stereo i16 @ 50 kSPS, I=L/Q=R; labelled 62.5 kSPS before 2026-09-27, so older dumps play 25 % fast). `?seconds=N` (1..60, default 5). SDRTrunk-ingestible |
| `/api/traffic_iq_dump` | GET | WAV (audio/wav) | Same as control side, centered on the follower's current NCO offset |
| `/api/control_lsm_control` | GET | JSON | Read all 4 control-chain `lsm_control` bits (enable / dma_enable / dc_block / agc). `?dc_block=0\|1` toggles DC blocker |
| `/api/traffic_lsm_control` | GET | JSON | Same, traffic chain. `?dc_block=0\|1` and `?agc=0\|1` writable |
| `/api/nid_capture` | GET | JSON | Per-DUID NID ring with BCH distance + sync distance |
| `/api/dibit_delivery` | GET | JSON | Change 054. Per ring (`control`, `traffic`): `requested_mode` / `active_mode`, `age` (dibit age at delivery = poll time − estimated production time: `mean_ms`, `p50_ms`, `p90_ms`, `p99_ms`, `max_ms`, histogram), `clock` (production-clock `uncertainty_dibits` / `uncertainty_ms`, reseeds), `counters` (polls, bytes / dibits delivered, resyncs + skipped bytes, phase mismatches, copy errors, cuts recorded / applied / clamped, `epoch_splits`, framer resets, dibits fed / gated / `dibits_discarded_presettle`), `last_resync`; traffic also `recent_cuts` (last 64 applied epoch cuts). |
| `/api/dibit_delivery` | POST | JSON | Change 054 runtime switch for bench A/B: `?mode=airtime\|poll\|legacy` (every ring, or `&ring=control\|traffic\|traffic2`; change 066: `traffic` = every traffic chain in use), `&poll_ms=N` (5..1000), `&settle_dibits=N`, `&reset=1` (clear stats). |

### `api/debug` — visual diagnostics

| Path | Method | Returns | Purpose |
|---|---|---|---|
| `/api/spectrum` | GET | JSON | FFT bins from the IQ ring. `?chain=control\|traffic&fft=<1024\|2048\|4096\|8192\|16384>&averages=<N>`. Power-averages N non-overlapping segments — noise floor drops by ~10·log₁₀(N) dB, carriers stay put. Response includes `fft_size` + `averages_used`. |
| `/api/constellation` | GET | JSON | IQ scatter from the LSM slicer input. `?chain=control\|traffic` |
| `/api/wideband_iq_capture` | GET, POST | JSON | Raw 4 MSPS pre-DDC IQ tap. POST `?seconds=N` (1..30) opens `/tmp/p25_iq_captures/wb_iq_<ts>_<N>s.cs16` (interleaved i16 LE I/Q). GET returns active-capture progress + `last_path`. Auto-enables wideband_iq DMA on POST; disable via `/api/sw_demod?enabled=0`. (2026-05-03 dual-DDC pivot.) |
| `/api/sw_demod` | GET, POST | JSON | Live SW demod runtime gate over wideband_iq_dma. POST `?enabled=0\|1`. GET returns runtime stats. (2026-05-03.) |

### `api/forensics` — Track-2 HDL-vs-SW dibit forensics (2026-05-03+)

On-device dibit ring + auto-triggered wideband IQ for HDL-vs-SW
divergence diff. Companion host tool: [`tools/p25_forensics_pull.py`](../tools/p25_forensics_pull.py). Output dir per call: `/tmp/p25_forensics/run_<unix>_tg<TG>_<freq>/{meta.json, hdl_dibits.bits, FINDINGS.md}`. The matching wideband cs16 path is recorded in `meta.wideband_remote`.

| Path | Method | Returns | Purpose |
|---|---|---|---|
| `/api/forensics_arm` | POST | JSON | Arm the on-device dibit ring + wideband auto-trigger. Query params: `dibit_max_mb` (1..64, default 8 — RAM cap for the dibit buffer), `wideband_seconds` (1..30, default 30 — wideband IQ duration), `auto_rearm` (default `true`), `follow_encrypted` (default `false` — if true, bypasses grant_follower's encrypted rejection so the chain follows encrypted calls; audio is still garbled but dibits are usable for diff). |
| `/api/forensics_disarm` | POST | JSON | Clear `armed`. In-flight capture (if any) still finalises. |
| `/api/forensics_status` | GET | JSON | `armed`, `auto_rearm`, `follow_encrypted`, `active`, `dibit_max_bytes`, `wideband_seconds`, lifetime counters (`total_runs_completed`, `total_dibits_captured`), `last_run_dir`, and `active_call` (if a call is being captured) with live dibit count + truncation flag. |

### `api/ws` — WebSocket streams

| Path | Method | Framing | Purpose |
|---|---|---|---|
| `/ws/events` | WS upgrade | JSON text | Real-time event stream (`TsbkEvent` + system events). **Stage 2**: synthetic `{"event_type":"ws_lag"}` frame sent when the broadcast channel overruns a slow consumer, so the connection stays up instead of closing |
| `/ws/audio` | WS upgrade | binary + text control | Vocoded PCM at 8 kHz 16-bit mono, 320-byte binary frames (160 samples = 20 ms per frame). **Stage 2**: on Lagged, server sends a text control frame `{"type":"lag","skipped":N}` so the client can flush its jitter buffer. Change 062: a text frame `{"type":"meta","tg":N,"src":N,"call_id":N}` precedes the first audio frame of each talkgroup / call (the web UI routes talkgroups to the left / right speaker); clients ignore text types they do not know. Change 066: carries traffic chain 1 only; `/ws/audio?v=2` carries every chain, each binary frame prefixed with 4 bytes `[lane, 0, 0, 0]` (0 = chain 1, 1 = chain 2) and each meta frame carrying `"lane"` |
| `/ws/iq` | WS upgrade | binary + text hello | Complex IQ from the selected chain + source. Query params: `?chain=control\|traffic&source=pre_diff`. `pre_diff` (the only live source since Phase 10.8) streams the LSM demod after rotate + AGC and before the diff demod, at 9.6 kSPS (2 samples/symbol), for eye plots. The `post_ddc` / `post_lsm` rings are gone from the bitstream. First message is a JSON hello: `{"type":"hello","sample_rate_hz":<sr>,"format":"i16le-iq-stereo","chain":"...","source":"...","buf_bytes":32768}`. Subsequent messages are binary, one 32 KB sub-buffer each (8192 complex i16 samples). **Single-consumer today** — multiple subscribers race for ring sub-buffers; multi-consumer broadcast is a follow-up if the race becomes measurable |

### Client reconnect guidance

- `/ws/events`: exponential backoff (1s → 2s → 4s → 8s → 15s ceiling). Reset to 1s on first successful message. The embedded dashboard implements this; other clients (Android app) should do the same. A flat reconnect delay hammers the server during daemon restart.
- `/ws/audio`: user-initiated (Play Audio button). No auto-reconnect today — if the connection drops mid-call the client should show a "disconnected" indicator and let the user retry. Server sends `ws_lag` control frames on broadcast-channel overrun; clients should use them to flush any downstream jitter buffer.
- `/ws/iq`: user-initiated (Debug tab's Live toggle on the eye / spectrum card). Dashboard implements exponential backoff 1 s → 15 s ceiling.

---

## Typed responses

### `GET /api/system` → `SystemInfo`

```json
{
  "nac": "8A1",
  "wacn": "BEE00",
  "system_id": "8A0",
  "rfss_id": 1,
  "site_id": 1,
  "lra": 0,
  "control_channel": "0-1593",
  "secondary_cch_a": "0-1349",
  "secondary_cch_b": "0-1277",
  "sndcp_downlink_channel": "0-1117",
  "sndcp_uplink_channel": "15-4095",
  "system_clock": "2026-04-11 18:52 UNLOCKED",
  "build": "2026-04-11-phase6g.1-preserve-grant-source-id-on-update"
}
```

Optional fields are omitted (`#[serde(skip_serializing_if = "Option::is_none")]`)
when the decoder hasn't seen the corresponding TSBK yet. The `build`
field is the canonical "which binary is running" identifier — bumped
on every feature-flag commit per the
[`feedback_bump_build_tag.md`](../../.claude/projects/c--Users-Andy-Projects-MAIA-SDR-maia-sdr/memory/feedback_bump_build_tag.md)
rule.

### `GET /api/grants` → `Vec<ChannelGrant>`

```json
[
  {
    "channel": "0-1117",
    "talkgroup": 202,
    "talkgroup_alias": null,
    "source": 1011,
    "frequency_mhz": 857.9875,
    "age_secs": 12
  }
]
```

**De-dupe rules** (Phase 6G.1):

- The decoder's internal `grants` map is keyed by channel but is
  also TG-deduped: when a `GroupVoiceChannelGrant` or
  `GroupVoiceChannelGrantUpdate` arrives for an active TG on a new
  channel, the prior entry is dropped.
- The HTTP handler reads grants from `lsm_decoder` (single source
  of truth since the Phase 9 retirement). Pre-Phase-9 this was a
  union with `iq_lsm_decoder.grants`; removing the union also
  fixed a "stale age" bug where the retired decoder had no expire
  loop and its grants stuck at their discovery timestamps forever.
- `source` is **preserved across updates** (commit `1e29839`):
  `GroupVoiceChannelGrantUpdate` doesn't carry a source, so the
  decoder pulls the prior source from any existing entry for the
  same TG. Initial `GroupVoiceChannelGrant` always uses the fresh
  source from the TSBK.

Sorted by `age_secs` ascending (newest first).

### `GET /api/bands` → `Vec<BandInfo>`

```json
[
  {
    "identifier": 0,
    "base_frequency_mhz": 851.00625,
    "channel_spacing_khz": 6.25,
    "transmit_offset_mhz": -45.0,
    "bandwidth_khz": 12.5
  }
]
```

Unioned across both decoders, keyed by `identifier`, first-write
wins. Sorted by `identifier` ascending.

### `GET /api/stats` → `DecoderStats`

```json
{
  "recent_messages": 1000,
  "active_grants": 2,
  "bands_known": 6,
  "system_acquired": true,
  "dibit_count": 30482512,
  "overflow": false,
  "dma_next_address": 402653184,
  "rx_gain_db": 27.0,
  "rx_rssi_db": 105.5
}
```

`rx_gain_db` and `rx_rssi_db` come from the AD9361 via libiio. Slow
AGC parks high (~70-76 dB) on weak signals, low (~10-30 dB) on
strong. RSSI is on a relative dB scale; ~100-110 dB is normal P25
reception on this site (cross-checked against PlutoSDR + SDRTrunk).

### `GET /api/recent_tsbks` → JSON

```json
{
  "count": 50,
  "messages": [
    {"age_secs": 0.4, "block": "TSBK1", "summary": "GRP_V_CH_GRANT_UPDT CH_A:0-1117 TG_A:00202 ..."},
    {"age_secs": 0.5, "block": "TSBK2", "summary": "TDMA_SYNC_BCST 2026-04-11 18:52 UNLOCKED"}
  ]
}
```

`block` is `TSBK1` / `TSBK2` / `TSBK3` matching the TSBK position
within the parent TSDU. `summary` is the human-readable string used
in the dashboard activity feed and is the same string the WebSocket
event stream uses.

### `GET /api/tsbk_opcodes` → JSON

```json
{
  "opcodes": [
    {"opcode": 22, "label": "SNDCP_DCH_ANN_EX", "ok": 893, "fail": 239, "parsed": true},
    {"opcode": 11, "label": "(unknown)", "ok": 110, "fail": 163, "parsed": false}
  ],
  "by_position": {
    "tsbk1": {"attempts": 2418, "crc_ok": 1808, "pct": 74.8},
    "tsbk2": {"attempts": 2417, "crc_ok": 1843, "pct": 76.3},
    "tsbk3": {"attempts": 2417, "crc_ok": 1651, "pct": 68.3}
  },
  "tsbk_block_attempts_total": 7252,
  "crc_ok_total": 5302,
  "crc_fail_total": 1950,
  "crc_ok_pct": 73.1,
  "tsdu_attempts": 2418,
  "blocks_per_tsdu": 3.0,
  "mfid_breakdown": {"standard_0x00": 4243, "motorola_0x90": 1059, "harris_0xA4": 0, "other": 0}
}
```

`parsed: false` means the decoder sees the opcode but has no
matching `TsbkMessage` variant — typically vendor-specific
(Motorola `mfid=0x90`, Harris `mfid=0xA4`) or pure-status
acknowledgments. See doc 030 for the rationale on which opcodes
are intentionally not parsed.

### `GET /api/decoder_compare` → JSON

The canonical A/B diagnostic surface. Five top-level keys, one
per pipeline:

```json
{
  "pl_hdl": {
    "label": "PL HDL LSM chain (FPGA gateware)",
    "total_nids": 6708,
    "valid_nids": 6628,
    "valid_pct": 98.81,
    "drop_count": 0,
    "winner_nac": "0x8A1",
    "sync_distance": 0,
    "pll_dbg": 601,
    "sp_dbg": 3395,
    "iq_overflow_ticks": 1,
    "dibit_overflow_ticks": 0
  },
  "ps_c4fm": { "...": "C4FM software decoder (HDL C4FM dibit-fed, DORMANT on LSM sites)" },
  "ps_lsm":  { "...": "PS LSM framer (software framer on PL HDL LSM dibits — production)" }
}
```

**Phase 9 retirement (2026-04-15):** the `ps_iq_lsm` and
`ps_phase6d` sections were removed when the Phase 6D software LSM
pipeline retired. The response is now a 3-column matrix:
`ps_c4fm` (kept as a dormant fallback for future C4FM sites),
`ps_lsm` (the PS framer consuming the PL LSM dibit DMA ring —
this is the current production control-channel decoder), and
`pl_hdl` (the FPGA LSM chain's own runtime heartbeat).

Per-pipeline fields for the two PS framers (`ps_c4fm`, `ps_lsm`):

- `tsbk_block_attempts`, `tsbk_crc_ok`, `tsbk_crc_failures`,
  `tsbk_crc_ok_plain`, `tsbk_crc_ok_xored`, `tsbk_unknown_opcode`,
  `tsbk_trellis_failures`
- `nid_attempts`, `nid_decoded_ok`, `nid_decoded_tsdu`,
  `nid_decode_failures`, `nid_invalid_duid`
- `sync_hits`, `sync_near`, `sync_best_dist`
- `total_dibits`, `messages` (capped at `max_recent`)
- `active_grants`, `bands_known`, `system_nac`

`pl_hdl` is the HDL chain heartbeat — NID-level only (no TSBK
framing, which is the PS framer's job). Because every HDL
hard-sync hit triggers exactly one BCH decode in gateware, the
following PL-side aliases are computable from
`{total_nids, valid_nids}`:

- PL sync hits        = `total_nids` (every HDL sync → NID pipeline)
- PL NID attempts     = `total_nids` (sync hit == attempt)
- PL NID decoded OK   = `valid_nids`
- PL NID BCH failures = `total_nids - valid_nids`

The dashboard's Decoder Comparison table fills those aliases into
the PL column automatically.

### `GET /api/sync_tune`

Runtime sync-threshold knob (Phase 6F.7+). GET returns:

```json
{"threshold": 14, "default": 14, "min": 0, "max": 47}
```

PUT with `?threshold=N` (or POST body) overrides. Threshold is the
maximum 48-bit sync correlator Hamming distance accepted as a sync
hit; lower = stricter, higher = more permissive. Doc 030 settled
on `14` as the steady-state default.

### `GET /api/decoder_reset`

Resets all per-pipeline cumulative counters to zero. Useful after
flash so cumulative percentages reflect post-PLL-lock steady state
rather than including the early acquisition window. Returns:

```json
{"ok": true, "note": "lsm_decoder counters + histograms cleared. System identity, bands, grants, and aliases preserved."}
```

### `GET /api/control_lsm_control`

Phase 6G.2. Read-back of all three `lsm_control` register bits, with
an optional `?dc_block=0/1` query-param shortcut to toggle the DC
blocker in-place. The two other bits (`lsm_enable`,
`lsm_dibit_dma_enable`) are read-only from this endpoint — flipping
them at runtime would tear down the radio for no debugging benefit,
and the `devmem` escape hatch is still there if you really need it.

```bash
# Read current state:
curl http://192.168.2.1:8080/api/control_lsm_control

# Disable the DC blocker (and read back to confirm):
curl 'http://192.168.2.1:8080/api/control_lsm_control?dc_block=0'

# Re-enable:
curl 'http://192.168.2.1:8080/api/control_lsm_control?dc_block=1'
```

Response shape:

```json
{
  "lsm_enable":           true,
  "lsm_dibit_dma_enable": true,
  "lsm_dc_block_enable":  true,
  "updated_from":         null,
  "register_address":     "0x7C4600A0",
  "bit_layout": {
    "lsm_enable":            "[0]",
    "lsm_dibit_dma_enable":  "[1]",
    "lsm_dc_block_enable":   "[2]"
  },
  "note": "..."
}
```

`updated_from` is `null` if no `dc_block` query param was passed,
or the previous value of the bit (`true` / `false`) if a write
happened. So a write request returns the **prior** value in
`updated_from` and the **new** value in `lsm_dc_block_enable`.

The handler takes the `ip_core` lock once and does the optional
write + the readback under it, so a write+read sequence is atomic
from the perspective of any other PS code touching the register.

### `GET /api/traffic`

Phase 7A.1. Traffic-channel grant follower state, dibit DMA
counters, and optional manual control of the traffic DDC NCO and
demod_enable bit. The endpoint is read-only by default; passing
any of the four documented query params performs a write before
the snapshot read.

**Read shape (no params):**

```bash
curl http://192.168.2.1:8080/api/traffic
```

```json
{
  "state":                     "Acquiring",
  "follower_enabled":          true,
  "current_channel":           1117,
  "current_talkgroup":         202,
  "current_frequency_hz":      857987500,
  "nco_word":                  6029312,
  "nco_word_hex":              "0x005C0000",
  "last_offset_hz":            -112500,
  "grants_seen":               17,
  "retunes":                   3,
  "last_retune_secs_ago":      1.84,
  "stats": {
    "wakeups":            234,
    "total_buffers":      234,
    "total_bytes":        958464,
    "total_dibits":       3833856,
    "dibit_hist":         [958464, 958464, 958464, 958464],
    "dibit_hist_pct":     [25.0, 25.0, 25.0, 25.0],
    "started_secs_ago":   1.85,
    "last_secs_ago":      0.04
  },
  "irq": {
    "traffic_dma_total":  234
  },
  "applied":              [],
  "errors":               [],
  "phase":                "7A.1",
  "modulation":           "C4FM-only (LSM traffic chain coming in 7A.2)",
  "controls": {
    "reset_stats":   "?reset_stats=1            -- zero TrafficStats",
    "follower":      "?follower=on|off          -- pause/resume 50 ms poll",
    "retune_hz":     "?retune_hz=<i64>          -- manual NCO offset (Hz, signed)",
    "demod_enable":  "?demod_enable=0|1         -- manual demod_enable bit"
  },
  "note": "..."
}
```

**Manual-control query params** (applied in this fixed order
before the snapshot read, so a single combined call does the right
thing):

| Order | Param | Effect |
|---|---|---|
| 1 | `?reset_stats=1` | Zero out TrafficStats (`wakeups`, `total_*`, `dibit_hist`). |
| 2 | `?follower=on\|off` | Pause/resume the 50 ms grant-follower polling task. When `off`, manual retunes won't be immediately overridden. State does NOT persist across `p25-httpd` restarts. |
| 3 | `?retune_hz=<i64>` | Manually write the traffic DDC NCO offset in Hz, signed, relative to the AD9361 RX LO. Bypasses the grant follower entirely. Does NOT touch `demod_enable` -- explicit by design. |
| 4 | `?demod_enable=0\|1` | Manually flip the `traffic_demod_control.demod_enable` bit. Required after a manual retune to actually start the dibit stream. |

The `applied` array in the response echoes the writes that fired,
and `errors` lists any params that failed to parse. So a successful
combined call:

```bash
curl 'http://192.168.2.1:8080/api/traffic?follower=off&reset_stats=1&retune_hz=2862500&demod_enable=1'
```

returns `"applied": ["reset_stats=1", "follower=off", "retune_hz=2862500", "demod_enable=true"]`
and the snapshot fields will reflect the new state immediately.

**Why both a follower pause AND an explicit demod toggle?** The 50
ms grant-follower task drives the traffic DDC and `demod_enable`
based on whatever the canonical LSM control-channel decoder
(`lsm_decoder`) reports as the most recent grant. Without
`?follower=off`, any manual retune would be silently overridden
within ~50 ms by whatever the next grant snapshot says. And without
the explicit `?demod_enable=1`, a manual retune leaves the dibit
ring quiet -- you wouldn't see any dibits at the new frequency. The
two controls compose: pause the follower, retune, enable demod.

**Why the dibit histogram is the headline metric at 7A.1.** The
traffic chain is C4FM-only at 7A.1 and the day-one validation
target (Clay County) is LSM, so the dibit *content* is expected
garbage on real LSM voice channels -- the C4FM slicer running on
LSM produces a roughly even spread across {0,1,2,3} (essentially
random). The histogram is enough to confirm "the chain is alive"
(non-zero, even spread) vs. "the chain is dead" (all zeros, all
the same value, or no IRQs firing). Phase 7A.2 adds an LSM
parallel chain on the traffic side and the histogram becomes
decode-quality data.

### `GET /api/traffic` -- Phase 7A.2 additions

Phase 7A.2 added an LSM demod chain on the traffic side
(mirroring Phase 6E.9 on the control side) and a 16 ms heartbeat
task that polls the new `traffic_lsm_status` register bank for NID
events and dispatches each DUID to a TrafficManager handler:

| DUID | Name | Dispatch |
|------|------|----------|
| `0x0` | HDU (Header) | `hdu_received(now, nac)` -- call start, refresh activity |
| `0x3` | TDU | `tdu_received(now, nac, false)` -- call end, start 2 s post-TDU hold |
| `0x5` | LDU1 (voice + LC) | `ldu_received(now, nac, false)` -- activity refresh |
| `0xA` | LDU2 (voice + ESS) | `ldu_received(now, nac, true)` -- activity refresh |
| `0xF` | TDU_LC | `tdu_received(now, nac, true)` -- call end with LC payload |

The post-TDU hold window matches SDRTrunk PR #2010 semantics: a TDU
does NOT immediately deallocate the slot. The slot stays bound to
the same TG for 2 seconds after the TDU so that PTT releases
between speakers in a multi-speaker conversation reuse the same
slot. If the TG resumes within the hold (a new HDU or LDU arrives),
the hold is cancelled. Otherwise the hold expires and the lock is
released.

**New JSON fields in the `/api/traffic` snapshot (Phase 7A.2):**

```json
{
  "phase":                     "7A.2",
  "modulation":                "C4FM + LSM (parallel chains, LSM is the active one for HDU/TDU/LDU dispatch)",
  "last_duid":                 5,
  "last_duid_hex":             "0x5",
  "last_duid_label":           "LDU1",
  "last_nac":                  2209,
  "last_nac_hex":              "0x8A1",
  "hdus_seen":                 1,
  "ldus_seen":                 27,
  "tdus_seen":                 0,
  "post_tdu_hold_remaining_ms": null,
  "irq": {
    "traffic_dma_total":       234,
    "traffic_lsm_dibit_total": 12
  },
  "traffic_lsm_chain": {
    "enabled":           true,
    "dibit_dma_enabled": true,
    "dc_block_enabled":  true,
    "bch_busy":          false,
    "in_nid_window":     false,
    "nid_event":         false,
    "nid_valid":         true,
    "n_errors":          2,
    "sync_distance":     1,
    "dibit_overflow":    false,
    "drop_count":        0,
    "dibit_last_buffer": 3,
    "dibit_next_addr":   "0x1B003800",
    "pll_dbg":           1234,
    "sample_point_dbg":  17542
  }
}
```

`post_tdu_hold_remaining_ms` is `null` when no hold is active. When
a TDU has just arrived it is `2000` and counts down each subsequent
poll. If a new LDU arrives during the window, it is cleared back to
`null` (the conversation continues).

`traffic_lsm_chain` is a snapshot of the new `traffic_lsm` register
bank (offset `0x7C46_00C0`). Field semantics are identical to
the control-side `lsm` bank (see `doc/P25_ADDRESS_MAP.md` Phase
7A.2 detail section). The `nid_event` Rsticky bit is cleared by
the `/api/traffic` read itself, so this snapshot reflects "is a
new event pending right now" rather than the cumulative count
(use `hdus_seen + ldus_seen + tdus_seen` for the cumulative
count).

**Verification on a clean Clay County voice grant:**

```bash
# Wait for an active call, then snapshot every second:
for i in 1 2 3 4 5; do
    curl -s http://192.168.2.1:8080/api/traffic | python -c "
import sys,json
d=json.load(sys.stdin)
print(f't={i} state={d[\"state\"]} tg={d[\"current_talkgroup\"]} '
      f'duid={d[\"last_duid_label\"]} hdus={d[\"hdus_seen\"]} '
      f'ldus={d[\"ldus_seen\"]} tdus={d[\"tdus_seen\"]} '
      f'hold={d[\"post_tdu_hold_remaining_ms\"]}')
"
    sleep 1
done
```

Expected pattern: `state=Active tg=202 duid=LDU1` or `LDU2` for
the duration of the call, `hdus_seen` increments by 1 at the
start, `ldus_seen` increments rapidly throughout (at ~7-8 LDUs/sec
since each LDU is ~140 ms), `tdus_seen` increments by 1 at the
end, then `state` transitions to Idle ~2 s after the TDU when the
post-TDU hold window expires.

### `GET /api/traffic` -- Phase 7C additions

Phase 7C added an LDU sync + IMBE frame extraction pipeline that
runs the new traffic-side LSM dibit DMA ring through a fourth
`ControlChannelDecoder` instance (the existing three only see the
control channel). The decoder's `process_dibit` state machine
gained LDU1 / LDU2 / HDU / TDU / TDU_LC dispatch arms (was
TSDU-only) that feed a `VoiceHandler` trait. Phase 7C ships an
`ImbeCounter` voice handler that updates atomic counters; Phase 7D
will replace it with an `ImbeForwarder` that pushes raw 144-bit
IMBE frames to a vocoder mpsc channel.

Also: encryption flag plumbed end-to-end from the
`GroupVoiceChannelGrant` TSBK service options byte through
`GrantInfo` to `/api/grants` and `/api/traffic.current_call_encrypted`.
**This is operationally equivalent to HDU encryption-flag parsing
without needing the trellis + RS(36,20,17) decoder** -- the
control channel grant arrives before the HDU does, so the
encryption decision happens earlier and saves the DDC retune for
encrypted calls when `?ignore_encrypted=1` is added in Phase 7D.

**New top-level fields in `/api/traffic` (Phase 7C):**

```json
{
  "phase":                  "7C",
  "current_call_encrypted": false,
  "imbe": {
    "hdu_count":            1,
    "ldu1_count":           14,
    "ldu2_count":           13,
    "tdu_count":            0,
    "tdu_lc_count":         1,
    "imbe_frames_extracted": 243,
    "last_imbe_secs_ago":   0.18
  },
  "traffic_lsm_decoder": {
    "sync_hits":            28,
    "sync_near_misses":     2,
    "best_sync_distance":   1,
    "recent_msg_count":     0,
    "ldu1":                 14,
    "ldu2":                 13,
    "hdu":                  1,
    "tdu":                  0,
    "tdu_lc":               1
  }
}
```

**Field semantics:**

- **`current_call_encrypted`**: encryption flag for the
  currently-locked TG, read from the `lsm_decoder.grants` store.
  `null` if no call is active or if the locked TG isn't in the
  grant store. `false` for clear voice, `true` for encrypted.
  Phase 7D vocoder reads this to gate IMBE -> PCM decoding.
- **`imbe.hdu_count` / `ldu1_count` / `ldu2_count` / `tdu_count` /
  `tdu_lc_count`**: cumulative count of each DUID type the
  voice handler has seen. Update synchronously from inside the
  dibit decoder task via `AtomicU64` so `/api/traffic` reads
  them with no lock.
- **`imbe.imbe_frames_extracted`**: total raw 144-bit IMBE
  frames pushed to the (future) vocoder. **Should equal**
  `(ldu1_count + ldu2_count) * 9` exactly -- any divergence
  indicates an extraction failure (wrong status-strip math,
  wrong dibit count, etc).
- **`imbe.last_imbe_secs_ago`**: seconds since the most recent
  IMBE frame batch was extracted. `null` if no frames have been
  seen yet. Useful for the dashboard to show "audio active" /
  "audio silent" indicators.
- **`traffic_lsm_decoder`**: framer-internal counters from the
  `traffic_lsm_decoder` itself. `sync_hits` is the most
  important diagnostic -- if zero during an active call, the
  decoder isn't finding sync in the dibit stream (indicates
  either an HDL bug from Phase 7A.2, or my length_dibits
  corrections in Phase 7C are off). `ldu1`/`ldu2`/`hdu`/`tdu`/`tdu_lc`
  here should track 1:1 with the corresponding `imbe.*_count`
  fields above.

**New per-grant fields in `/api/grants` (Phase 7C):**

```json
[
  {
    "channel":          "0-1117",
    "talkgroup":        202,
    "talkgroup_alias":  "FIRE OPS",
    "source":           1011,
    "frequency_mhz":    857.9875,
    "age_secs":         3,
    "encrypted":        false,
    "emergency":        false
  }
]
```

`encrypted` and `emergency` come from the
`GroupVoiceChannelGrant` TSBK service options byte (bits 6 and 7
respectively). Both are preserved across `GroupVoiceChannelGrantUpdate`
refreshes via `take_other_grants_for_talkgroup` so the values
don't get wiped on every periodic update.

### `GET /api/aliases` / `PUT /api/aliases` → `AliasMap`

Talkgroup-id → display-name map. Change 056: stored in the UI settings document
(`/mnt/jffs2/p25-ui-settings.json`, `tg_aliases`), restored at boot, and applied to both
control-channel decoders so `/ws/events` TSBK events carry `talkgroup_alias`. (Before 056
the map lived only in the C4FM decoder's memory: not persisted, and invisible on LSM
sites.) Radio-unit names (`unit_aliases`) are edited through `/api/ui/settings`.

```json
{"202": "FIRE OPS", "402": "PD CH 4", "300": "EMS"}
```

PUT replaces the entire map (blank names remove an entry; names are trimmed to 48
characters; TG 0 → 400).

### `GET /ws/events` (WebSocket)

Real-time TSBK event stream. Each frame is a `TsbkEvent`:

```json
{
  "timestamp": "01:31:30.344",
  "event_type": "GRP_VCH_GRANT",
  "summary": "TSBK2 GRP_VCH_GRANT TG:00202 SRC:3406028 -> 0-1117 (857.9875 MHz)",
  "talkgroup": 202,
  "talkgroup_alias": "FIRE OPS",
  "channel": "0-1117",
  "frequency_mhz": 857.9875,
  "source": null
}
```

One frame per parsed TSBK (housekeeping opcodes — NET/RFSS/ADJ status,
IDEN, TDMA sync, SCCB, SNDCP, vendor — are suppressed), plus traffic
`TRF_*` voice-frame events and `recording_saved`. The web UI uses it
only as a trigger for an early `/api/ui/state` poll (`GRP_VCH_GRANT`,
`TRF_HDU`, `TRF_TDULC_CALL_TERM`, `TRF_VOICE_END`, `recording_saved`).

Change 057: `TRF_VOICE_END` `{call_id, tg, lc, air_ms}` marks the end
of a voice transmission (first LC-valid TDULC after the call's voice;
`lc` talk_complete\|call_termination\|network_teardown\|channel_user\|link_control).
The call closes `call.end_grace_ms` later unless voice resumes. Also in
`/api/log` (`voice` category).

**Important:** the WebSocket is the **only** structured per-event
source. `/api/recent_tsbks` returns just `{age_secs, block, summary}`
(3 fields, summary is a human-readable string) — useful for a one-shot
dashboard snapshot but lossy for any tool that wants to filter on
talkgroup, channel, or source. `TsbkEvent` carries 8 structured fields
including `talkgroup` (u16), `talkgroup_alias` (looked up from
`/api/aliases`), `channel`, `frequency_mhz`, and `source` (caller
RadioId — preserved across grant updates as of commit `1e29839`).
External clients that want to react to specific TSBKs (talkgroup
loggers, alerters, audio-tap triggers, future voice-channel followers)
should subscribe to `/ws/events` and not poll the REST endpoints.

The connection opens with no auth, no subscribe message, no filter
— every parsed TSBK becomes a frame, ordered. The dashboard JS just
does:

```js
const ws = new WebSocket(`${proto}//${location.host}/ws/events`);
ws.onmessage = (e) => { /* prepend to activity feed */ };
```

---

## Web UI views and their backing endpoints (change 056)

The page at `/` (ES modules under `p25-httpd/src/httpd/ui/`) renders
from two documents; other endpoints are polled only while the view that
needs them is open.

| View | Endpoint(s) | Cadence |
|---|---|---|
| Header, Now (current call, site, recording) | `/api/ui/state` | 1 s (5 s when the tab is hidden) + `/ws/events` kick |
| Now · Recent calls | `/api/ui/calls` | when `calls_rev` changes, else every 30 s |
| Radio | `/api/stats`, `/api/ppm`, `/api/modulation` (2 s); `/api/site/plan` (10 s, change 070); `/api/presets`, `/api/sites` (once); `/api/spectrum_wide` or `/api/spectrum` (1 s); writes: `/api/tune`, `/api/preset`, `/api/site`, `/api/site/plan`, `/api/site/recentre`, `/api/rx_gain`, `/api/modulation`, `/api/ppm/auto`, `/api/ppm_calibrate` | while open |
| Diagnostics | `/api/pipeline`, `/api/dibit_delivery`, `/api/traffic`, `/api/sys_health` (2 s); `/api/log?tail=1` then `?since=` (2 s); `/api/endpoints` (once) | while open |
| Settings | `/api/ui/settings` (on open and when `settings_rev` changes), `/api/grant_map`, `/api/encrypted_tgs`; writes: PUT `/api/ui/settings`, PUT `/api/encrypted_tgs`, POST `/api/set_time` | on demand |
| Listen button | `/ws/audio` | while playing |

The page POSTs `/api/set_time` once per load only when the board clock
is invalid (< 2020) or more than 2 minutes off, and the "set automatically"
preference is on (default). Change 067: never while the clock source is
`site` and the control channel's time is decoded (the radio follows the
site on purpose; a replayed site may be months off the browser).

## Board clock (change 067)

The radio has no battery-backed clock. `settings.clock.source` picks what sets it:

- `site` (default): the control channel's SYNC_BCST (TSBK 0x30). Date, hour and minute,
  plus 7.5 ms micro-slots when the site locks them to the minute; otherwise the second is
  found from the minute rollover. An unset clock and the first correction since start
  step the clock; later differences over 0.5 s are slewed (`adjtime`, no jumps back) and
  only more than 30 s off steps again. One NTP attempt at start covers the time before
  the control channel is decoded.
- `ntp`: internet time at start and hourly (every 5 min until one works).
- `manual`: only `POST /api/set_time`.

`/api/ui/state` `site.site_time` (`unix_ms`, `precision` precise\|second\|minute,
`ext_locked`, `local_offset_min`, `board_offset_ms`, `age_ms`) and `site.clock_source`
report it. The bench's `rf.p25_corpus` pins the source to `manual` for a run (the replay's
site time is a different day per item) and restores it.

## Legacy dashboard (retired)

The pre-056 single-file dashboard (`dashboard.html` at `/legacy`) was removed after
change 059; the web UI at `/` replaced it in change 056. Its panel-to-endpoint map is in
git history (this file before the retirement commit).

---

## Endpoints we do NOT have yet

Things you might reasonably expect to find here that aren't
implemented (in roughly the order they'd be useful):

| Want | Why missing | Status |
|---|---|---|
| `GET /api/talkgroups` (catalogue of TGs ever heard, not just currently active) | DEVPLAN.md mentions it as a Phase 2 deliverable but we never built it | Medium — need to grow `ControlChannelDecoder` to retain a TG-history map |
| `GET /api/voice_channel/<grant_id>` (initiate voice follow on a granted channel) | Phase 7 voice-channel-following work, not started | Significant — depends on voice-follow infrastructure |
| `GET /api/audio.opus` (live decoded voice) | Requires IMBE/AMBE vocoder + audio output | Significant + licensing question |

These are real new features for Phase 7+ with their own design
questions. The previously-missing `/api/control_lsm_control` runtime
read/write endpoint shipped in Phase 6G.2 and is now in the table
above.

---

## See also

- [`P25_ADDRESS_MAP.md`](P25_ADDRESS_MAP.md) — register-bank layout
  documentation; the bit-level "what does PS read/write" reference
- [`DEVPLAN.md`](../DEVPLAN.md) — the original (somewhat-outdated)
  P25 trunking dev plan
- [`changes/`](changes/) — phase-by-phase change docs (latest is
  [057](changes/057_call_close_per_call_counters_sd_recordings.md), call close,
  per-call counters and SD recordings)
- [`tools/p25_status_and_next_step.py`](../tools/p25_status_and_next_step.py)
  — comprehensive status snapshot + next-step recommendation script
