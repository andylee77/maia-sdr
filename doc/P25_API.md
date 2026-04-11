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

All 21 routes registered in `httpd/mod.rs`:

| # | Path | Method | Returns | Purpose |
|---|---|---|---|---|
| 1 | `/` | GET | HTML | Embedded dashboard (`index_html`) |
| 2 | `/api/system` | GET | `SystemInfo` | System identity (NAC, WACN, RFSS, site, control channel, secondary CCH, SNDCP channels, system clock, build tag) |
| 3 | `/api/grants` | GET | `Vec<ChannelGrant>` | Active voice grants (talkgroup-deduped, source preserved across updates) |
| 4 | `/api/bands` | GET | `Vec<BandInfo>` | Frequency band table from `IDEN_UPDATE` opcodes |
| 5 | `/api/stats` | GET | `DecoderStats` | Decoder + FPGA-side counters (dibit count, overflow flag, AGC gain, RSSI) |
| 6 | `/api/lsm` | GET | JSON | PS-side raw IQ + soft sync stats (Phase 6D `LsmPipeline`) |
| 7 | `/api/hdl_lsm` | GET | JSON | HDL LSM chain runtime stats (cumulative, live, last NID, NID ring buffer) |
| 8 | `/api/irq_stats` | GET | JSON | Per-IRQ wait counts and average wait times (dibit DMA, IQ DMA, LSM dibit DMA) |
| 9 | `/api/decoder_compare` | GET | JSON | Side-by-side per-pipeline counters: `pl_hdl`, `ps_c4fm`, `ps_lsm`, `ps_iq_lsm`, `ps_phase6d` |
| 10 | `/api/dibit_dump` | GET | JSON | Raw dibit DMA dump (inner/outer ratio, raw_duid histogram) |
| 11 | `/api/lsm_dibit_dump` | GET | JSON | Raw LSM dibit DMA dump from the HDL LSM chain |
| 12 | `/api/lsm_capture` | GET | JSON | Pull a raw IQ capture window from the IQ DMA ring (for offline cross-validation) |
| 13 | `/api/lsm_capture_aligned` | GET | JSON | Same as `lsm_capture` but aligned to a sync hit boundary |
| 14 | `/api/tsbk_opcodes` | GET | JSON | Per-opcode + per-block-position histogram with parsed/unparsed flag and MFID breakdown |
| 15 | `/api/recent_tsbks` | GET | JSON | Newest 50 TSBKs as `{age_secs, block, summary}` strings |
| 16 | `/api/sync_tune` | GET, PUT | JSON | Read or set the runtime sync threshold (Phase 6F.7+) |
| 17 | `/api/decoder_reset` | GET, POST | JSON | Reset the decoder counters to zero (for clean post-flash measurements) |
| 18 | `/api/lsm_control` | GET | JSON | Read all 3 `lsm_control` bits + optional `?dc_block=0/1` query-param shortcut to toggle the DC blocker without ssh+devmem (Phase 6G.2) |
| 19 | `/api/traffic` | GET | JSON | **Phase 7A.1** -- traffic-channel grant follower state, dibit DMA counters, optional `?reset_stats=1`, `?follower=on/off`, `?retune_hz=N`, `?demod_enable=0/1` manual controls |
| 20 | `/api/aliases` | GET, PUT | `AliasMap` | Get or set the talkgroup-id → display-name map |
| 21 | `/ws/events` | WS upgrade | JSON frames | Real-time TSBK event stream (`TsbkEvent`) — one frame per parsed TSBK |

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
- The HTTP handler does a second-pass collapse by talkgroup across
  the union of `lsm_decoder.grants` and `iq_lsm_decoder.grants`,
  keeping the youngest entry per TG. Wildcard `talkgroup=0` is
  excluded from the dedup so unrelated "no-talkgroup" sentinels
  don't collapse.
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
  "ps_c4fm":   { "...": "C4FM software pipeline (HDL c4fm dibit-fed)" },
  "ps_lsm":    { "...": "LSM software pipeline (HDL lsm dibit-fed)" },
  "ps_iq_lsm": { "...": "IQ-LSM software pipeline (raw IQ + soft sync -> TSBK)" },
  "ps_phase6d":{ "...": "Phase 6D sync-only path on raw IQ" }
}
```

Per-pipeline fields for the three full pipelines (`ps_c4fm`,
`ps_lsm`, `ps_iq_lsm`):

- `tsbk_block_attempts`, `tsbk_crc_ok`, `tsbk_crc_failures`,
  `tsbk_crc_ok_plain`, `tsbk_crc_ok_xored`, `tsbk_unknown_opcode`,
  `tsbk_trellis_failures`
- `nid_attempts`, `nid_decoded_ok`, `nid_decoded_tsdu`,
  `nid_decode_failures`, `nid_invalid_duid`
- `sync_hits`, `sync_near`, `sync_best_dist`
- `total_dibits`, `messages` (capped at `max_recent`)
- `active_grants`, `bands_known`, `system_nac`

`pl_hdl` is the HDL chain itself (NID-level only — no PS TSBK
processing). `ps_phase6d` is the legacy raw-IQ sync detector kept
for the soft/hard event count comparison.

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
{"ok": true, "reset": ["lsm_decoder", "iq_lsm_decoder", "c4fm_decoder", "phase6d", "hdl_lsm"]}
```

