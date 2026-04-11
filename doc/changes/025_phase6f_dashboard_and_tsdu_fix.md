# 025 -- Phase 6F: --lo-ppm CLI, dashboard migration to LSM, PS-vs-PL diagnostic dashboard, TSDU deinterleaver fix

**Date:** 2026-04-10 .. 2026-04-11
**Phase:** Phase 6F (PS dashboard + bring-up diagnostics)
**Branch:** fishball-p25
**Status:** Six commits landed; 6F.2c (TSDU deinterleaver fix) awaiting on-target verification.

---

## TL;DR

Six PS-only commits, no HDL or Vivado work. Three threads of work:

1. **Production calibration cleanup** -- replace the hardcoded
   `--control-freq 860962963` workaround from doc 024 with a clean
   `--lo-ppm` CLI argument that does the math internally. Tezuka init
   script now passes `--lo-ppm -0.54` (matching SDRTrunk's tuner panel
   value for the same Pluto). The calibration is finally a
   human-readable knob instead of a magic number.

2. **Dashboard migration to the LSM data sources (Phase 6F.1)** --
   the existing dashboard panels (System Identity, Decode Stats,
   Active Grants, Frequency Bands) were silently reading from the C4FM
   `ControlChannelDecoder`, which never validates anything on this
   site (Clay County NAC `0x8A1` is LSM, not C4FM). Switched the four
   route handlers to read from the LSM-side `ControlChannelDecoder`
   that's fed by `lsm_dibit_dma`. Added a parallel `expire_grants`
   tokio task for the LSM decoder so its grant table doesn't grow
   forever once decodes start landing.

3. **PS-vs-PL diagnostic dashboard (Phase 6F.2 / 6F.2b)** -- the
   single biggest blind spot in the bring-up was that the dashboard
   only ever showed ONE decoder's view at a time (originally C4FM,
   then LSM after 6F.1) with no way to compare. The four decoder
   paths in play -- PS C4FM, PS LSM, PS Phase 6D (raw IQ), and PL HDL
   LSM (FPGA gateware) -- are now all surfaced side-by-side in a
   single comparison matrix card, plus dedicated cards for the PL HDL
   chain detail and the IRQ source counters. New REST endpoints
   (`/api/hdl_lsm`, `/api/irq_stats`, `/api/decoder_compare`,
   `/api/lsm_dibit_dump`) feed the new cards. Added a `BUILD_TAG`
   constant logged at startup and exposed via `/api/system` so the
   "is the binary I just flashed actually the one I just built?"
   guessing game finally has a definitive answer.

4. **TSDU deinterleaver alignment bug fix (Phase 6F.2c)** -- the
   diagnostic dashboard from 6F.2b immediately exposed a clean
   100 % TSBK CRC failure rate on the PS LSM software decoder while
   trellis decode succeeded on every block. Root cause: the
   `TsduDeinterleaver` was using period 35 (should be 36) and offset
   34 (should be 14, derived from the existing NID-position-11 status
   skip). Fixed; bug fix awaiting on-target verification.

---

## Commits

| SHA | Subject | Files |
|---|---|---|
| `711163c` | p25-httpd: --lo-ppm CLI arg for Pluto crystal calibration | `main.rs` |
| `4cb172d` | p25-httpd: Phase 6F.1 dashboard migration to LSM ControlChannelDecoder | `httpd/mod.rs`, `main.rs` |
| `d4171fd` | p25-httpd: BUILD_TAG startup banner + /api/system build field | `main.rs`, `httpd/mod.rs`, `p25-json/lib.rs` |
| `70f1889` | p25-httpd: Phase 6F.2 PS-vs-PL diagnostic dashboard | `main.rs`, `httpd/mod.rs`, `fpga.rs` |
| `71fae9c` | p25-httpd: Phase 6F.2b PS C4FM vs PS LSM dibit-stream + pipeline failure dashboard | `httpd/mod.rs`, `main.rs`, `p25/control_channel.rs` |
| `4ef7b89` | p25-httpd: Phase 6F.2c TSDU deinterleaver alignment fix | `p25/fec.rs`, `main.rs` |

Companion tezuka_fw commits:

| SHA | Subject |
|---|---|
| `0a35631` | overlay_p25: pass --lo-ppm -0.54 to p25-httpd |
| `95d385f` | Bitstream: refresh P25 XSA to bake E (CORDIC + lerp pipeline) |

---

## 1. `--lo-ppm` CLI argument (commit `711163c`)

