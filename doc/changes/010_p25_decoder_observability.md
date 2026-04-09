# 010 -- P25 Decoder Observability + Init Hardening

**Date:** 2026-04-09
**Phase:** 5 (still hardware bring-up; this is the diagnostic infrastructure
that the LSM debug session in change 011 needed)
**Branch:** fishball-p25

---

## Summary

Bundles a series of small but high-leverage changes that turn the P25
decoder from a black box ("the dashboard says Searching") into something
we can actually debug from a browser:

1. **Tezuka init script log redirect.** `S60p25-httpd` was launching the
   binary via `start-stop-daemon -b` which detaches from the controlling
   terminal and closes stdout/stderr. **Every `tracing::info!` we have
   ever written has been going straight to /dev/null.** Now stdout+stderr
   land in `/var/log/p25-httpd.log`.
2. **Explicit `EnvFilter` setup in `main.rs`** so the default level is
   `info,p25_httpd=info` instead of relying on `tracing_subscriber::fmt::init()`'s
   undocumented defaults.
3. **`/api/stats` exposes AD9361 RX gain + RSSI.** No more "ssh in and
   `cat in_voltage0_rssi`" while debugging — both values come back in the
   same poll as the FPGA dibit count and overflow flag.
4. **`/api/dibit_dump` exposes inner/outer histogram percentages and a
   raw on-air DUID histogram.** The DUID histogram in particular is the
   diagnostic that broke open the LSM debug session in change 011 — it
   showed a near-uniform spread across all 16 DUID nibble values, which
   is impossible on a real control channel and immediately implicated
   the demodulator architecture.
5. **`SYNC_THRESHOLD` widened from 4 to 10** (with a long comment
   explaining why and when it should drop back to 4). With the demod
   producing ~12 bit errors per NID due to the LSM/C4FM mismatch, the
   tight threshold-4 detector matched essentially nothing. Loosening to
   10 made the failure mode visible (and let us see *which* error
   patterns were happening) but is a temporary diagnostic measure, not
   a fix.
6. **Periodic `expire_grants(30)` task in `main.rs`.** The
   `ControlChannelDecoder` accumulates grants from TSBK_GRANT messages
   into a `HashMap` but had no pruning, so the dashboard's "Active
   Grants" count would grow forever once decoding started working. Now
   a 5-second tokio interval calls `expire_grants(30)` to drop entries
   older than 30 seconds (P25 typical call timeout).
7. **NID DUID hardcode hack** in `decode_nid` so the decoder enters its
   TSBK-reading state machine on every sync. This was a knowingly
   incorrect hack to flush out downstream bugs faster — see "Hack and
   why" below. Replaced by proper BCH(64,16) FEC in a future change.
8. **Diagnostic raw_duid histogram** maintained in `ControlChannelDecoder`
   so we can observe the *true* on-air DUID distribution while the
   hardcoded value lets the rest of the pipeline run.
9. **Category 1 unused-import cleanups** (`put` from `axum::routing`;
   `TsbkMessage` from `traffic_manager.rs`). The traffic-following
   placeholder code gets a module-level `#![allow(dead_code)]` with a
   `TODO(traffic-following)` note, instead of being deleted.
10. **Removed dead `tracing::debug!`** in `fpga.rs`. The default tracing
    filter is INFO, so this line was emitting nothing. The information
    it carried (DDC NCO offset) was folded into the existing `info!`
    that fires when `configure_ddc` succeeds.

## Why all of this in one change

These are "small fixes that the user finds annoying individually but
together turn an opaque box into an instrumented system." Bundling them
keeps the commit history one commit per "wave of debugging insights."
Each item came from the same long debug session on 2026-04-09 trying
to figure out why the on-target P25 decoder was producing garbage.

## The big one: stdout was going to /dev/null

This deserves its own paragraph because it caused **the most debug
time waste** of any single bug in the project so far. The init script
at `tezuka_fw/board/tezuka/common/overlay_p25/etc/init.d/S60p25-httpd`
was using:

```sh
start-stop-daemon -S -b -q -m -p /var/run/p25-httpd.pid \
    -x /usr/bin/p25-httpd -- ...
```

`-b` (background) detaches the process from the controlling terminal.
On busybox, the standard streams of the daemonised process are then
closed -- meaning every `tracing::info!`, `tracing::warn!`, and
`tracing::error!` call disappeared into the void. We had been
"debugging" with no log output the entire time.

The fix wraps the binary in `sh -c 'exec ... >> /var/log/p25-httpd.log
2>&1'`. The `exec` is important: without it, the shell would be the
process whose pid is recorded in the .pid file, and stop/restart would
target the shell rather than the daemon. With `exec`, the shell
replaces itself with the daemon, the pid in `/var/run/p25-httpd.pid`
points at the daemon directly, and stop/restart work normally.

We also rotate the previous log to `.prev` on each start so a crash
leaves a diagnostic trail across reboots.

The same bug exists in `S60maia-httpd` for the Maia HTTPD. Not fixed
in this change because this is the P25 branch -- but worth fixing in
a future change to `fishball-dev`.

## The DUID histogram is the most important new diagnostic

For an LSM control channel that's working correctly, the raw DUID
nibble extracted from the NID should be `0x7` (TSDU) ~100% of the
time. Anything else means the NID has uncorrected bit errors.

Before this change, the dashboard showed dibit histograms but not
*what was actually being decoded*. The on-target dump:

