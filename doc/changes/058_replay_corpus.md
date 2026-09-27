# 058 — P25 replay validation corpus (`rf.p25_corpus`, SD relay, modes A/B/C)

**Date:** 2026-09-27. **Branch:** fishball-p25. **Bake required:** NO. Host (fbench) and
on-board agent only; redeploy `fbench-agent` to unit B (`replay` subcommand). p25-httpd
unchanged.

## Why

Until now one 28 s clip (4 transmissions) was replayed, looped hundreds of times
(`rf.p25_replay`). A decoder that is right on those 297 IMBE frames can still be wrong
on the next call. This change replays *many different* recordings once each and scores
every transmission against SDRTrunk's own decode of the same air, including the call
Andy pointed at: the 2026-05-03 09:11 TG 300 / unit 1014 two-tone alert
(`20260503_091114_858437500_…_T-LCN-11_79_baseband.wav`, `.mbe`
`20260503_091125_858437500_1_300_1014`, MP3s `…091116…` and `…091124…`).

## Inventory (`tools/p25_corpus_index.py`, 2026-09-27)

| Item | Count |
|---|---|
| `.mbe` calls / transmissions (frame runs split at > 0.5 s) | 320 / 332 (241 clear, 91 encrypted), 51 129 IMBE frames |
| Wideband captures (`my_captures`, 4 MSPS, recorded by unit A without correction) | 18, 36.8 min, 35.3 GB; 14 of useful length, 9 with `.mbe` truth inside |
| Channel recordings (50 kSPS) | 313: 26 CC (42 s to 36 min), 287 traffic (213 with `.mbe` truth) |
| Recordings aligned to a `decoded_messages` log clock | 313 of 313 |
| `.bits` / MP3 / `decoded_messages` logs | 310 / 2377 / 737 |

Coverage per mode (distinct transmissions and calls; calls = `.mbe` files):

| Mode | Transmissions | Clear | Encrypted | Calls (clear) | Clear IMBE frames | Stream | Staged data | Upload at 9 MB/s |
|---|---|---|---|---|---|---|---|---|
| A whole captures (SD relay) | 47 | 34 | 13 | 45 (32) | 3 420 | 28.2 min, 9 captures | 20.3 GB `cs12` | 38 min |
| A windows (RAM) | 34 | 34 | 0 | 32 (32) | 3 420 | 6.3 min, 11 windows | 4.5 GB `cs12` | 8 min |
| B synthetic full system | 320 | 231 | 89 | 309 (222) | 35 514 | 49.7 min, 42 scenes | 24.6 GB `cs8` | 46 min |
| C traffic only | 332 | 241 | 91 | 320 (231) | 36 855 | 36.3 min, 213 items in 8 batches | 15.2 GB `cs8` | 28 min |

Mode A is limited by the site, not the tooling: the captures hold few voice calls with
`.mbe` truth (TG 300 mostly; 402/414/417/700 are encrypted and many of their grants
were not recorded). The CC logs show grants to 857.2125 MHz, outside the captures'
±1.84 MHz band; those calls cannot be replayed from them (`out_of_band` per capture).
The focus call is in no wideband capture (none covers 09:11); it is the first scene of
mode B (`B_20260503_084247_1695`, 37 s, CC + recordings `_79` and `_80`) and the first
item of mode C (`C_000`).

## Alignment of channel recordings (mode B)

SDRTrunk's `decoded_messages` lines carry 1 s stamps, but the framer accounts for every
bit, so each log gets a 9600 bit/s clock fitted to the stamps (the method of
`tools/sdrtrunk_teardown_stats.py`, ported to `fbench/analysis/sdrtrunk.py`). Checked
against the recordings themselves with a frame-sync detector (`p25_dsp.frame_syncs`,
phase advance over one symbol, correct for C4FM and LSM):