Replaces the hardcoded `--control-freq 860962963` workaround from doc
024 with a `--lo-ppm` argument. The argument accepts a signed float
(default 0.0) and shifts the DDC NCO by `-ppm * 1e-6 * rx_lo` Hz,
folded into the existing `nco_offset = control_freq - rx_lo`
computation.

```rust
let nco_lo_shift_hz = -args.lo_ppm * 1e-6 * args.rx_lo as f64;
// ...
let nco_offset =
    args.control_freq as f64 - args.rx_lo as f64 + nco_lo_shift_hz;
```

The shift is applied **only** to the NCO, not to the AD9361 LO
request. This is critical (doc 024) because the AD9361 LO synthesizer
step at our operating range is much coarser than a typical PPM-scale
shift, so asking for a small LO offset gets rounded back to the
nominal value while the NCO computation still moves, doubling the
post-DDC offset and breaking lock. The DDC NCO is generated in fabric
at 1 Hz precision and is the only place a sub-step shift can actually
be applied.

For the Clay County test Pluto, `--lo-ppm -0.54` puts the post-DDC
signal at DC and gives the Costas PLL full clamp headroom on both
sides. SDRTrunk's tuner panel exposes the same setting under "PPM"
and is the reference for the value.

The Tezuka init script (`overlay_p25/etc/init.d/S60p25-httpd`,
commit `0a35631`) now passes:

```bash
--rx-lo 858100000 \
--sample-rate 8000000 \
--control-freq 860962500 \
--lo-ppm -0.54 \
```

i.e. the nominal control frequency with the calibration as a separate
human-readable knob instead of a magic frequency offset.

## 2. Dashboard migration to LSM decoder (Phase 6F.1, commit `4cb172d`)

Background: there are TWO `ControlChannelDecoder` instances in
`p25-httpd/src/main.rs`:

1. **PS C4FM `decoder`** -- fed by the C4FM HDL chain via
   `dibit_dma`. The original Phase 2A decoder. Doesn't decode anything
   on the LSM signal we're testing against because the C4FM HDL chain
   produces dibits with the wrong symbol convention for LSM.
2. **PS LSM `lsm_decoder`** -- added in Phase 6E.10. Fed by the HDL
   LSM chain via `lsm_dibit_dma`. This is the one that should be
   producing valid TSBKs on the LSM site.

Until 6F.1 the dashboard's `/api/system`, `/api/grants`, `/api/bands`,
and `/api/stats` handlers all read from `state.decoder` (C4FM). The
LSM decoder existed but never fed the UI.

Changes:

- New `lsm_decoder` field added to `httpd::AppState`.
- The four handlers above switched to read `state.lsm_decoder`.
- `get_dibit_dump` left on `state.decoder` -- it's a C4FM-specific
  diagnostic endpoint (recent_dibits buffer, sync correlator, raw
  DUID histogram on the C4FM dibit stream).
- New `expire_grants` tokio task on the LSM decoder, mirroring the
  existing C4FM expiry task on the same 5 s tick / 30 s threshold.

After this commit the dashboard fields are sourced from the LSM
decoder. They will populate as soon as a TSDU TSBK validates -- which
turned out to never happen until the 6F.2c fix.

## 3. BUILD_TAG (commit `d4171fd`)

Buildroot zeros file mtimes to 1970, doc-comment strings don't
survive into the binary, and Rust identifiers are stripped from
release builds. Result: every "is the binary I just flashed actually
the one I just built?" check we tried during 6F.1 bring-up was a
dead end. Wasted real cycles.

The fix is a single bumpable `&'static str` constant in `main.rs`
that gets logged at startup:

```rust
pub const BUILD_TAG: &str = "...";
```

```text
INFO p25_httpd: p25-httpd build: 2026-04-11-phase6f.2c-tsdu-deinterleave-fix (dashboard_source=lsm_decoder, Phase 6F.1)
```

It's also returned in `/api/system` as a new `build` field, which the
dashboard displays in the header next to the status dot.
**Bump this string in every commit that affects feature behavior**
so on-target verification is a one-line `grep "p25-httpd build"
/var/log/p25-httpd.log` or browser glance.

## 4. PS-vs-PL diagnostic dashboard (Phase 6F.2, commit `70f1889`)

The single biggest investigation tool in this batch. Until 6F.2 the
dashboard could only show ONE decoder's data at a time, which meant
diagnosing the LSM decoder bring-up required cross-referencing
heartbeat task logs, IRQ task logs, the LSM Phase 6D pipeline log,
and the PL HDL `lsm_status` register reads -- all independently and
all from the on-target shell. With four parallel decoder paths
running this was unworkable.

