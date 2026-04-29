# 048 — Session lifecycle refactor: capture-time routing, GRANT-driven open/close, multi-speaker bundling

**Build tag:** `2026-04-26-session-lifecycle-refactor`
**Files touched:** `audio/mod.rs`, `audio/recorder.rs`, `app/imbe_forwarder.rs`,
`app/vocoder_task.rs`, `app/grant_follower.rs`, `app/grant_stats.rs`,
`httpd/dashboard.html`, `main.rs` (BUILD_TAG)

## Problem

Post-flash diagnostics from `2026-04-26-audio-driven-speakerend` showed
`recorder_chunks_dropped_no_active = 1980` over a single session — ~40 s of
audio that played live but never landed in any recording. Capture rate
across 17 grants was only 18 % of vocoder output. Two root causes:

1. **Audio-vs-lifecycle decoupling failure.** The audio pipeline (LSM lock →
   LDU dispatch → IMBE → vocoder → broadcast) ran independently of the
   GRANT lifecycle. Chunks routed by `chunk.call_id` (stamped at vocoder-
   submit time), but `mirror_active` lag, vocoder PCM lag (~200-400 ms
   typical), and pre-CallOpen race windows meant chunks for the right
   call frequently landed with the wrong (or zero) call_id and were
   dropped.
2. **Per-PTT splitting bundled poorly.** Multiple primary GRANTs for the
   same TG/freq created separate sessions (e.g. TG 700 saw 6 distinct
   call_ids in 30 s; should have been one bundled session). Audio-driven
   close (`last_audio + 600 ms`) and TDULC-terminator close fragmented
   real continuous PTT trains.

## Operator-defined spec (this change)

> "A primary grant opens the recording session. The first audio block
> starts the audio. The last audio block ends the audio. The grant
> waits for end of call to anticipate gaps in audio blocks."
>
> "We should be bundling speakers if there are more than one in a grant.
> We are doing per-grant recording — only split if TG changes."
>
> "Only close if TG different."
>
> "There is no MOT if the last speaker was a dispatcher."
>
> "As long as we timestamp the audio packets that occurred before the
> end of call signal comes we should know all those are the packets
> to be part of the call that ended and same recording."

Distilled rules:

| Event | Action |
|---|---|
| Primary `GRP_VCH_GRANT`, no active session | Open new session |
| Primary GRANT, same TG + freq as active | Bundle: refresh + add SRC |
| Primary GRANT, different TG, same channel | Pre-empt close + open new |
| Primary GRANT, different freq | Ignore (sticky-locked elsewhere) |
| `GRP_VCH_GRNT_UPD` (UPDATE) | Refresh-only (no SRC, no enc, no split) |
| HDU dispatch | Refresh, no split |
| LDU1 LC voted SRC | Add to `sources_observed` |
| TDULC MOT TalkComplete | Add SRC to `sources_observed` (NOT close) |
| TDULC standard CallTermination | Add SRC to `sources_observed` (NOT close) |
| Bare TDU | Ignore (heavy false-positive) |
| 5 s no audio | Close (`Timeout`) |

## Implementation

### 1. Audio packet capture-time stamp

Added `AudioChunk.captured_at_ms: u64` (wall-clock unix ms). Stamped in
`ImbeForwarder::forward_frames` when frames are submitted to the vocoder
mpsc — this is when the chain *saw* the dibits, before any vocoder /
queue lag. Propagated through the `(tg, src, call_id, captured_at_ms,
[ImbeFrameRaw; 9])` tuple, set on the broadcast `AudioChunk` in
`vocoder_task`.

### 2. Capture-time-window routing in recorder

Replaced `chunk.call_id == active.call_id` matching with
`chunk.captured_at_ms ∈ [active.open_at_ms, active.close_at_ms or u64::MAX]`.

- `open_at_ms` set on `CallOpen` from `ev.timestamp_unix_ms`.
- `close_at_ms` set on `CallClose` from `ended_unix_ms` payload.
- Chunks captured before `open_at_ms` → drop (belong to prior session).
- Chunks captured during open window or while in closing-drain
  (`close_at_ms` set, `captured_at_ms <= close_at_ms`) → append.
- Chunks captured after `close_at_ms` → drop (belong to next session).

Late chunks for a closed session still land correctly regardless of
arrival time vs the close event. Vocoder lag of any size is absorbed.

### 3. Closing-state drain on the recorder side

Removed the inline drain loop that previously held `audio_rx` open after
`CallClose`. Replaced with passive: `CallClose` now stamps `close_at_ms`
on `active`; the periodic tick finalises the recording
`CLOSING_DRAIN_MS = 2 s` after `close_at_ms`. While in closing state,
the main `select!` loop continues to service audio chunks via the
capture-time-window route. New `CallOpen` during drain force-finalises
old (defensive); the lifecycle layer's TG-change-only split policy
makes this rare.