- every traffic recording in a 25-file sample and all 26 CC recordings start at their
  log's bit 0 + 10..13 ms (CC: matched by the TSDU length sequence, which removes the
  periodic ambiguity of continuous TSDUs);
- `.mbe` first-frame time minus the log's first LDU start: median 0.186 s, p10..p90
  0.158..0.213 s over 332 transmissions, i.e. the log clocks agree with SDRTrunk's
  frame clock to about ±27 ms.

So mode B places every source at `bit0 + 12 ms` and the CC-to-traffic residual is about
±30 ms, far below the grant-to-voice latency; mode B is not experimental. A recording
without a log would fall back to its 1 s file-name stamp (`align: name`, none today).

## Frequency offsets of the channel recordings (modes B and C)

The brief assumed the channel recordings carry unit A's uncorrected reference like the
wideband captures (+472 Hz at the CC). Measured (power-weighted mean phase advance,
`p25_dsp.carrier_offset`, first 10 s of each of the 313 recordings), SDRTrunk's
correction differed by session instead: CC recordings at +110..+140 Hz (2026-04-18 ..
05-02 09:47), +446..+469 Hz (05-02 10:01 and 10:40: A uncorrected) and −6..−27 Hz (from
05-02 10:50); traffic recordings run +67 Hz (median; p10..p90 +34..+100 Hz) above the CC
session that covers them. So the mixer removes each recording's offset before placing it
(`correct_hz` in the manifest: the covering CC session's offset scaled to the channel
frequency, else the recording's own estimate; in mode C scaled to the batch channel),
and the B/C TX LO is trimmed by B's own reference only (`−units.B.ref_ppm × f`,
"reference-true"). Measured on a rendered mode C batch: CC placed at −0.4 Hz, items at
+0.7 / +2.4 Hz (own estimate) and the focus recording at +55 Hz (its session's CC
correction plus the traffic-over-CC residual). Mode A keeps the capture model (A's
uncorrected reference, TX LO trimmed by `ref_ppm A − ref_ppm B`).

Rendering check on the real recordings (`fbench/corpus.py` `Mixer`): the focus scene
rendered at 3.5 MSPS, each channel down-converted and decimated back to 50 kSPS: 486 of
486 CC syncs and 110 + 87 traffic syncs at their expected stream positions (0 ms median
error); a mode C batch likewise (327 CC, 110 traffic in its first 25 s). Rendering runs at
1.45–2× real time, faster than the upload.

## Design

### Items and staging

An item is one continuous stream the TX board plays once: a capture (A, its 1 GiB
`cs12` segments back to back, gain 5.0 so the 12-bit peak maps under
`P25_TX_FULL_SCALE`), a window of a capture (A, `a_unit=window`), a scene (B) or a batch
(C). Files are rendered while they upload (`Ssh.put_stream`, `cat > X.part`, then `mv`),
named `<mode>_<id>_<recipe key>.<k>.<fmt>`, stored in `/mnt/sd/bench/corpus/` (or
`/root/fbench_corpus/` for `source=ram`), found again by name and size (`ls -ln`), with
the sha256 in `bench/.state/corpus/staged_B.json` and a sidecar `.json` on the card. The
free space is checked first (`df -k`, or MemAvailable for RAM). `cs12` is lossless for the
AD9361's 12-bit samples; the synthetic modes use `cs8` (with the channel at 12 % of full
scale the in-channel quantization SNR is above 50 dB).

Synthetic band plan (`p25_corpus.band_rate`): the lowest rate (3, 3.5, 4, 5, 6 MSPS, all
multiples of 50 kSPS) that keeps every channel within ±0.40 fs (inside the AD9361 TX
interpolator passband), with the TX centre chosen so that every channel's TX IQ image
(mirrored around the LO, `2c − f`) and the LO leakage stay ≥ 100 kHz from every channel.
The obvious centre, midway between the CC and the traffic channel, would put each one's
image (typically only 40–50 dB down) exactly on the other. Mode B uses 3.5 / 4 / 5 MSPS
(14 / 23 / 5 scenes); mode C 3.5 MSPS with the TX centre at 859.660 MHz (105 kHz image
clearance). Mode A cannot choose: the captures' own centres put the CC image 26 kHz from
857.4375 MHz, as on the air they were recorded from.