This commit pulls everything into a single dashboard view. Concretely:

### New shared state structs (in `main.rs`)

- `HdlLsmRuntime` -- snapshot + cumulative state for the PL HDL chain.
  Live register values (`pll_dbg`, `sp_dbg`, `sync_distance`,
  `bch_busy`, `in_nid_window`, `dibit_overflow_latched`,
  `iq_overflow_latched`, last NAC/DUID/drop_count), cumulative
  counters (total NID events, valid NID events, dibit/iq overflow
  ticks), per-second window snapshot (pll/sp min-max, sync_dist best,
  iq KB/s + buffer rolls), NAC histogram, and a chronological
  32-deep ring buffer of the last NID events.

- `IrqStats` -- per-source IRQ counters (`total`, `dibit`, `traffic`,
  `iq`, `lsm_dibit`) plus `started_at` / `last_at` for rate
  computation.

Both are wrapped in `Arc<tokio::sync::Mutex<...>>` and threaded into
`AppState`.

### Refactored tasks

- The HDL LSM heartbeat task in `main.rs` now writes its task-local
  variables out to the shared `HdlLsmRuntime` on every 16 ms tick
  (live register snapshot), every NID event (cumulative counts +
  NAC histogram + ring buffer mirror), and at the end of every 1 s
  heartbeat window (window snapshot). The existing log-only
  behaviour is preserved.

- The `InterruptHandler::run` signature in `fpga.rs` was extended to
  accept the shared `IrqStats`. The handler updates it on every IRQ
  before falling through to its existing log throttling. No
  contention because nothing else writes the struct.

### New REST endpoints

- `GET /api/hdl_lsm` -- full PL HDL chain state in a single JSON
  blob: `live`, `cumulative`, `last_window`, `top_nacs`, `nid_ring`.
- `GET /api/irq_stats` -- IRQ source counters with rates per second.
- `GET /api/decoder_compare` -- side-by-side comparison: same set of
  metrics for each of `ps_c4fm`, `ps_lsm`, `ps_phase6d`, `pl_hdl`.

### New dashboard cards

- **Decoder Comparison Matrix** (top of page) -- single wide card
  with 4 columns (PS C4FM | PS LSM | PS Phase 6D | PL HDL) and one
  row per metric. `--` where a metric doesn't apply to a given
  decoder. This is the at-a-glance diagnostic view.
- **HDL LSM Chain (PL)** -- live FPGA register reads, cumulative
  NIDs, last NAC/DUID, drop_count, last 1 s window stats, ALIVE/STALLED
  status badge.
- **IRQ Source Counters** -- per-source totals + rates, last IRQ
  age, uptime.
- **HDL LSM NID Ring (last 32, PL)** -- chronological table of the
  last 32 NID events with seq, time, NAC, DUID, valid checkmark,
  n_errors, sync_distance, drop_count, pll, sp. Same data the
  heartbeat task dumps to the log on crash transition, but always
  visible.
- Existing **System Identity** and **Decode Stats** cards renamed
  with explicit `PS · LSM software decoder · HDL dibit-fed` labels
  + footer notes that explain that Dibit Count + Overflow remain
  C4FM-fed.

After this commit the entire bring-up state can be observed from a
single browser tab without ever ssh'ing to the target.

## 5. Pipeline failure counters + dibit-stream side-by-side (Phase 6F.2b, commit `71fae9c`)

The 6F.2 dashboard answered "what is each decoder seeing" but not
"WHERE in each decoder's pipeline is data being lost". On the first
on-target run of 6F.2 we saw PS LSM stuck at 0 messages decoded with
a randomly-floating false-positive NAC, no way to tell whether the
problem was at sync detection, NID FEC, trellis decode, or CRC.

This commit adds:

### Per-decoder pipeline failure counters (in `ControlChannelDecoder`)

11 new public counter fields, incremented in-place at the failure
sites in `process_dibit` and `process_tsdu`:

```rust
pub nid_attempts: u64,           // sync hit -> ReadingNid transition
pub nid_decode_failures: u64,    // GolayDecoder::decode_nid -> None
pub nid_invalid_duid: u64,       // BCH OK, DUID nibble unknown
pub nid_decoded_ok: u64,         // -> ReadingDataUnit
pub nid_decoded_tsdu: u64,       // -> ReadingDataUnit AND DUID==Tsdu

pub tsdu_attempts: u64,          // process_tsdu entered
pub tsbk_block_attempts: u64,    // TSBK blocks sent to trellis
pub tsbk_trellis_failures: u64,  // TrellisDecoder::decode -> None
pub tsbk_crc_failures: u64,      // trellis OK but CRC fails
pub tsbk_crc_ok: u64,            // -> handle_tsbk
pub tsbk_unknown_opcode: u64,    // CRC OK but block.decode() == None
```

