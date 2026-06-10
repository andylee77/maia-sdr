# Change 052 — On-device forensics capture (2026-05-03)

BUILD_TAG: `2026-05-03-on-device-forensics`

## Goal

Move the Track-2 HDL-vs-SW dibit forensics capture from host-side
polling to on-device buffering. The host-poll model (running in
`tools/p25_chain_forensics_capture.py`) lost ~78 % of dibits to
poll-rate gaps because the rolling `/api/traffic_dibit_capture` ring
is only 2048 dibits (= 426 ms at 4800 sym/s); any HTTP poll lagging
> 426 ms overflowed the ring.

## What ships

PS-only change. No HDL bake required.

### `p25-httpd/src/app/forensics.rs` (new, ~360 LOC)

`ForensicsRing` struct: lock-free fast path (`AtomicBool`) + slow path
(`Mutex<ForensicsInner>`). Three states: armed/unarmed × idle/active.

- **Hot path**: `record_dma_words(words)` called from
  `dibit_readers::spawn_hdl_lsm_traffic_reader` on every wakeup.
  Atomic-checks `active`; returns immediately when no call is being
  captured. When active, packs raw 64-bit DMA words into SDRTrunk
  MSB-first 4-per-byte format — same packing as
  `tools/p25_dibit_diff.py` consumes.
- **Lifecycle**: `spawn_forensics_task()` subscribes to the existing
  `CallTrackerEvent` broadcast.
  - On `CallOpen` (when armed): allocates run dir under
    `/tmp/p25_forensics/run_<unix>_tg<TG>_<freq>/`, fires
    `WidebandIqCaptureState::start()` for parallel wideband IQ, sets
    `active=true`.
  - On `CallClose`: clears `active`, drains wideband (~2 s), disables
    wideband DMA, finalises the run dir (writes `meta.json`,
    `hdl_dibits.bits`, `FINDINGS.md`).
- **Auto-rearm**: optional. Single-shot when off; when on, ring stays
  armed for the next call.
- **Encrypted-call follow override**: while armed with
  `follow_encrypted=1`, `app::forensics::follow_encrypted_enabled()`
  returns true. `grant_follower` reads this in its
  encrypted-rejection branch and skips the rejection — letting the
  HDL chain follow encrypted grants for diff testing (audio is still
  garbled, but dibits are usable).
- **Truncation handling**: `dibit_max_bytes` cap (default 8 MB =
  ~110 minutes of dibits). When hit, `dibits_truncated=true` is set
  and further dibits are dropped; status surfaces this so the host
  knows the run isn't complete.

### Hooks

| File | Change |
|---|---|
| `app/dibit_readers.rs` | `spawn_hdl_lsm_traffic_reader` takes `Arc<ForensicsRing>`; calls `forensics.record_dma_words(words)` on every read |
| `app/grant_follower.rs` | Encrypted-rejection branch checks `forensics::follow_encrypted_enabled()`; bypass when set |
| `httpd/mod.rs::AppState` | New field `forensics: Arc<ForensicsRing>` |
| `main.rs` | `ForensicsRing::new()` + `spawn_forensics_task()` wired to `call_tracker_tx` + `wideband_iq_capture` + `ip_core` |

### API

| Endpoint | Method | Behaviour |
|---|---|---|
| `/api/forensics_arm` | POST | Arms the ring. Query params: `dibit_max_mb` (1-64, default 8), `wideband_seconds` (1-30, default 30), `auto_rearm` (default true), `follow_encrypted` (default false). Returns status. |
| `/api/forensics_disarm` | POST | Disarms; in-flight capture still finalises. Returns status. |
| `/api/forensics_status` | GET | Returns: `armed`, `auto_rearm`, `follow_encrypted`, `active`, current `active_call` meta (if any), `last_run_dir`, lifetime counters. |

### Output format (per run)

```text
/tmp/p25_forensics/run_<unix>_tg<TG>_<freq>/
  meta.json           run metadata: build_tag, call_id, freqs,
                       timestamps, close_reason, dibit_count,
                       dibits_truncated, wideband_remote
  hdl_dibits.bits     SDRTrunk-format MSB-first 4-per-byte dibits.
                       Compatible with tools/p25_dibit_diff.py and
                       tools/p25_chain_compare.py.
  FINDINGS.md         skeleton record for the operator
```

The wideband IQ file lives at the path recorded in
`meta.wideband_remote` (typically `/tmp/p25_iq_captures/wb_iq_<...>.cs16`).

## Operator usage

```bash
# Arm with default settings (auto-rearm, 8 MB dibit cap, 30 s wideband).
curl -X POST 'http://192.168.2.1:8080/api/forensics_arm'

# Arm with encrypted-follow for diff testing
curl -X POST 'http://192.168.2.1:8080/api/forensics_arm?follow_encrypted=1'

# Watch progress
curl 'http://192.168.2.1:8080/api/forensics_status' | jq

# After a call completes:
scp -O -r 'root@192.168.2.1:/tmp/p25_forensics/run_<...>/' ./

# Disarm when done
curl -X POST 'http://192.168.2.1:8080/api/forensics_disarm'
```

## Design notes

- **No host polling**: the dibit tee runs inside the existing
  traffic-dibit reader task on every IRQ wakeup. Zero polling rate;
  dibits are captured at full bus rate.
- **Lock-free fast path**: when not capturing, the reader sees only
  one atomic load (`active.load(Relaxed)`) and bails. The mutex is
  held only during a 3-30 s active call.
- **try_lock on the hot path**: if `/api/forensics_status` happens
  to be holding the slow-path mutex when a wakeup fires, the reader
  drops one wakeup's dibits rather than blocking the framer pipeline.
  Status reads are short (~µs) so the loss window is tiny.
- **`auto_rearm` default = true**: matches the operator's expected
  usage (arm and walk away, capture the next several calls).
- **No background overhead when disarmed**: `active=false` causes
  `record_dma_words` to return on the first atomic load. The
  forensics task's `recv()` blocks on the broadcast channel waiting
  for events.

## Memory hits closed

- `feedback_p25_offline_polling_lost_dibits.md` (implicit — was the
  motivating bug from `tools/p25_chain_forensics_capture.py` v1)

## Cross-target compile

`cargo check --target armv7-unknown-linux-gnueabihf` passes the rust
phase clean (link step fails on `aws-lc-sys` as expected per
`feedback_cfg_linux_host_check_blindspot.md`).

## Pending follow-ups

- Update `tools/p25_chain_forensics_capture.py` to call the new
  device endpoints instead of host-polling.
- NID ring + log-event capture into the run dir (currently just dibits +
  wideband); add when the diff workflow needs them.