### The SD relay (`fbench-agent replay stream`)

```text
B: setsid sh -c 'fbench-agent replay stream --playlist P --status S --report R \
                   | iio_writedev -u local: -b 262144 cf-ad9361-dds-core-lpc voltage0 voltage1'
```

- Reader thread: sequential reads (1 MiB, `POSIX_FADV_SEQUENTIAL`, consumed pages dropped
  every 32 MiB) of the playlist's ranges into an SPSC ring in RAM, in storage format.
- Buffer sizing: 4 MSPS needs 16 MB/s of int16, but the ring holds the storage format,
  so `cs12` needs 12 MB/s from the card and `cs8` 6–10 MB/s. With the measured 23.6 MB/s
  sequential read, a 192 MiB ring (default `ring_mb`; 128–256 fits B's ~880 MB with
  p25-httpd stopped) is 16.8 s of `cs12` at 4 MSPS, refilled at 11.6 MB/s net; mode C's
  3.5 MSPS `cs8` needs 7 MB/s, so the same ring is 29 s. A stall shorter than the ring's
  lead costs nothing, and an empty 16.8 s ring refills in ~17 s. The prefill (default: the
  whole ring, ~8.5 s) happens before the first sample airs.
- Main thread: converts ring bytes to int16 with the item's gain (per-item segment table,
  integer Q12) and writes stdout through fd 1 unbuffered (Rust's stdout is line buffered;
  the pipe is raised to 1 MiB). `iio_writedev` drains at the DAC rate, which paces the
  relay.
- Stall handling: every read call slower than `--stall-ms` (500) is listed with its input
  position and the ring fill at the time; the read-latency histogram is in the status. If
  the ring does run dry after the first sample (an underrun), the event is counted with
  its stream position and length. `--on-underrun wait` (default) keeps every sample: the
  downstream buffers (pipe + iio blocks of 262144 samples, a few hundred ms) cover short
  gaps and the timeline slips by the rest; `zero` writes zeros after `--zero-after-ms`
  and then skips as many source samples, so every sample airs at its nominal time. The
  scorer re-votes the time offset per transmission, so a slip does not misplace the
  windows of later transmissions.
- stdout carries samples, so the reply JSON goes to stderr and `--report`; `--status` is
  rewritten every second for the host (`cat`).
- Verified: 3 unit and 3 end-to-end agent tests (conversion of all formats, gains,
  saturation, ring wrap, zeros items, item order, truncated ranges, injected 300 ms stalls
  in both underrun modes with a 1 MiB ring, stdout carrying exactly the samples); the conversion is integer (gain in Q12, output
  written in place): ~285 MSPS on the host, and the ARM build under qemu streams 3 s of
  4 MSPS `cs12` in 0.75 s (emulated, so the Cortex-A9 margin is an estimate).

### Mode C and the p25-httpd gates

p25-httpd's air-time delivery (054) feeds traffic dibits to the framer only under a
talkgroup context; a manual `/api/traffic?follower=off&retune_hz=…` leaves the context at
TG 0, so nothing would decode. Each batch therefore starts with a CC primer: a real CC
recording positioned 4 s before a clear TG 300 grant to 858.4625 MHz (the traffic channel
nearest the CC, so the band fits 3.5 MSPS), CC continuing for the whole batch. The test sets
`follower=on&lock=off`, waits until `/api/traffic` shows the chain parked on 858.4625 MHz
with a TG, then sets `lock=on&follower=off` (lock also keeps the chain through call
closes) and restores the original follower/lock at the end. The traffic items are placed
on that channel whatever their original frequency.