### `GET /api/lsm_control`

Phase 6G.2. Read-back of all three `lsm_control` register bits, with
an optional `?dc_block=0/1` query-param shortcut to toggle the DC
blocker in-place. The two other bits (`lsm_enable`,
`lsm_dibit_dma_enable`) are read-only from this endpoint — flipping
them at runtime would tear down the radio for no debugging benefit,
and the `devmem` escape hatch is still there if you really need it.

```bash
# Read current state:
curl http://192.168.2.1:8080/api/lsm_control

# Disable the DC blocker (and read back to confirm):
curl 'http://192.168.2.1:8080/api/lsm_control?dc_block=0'

# Re-enable:
curl 'http://192.168.2.1:8080/api/lsm_control?dc_block=1'
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

### `GET /api/aliases` / `PUT /api/aliases` → `AliasMap`

Talkgroup-id → display-name map persisted in `~/.config/p25-httpd/aliases.json`.

```json
{"202": "FIRE OPS", "402": "PD CH 4", "300": "EMS"}
```

PUT replaces the entire map.

### `GET /ws/events` (WebSocket)

Real-time TSBK event stream. Each frame is a `TsbkEvent`:

```json
{
  "timestamp": "2026-04-11T18:52:30Z",
  "event_type": "GRP_GRANT",
  "summary": "GRP_V_CH_GRANT_UPDT CH:0-1117 TG:00202",
  "talkgroup": 202,
  "talkgroup_alias": "FIRE OPS",
  "channel": "0-1117",
  "frequency_mhz": 857.9875,
  "source": null
}
```

One frame per parsed TSBK. The dashboard uses this **only** for the
"Live Activity" feed at the bottom of the page; everything else on
the dashboard is polled from REST every 2 seconds via
`setInterval(refresh, 2000)`.

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

## Dashboard panels and their backing endpoints

For reference, this is what the embedded `index_html` page renders
and where each panel's data comes from. All 9 polled endpoints are
fetched on a 2-second interval; the WebSocket runs in parallel.

| Dashboard panel | Endpoint(s) | Notes |
|---|---|---|
| Header build tag | `/api/system.build` | the "is the right binary on the box?" check |
| Decoder Comparison Matrix | `/api/decoder_compare` | side-by-side `pl_hdl` / `ps_c4fm` / `ps_lsm` / `ps_iq_lsm` / `ps_phase6d` counters |
| System Identity | `/api/system` | NAC, WACN, RFSS/Site, control channel |
| Decode Stats | `/api/stats` | dibit count, overflow, AGC gain, RSSI |
| HDL LSM Chain (PL) | `/api/hdl_lsm` | cumulative + live + 1 s window stats; nested `last_window`, `nid_ring`, `top_nacs` keys |
| HDL LSM NID Ring (last 32) | `/api/hdl_lsm.nid_ring` | per-NID `{t_ms, nac, duid, valid, n_err, sync_d, drop, pll, sp}` |
| IRQ Source Counters | `/api/irq_stats` | per-source IRQ count + rate (dibit/traffic/iq/lsm_dibit) |
| LSM Decoder (Phase 6D) | `/api/lsm` | wakeups, IQ samples, hard/soft sync events, `last_sync`, `top_nacs` |
| Top NACs (LSM) | `/api/lsm.top_nacs` | NAC histogram from soft+hard sync events |
| PS C4FM Dibit Stream | `/api/dibit_dump` | per-bucket dibit histogram, inner/outer ratio, raw_duid histogram |
| PS LSM Dibit Stream | `/api/lsm_dibit_dump` | same shape as `/api/dibit_dump` but on the LSM HDL stream |
| Active Grants | `/api/grants` | TG-deduped, source-preserved across updates |
| Frequency Bands | `/api/bands` | unioned across both decoders |
| Live Activity | **`/ws/events`** | the only WebSocket consumer; richer than `recent_tsbks` |
| Aliases dialog | `/api/aliases` (GET, PUT) | TG-id → name map persisted on the board |

Endpoints **not** consumed by the dashboard (snapshot / debugging
tools only): `/api/recent_tsbks` (used by `tools/p25_check_phase6f4.py`
and `tools/p25_status_and_next_step.py`), `/api/tsbk_opcodes`
(opcode coverage report), `/api/lsm_capture` and
`/api/lsm_capture_aligned` (raw IQ pulls for offline cross-validation),
`/api/sync_tune` and `/api/decoder_reset` (operator knobs).

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
questions. The previously-missing `/api/lsm_control` runtime
read/write endpoint shipped in Phase 6G.2 and is now in the table
above.

---

## See also

- [`P25_ADDRESS_MAP.md`](P25_ADDRESS_MAP.md) — register-bank layout
  documentation; the bit-level "what does PS read/write" reference
- [`DEVPLAN.md`](../DEVPLAN.md) — the original (somewhat-outdated)
  P25 trunking dev plan
- [`changes/`](changes/) — phase-by-phase change docs (latest is
  doc 031, Phase 6G.1)
- [`tools/p25_status_and_next_step.py`](../tools/p25_status_and_next_step.py)
  — comprehensive status snapshot + next-step recommendation script
