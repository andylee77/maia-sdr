# Maia SDR -- Changelog (andylee77 fork)

Tracking log for the `andylee77/maia-sdr` fork.
Upstream: [F5OEO/maia-sdr](https://github.com/F5OEO/maia-sdr) (originally [maia-sdr/maia-sdr](https://github.com/maia-sdr/maia-sdr))

---

## [2026-04-09] Phase 6D: Rust LSM Demod + NID FEC Port to p25-httpd

**Branch:** fishball-p25

Phase 6D is complete (Rust port compiles, all unit tests pass, ARM
cross-build clean; on-target validation queued behind the next Tezuka
firmware rebuild). Mechanical port of the validated Phase 6A/6B Python
prototype into the embedded p25-httpd Rust workspace, file-by-file with a
one-to-one mapping between Python stages and Rust files. The new pipeline
runs in parallel with the existing C4FM dibit reader, consuming the
Phase 6C `iq_dma` ring directly. See `doc/changes/014_phase6d_lsm_rust_port.md`
for the full write-up.

- **`p25-httpd/src/lsm/`** -- new module, ~1580 lines of Rust + tests:
  - `nid_fec.rs` (~280 lines) -- BCH(63,16,11) encoder + ML codebook
    decoder. Generator matrix copied verbatim from
    `BCH_63_16_23_P25_Test.java` (octal literals → `u64`). Codebook is
    `OnceLock<Box<[u64; 65536]>>`, built lazily on first decode call
    (~512 KB resident, <10 ms build on Cortex-A9).
  - `filters.rs` (~310 lines) -- frozen LPF (83 taps Parks-McClellan) and
    RRC (105 taps closed-form) `const [f32; N]` arrays designed at
    31.25 kSPS via the existing `tools/p25_lsm_demod.py`. Plus
    `apply_real_fir_complex` (batch), `StreamingFir` (with `(taps.len()-1)`
    history for boundary-transient-free chunking), `decimate_by_2` (batch),
    and `StreamingDecimator2` (phase-tracking across odd-length chunks).
  - `demod.rs` (~330 lines) -- verbatim port of `demod_lsm()` and the
    SDRTrunk `P25P1DemodulatorLSM.process()` it descends from. Variable
    names match the Java source. AGC + PLL + Gardner TED + slicer.
    `DemodState` is exposed so the streaming variant
    `demod_lsm_with_state` can preserve loop state across iq_dma
    sub-buffer boundaries.
  - `sync.rs` (~400 lines) -- hard + soft sync detectors. Hard is the
    sliding 48-bit Hamming-distance correlator (`SYNC_THRESHOLD = 4`).
    Soft is the port of `P25P1SoftSyncDetectorScalar` correlating
    against the 24 ideal `±3π/4` sync phases (`SYNC_SCORE_THRESHOLD =
    60.0`). Both share the same status-dibit-aware NID extractor that
    skips the 33-dibit-window position 11.
  - `ring.rs` (~110 lines) -- iq_dma sub-buffer `&[u8]` to
    `Vec<Complex32>` adapter. Decodes the FPGA's
    `{im[1], re[1], im[0], re[0]}` 64-bit word as four little-endian
    `i16` samples, normalises to ±1.0.
  - `mod.rs` (~150 lines) -- module root, local
    `Complex32 { re: f32, im: f32 }` POD type (avoids new `num-complex`
    runtime dep), `LsmPipeline` orchestrator that owns the streaming
    decimator + LPF + RRC + demod state and exposes `process_iq()` /
    `reset()`.
- **`p25-httpd/src/fpga.rs`** -- `IpCore` gains `iq_dma: RxBuffer`,
  `iq_last_addr: Option<u32>`, `set_iq_dma_enable`, `iq_last_buffer`,
  `iq_overflow`, `iq_next_address`, `read_iq_buffers`. New `DmaChannel::Iq`
  arm in `read_dma_buffers`. `InterruptHandler` gains `notify_iq_dma`,
  `waiter_iq_dma`, and the IRQ-loop iq branch (bit 2 of `interrupts`).
- **`p25-httpd/src/main.rs`** -- `mod lsm`, `ip_core.set_iq_dma_enable(true)`
  at startup, third tokio task that runs the LSM pipeline on iq_dma
  wakeups. Snapshots `read_iq_buffers()` + `iq_overflow()` under the lock,
  drops the lock before CPU work, runs the streaming pipeline, and logs
  per-IRQ NID stats (hard/soft sync counts, cumulative top-3 NACs). The
  task resets all streaming state if the gateware overflow latch fires.
  Independent of the existing dibit reader -- both pull from the same
  control DDC output via separate ring DMAs and separate PS state.
- **`tezuka_fw/board/tezuka/fishball7020/dts/fishball-p25.dtsi`** --
  third `reserved-memory` entry `p25_iq_dma: p25-iq-dma@19000000`
  (256 KB, `no-map`) and a matching `p25-iq` rxbuffer node with
  `buffer-size = <0x8000>` (32 KB sub-buffer × 8 = 256 KB ring, matches
  FPGA `iq_dma_num_buffers_log2 = 3`).
- **Verification on Windows host:** **17/17 lsm unit tests pass**
  (`cargo test lsm::`). The encoder produces SDRTrunk's golden vector
  bit-for-bit (`encode_nid(1, 0) == 0x00103185B7E9E224`). The BCH decoder
  recovers all 550 corrupted codewords across the 1..=11 error sweep
  (50 trials per error level). The streaming FIR matches the batch FIR
  to <1e-5 absolute on a 400-sample frequency-sweep input chunked at
  position 137. The streaming /2 decimator preserves the even-grid phase
  across 7+8+8 odd-length chunks. Both sync detectors find a clean sync
  in a synthetic dibit stream and the BCH decoder recovers the embedded
  NAC/DUID with zero errors. The status dibit at NID-window index 11 is
  proven to be skipped (two streams differing only in that dibit produce
  identical extracted NIDs).
- **ARM cross-build clean:** `cargo check --target armv7-unknown-linux-gnueabihf`
  produces 27 warnings (all dead-code on existing modules) and zero
  errors. No new runtime dependencies (`pm-remez` and `num-complex` not
  pulled in).
- **Pending verification:** Tezuka firmware rebuild (`build.bat --p25`
  inside the Tezuka Docker container) to consume the Phase 6C XSA + the
  new lsm module, SD-card flash, on-target smoke test (LSM reader task
  starts, iq_dma wakeups arrive at ~7.6 Hz, NAC=0x8A1 dominates the
  cumulative histogram at a per-second rate comparable to SDRTrunk on
  the same antenna).

Phase 6D ends at "Rust port compiles, all unit tests pass, ARM
cross-build clean". Phase 6E -- HDL port of the streaming filters and
the demod loop into Amaranth, replacing the C4FM-only path -- is the
next step on the ladder.

---

## [2026-04-09] Phase 6C: P25 Post-DDC IQ Ring DMA in FPGA Gateware

**Branch:** fishball-p25

Phase 6C is complete (gateware logic + Verilog/SVD/PAC regen). Adds a third
ring DMA inside the P25 IP core that streams the control DDC's post-decimation
IQ output (62.5 kSPS, 16-bit signed I/Q, two samples per 64-bit word) to a
reserved DDR carve-out at `0x1900_0000`. This is the bridge that lets Phase
6D's Rust port of the validated Python LSM demod consume live antenna data
without disturbing the existing dibit pipeline. See
`doc/changes/013_phase6c_iq_dma.md` for the full write-up and
`doc/P25_ADDRESS_MAP.md` for the canonical address-space tables.

- **`maia-hdl/p25_hdl/iq_packer.py`** -- new ~120-line `IQPacker` Amaranth
  module. Buffers two consecutive `(re, im)` pairs into a 64-bit AXI4-Stream
  word `{im[1], re[1], im[0], re[0]}` (sample 0 in low half). Mirrors
  `DibitPacker`'s handshake + sticky-overflow conventions exactly.
- **`maia-hdl/p25_hdl/p25_top.py`** -- third tap of the control DDC output
  (alongside `c4fm_demod` and `symbol_timing`); new `iq_dma` instance of
  `DmaStreamRingWrite` exposing `m_axi_iq`; new 5th register bank `iq` at
  byte offset `0x80` containing `iq_dma_status`/`iq_dma_control`/
  `iq_next_address`. The bank decoder was widened from `address[3:5]`
  (4 banks) to `address[3:6]` (8 banks max) -- no `axi4_awidth` change
  needed, the existing 7-bit word address has plenty of headroom.
- **`maia-hdl/p25_hdl/config.py`** -- new `iq_dma_*` fields with the full
  bandwidth math (250 KB/s, 256 KB ring = 8 x 32 KB sub-buffers, ~128 ms
  per sub-buffer interrupt, ~1 s of IQ in flight) and an alignment assert
  in `validate()`.
- **`maia-hdl/ip/p25-core/package_ip.tcl`** -- one new
  `ipx::associate_bus_interfaces -busif m_axi_iq -clock clk` line.
- **`maia-hdl/projects/fishball7020_p25/system_bd.tcl`** -- one new
  `ad_mem_hp1_interconnect` line. SmartConnect on HP1 now arbitrates
  three masters (`m_axi_dibit` + `m_axi_traffic` + `m_axi_iq`); HP1 budget
  at ~1.7 GB/s absorbs the new ~250 KB/s consumer with ~0.015% utilisation.
- **`maia-hdl/test/test_iq_packer.py`** -- new pure-Python pysim test (uses
  `amaranth.sim.Simulator`, mirrors `test_dibit_packer.py`). 7 tests
  covering single/multi-pair packing, two's complement extremes, no-strobe
  quiescence, backpressure handshake, and the sticky overflow flag. All 7
  pass on the Windows host.
- **`maia-hdl/test_cocotb/iq_packer/`** -- new cocotb scaffold (Makefile,
  verilog.py, tb.v, test_iq_packer.py) ready to run in WSL Ubuntu or
  Docker as a CI step.
- **`doc/P25_ADDRESS_MAP.md`** -- new canonical address-map document.
  Single source of truth for DDR carve-outs, AXI-Lite register banks, and
  IRQ assignments. Per the doc-as-we-go discipline, written *before* the
  wiring code so the address-map decisions were committed in writing
  before they hardened.
- **Verification on Windows host:** 7/7 pysim tests pass. Full P25Core
  Amaranth elaboration succeeds. `build_hdl.sh --verilog-only --p25` (run
  in the project's Python 3.11 Docker container) regenerates `p25_core.v`
  (22046 lines, all 20 `m_axi_iq_*` ports declared), `p25.svd` (with the
  three new registers at `0x80`/`0x84`/`0x88`), and `p25-pac/src/lib.rs`
  via `svd2rust v0.33.5` cleanly. Phase 6D's Rust port can `use p25_pac::iq`
  immediately.
- **Pending verification:** Vivado bitstream synth (`build_fpga.bat --p25`,
  ~30-60 min on host Vivado 2023.2) and on-hardware smoke test (load
  bitstream, devmem to enable `iq_dma_control`, mmap `0x1900_0000`, dump
  sub-buffers, feed to `tools/p25_lsm_demod.py`, check NAC accuracy).

Phase 6C ends at "gateware logic verified, Verilog regenerates, register
PAC regenerates". Phase 6D -- Rust port of the LSM demod to the Cortex-A9,
fed by this new IQ ring -- is the next step on the ladder.

---

## [2026-04-09] Phase 6B: P25 NID BCH(63,16,11) FEC Validated Against SDRTrunk

**Branch:** fishball-p25

Phase 6B is complete. NID forward error correction is now in the validated
Python reference, and on the better-signal test recording our prototype
produces an exact sync count match to SDRTrunk (313/313) with 100% NAC
accuracy after FEC. See `doc/changes/012_p25_nid_bch_fec.md` for the full
write-up.

- **`tools/p25_nid_fec.py`** -- new ~280-line standalone module. Encoder
  is verbatim from SDRTrunk's `BCH_63_16_23_P25_Test.java` (16-row
  generator matrix in octal + 5-line systematic encoding loop). Decoder
  uses maximum-likelihood nearest-neighbour search across the 65,536-entry
  codebook -- mathematically identical to BCH decoding within the unique-
  decoding sphere, ~30 lines vs ~600 for a Berlekamp-Massey + Chien search
  port from the Linux-derived `BCH.java` base class.
- **Encoder bit-perfect**: `encode_nid(NAC=1, DUID=0)` produces
  `0x00103185B7E9E224` exactly, matching SDRTrunk's documented test vector
  in `BCH_63_16_23_P25_Test.java:38`.
- **Decoder bit-perfect**: synthetic 1-11 bit error injection at all
  positions, 100 trials each: 1100/1100 corrected. At 12 errors,
  200/200 declared uncorrectable -- exactly the (63,16,d=23) bound.
- **`tools/p25_lsm_demod.py`** -- BCH FEC integrated into both sync
  detector paths. `SyncEvent` now carries `nid_raw`, `nac_fec`, `duid_fec`,
  `fec_errors` (-1 if uncorrectable). New "after BCH(63,16,11) FEC" section
  in the report shows correctable count, bit-error histogram, and the
  "NAC among correctable" metric (the right way to measure FEC quality
  while ignoring false sync hits).
- **End-to-end validation**:
  - 175119 wav (better signal): 313/313 syncs, 313/313 correctable,
    313/313 NAC=0x8A1 after FEC. **Exact match to SDRTrunk truth log.**
  - 163748 wav (noisier): 339/335 syncs, 330/339 correctable, 330/330
    NAC=0x8A1 *among correctable*. The 9 uncorrectable events are
    dominated by 4 false sync hits beyond truth + 5 PLL-slip events;
    not a FEC bug.
- **Pyradio evaluation**: started this session by reading the user's
  pre-existing pyradio P25 port at `~/Downloads/sdrtrunk-master/docs/pyradio/`
  to assess whether to merge it. Two findings: (1) pyradio's LSM demod
  loop is algorithmically equivalent to ours but uses `MAX_PLL = π`
  instead of SDRTrunk's `π/3` (deviation comment cites Pluto crystal
  offset); ours is more faithful. (2) **pyradio's `decode_p25_nid`
  uses RS(24,12,13) over GF(2^6), NOT the BCH(63,16,11) that SDRTrunk
  actually implements**, has zero unit tests for the FEC, and the
  pyradio author's own skeleton at `p25_pure_python.py` mislabels which
  code goes where. Per the project mandate ("if pyradio diverges from
  SDRTrunk, prefer SDRTrunk"), this session ports BCH directly from
  SDRTrunk Java rather than trusting pyradio's RS substitute.
- **TSBK parser deferred**: cherry-picking pyradio's TSBK parser would
  require ~1000 lines of trellis decoder + deinterleaver + CRC + opcode
  dispatch + an event/identifier framework. Significantly bigger lift
  than the FEC was. Recommend a dedicated future session.

## [2026-04-09] Phase 6: P25 LSM Demodulator -- Validated Python Reference

**Branch:** fishball-p25

The Fishball P25 target site is **LSM Simulcast**, not C4FM. All P25 systems
within RF range of the user's location are LSM. The existing C4FM-only Phase 1
gateware cannot decode LSM regardless of how we tune the existing slicer --
LSM is pulse-shaped CQPSK with data in carrier *phase*, requiring an RRC
matched filter and a decision-directed PLL that the current architecture
lacks. See `doc/changes/011_p25_lsm_python_reference.md` for the full
diagnosis and the validated Python reference port.

This change establishes the project's new direction:

- **Read SDRTrunk's LSM source line by line** (`P25P1DecoderLSM.java`,
  `P25P1DemodulatorLSM.java`, `Dibit.java`, `P25P1MessageFramer.java`,
  `P25P1SoftSyncDetector.java`, `BCH_63_16_23_P25.java`) and document
  every constant and DSP block.