`/api/traffic` and `/api/pipeline` read the traffic LSM status word, whose `nid_event` bit
is read-to-clear and is what p25-httpd's heartbeat uses (loss-of-sync detection, end-close
cancellation). The test reads `/api/traffic` only at item boundaries and during the mode C
primer; the per-frame tap is `/api/imbe_dump` (last 128 raw frames, a mutex read) polled at
1 Hz and stitched by overlap (`p25_score.merge_dumps`; frames arrive in LDU batches of 9;
repetitive content such as tones is resolved by the expected frame rate; polls without
overlap are counted as `tap_gaps`).

### Scoring

Revised after the first bench runs (see "Bench feedback" below).

- **Recovery, the verdict:** p25-httpd's own per-call counts. Each `.mbe` transmission is
  matched to the `/api/ui/calls` call with the same TG and source whose open interval
  overlaps it (DUT clock offset voted from TG/source-matched call starts), and the call's
  `imbe` (exact per call_id since 057) is credited to its transmissions in time order, each
  up to its truth frame count; the remainder is the call's `excess`.
- **Bit accuracy, report only:** the `/api/imbe_dump` tap (0.5 s polls of the 128-frame
  ring, a baseline dump before the stream so older ring content is excluded) aligned in
  order with the `.mbe` frames, earliest match within 24 of 144 bits (silence and tone
  codewords repeat, so "best within a window" would skip ahead); `tap_coverage_pct`,
  `hex_aligned_*`, `hex_exact_pct_of_aligned`, `hex_mean_bit_diff`. Never changes the
  recovery.
- Followable = clear and not the loser of two overlapping calls on different channels (one
  traffic chain follows one call). Pass: clear recovery ≥ 90 % over followable
  transmissions, 0 relay underruns, 0 focus-tone dropouts.
- Focus tone: SDRTrunk's MP3 of the first transmission decodes (ffmpeg) to an alternating
  806.7 / 1506.2 Hz two-tone alert (44 tonal 20 ms frames, 0.97 s). `/ws/audio` is
  recorded during every item; for focus items the tone span is located, and per-tone mean,
  standard deviation and maximum deviation, dropouts (frames off-tone or below 10 % of the
  tone RMS inside the span), arrival gaps and lag events are reported.

## Runbook

From the repo root with `PY=.venv-hdl/Scripts/python.exe`; the DUT is A, so always
`--tx B --rx A` (interlock: 55 dB attenuation, −55 dBm at A through the 20 dB pad).

| Step | Command | Upload | Run |
|---|---|---|---|
| Manifest | `$PY tools/p25_corpus_index.py` | — | ~15 s |
| Agent on B | `bash bench/scripts/build_agent.sh` then `$PY bench/fbench.py setup agent --unit B --json` | 1.7 MB | < 1 min |
| Focus, mode B | `$PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=B -p items=focus --json` | 0.26 GB | ~1.5 min |
| Focus, mode C | `$PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=C -p items=focus --json` | 2.1 GB | ~4 + 6 min |
| Stage mode C | `$PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=C -p stage_only=true --json` | 15.2 GB | ~28 min |
| Mode C | `$PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=C --json` | — | ~42 min |
| Stage mode A | `$PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=A -p stage_only=true --json` | 20.3 GB | ~38 min |
| Mode A | `$PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=A --json` | — | ~32 min |
| Mode A windows (RAM) | `$PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=A -p a_unit=window -p source=ram --json` | 4.5 GB each run | ~8 + 9 min |
| Stage mode B | `$PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=B -p stage_only=true -p purge=true --json` | 24.6 GB | ~46 min |
| Mode B | `$PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=B --json` | — | ~62 min |

Run times are stream time plus ~20 s per item (prefill, tail, relay start/stop). A+C+B do
not fit B's 57 GB at once, hence `purge=true` before B. Stop any run with
`touch bench/.state/corpus/STOP`; continue with `-p resume=auto`. If a mode C run dies
without its cleanup, restore the DUT with
`curl 'http://192.168.2.1:8080/api/traffic?follower=on&lock=off'`.

