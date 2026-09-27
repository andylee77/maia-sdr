# 053 — Hardware validation bench (`fbench` + `hwval` bitstream)

**Date:** 2026-09-26. **Branch:** fishball-p25. **Bake required:** only for Tier 1
(`hwval` image); Tier 0 runs on the current P25 image.

## Why

Live P25 debugging kept circling through the web UI of the stack under test. This change
adds a bench suite that measures the hardware underneath directly, with two cabled
Fishballs, deterministic stimulus, self-checking data, and machine-readable results that
a Claude session can drive from the CLI. Design contract:
[`doc/HW_VALIDATION_SUITE.md`](../HW_VALIDATION_SUITE.md).

## Evidence gathered before building

A 13-reader sweep of the board docs and schematics, Vivado project and reports, Maia/P25
HDL, maia-kmod, p25-httpd hardware layer, Tezuka DT/defconfig/boot scripts, ADI HDL and
driver test hooks, existing tools and diagnostics history, plus a read-only probe of the
connected unit. The DMA ring path (HDL writer → DDR → maia-kmod → Rust readers) was then
audited separately, and each suspected defect was put to two independent refutation
attempts. Survivors are listed with their status.

### Ring transport

1. **`gap_dibits=14336, overflows=1` is a measurement artifact.** The traffic dibit ring
   publishes 4 KiB sub-buffers (16384 dibits, 3.413 s at 4800 sym/s); the host tool
   polled a 2048-dibit API window, so every observed sub-buffer scores exactly
   16384 − 2048 = 14336. All counter values in the 2026-05-03 run are multiples of 16384.
   The number neither implicates nor exonerates the ring. Real ring loss has never been
   measured. (4/4 verifiers confirmed.)
2. **Overflow telemetry is blind.** The Rsticky overflow bit shares a register with
   `last_buffer` (`iq`, `pre_diff_iq`, `wideband_iq`, `traffic_*`), any read clears it,
   and `read_dma_buffers()` reads `last_buffer` first. `wideband_iq_overflow()` only sees
   pulses in the few ms between the two reads. (`p25_top.py:401-405`,
   `register.py:127-138`, `fpga.rs:1351-1358`, `wideband_iq_task.rs:188-197`.)
3. **No lap detection anywhere.** `last_buffer` is log2(N) bits, data carries no sequence,
   the reader works modulo N. A reader stall ≥ (N−1) sub-buffers (0.49 s for wideband at
   8 MSPS) loses data silently; exactly N returns nothing.
4. **Dibits are delivered in 3.41 s blocks, and gating happens at delivery time.**
   `DmaStreamRingWrite` publishes only whole sub-buffers (no flush/timeout). The traffic
   reader samples `current_talkgroup` once per 16384-dibit batch while TG changes and
   framer resets act in real time. On same-TG re-grants, same-frequency channel reuse and
   same-frequency not-followed/encrypted grants, up to 3.41 s of the previous call's
   dibits are decoded under the new call (cross-call bleed), and batches arriving after
   CallClose are dropped (tail truncation). The codebase already notes the 3.4 s cadence
   (`chain.rs:184-186`, doc 035), and cold First-IMBE maxima cluster just below 3.41 s
   (doc 049). **This is a strong live-glitch candidate independent of RF.**
5. **Thin elasticity on the wideband ring.** One 64-bit holding word plus a 5-burst
   write-response cap gives ≈16 µs of HP1 write-latency tolerance at 32 MB/s (the upstream
   recorder has a FIFO18 in front, ≈5× more). Simulating the unmodified production
   `IQPacker` + `DmaStreamRingWrite` at 8 MSPS reproduces the cliff: no loss at 16.3 µs
   write-response latency, loss from 16.6 µs, 11 % at 19.2 µs, as silent whole-word
   gaps with contiguous addresses. The same simulation shows `IQPacker` pulsing overflow
   when nothing was lost (45 pulses for 20 lost words), and a source that stops while an
   AW is accepted leaving that burst open on HP1. Whether field latency ever reaches
   16 µs is what `hw.legacy_ring` measures.
6. **Enable toggling and `sdr_reset` are unsafe mid-stream.** Re-enable resumes mid
   sub-buffer (the first delivered buffer splices the previous session); `awvalid` can
   drop without a handshake; `sdr_reset` resets the masters but not the HP1 interconnect
   (stranded partial bursts; doc 023 records the resulting reboot).
7. **Blocking SD writes in the wideband drain task** (`std::fs::write_all` + `sync_all`
   in a tokio task) are a latent lap trigger for long captures at 8 MSPS; short captures
   probably fit the page cache.
8. **`notify_waiters()` loses IRQ wakeups** (no permit); impact is bounded to one IRQ
   period of latency because the reader drains up to `last_buffer` on each wake.