These are maintained on **both** the C4FM and LSM software decoder
instances independently. They're surfaced via `/api/decoder_compare`
in the existing `ps_c4fm` and `ps_lsm` blocks, and rendered as 11
new rows under a `── pipeline ──` divider in the comparison matrix.

### LSM-side dibit dump (`/api/lsm_dibit_dump`)

The existing `/api/dibit_dump` reads from the C4FM `decoder`. This
adds a parallel endpoint reading from `lsm_decoder` with the same
JSON shape. The handler body is now factored into a private
`dibit_dump_json(decoder, source_label)` helper used by both
endpoints.

The endpoint reports dibit histogram (with inner/outer ratio), sync
correlator (hits, near misses, best Hamming distance), raw on-air
DUID histogram (TSDU/LDU1/LDU2/HDU bucket percentages), and now also
a `pipeline` block with all 11 failure counters above.

### Restructured Dibit Stream Diagnostics section

Replaces the old single-column "Dibit Histogram" + "Sync Correlator"
cards with a 2-column side-by-side layout:

- **PS C4FM Dibit Stream** (`c4fm_dibit_dma`)
- **PS LSM Dibit Stream** (`lsm_dibit_dma`)

Each card shows the histogram, sync correlator, AND raw on-air DUID
histogram for its respective decoder. If the histograms differ, the
two HDL slicers see different signal statistics. If the sync best
distances differ, frame alignment between the two streams is
diverging. If the TSDU bucket percentage is < 90% on either,
NID payload bits are being corrupted upstream.

This is the data that exposed the 6F.2c bug.

## 6. TSDU deinterleaver alignment fix (Phase 6F.2c, commit `4ef7b89`)

### What the diagnostic data showed

After flashing 6F.2b, the on-target dashboard immediately reported a
very specific failure pattern in the PS LSM column:

| Stage | Count |
|---|---|
| Sync hits / NID attempts | 941 |
| NID BCH decode failures | 161 (17 %) |
| **NID decoded OK** | **746 (79 %)** |
| NID decoded OK (TSDU only) | 446 (47 %) |
| TSDU attempts | 446 |
| TSBK block attempts | 446 |
| **TSBK trellis failures** | **0** |
| **TSBK CRC failures** | **446 (100 %)** |
| TSBK CRC OK | 0 |
| Messages decoded | 0 |

The signature: BCH validates 79 % of NIDs, **every** TSBK block
trellis-decodes successfully, and **every** TSBK block fails CRC.
Trellis succeeding while CRC fails is a very specific shape -- the
trellis output bytes are plausible (no uncorrectable errors) but the
CRC check disagrees on every single one. Random bit corruption would
break trellis sometimes too. This means: the bytes are systematically
shifted by a small consistent amount that exceeds CRC's tolerance but
not trellis's.

### Root cause

`TsduDeinterleaver::deinterleave` was using:

```rust
if (i + 1) % 35 != 0 {
    data.push(dibit);
}
```

That is **period 35, offset 34**, dropping dibits at
{34, 69, 104, 139, 174, 209, 244, 279, 314}. Two things wrong:

1. **Wrong period.** P25 TIA-102.BAAA-A inserts one status symbol
   every 70 information bits = 35 data dibits, so the on-air repeat
   is one status per 36 raw dibits, not 35. SDRTrunk's
   `P25P1MessageFramer` uses period 36 (`mStatusSymbolDibitCounter
   == 36`). With a 36-period spec and a 35-period skipper, the
   deinterleaver drifts by 1 dibit per status period.

2. **Wrong offset.** Even with the right period the offset is
   constrained by where the previous status symbol fell. The decoder
   above (`process_dibit`) explicitly skips a status dibit at NID
   position 11 (= post-sync position 11). The next status in the
   on-air stream is at post-sync position 11+36 = **47**, which is
   TSDU-body relative position 47-33 = **14** since the 33-dibit
   NID has already been consumed when `process_tsdu` is called.

### The fix