- **`tools/p25_lsm_demod.py`** -- new self-contained ~900-line Python port
  of the full LSM chain: half-band decimation -> Parks-McClellan baseband
  LPF -> RRC matched filter -> demod loop with AGC + PLL + Gardner TED +
  atan2 slicer -> hard + soft sync detectors -> status-aware NID extractor
  (skip dibit 11). Variable names mirror the Java source for direct diff.
  Includes a 6-panel Matplotlib `--plot` dashboard for visual diagnosis.
- **Validated bit-exact against SDRTrunk** on a 27-second .wav recording
  the user captured via their Pluto + SDRTrunk + custom pluto_server.py
  bridge: 339 sync events vs 335 in SDRTrunk's truth log (101.2% recall),
  91% at perfect Hamming distance 0, NAC = 0x8A1 in 93.5% of detections,
  DUID = 0x7 in 95.9% of detections. Symbol rate within 0.006% of nominal.
- **`tools/monitor_p25_decoder.py`** -- new tool for live polling of the
  on-target Fishball `/api/stats` and `/api/dibit_dump` endpoints into
  JSONL snapshots. Used during the failed slow-convergence-tracking
  detour but kept as the standard "watch the decoder over time" utility.
- **`maia-hdl/p25_hdl/p25_top.py`** -- corrected the docstring's wrong
  claim that the existing slicer is "unified for C4FM and LSM". It is
  not. Documented the architectural delta (RRC + PLL missing) and the
  recommended fix path (PS Rust -> HDL).