9. **maia-kmod** invalidates L1 before L2 (reverse of Linux dev→CPU order; unreachable in
   today's single-reader p25-httpd, a hazard for new tools) and keeps a per-device VA for
   invalidation (latent multi-process hazard). **The `maia-sdr.ko` in the P25 image is a
   stale leftover** (mtime 2026-03-02, upstream v0.10.0) from an earlier Maia build in
   the persistent Docker volume; the P25 defconfig neither builds nor loads it, so a
   clean P25 build would have no `/dev/p25-*` nodes.

### Hardware, clocks, memory

1. The FPGA I/O banks 34/35 are tied to 1.8 V on both schematics, but the XDC uses
   LVDS_25 with DIFF_TERM and LVCMOS25 (2.5 V-only standards). Needs a DMM check; eye
   margins are the practical measure.
2. At 8 MSPS 1R1T the LVDS DATA_CLK is 16 MHz: IDELAY (≈2.4 ns range) and AD9361 delay
   sweeps pass everywhere, so interface margin must be measured at 30.72–61.44 MSPS. The
   boot digital tune sweeps only the AD9361-side delays (flags = 0); FPGA IDELAY stays at
   30 of 31; there is no re-tune when p25-httpd changes the rate; no I/O delays are
   constrained (slack "inf").
3. The 40 MHz AD9361 reference appears to be a VCTCXO; its control pin `XTAL_VTC` reaches
   JP5 pin 15, and only the stock Tezuka Maia bitstream drives a GPSDO core on JP5. P25
   builds leave it undriven (possible reference wander — measurable).
4. OpenSDRLab TX outputs have PGA-102+ gain blocks (the shared docs omit them): TX can
   reach roughly +15 to +20 dBm, hence the TX-attenuation interlock.
5. The shared hardware docs' AD9361 pin tables are the Pluto+ CMOS map and are wrong for
   this LVDS board; the XDC and schematic PDFs are authoritative.
6. DDR: the shipped FSBL programs DDR3-1066 CL7/CWL6 at 533 MHz; the P25 XSA's
   `ps7_init` carries CL11/CWL8 (not deployed); an overclock FSBL (DDR 600 MHz with the
   same timings) ships on the SD image. ECC off, no EDAC.
7. HP1 is `axi_interconnect` 2.1 at 62.5 MHz (≈500 MB/s, with Vivado-inserted
   packet-mode FIFOs), not SmartConnect at 1.7 GB/s. HP0 and HP3 are unused.
8. `p25_core` takes samples from `util_wfifo` without its valid qualifier (cpack/libiio
   honours it). In steady-state 1R1T the 8-sample bursts tile exactly, so valid should
   be continuous; it has never been measured.
9. The original (AD9361) unit's Tezuka P25 SD card fails to come up cleanly on its last
   test firmware: Windows RNDIS Code 10, iiod "Broken pipe" on USB, and the device
   re-enumerated and dropped within 2.5 minutes. The same board then booted its factory
   SD card (`plutosdr-fw v0.38`, 2R2T, DDS present). Firmware serials follow the SD card
   (`OVHNVI2FJEBXAB4M` P25 card, `104473023196000bf5ff1a00aae12c3ca8` factory card), so
   the bench identifies boards by label and, on `hwval`, by the PL device DNA. The newer
   unit carries an AD9363 instead of the AD9361 (board photos).

### First hardware runs (unit A, P25 card, 2026-09-26)

1. **Pre-diff DMA carve-out is not reserved; the FPGA writes into free RAM.** U-Boot
   `initrd_high=0x20000000` places the 24 MB ramdisk over `0x1F000000`, the kernel logs
   `failed to reserve memory for node 'p25-pre-diff-iq-dma@1f000000'`, `/proc/iomem`
   shows `0x1C040000–0x1FFFFFFF` as System RAM after the initrd is freed, and the
   pre-diff ring is enabled (`0x1A4 = 1`). Fix: `initrd_high=0x18000000` and
   `fdt_high=0x18000000` (SD `uEnv.txt` now, Tezuka U-Boot env permanently).
2. **An audit read hung the board.** The first `sys.audit` read the AFI registers of all
   four HP ports; HP0/HP3 are unclocked in the P25 bitstream, the read never completed
   and the system watchdog reset the board (REBOOT_STATUS `0x00410000`, SWDT bit). AFI
   reads are now opt-in per clocked port.
3. **The production wideband ring is bit-exact.** `xport.p25_ring_prbs`: AD9361 BIST PRBS
   through `util_wfifo` → `p25_core` → `IQPacker` → `DmaStreamRingWrite` → DDR →
   maia-kmod → agent reader, 1.92 GB (every sample of 60 s at 8 MSPS) with **0 anomalies**
   (no gaps, laps, torn/stale buffers, repeats or bit errors). The ring hardware, the
   kernel module's cache handling and `p25_core`'s valid-less ingest are exonerated when
   the reader keeps up.
4. **Lap blindness confirmed.** `xport.p25_ring_lap`: reader stalls ≤ 450 ms lose
   nothing; from 500 ms each stall silently loses exactly one lap (4,194,304 samples);
   model (N−1)·T_buf = 491.5 ms.
5. **The SD card cannot carry wideband captures.** `store.sd_bench`: 19.1 MB/s write,
   22.8 MB/s read, write-latency p99 68 ms, max 3.36 s, fsync 6.1 s. An 8 MSPS capture
   needs 32 MB/s, and any write stall over 0.49 s laps the ring. Together with 3 and 4,
   this explains "the wideband ring never produced a stable capture": the SD sink laps a
   healthy ring, invisibly. Captures must be RAM-first (design doc §2.3).
6. **Interface margins are healthy.** `iface.prbs_soak` 0 error intervals in 60 s at
   8 MSPS; `iface.eye_ad9361` at 61.44 MSPS: 186/256 delay cells pass, clean diagonal eye,
   chosen (clock 0, data 5) with margin 6, DT boot value `rx-data-delay 4` inside;
   `iface.eye_idelay` 30–32 of 32 taps pass on all lanes (the 2.4 ns IDELAY range is
   shorter than the 4 ns UI, so the AD9361 delays are the effective control);
   `iface.tx_link` 0 PN errors at TX delay 0x90.
7. `mem.ps_memtest` 256 MiB × 7 patterns: 0 errors. `mem.ps_bw`: cached memcpy 285 MB/s,
   write 443 MB/s, read 354 MB/s.
8. Baseline otherwise matches expectations: DDR3L at 533 MHz CL7 (tAA 13.125 ns), CPU
   666.67 MHz, FCLK0/1 100/200 MHz, PL310 with data/instruction prefetch and early
   BRESP on, CMA 64 MiB at 0x3C000000, stale `maia-sdr.ko` (mtime 2026-03-02) loaded,
   no leftover Maia services running, rails nominal, Zynq 49 °C, AD9361 DATA_CLK
   16.0004 MHz for 8 MSPS.

### Two-board runs (A + B, cabled through 20 dB, 2026-09-26)

1. Unit B (AD9363) repeats A's results: `xport.p25_ring_prbs` 0 anomalies,
   `iface.eye_ad9361` margin 5. Bench network: A↔B 105.5 MB/s over GbE, host↔A
   28.4 MB/s over USB, host→B routed through A.
2. **The boards' references differ by 0.66 ppm, and it is the boards, not the
   transceivers.** `rf.cw_ppm` 300 s each way at 858.3 MHz: B→A (AD9363 TX, AD9361 RX)
   +0.6669 ppm, A→B (AD9361 TX, AD9363 RX) −0.6581 ppm. The two directions mirror within
   0.009 ppm, so A's 40 MHz reference runs 0.662 ppm (≈570 Hz at 861 MHz) below B's.
   Drift −0.002 / +0.006 ppm per 10 min, so there is no VCTCXO wander (F10). SNR ≥ 33 dB.
3. **The stored p25-httpd corrections match to 0.11 ppm.** A carries a manual
   `lo_shift_hz` +470 (`lo_ppm` −0.548) in `/mnt/jffs2/p25-ppm-cal.json`. B has no file
   and runs at `--lo-ppm 0`. Predicted B→A offset +0.548 ppm against +0.656 measured
   leaves a residual of +0.108 ppm (+93 Hz). This matches "A needs a larger correction,
   B about 0". The pair measurement cannot tell which board carries the 0.11 ppm. The
   likely explanations are that A's manual value is 0.11 ppm short, or that B sits about
   +0.11 ppm off true. An off-air check against the site's control channel would settle
   it. `rf.cw_ppm` now reads both files and fails when the residual exceeds 0.2 ppm.
   Only the difference of the two corrections is checked, so an error they share cancels.

## What was built

| Piece | Location |
|---|---|
| Design contract | `doc/HW_VALIDATION_SUITE.md` |
| Validation gateware (`hwval_core`): ring v2, production-replica ring, 2× AXI memory testers, clock census, ingest monitor with per-sample PRBS BER, CTRL_OUT event recorder, always-responding register bridge with snapshot CDC | `maia-hdl/hwval_hdl/`, tests `maia-hdl/test/test_hwval_*.py` |
| Vivado IP + project (HP0/HP3 enabled, DDS enabled, real `rx_clk` period, TX path timing-checked) | `maia-hdl/ip/hwval-core/`, `maia-hdl/projects/fishball7020_hwval/`, `build_fpga.bat --hwval` |
| Tezuka DT + SD dual-image layout | `tezuka_fw/board/tezuka/fishball7020/dts/fishball-hwval.dts(i)`, `post-image.sh` |
| On-board agent (static ARM binary) | `bench/agent/` |
| Host CLI + tests + analysis | `bench/fbench/`, `bench/tests_host/` |

## Follow-ups

- Tier 0 first runs on the current image: `smoke`, `sys.audit` on both boards,
  `store.sd_bench`, `mem.ps_memtest`, `iface.*`, `xport.p25_ring_prbs` (first real
  measurement of production ring loss).
- `hwval` bake (Andy) → Tier 1.
- `p25diag`: port ring v2 into P25; apply the air-time gating fix for finding 4; make the
  P25 defconfig build and load maia-kmod explicitly; swap the kmod invalidation order.