### 4. Lifecycle: GRANT-driven, terminator-agnostic

`grant_follower` rewritten:

- Removed `RETRANSMIT_WINDOW_MS`, `CALL_TIMEOUT_MS`, `HDU_SPLIT_GAP_MIN_MS`,
  `CALL_QUIET_MS`, `CALL_TDULC_QUIET_MS`, `CALL_NO_AUDIO_HARD_CAP_MS`.
- Added `HARD_TIMEOUT_MS = 5_000` (single backstop, no audio).
- `ArrivalDisposition`: `Bundle | Ignore | TgChange` (was `Retransmit |
  Ignore | TgChange | SpeakerChange`).
- `OpenAction`: `Open | Bundle | Preempt(close, open) | None` enum so
  the CcGrantArrival match arm reads as a state transition.
- `CloseReason`: `Timeout | TgChange | StreamLag` (was 5 variants).
- `OpenReason`: `CcGrant | TgChange` (was `CcGrant | SpeakerChange`).
- `SpeakerEnd { source, kind }` boundary: kind no longer drives close.
  `MotTalkComplete` and `CallTermination` both add SRC to
  `sources_observed` and stamp `actual_speaker`. `BareTdu` is no-op.
- HDU: stamps `first_hdu_at_unix_ms` / `first_audio_at_unix_ms`,
  refreshes activity. Never splits.

### 5. Multi-speaker tracking

`ActiveCall.sources_observed: Vec<u32>` (insertion order, deduped) tracks
every distinct SRC seen during the bundled session — primary GRANT.SRC
on each matching arrival, LDU1 LC voted SRC, TDULC MOT BY:. Carried
through `CallTrackerEventKind::CallClose.sources_observed`,
`GrantDecodeSummary.sources_observed`, and `RecordingEntry.sources_observed`.
Dashboard `Recent Calls` column renders comma-separated when len > 1.

### 6. Lag instrumentation

`ActiveCall` (recorder side) tracks `max_chunk_lag_ms`,
`total_chunk_lag_ms`, `lag_count`. Each chunk arrival computes
`now_ms - chunk.captured_at_ms`. Surfaced on `RecordingEntry` as
`max_chunk_lag_ms` and `mean_chunk_lag_ms`. Right-sizes
`CLOSING_DRAIN_MS` empirically — if max is consistently > 2 s, raise the
constant.

### 7. Removed: bare-TDU emission contribution to lifecycle

`audio::TerminatorKind` enum (`BareTdu | MotTalkComplete | CallTermination`)
distinguishes the three SpeakerEnd flavours at boundary emission so the
lifecycle layer can ignore `BareTdu` cleanly. The on_tdu / on_tdu_lc
emission sites in `imbe_forwarder` now tag the kind.

## Behaviour expectations after this build

- `recorder_chunks_dropped_no_active` should drop sharply — only fires
  when chain produces audio with NO grant ever landed (parser miss).
- `recorder_chunks_dropped_call_id_mismatch` semantics changed — now
  counts chunks captured outside the active session's [open, close]
  window. Should also be near zero in steady state.
- Multi-speaker grants on TG 300 / 301 / 318 should bundle into one
  recording with `sources_observed = [src1, src2, ...]` instead of
  fragmenting into per-PTT recordings.
- `first_chunk_after_open_ms` now reflects true acquire latency
  (capture-time of first chunk that landed) regardless of vocoder lag.
- Sessions close when (a) a new GRANT for a different TG arrives, or
  (b) 5 s of no audio elapses. No close on TDULC or bare TDU.
- Encrypted grants still appear in `grant_stats` with `not_followed =
  Some("encrypted")`, close on Timeout (5 s after open since no audio
  flows).

## Log-noise cleanup bundled with this change

Two unrelated logging fixes shipped in the same build:

1. **Per-NID DUID log throttled** — `protocol::p25::control_channel`
   used to push a `Duid`-category event for every successful BCH-
   decoded NID on every chain. On a busy CC this was 95%+ `bch_err=0`
   noise (190 / 200 entries observed). Throttled to fire only when
   `n_errors > 0`, so the log now reflects FEC-correction events
   only. Aggregate counters (`raw_duid_total`, `nid_decoded_tsdu`,
   `nid_decoded_ok`) on `/api/chain` cover the volume diagnostic.
2. **`log_duid` calls removed from `imbe_forwarder`** — per-dispatch
   `log_duid("LDU1")` / `LDU2` / `HDU` / `TDU` / `TDU_LC` calls
   removed from the five framer entry points. These were duplicating
   what `/api/traffic` counters (`hdu_count`, `ldu1_count`, ...)
   already provide. The function definition is retained for future
   debug use but unwired.