```
raw DUID histogram (n=203):
0=2%  1=2%  2=3%  3=1%  4=2%  5=6%  6=17% 7=29%
8=3%  9=1%  A=8%  B=5%  C=4%  D=3%  E=8%  F=5%
```

is the immediate proof that the demodulator is broken: a bit-error rate
of ~27% in the 4-bit DUID field, way beyond what BCH(64,16) FEC could
correct (t=11 -> ~17% raw BER ceiling). That number directly motivated
the "we need to look at SDRTrunk's actual algorithm" investigation
that landed in change 011.

Without the histogram in `/api/dibit_dump`, we would have spent another
day trying to fix the slicer.

## Hack and why: hardcoded DUID = TSDU

`p25-httpd/src/p25/fec.rs::GolayDecoder::decode_nid` currently returns
`(nac, 0x7, raw_duid)` -- it always reports the DUID as TSDU regardless
of what the actual on-air bits say, while preserving the raw value in a
side channel for the diagnostic histogram above.

This is a deliberate, documented hack with a clear lifecycle:

- **Why:** Without it, the decoder bounces back to "Hunting" the moment
  it sees an invalid DUID (which is ~96% of the time today). With the
  hack, the decoder enters its TSBK reading state machine on every
  sync, exercising the deinterleaver / trellis decoder / TSBK CRC
  validator / opcode parser code paths. This let us discover whether
  *those* layers had additional bugs hiding behind the demod failure.
- **Why this is OK:** The control channel only carries TSDUs, period.
  Misclassifying a TSBK as a TSDU is a no-op because they would all
  *be* TSDUs. The downstream TSBK CRC will reject any garbage that
  results from the demod still being broken, so the false TSDUs do
  not pollute the upstream state machine.
- **When it goes away:** Replaced by proper BCH(64,16) NID FEC in a
  future change. At that point the function returns the actual decoded
  DUID and the decoder behaves normally.
- **Why it does NOT belong in production:** It would silently
  misclassify voice DUIDs (HDU/LDU1/LDU2/TDU) as TSDUs if the same
  code path were ever used for traffic channel decoding. The function
  docstring loudly warns about this.

## Files changed

| File | Change |
|------|--------|
| `tezuka_fw/board/tezuka/common/overlay_p25/etc/init.d/S60p25-httpd` (Tezuka repo) | Wrap `start-stop-daemon -b` in `sh -c 'exec ... >> /var/log/p25-httpd.log 2>&1'`; rotate previous log to `.prev` on start |
| `p25-httpd/src/main.rs` | Explicit `tracing_subscriber` `EnvFilter` setup; thread `Ad9361` handle into `AppState`; spawn periodic `expire_grants(30)` tokio task |
| `p25-httpd/src/httpd/mod.rs` | Drop unused `put` import; new `ad9361` field in `AppState`; AGC gain + RSSI in `/api/stats`; inner/outer pct + raw_duid histogram + sync threshold in `/api/dibit_dump` |
| `p25-httpd/p25-json/src/lib.rs` | New `rx_gain_db` and `rx_rssi_db` fields on `DecoderStats` (skip-serialize-if-none) |
| `p25-httpd/src/iio.rs` | Add `rx_rssi` IIO attribute via existing macro; module-level `#![allow(dead_code)]` for the symmetric get/set surface |
| `p25-httpd/src/fpga.rs` | Drop the dead `tracing::debug!`; fold NCO frequency into the existing `configure_ddc` `info!` |
| `p25-httpd/src/p25/control_channel.rs` | Bump `SYNC_THRESHOLD` 4 -> 10 (with comment); `pub` it for the dump endpoint; track raw_duid histogram; log raw histogram in periodic stats; expose `raw_duid_histogram()` accessor |
| `p25-httpd/src/p25/fec.rs` | Hardcode DUID = 0x7 in `decode_nid`; return `(nac, hard_duid, raw_duid)` 3-tuple; new test `test_nid_decode_records_raw_duid` |
| `p25-httpd/src/p25/traffic_manager.rs` | Drop unused `TsbkMessage` import; add module-level `#![allow(dead_code)]` with `TODO(traffic-following)` note |

## Verification

After flashing the new bitstream + rebuilt p25-httpd:

```
ssh root@<target> 'tail -50 /var/log/p25-httpd.log'
```

shows actual log output for the first time. Expected lines include
`p25_decoder: NID OK: NAC=0xE28 DUID=0x7 (Tsdu) raw_DUID=0x5 len=336`
(with `raw_DUID` differing from the hardcoded `0x7`, which is the
explicit visibility we wanted).

```
curl http://<target>:8080/api/stats
```

returns `rx_gain_db: 73.0` and `rx_rssi_db: 102.0` alongside the
existing dibit_count / overflow / dma_next_address fields.

```
curl http://<target>:8080/api/dibit_dump
```

returns the new `histogram.inner_pct` / `histogram.outer_pct` and
`raw_duid` blocks alongside the existing dibit hex + sync stats.

## Follow-ups

- **Maia init script has the same `start-stop-daemon -b` bug.** Same
  one-line wrapper fix would give Maia visible logs too. Out of scope
  for the P25 branch but worth doing in `fishball-dev`.
- **The DUID hardcode and threshold-10 are temporary.** Both should
  drop back to "no hack, threshold 4" once the LSM demod work in
  change 011 lands a proper Python -> Rust -> HDL solution.
- **The traffic_* dead-code warnings in `fpga.rs`** are not silenced
  in this change; they're interleaved with used methods so the cleanest
  fix is splitting them into a separate `impl` block. Deferred.