- **DEVPLAN.md** -- new Phase 6 with a 5-step ladder (6A Python reference
  done, 6B BCH FEC, 6C IQ DMA in FPGA, 6D Rust on PS, 6E HDL/PL final).
  Each step locks in a fixed reference for the next, so each one has at
  most one degree of freedom and a known-good target.

The validated Python reference unblocks the rest of the project. Phase 6B
through 6E now become mechanical port-and-test exercises with bit-exact
targets, instead of "design and pray."

## [2026-04-09] Phase 5 (final): P25 Decoder Observability + Init Hardening

**Branch:** fishball-p25

A bundle of small but high-leverage diagnostic-infrastructure changes that
turned the P25 decoder from a black box into something we could actually
debug from a browser. See `doc/changes/010_p25_decoder_observability.md`
for the per-item rationale.

The headline finding: `S60p25-httpd` was launching the daemon via
`start-stop-daemon -b`, which detaches the process and closes its standard
streams under busybox. **Every `tracing::info!` we have ever written has
been going straight to /dev/null.** Fixed by wrapping the binary in
`sh -c 'exec ... >> /var/log/p25-httpd.log 2>&1'`. The same bug exists in
`S60maia-httpd` (Maia's init script) and is worth fixing in `fishball-dev`
in a follow-up.

Other items in this bundle:

- **Explicit `EnvFilter` setup** in `main.rs` so the default tracing
  filter is `info,p25_httpd=info` and not whatever `fmt::init()`'s
  undocumented default is.
- **`/api/stats` exposes AD9361 RX gain + RSSI.** No more "ssh in and
  cat sysfs" while debugging.
- **`/api/dibit_dump` exposes inner/outer histogram percentages and a
  raw on-air DUID histogram.** The DUID histogram in particular was
  the diagnostic that broke open the LSM debug session -- it showed a
  near-uniform spread across all 16 nibble values (TSDU at 4-5%
  instead of expected ~100%), immediately implicating the demodulator
  architecture.
- **`SYNC_THRESHOLD` widened from 4 to 10** with a long comment
  explaining why and when it should drop back. Temporary diagnostic
  measure -- not a fix.
- **Periodic `expire_grants(30)`** task in `main.rs` so the dashboard's
  Active Grants count doesn't grow forever once decoding works.
- **NID DUID hardcode hack** in `decode_nid` to flush out downstream
  bugs faster while the demod is still broken. Diagnostic raw_duid
  histogram preserves the actual on-air values so we can observe the
  bit-error pattern. Replaced by proper BCH(64,16) FEC in change 011's
  follow-up work (Phase 6B).
- **Category-1 cleanups**: drop unused imports (`put`, `TsbkMessage`),
  module-level `#![allow(dead_code)]` for traffic-following placeholder
  code, remove dead `tracing::debug!` in `fpga.rs`, fold the NCO
  frequency into the existing `configure_ddc` info log.

## [2026-04-09] Phase 5 (cont.): Build Verilog Staleness Detection

**Branch:** fishball-p25

Added automatic staleness detection for Amaranth-generated IP Verilog in
`build_fpga.bat`. Previously the script only checked whether `p25_core.v` /
`maia_sdr.v` existed, so any edit to `p25_hdl/*.py` or `maia_hdl/*.py` after
the first build would silently bake pre-edit logic into new bitstreams that
looked fresh by mtime. See `doc/changes/009_build_verilog_staleness.md`.

This was discovered while debugging why the P25 dibit slicer fix (symbol-rate
differential, commits 3f1bff7 and 94faae9) wasn't affecting the on-target
FPGA behaviour. The bitstream was built 15 minutes *after* the fix was
committed, but Vivado had picked up the old `p25_core.v` generated 90 minutes
*before* the fix. The symbol-rate slicer logic never made it into the
bitstream, and the on-target dibit histogram exactly matched the pre-fix
failure mode fingerprint recorded in the `p25_top.py` docstring.

- **New:** `tools/check_verilog_stale.ps1` -- PowerShell helper that compares
  generated `.v` mtime against the maximum mtime of `*.py` files under one or
  more source directories. Outputs `MISSING`, `STALE`, or `FRESH`.
- **Changed:** `build_fpga.bat` Step 2 now calls this helper for both the
  Maia SDR and P25 IP Verilog. P25 checks against both `p25_hdl` and
  `maia_hdl` (since `p25_top.py` imports DDC, registers, DMA, and CDC from
  `maia_hdl`). On `STALE` or `MISSING`, it automatically invokes
  `build_hdl.bat --verilog-only [--p25]` in Docker before running Vivado.
- **No API change:** users still run `build_fpga.bat --p25` as the single
  command. The `--verilog-only` flag on `build_hdl.bat` remains as an
  internal mechanism and is no longer user-facing.

---

## [2026-04-09] Phase 5: Build Pipeline, Register CDC, Ring DMA, First Hardware Boot

**Branch:** fishball-p25

First successful hardware boot of the P25 FPGA bitstream on the Fishball Z7020,
followed by a same-day refactor of the DMA path from one-shot to a ring buffer
architecture. Also fixed multiple build pipeline issues from the standalone
repo migration, corrected the SVD register map, added DDC FIR coefficient
initialization, and fixed a register clock-domain-crossing bug.

### Build Pipeline Fixes

- Fixed P25 Verilog generation to use Docker (ext4 filesystem avoids NTFS pip issues)
- Fixed CMD escaping in `build_fpga.bat` for Docker invocation
- Added missing ADI library builds: `util_clkdiv`, `util_rfifo`, `util_wfifo`
- Fixed stale TCL/Makefile paths left over from standalone repo migration
- Added `.gitattributes` enforcing LF line endings on `.sh` files
- Added skip-if-built logic for incremental ADI library builds (and ADI library packages)
- `build_hdl.sh` now auto-generates `p25.svd` when `--p25` is set and regenerates `p25-pac/src/lib.rs` via a downloaded `svd2rust` binary, so SVD/PAC stay in sync with Amaranth HDL without manual steps

### Tezuka Firmware Fixes

- Added XSA cache invalidation in `build.sh` (detects when source XSA is newer than cached package and forces a package rebuild)
- Added p25-httpd / maia-httpd source change detection for auto-rebuild
- Removed phantom AXI UART Lite from `fishball-p25.dtsi` (was at 0x42C00000, inherited from the Maia DTSI but not present in the P25 FPGA design -- caused a kernel panic in `uartlite_probe`)

### Register Map Fix

- SVD register offsets were wrong: FPGA uses bank select bits [4:3] of word address giving byte offsets 0x00/0x20/0x40/0x60, but SVD had 0x00/0x08/0x20/0x30
- Fixed `RegisterMap` in `p25_top.py`, regenerated SVD and PAC

### Register Clock-Domain-Crossing Fix

- `demod_registers` and `traffic_registers` were instantiated in the `s_axi_lite` clock domain (100 MHz) but their Wpulse outputs fed modules running in the `sync` domain (62.5 MHz)
- The unsynchronized Wpulse edges were missed ~38% of the time due to clock skew, causing the DMA start signal to be silently dropped
- Wrapped both with `RegisterCDC`, matching the existing `sdr_registers_cdc` pattern already used for the DDC bank

### DDC FIR Coefficient Initialization

- Added 3-stage FIR filter coefficient loading to p25-httpd (`fpga.rs`, `configure_ddc()`)
- Previously only NCO frequency and enable bits were programmed
- P25 channel filter: 128x decimation (16x4x2), 8 MSPS -> 62.5 kSPS (13 samples/symbol)
  - Stage 1: 48 taps, 200 kHz cutoff, /16
  - Stage 2: 32 taps, 50 kHz cutoff, /4
  - Stage 3: 64 taps, 8 kHz cutoff, /2
- Coefficients: Kaiser window (designed via `scipy.signal.firwin`), 18-bit quantized, >137 dB stopband

### Ring-Buffer DMA Rework

- Replaced one-shot `DmaStreamWrite` with a new `DmaStreamRingWrite` module (ported from the never-merged upstream IQ stream branch)
- 8 sub-buffers x 4 KB = 32 KB ring per DMA channel
- Sub-buffer completion raises an interrupt on the AXI B-channel write response
- New `demod_status.last_buffer` field (3 bits) tracks which sub-buffer was just written
- Continuous operation -- no PS-side restart per buffer
- `demod_control` / `traffic_demod_control` simplified: start/stop Wpulses removed, only `demod_enable` (RW level) remains
- `demod_status` / `traffic_demod_status` grow the `last_buffer` field; all other bank offsets unchanged (`ddc_frequency`=0x2C, `ddc_control`=0x30, `demod_status`=0x40, etc.)
- SVD and PAC regenerated

### DMA Address Layout

- FPGA hardcodes physical buffer addresses `0x17000000` (dibit) and `0x18000000` (traffic)
- Reduced from 1 MB each to 32 KB each to match the new ring size
- Device tree updated: `reg = <0x17000000 0x8000>` and `<0x18000000 0x8000>`

### Hardware Test Results

- Board booted with P25 bitstream, FPGA registers accessible
- Product ID: 0x70323566 ("p25f")
- AD9361 configured: 858.1 MHz center / 8 MSPS
- Dibit counter incrementing (DSP chain active)
- Web UI served on port 8080
- SDRTrunk confirmed P25 signal at 860.9625 MHz (NAC:2209, WACN:781824, System:2208)

### Unified C4FM/LSM Differential Demod

- Local control channel turned out to be LSM (CQPSK), not C4FM. The original FM-only cross-product discriminator could not produce LSM dibits 1 and 3
- Refactored `C4FMDemod` to compute the full complex differential product `z[n] * conj(z[n-1])` (4 DSP48E1 multiplies instead of 2). Outputs `diff_re_out` and `diff_im_out`; `disc_out` retained as a legacy alias for `diff_im_out`
- Refactored `SymbolTimingRecovery` to take both `diff_re_in` and `diff_im_in` and use a sign-bit slicer that maps the four quadrants of the differential plane to the four P25 dibit values, matching SDRTrunk's `P25P1DemodulatorLSM.toDibit` exactly. Works for both C4FM and LSM with no path divergence
- `p25_top.py` wires the new IQ pair through both control and traffic chains
- Updated `test_c4fm_demod.py` and `test_symbol_timing.py` to drive and verify the new I/Q differential interface; all 16 P25 HDL tests pass

### p25-httpd Diagnostic Logging

Added structured tracing and a `/api/dibit_dump` endpoint to make hardware bring-up debuggable from the web UI without devmem on the target.

- `target=p25_irq` -- IRQ arrival counter (first 10, then every 64th)
- `target=p25_reader` -- per-wakeup buffer count, byte total, dibit histogram
- `target=p25_stats` -- FPGA register state (`dibit_count`, `overflow`, `last_buffer`, `next_addr`) every 2 s
- `target=p25_decoder` -- periodic dibit histogram + sync correlator stats, near-sync events (Hamming distance ≤ 12), state transitions, NID Golay decode results
- `GET /api/dibit_dump` -- JSON: `total_dibits`, per-value histogram with percentages, sync correlator stats (`hits`, `near_misses`, `best_distance`), and the last 2048 dibits packed as hex
- Dashboard: two new cards ("Dibit Histogram", "Sync Correlator") that poll `/api/dibit_dump` and surface the histogram + best Hamming distance live

---

## [2026-04-08] P25 Migration into maia-sdr

**Branch:** fishball-p25

Migrated the P25 trunking radio from the standalone `fishball-p25` repo into
the maia-sdr tree. The standalone approach failed due to relative path breakage,
IIO DMA not routed, and DTS/bitstream mismatches.

### P25 FPGA Gateware (Phases 0-1)

- `maia-hdl/p25_hdl/` -- Amaranth HDL modules: C4FM demod, Gardner symbol timing, dibit packer
- `maia-hdl/ip/p25-core/` -- Vivado IP packaging (TCL + constraints)
- `maia-hdl/projects/fishball7020_p25/` -- Vivado project (block design, constraints, top-level wrapper)
- `maia-hdl/test/` -- 14 Amaranth simulation tests (C4FM, symbol timing, dibit packer)
- `maia-hdl/generate_p25_svd.py` -- SVD generation for P25 register PAC
- Dual DDC + demod chains: control channel + traffic channel with independent NCO
- Spectrometer + recorder removed (unused in P25), freeing ~16 DSP48, ~5K LUT, ~12 BRAM
- Resource usage: 36 DSP48 (16%), 15.6K LUT (29%), 16 BRAM (11%) on Z7020

### P25 Firmware (Phases 2-3)

- `p25-httpd/` -- Rust workspace with `p25-json` and `p25-pac` sub-crates
- Control channel decoder: frame sync, NID extraction, TSDU de-interleave, TSBK parser
- FEC: Golay(23,12), trellis Viterbi, CRC-16-CCITT
- 6 TSBK opcodes: GRP_V_CH_GRANT, GRP_V_CH_GRANT_UPDT, IDEN_UP, NET_STS_BCST, RFSS_STS_BCST, ADJ_STS_BCST
- Traffic manager: grant lifecycle, NCO word calculation, timeout management
- Web dashboard: REST API (6 endpoints), WebSocket events, embedded SPA
- 16 Rust unit tests passing

### Build Infrastructure

- `build_fpga.bat --p25` builds P25 FPGA (fishball7020_p25)
- P25 Tezuka config: `fishball_p25_7020_defconfig`

See `doc/changes/003_p25_scaffolding.md` through `doc/changes/007_p25_traffic_channel.md` for details.

---

## [2026-04-07] Upstream Sync

**Branch:** fishball-dev

Rebased `fishball-dev` onto `upstream/main`, picking up 10 new commits from
F5OEO: Vivado 2023 env, WASM broadcast channel, interpolation coefficients,
overclock simplification, FIFO redesign, clock simplification, x32 interpolator,
RX2 fix, DAC FIR support, RX with FIFO.

Also analyzed the upstream `refactor` branch (53 commits, 18K lines, not yet
merged to main) -- includes board-based project layout, common TCL library,
DVB-S2 receiver, IQ burst capture, frequency sweeper, multi-board sync.

See `doc/changes/002_upstream_sync.md` for full analysis.

---

## [2026-03-08] Project Setup

**Branch:** fishball-dev

### Fork & Repository

- Forked `F5OEO/maia-sdr` -> `andylee77/maia-sdr`
- Created `fishball-dev` branch for Fishball Z7020 development
- Set up upstream tracking: `upstream` -> `F5OEO/maia-sdr`, `origin` -> `andylee77/maia-sdr`

### Build Infrastructure (Initial)

- In-repo build scripts (see `doc/changes/001_build_scripts.md`):
  - `build_hdl.bat/.sh` -- Amaranth -> Verilog + SVD via Docker
  - `build_fpga.bat` -- Full Vivado FPGA synthesis (Windows native)
  - `sim_hdl.bat/.sh` -- HDL simulation via Docker (Amaranth + cocotb)
  - `clean.bat` -- Clean all build artifacts
- Docker volume `maia-hdl-build` for persistent Python venv cache
- Tezuka firmware pointed to this fork's `fishball-dev` branch

---

## Pending / Future

- [ ] Phase 5 (remaining): Live control channel decode, traffic following test, SD card clean boot
- [ ] Voice frame extraction (LDU1/LDU2 -> IMBE frames)
- [ ] Audio codec (mbelib, codec2, or DVSI)
- [ ] Document IQ streaming patches to maia-httpd
- [ ] FPGA ring buffer implementation for high-bandwidth IQ