```rust
const STATUS_OFFSET: usize = 14;
const STATUS_PERIOD: usize = 36;

pub fn deinterleave(tsdu_dibits: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(327);
    for (i, &dibit) in tsdu_dibits.iter().enumerate() {
        let is_status =
            i >= Self::STATUS_OFFSET
            && (i - Self::STATUS_OFFSET) % Self::STATUS_PERIOD == 0;
        if !is_status {
            data.push(dibit);
        }
    }
    data
}
```

Status positions in the 336-dibit TSDU body are now
{14, 50, 86, 122, 158, 194, 230, 266, 302} -- 9 status dibits
removed, 327 data dibits returned. The corresponding test was
rewritten to assert exactly 9 status removals at the new positions.

### Why the C4FM decoder didn't expose this earlier

The C4FM software decoder uses the same `process_tsdu` /
`TsduDeinterleaver` code path. It doesn't reach the TSDU stage on the
LSM signal we're testing against because the C4FM HDL chain produces
dibits in a convention that BCH never validates -- so all 1652 sync
hits stop at "NID BCH decode failures: 1652". Until we got the LSM
HDL chain producing valid NIDs AND wired the LSM decoder to the
dashboard, nothing ever exercised the TSDU deinterleaver on a real
control channel signal end-to-end.

### Verification

```text
$ cargo test --bin p25-httpd p25::fec
running 8 tests
test p25::fec::tests::test_golay_decode_no_errors ... ok
test p25::fec::tests::test_golay_decode_single_error ... ok
test p25::fec::tests::test_golay_syndrome_zero ... ok
test p25::fec::tests::test_tsdu_deinterleave_removes_status ... ok
test p25::fec::tests::test_nid_decode_clean_clay_county ... ok
test p25::fec::tests::test_nid_decode_corrects_11_bit_errors ... ok
test p25::fec::tests::test_nid_decode_rejects_uncorrectable ... ok
test p25::fec::tests::test_nid_decode_records_raw_duid_under_corruption ... ok

test result: ok. 8 passed; 0 failed; 0 ignored
```

`cargo check` clean on host + armv7. Awaiting on-target flash.

### Expected on-target behaviour after the fix

In the dashboard's Decoder Comparison Matrix, scroll to the
`── pipeline ──` section and look at the PS LSM column:

- `TSBK CRC failures` should drop dramatically (from 100 % to maybe
  5-15 % depending on residual NID corruption).
- `TSBK CRC OK` should be > 0. Every CRC OK is a real decoded
  TSBK.
- `Messages decoded` (top of comparison matrix and System Identity
  card) should start counting up.
- Within 1-2 minutes the System Identity card should populate
  WACN, System ID, RFSS / Site from a Network Status Broadcast or
  RFSS Status TSBK.
- After a few minutes the Frequency Bands card should fill in from
  IDEN_UP messages.

If `TSBK CRC failures` stays at 100 %, the period or offset is still
wrong and we'll need an empirical sweep over `(period, offset)`
candidates.

---

## What this batch does NOT do

- Does NOT touch HDL or rebuild the bitstream. Bake E (`cfd691f`) is
  what's on the SD card and will continue to be.
- Does NOT investigate why the C4FM HDL chain occasionally produces
  clean LSM-looking NIDs. Curiosity, deferred.
- Does NOT add a heartbeat-observability fix that uses the LSM
  decoder's NID counter as the liveness signal (the existing one
  uses `lsm_status.nid_event` Rsticky which has a known CDC bug
  after long uptimes). Out of scope for this batch; the new
  `/api/hdl_lsm` and `/api/decoder_compare` endpoints provide
  better observability anyway.
- Does NOT add a configurable DDC sample-rate / decimation. The
  existing 8 MHz -> 62.5 kSPS chain is hardcoded; running at 4 MHz
  to share the AD9361 with SDRTrunk would require new FIR
  coefficient tables. Deferred.

## Files changed

```text
p25-httpd/src/main.rs                         (711163c, 4cb172d, d4171fd, 70f1889, 71fae9c, 4ef7b89)
p25-httpd/src/httpd/mod.rs                    (4cb172d, d4171fd, 70f1889, 71fae9c)
p25-httpd/src/fpga.rs                         (70f1889)
p25-httpd/src/p25/control_channel.rs          (71fae9c)
p25-httpd/src/p25/fec.rs                      (4ef7b89)
p25-httpd/p25-json/src/lib.rs                 (d4171fd)
```

Companion in tezuka_fw:

```text
board/tezuka/common/overlay_p25/etc/init.d/S60p25-httpd  (0a35631)
board/tezuka/fishball7020/bitstream/p25/system_top.xsa   (95d385f, refresh of bake E)
```
