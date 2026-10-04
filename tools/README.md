# tools/

Host scripts. Unit A is at `192.168.120.50`, unit B at `192.168.12.1`. The tools that talk to a
unit use the scanner's `/api/v1`. Scripts retired in the 2026-10 cleanup are in
`MAIA_SDR/_archive/cleanup_2026-10-04/maia-sdr/tools/`.

## References: SDRTrunk as ground truth

| Script | What it does |
|--------|--------------|
| [`sdrtrunk_dmr_reference.py`](sdrtrunk_dmr_reference.py) | Decodes 50 kSPS stereo IQ WAVs (`/api/v1/iq/control.wav`) with SDRTrunk's own `DMRDecoder` through a small Java harness (`sdrtrunk_dmr_harness/`). It prints `file\|timestamp\|timeslot\|valid\|class\|text` per message, so the scanner's DMR output can be diffed against it. The `DMR_CAPTURE_DIR` tests read its output. Uses Gradle `--offline`; nothing in the SDRTrunk repo changes |
| [`sdrtrunk_lsm_reference.py`](sdrtrunk_lsm_reference.py) | SDRTrunk's `P25P1DecoderLSM`, headless: `taps` prints its filters; `decode` writes `.bits` and messages for `_baseband.wav` or `/api/v1/iq/control.wav` captures (harness in `sdrtrunk_lsm_harness/`). It imports `classpath()` from the DMR reference |
| [`p25_lsm_compare.py`](p25_lsm_compare.py) | Aligns three dibit streams: the scanner's (`lsm_wavs`, an ignored test in `scanner/src/protocol/p25/lsm_tests.rs`), SDRTrunk's offline decode, and SDRTrunk's live `.bits` |
| [`sdrtrunk_teardown_stats.py`](sdrtrunk_teardown_stats.py) | Teardown and call-close timing distributions from SDRTrunk's `event_logs`. Its `--p25-calls` and `--p25-log` options read p25-httpd's old dumps |
| [`p25_corpus_index.py`](p25_corpus_index.py) | Indexes the SDRTrunk captures, recordings and `.mbe` truth into the replay manifest for `fbench run rf.p25_corpus` |

## Design

| Script | What it does |
|--------|--------------|
| [`p25_ddc_filter_design.py`](p25_ddc_filter_design.py) | Designs the lanes' three-stage DDC coefficients for each preset (unit DC gain, 50 kSPS out) and writes `scanner/src/hardware/presets/table.rs` (`--emit-rs`) |
| [`polyphase_proto_design.py`](polyphase_proto_design.py) | The prototype filter for the polyphase channelizer (`maia-hdl/p25_hdl/polyphase_proto_coeffs.py`), kept for 079 step 3b |

## The unit and its API

| Script | What it does |
|--------|--------------|
| [`api_fields.py`](api_fields.py) | Writes `scanner/doc/API_FIELDS.md`: every GET route's fields with type, example and meaning, sampled from a unit. Meanings are in [`api_fields_meanings.py`](api_fields_meanings.py); exit 3 names a field without one |
| [`scanner_live_check.py`](scanner_live_check.py) | Samples a running scanner every minute (status, recordings, activity; with `--ssh` its process and log) for long live runs |
| [`atsc_check.py`](atsc_check.py) | Runs a TV scan in ATSC mode and compares it with the HDHomeRun's lineup (`10.0.0.117`), channel by channel; writes `runs/atsc/<time>/` |

## Build

| Script | What it does |
|--------|--------------|
| [`build_progress.py`](build_progress.py) | Follows a Vivado or Tezuka build log and prints a line per phase (the `*_pretty.sh` wrappers) |
| [`check_verilog_stale.ps1`](check_verilog_stale.ps1) | Tells `build_fpga.bat` whether the generated Verilog is older than its Amaranth sources |