## Files

| File | Change |
|---|---|
| `tools/p25_corpus_index.py` | new: inventory, manifest, markdown report, MP3 tone references |
| `bench/fbench/analysis/sdrtrunk.py` | new: SDRTrunk names, WAV headers (> 4 GiB safe), `.mbe` truth, log bit clocks, CC grants |
| `bench/fbench/analysis/p25_corpus.py` | new: alignment, mode A/B/C plans, manifest (`fbench.p25corpus/1`) |
| `bench/fbench/analysis/p25_dsp.py` | new: frame-sync detector, `cs16`/`cs12`/`cs8` pack/unpack, tone frames |
| `bench/fbench/analysis/p25_score.py` | new: tap stitching, offset vote, frame matching, call matching, tone continuity |
| `bench/fbench/corpus.py` | new: items, mixer, staging, relay control, DUT taps |
| `bench/fbench/tests/corpus_tests.py` | new: `rf.p25_corpus` acquisition and analysis |
| `bench/fbench/wsaudio.py` | new: stdlib `/ws/audio` recorder |
| `bench/fbench/transport.py`, `services.py`, `agent.py`, `tests/__init__.py` | `Ssh.put_stream`, `Services.ws_audio`, contract for `replay`, registration |
| `bench/agent/src/cmd/replay.rs`, `cmd/mod.rs`, `main.rs`, `util.rs` | new `replay stream/check/verify`; reply to stderr when stdout is data |
| `bench/agent/tests/cli.rs` | 3 end-to-end relay tests |
| `bench/tests_host/conftest.py`, `test_corpus.py`, `test_catalog.py`, `test_cli.py` | relay/DUT fakes, synthetic SDRTrunk corpus, 25 corpus tests, catalog scenario, catalog count |
| `bench/README.md`, `bench/agent/README.md`, `doc/HW_VALIDATION_SUITE.md` | docs, catalog row, agent subcommand |

## Not verified on hardware

- The relay is now verified: two full Mode B passes (42 scenes, 24.6 GB from B's SD card)
  had 0 underruns. The ring never fell below 19.9 s, and the longest SD read was 77 ms.
- The `/api/traffic` lock sequence (read from the p25-httpd source: `lock=on` skips grants
  to other frequencies and keeps the chain through call closes; `follower=off` ignores
  grant events).
- (Verified on the 05:44 bench run: all 297 tapped frames align in order with SDRTrunk's
  `.mbe` frames, 19 exact, mean 2.2 bits apart, so the tap and the truth carry the same
  raw codewords.)

## Bench feedback (2026-09-27, 057b on unit A)

### Scoring: 155/297 on the 05:44 scene was the scorer, not the DUT

p25-httpd's own counts showed every call fully decoded (`imbe` 81/72/72/72 = 297). The
tap had captured exactly the 297 new frames (the `imbe_frames_extracted` delta, no gaps;
the ring is 128 frames, `get_imbe_dump` in `p25-httpd/src/httpd/api/traffic.rs`, so 1 Hz
polling was enough). Three scorer defects:

1. The per-transmission windows hung on a host-to-stream offset voted from *exact* hex
   matches, and two receivers' raw codewords are rarely bit-identical (19 of 297), so the
   vote was thin and windows slipped.
2. The first dump's 128 frames were older ring content and were counted.
3. `load_mbe` sorted frames by timestamp. SDRTrunk's stamps step back up to ~150 ms at LDU
   boundaries in 220 of 367 files, so sorting interleaved two LDUs.

Fixes: the recovery now comes from the per-call `imbe` (see Scoring); the tap is a
bit-accuracy check with a baseline dump, earliest-match alignment within 24 bits, and
`.mbe` frames kept in file order. Re-scored offline (copies of the run dirs): the 05:44
scene reads 297/297 (100 %) on the calls, tap coverage 100 %; the 09:11 scene 0/639 (0 %).

### The 09:11 scene and the HDL traffic chain

Tool: `tools/p25_lsm_hdl_replay.py`. It models the linear front end bit-true in numpy (the
`PRESET_8M` DDC taps parsed from `ddc_presets.rs`, `LsmDecimator2`, `LsmFir` LPF and RRC
with its Q1.17 quantisation, `>> 17` and saturation) and runs the Amaranth `LsmDemod`
itself (DC blockers, AGC with the 256 gate, Gardner timing, CORDIC PLL, NID) in the
simulator, about 2 minutes per second of signal. Dibits are searched for the frame sync
under all four quadrant rotations. The HDL source has not changed since the 2026-05-03
bake (`38442a7`), so this is the board's gateware.

| Input (2 s, 25 kSPS into `LsmDemod`) | Start | Level (Q1.15 RMS) | Syncs upright / rotated | Valid NIDs | PLL |
|---|---|---|---|---|---|
| A's own capture of the 09:11 scene (DDC model) | cold | 1230 (board level: 154 counts at the ADC × 8 DDC gain) | 34 / 0 | 10 | −200, settled |
| 09:11 recording `_79` | cold | 550, 1230, 2500 | 28–50 / 0 | 10–16 | −80, settled |
| 05:44 recording `_70` | cold | 550 | 20 / 0 | 11 | +110, settled |
| 09:11 recording `_79` | PLL at +8579 | 1230 | 0 / 28 | 0 | pinned 96 % |
| 05:44 recording `_70` | PLL at +8579 | 1230 | 0 / 18 | 0 | pinned 87 % |
| A's capture | cold | 120, 200 (under the AGC gate) | 0 / 34 | 0 | pinned; AGC gain stays 1.0, sample point stays at warmup |

The gateware decodes the 09:11 traffic from a cold start as well as the 05:44 traffic; the
signal does not make it diverge. Ruled out: the 25 kHz fold-back (the DDC rejects it by
101 dB; the scene carries no 858.4625 signal, only TG 600's grants, and A's capture shows
noise there), the carrier (+21 Hz on the board, tens of Hz in the recordings), DC and
spurs (none in either recording), and the grant-to-voice timing (both scenes: signal
within 50 ms of the grant).

What the board shows, PLL at +8579 from the first sample with 0 IMBE for every call, is
the PLL's absorbing state. The decision-directed loop's stable points are the bias plus
multiples of π/2. Once |pll| exceeds π/4, the decisions are a quadrant off and push toward
±π/2, which lies outside the ±π/3 clamp (`MAX_PLL_ABS_Q13 = 8580`): the loop pins at the
clamp for good, with every dibit rotated by 90°, so no sync, NID or IMBE. Both
recordings behave identically from that state. So on the board the 09:11 calls start
with the chain already outside π/4, or with the input under the AGC gate, and nothing
resets it: all seven calls sit on 858.4375 MHz, where every earlier item also ended, and
they hand over Active to Active (`close_reason` `tg_change`).

Candidate entries into that state, to be told apart from `/api/log?category=traffic` at the
first grant and the `/api/traffic` `traffic_lsm_chain` registers during the call:

- A cross-frequency retune after a clean call **coasts** (`freq_changed = !prev_clean`),
  inheriting a PLL that walked to the clamp while the chain was parked on noise (057b's own
  `resume_needs_reset` comment documents that walk). The sample point is converged (about
  11 100 in Q5.12, about 2 770 in the register's `[2:]` field) and the AGC gain is normal.
- Input under the AGC idle gate: `agc_gain_dbg` stays 128 (`GAIN_INIT`) and the sample
  point stays at its warmup (19 278 in Q5.12, about 4 820 in the register). This would
  need the traffic 7 dB+ weaker at `LsmDemod` than A's capture indicates.

### Proposed fixes (for review; nothing changed in p25-httpd or the HDL)

1. **HDL (bake), removes the absorbing state:** `MAX_PLL_ABS` from π/3 to 0.65 rad
   (`MAX_PLL_ABS_Q13` 5325, inside the π/4 basin boundary with margin). Simulated by
   patching the constant in-process: started at the new clamp, both the 09:11 and the
   05:44 traffic come back to about 0 within the first 0.25 s and decode (10 and 11 valid
   NIDs); a cold start is unchanged. The cost is a tracking range of ±500 Hz instead of
   ±800 Hz, while the traffic residual after the NCO correction is tens of Hz.
2. **PS (no bake), now:** a pinned-PLL watchdog in the traffic heartbeat. While a call is
   open and the chain enabled, if |`pll_dbg`| ≥ 8000 for 150 ms, pulse `traffic_lsm_reset`
   (cold start). Simulated: pinned for 0.5 s, then one cold reset, and the 09:11 traffic
   decodes about 80 ms later.
3. **PS:** apply the same `resume_needs_reset(pll_pre, ms_since_voice)` test on the
   cross-frequency path instead of coasting on `prev_clean` alone.

Follow-up: the board confirmed the entry path (PLL walking on carrier-gap noise). The
gateware fix is in [059](059_lsm_pll_timing_hold.md). It holds the PLL and timing while
the AGC gate reports no signal, and it adopts fix 1 (0.65 rad clamp) as the second guard.

Side note, separate issue: lowering the gate is not a cure for input that is too weak. At
a magnitude of about 80 with the gate at 64, the simulated AGC gain register reads 0 and
the PLL still pins, which is worth a look in `lsm_agc.py`.

### Full Mode B pass and scorer fixes (2026-09-27)

This pass ran 42 scenes and 320 transmissions on unit A with p25-httpd 057c, which has the
PS PLL watchdog, and core 0.1.0. The first analysis crashed with a 26.7 GiB numpy
allocation. A had rebooted without a clock, so calls were stamped 1970 + uptime until the
clock jumped to real time partway through, and `/api/ui/calls` still listed the previous
day's calls. The offset vote's histogram then spanned 56 years.

A second, offline pass over the saved run found more scorer errors, 24 of the 42
missed or partial rows:

- 18 were DUT-call matching errors:
  - p25-httpd stamps the grant's source, SDRTrunk the talker, and the two differ on
    console grants and talker changes;
  - ±3 s ties went to the neighbouring call;
  - one transmission can span two DUT calls.
- 5 were clock errors: 4 from the mid-scene clock step and 1 wrong offset vote.
- 1 came from the old conflict rule.

Fixes in `p25_score.py` and `corpus_tests.py`:

- **Offset vote:** a densest-window vote around the DUT-clock prior. Calls from a clock step
  mid-item are moved back onto the item's clock (`dut_clock_end`).
- **Call matching:** a transmission takes frames from every same-TG call on its frequency
  that covers it in time. The source is only a tie-break.
- **Followability:** a transmission is not followable only when the followed call was
  granted first and was still in voice, or SDRTrunk still held its channel (`blockers`).
  A lock held longer than that counts as a DUT miss.
- The runner now records the control-chain PLL/AGC, `grants_seen_new` and the watchdog
  counters at each item's start and end.
- `-p items=a,b` works: `runner.coerce` hands a str parameter a JSON list.

Result for that pass: 219 followable clear transmissions, 31455/34029 IMBE (92.4 %), 21
missed. Of the misses:

- 17 were scenes whose control channel decoded no grant for 4 s to over 30 s after the
  inter-scene silence;
- 3 were sticky-lock rejects during the 2 s end grace;
- 1 was a grant to 856.4375 MHz, outside the replay band.

The gateware in [059](059_lsm_pll_timing_hold.md) removes the first group. On core 0.2.0
the same corpus scores 33291/34029 (97.8 %) with 4 missed.
