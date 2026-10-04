# Hardware Validation Suite — `fbench` + `hwval` bitstream

**Status:** design + first build (2026-09-26). **Owner:** Andy (bench, bakes, flashes) +
Claude (code, analysis, CLI-driven test runs).

This document is the contract for the Fishball hardware validation work. It covers the
bench topology, the safety rules, the validation bitstream (`hwval`), the on-board agent
(`fbench-agent`), the host CLI (`fbench`), the test catalog, the ring-buffer redesign
and the memory-test design. Code that implements a piece of this document must use the
names defined here (register names, CLI verbs, JSON keys, test IDs) so the pieces fit
together.

## 1. Why this exists

The P25 work kept circling because every observation came through the web UI of the
same stack being debugged. The suite replaces that with direct, repeatable
measurements of the hardware underneath: two cabled Fishballs, deterministic stimulus,
self-checking data, and machine-readable results that Claude can run and analyse from
the CLI.

### 1.1 What the 2026-09-26 research sweep established

A 13-reader code/hardware sweep plus an adversarially verified ring audit (see
`doc/changes/053_hw_validation_bench.md` for the full evidence list). The findings that
shape this design:

| # | Finding | Status | Consequence |
|---|---|---|---|
| F1 | `gap_dibits=14336, overflows=1` (2026-05-03 forensics) is a tool artifact: one 4 KiB dibit sub-buffer (16384 dibits) minus the 2048-dibit API window. | Confirmed, 4/4 verifiers | Ring loss has **never actually been measured**. The suite measures it. |
| F2 | Every read of a `*_dma_status` register clears the Rsticky overflow bit that shares the word with `last_buffer`; the PS reads `last_buffer` first. Separately, `IQPacker` pulses overflow even when the old word is accepted in the same cycle (sim: 45 pulses for 20 words lost). | Confirmed (code + sim) | Production overflow telemetry is wrong in both directions. `hwval` measures loss as words in − words accepted. |
| F3 | No ring has lap detection: `last_buffer` is log2(N) bits, no sequence numbers in data. | Confirmed | Silent loss above (N-1) sub-buffers of reader stall (0.49 s for wideband at 8 MSPS). |
| F4 | Dibit rings publish only whole 4 KiB sub-buffers: dibits reach the PS in 3.41 s blocks, and the traffic TG/`locked` gate is applied at delivery time, not air time. | Confirmed; gone with change 079 (no dibit rings; the lane ring's packets carry sample indices) | Strong live-glitch candidate (call-tail truncation, cross-call bleed on same-freq re-grants). |
| F5 | Wideband packer has a 1-word holding register and the DMA caps at 5 un-acked bursts: ≈16 µs of write-latency tolerance at 32 MB/s (upstream recorder has ≈5× more). Simulation of the production blocks at 8 MSPS: no loss at 16.3 µs write-response latency, loss from 16.6 µs, 11 % lost at 19.2 µs, as silent whole-word gaps with contiguous addresses. A source that stops while an AW is accepted leaves the burst open indefinitely. | Structure confirmed, cliff reproduced in sim; field latency unmeasured | `hw.legacy_ring` measures field write latency against the cliff; `hw.contention` looks for the load that reaches it. |
| F6 | `sdr_reset` resets ring masters but not the HP1 interconnect; toggling `enable` splices the previous session into the next capture and can drop AWVALID without a handshake. | Confirmed | Ring v2 has quiesce + epoch semantics. |
| F7 | `p25_core` samples `util_wfifo` output every sampling clock and ignores `dout_valid`; libiio (cpack) honours it. | Mechanism real; whether valid ever gaps is unmeasured | `hwval` ingest monitor counts valid gaps. |
| F8 | `maia-sdr.ko` in the P25 image is a stale leftover of an old Maia build (upstream v0.10.0), not built by the P25 defconfig; it invalidates L1 before L2. | Confirmed | Suite audits module provenance; kmod fixes must also fix packaging. |
| F9 | FPGA banks 34/35 are 1.8 V on both schematics, but the XDC declares LVDS_25 + DIFF_TERM (2.5 V-only standards). | Unresolved in files | DMM check + eye-scan margin tests. |
| F10 | The 40 MHz reference looks like a VCTCXO whose control pin (`XTAL_VTC`, JP5 pin 15) is driven by a GPSDO core only in the stock Tezuka Maia bitstream; P25 builds leave it undriven. | Inferred | `rf.cw_ppm` / `hw.census` measure reference stability and wander. |
| F11 | OpenSDRLab TX outputs have PGA-102+ gain blocks (≈+15..+20 dBm possible). | Schematic text + board photos ("PGA1" parts on both TX paths, both units) | TX-attenuation interlock is mandatory (§3). |
| F12 | The shipped FSBL runs DDR3-1066 CL7/CWL6 at 533 MHz; the P25 XSA's `ps7_init` has CL11/CWL8; an overclock FSBL (600 MHz DDR, same timings) ships on the SD image. | Confirmed | `sys.audit` checks the live DDR registers every session. |
| F13 | HP1 is `axi_interconnect` 2.1 at 62.5 MHz (≈500 MB/s), not SmartConnect at 1.7 GB/s; HP0 and HP3 are unused. | Confirmed | `hwval` puts memory testers on HP0/HP3. |
| F14 | Both serials seen during the sweep are the **original (AD9361) unit**: `OVHNVI2FJEBXAB4M` is its Tezuka P25 SD card, whose last test firmware fails to come up (Windows RNDIS Code 10, iiod "Broken pipe", USB re-enumerating 10:33–10:36); `104473023196000bf5ff1a00aae12c3ca8` is the same board on its factory SD card (`plutosdr-fw v0.38`, 2R2T). The newer AD9363 unit was not connected. | Observed + Andy | The IIO serial follows the SD card, not the board. Units are identified by config label + `known_serials` + (on `hwval`) the 57-bit PL device DNA, never by IP. The failing P25 card is the first thing to diagnose (serial console on the DEBUG port). |
| F15 | Reading a vacant `p25_core` register bank (0x120–0x17F, 0x1E0–0x1FF) or a sync-domain bank while `sdr_reset=1` hangs the AXI-Lite bus. | Inferred from code; closed by change 079 (the radio core's bridge answers every address; its sync-domain banks read 0 while `sdr_reset` holds them) | Agent register access is allow-listed; `hwval` always responds. |
| F16 | The two boards differ only in transceiver: the newer unit has an **AD9363**, the older an **AD9361** (same die and register map; AD9363 is specified for 325 MHz–3.8 GHz LO and ≤20 MHz RF bandwidth, with looser RF specs). | Board photos (Andy) | Software cannot tell them apart, so `bench.toml` records `transceiver` per unit. RF tests run in both directions and report per unit; interface tests are identical. P25 at 860 MHz / 8 MHz is inside AD9363 limits. |
| F17 | DRAM on both boards is Micron FBGA code D9SHD = MT41K256M16TW-107: 4 Gb ×16 **DDR3L (1.35 V)**, DDR3-1866 bin, two parts = 1 GB ×32. | Board photos | Resolves the MT41J/MT41K doc conflict and matches the 1.349 V `vccoddr` reading. Running DDR3-1066 CL7 leaves ample margin; the overclock FSBL (CL7 at 600 MHz, tAA 11.7 ns < 13.125 ns) is out of spec. `sys.audit` also dumps the DDR IOB registers (SSTL135 vs SSTL15 configuration). |
| F18 | Both units are the OpenSDRLab variant: 20-pin JP5 header, FT2232HQ JTAG/UART on the DEBUG USB-C port, `EX_CLK`, `TX_LO`, `RX_LO` u.FL inputs. | Board photos | JP5 is available on both for later trigger/debug pins; the DEBUG port gives a serial console for soak logging and recovery. Per Andy's OpenSDRLab notes, `EX_CLK` reaches `FPGA_CLK` (K17, MRCC_35) only through R109 (33R, not populated): it is an FPGA clock input option, **not** a path into the AD9361 reference, so a shared-reference (coherent) bench needs rework. |
| F20 | **Live memory corruption risk on the P25 image.** U-Boot `initrd_high=0x20000000` puts the 24 MB ramdisk at ≈0x1E7x_xxxx–0x1FFF_FFFF, over the `p25-pre-diff-iq-dma@1f000000` carve-out. The kernel logs `Reserved memory: failed to reserve memory for node 'p25-pre-diff-iq-dma@1f000000'`, frees the initrd back to the page allocator (`/proc/iomem` shows 0x1C04_0000–0x1FFF_FFFF as System RAM), and p25-httpd still enables the pre-diff DMA (0x1A4 = 1), which writes ≈38 KB/s into 0x1F00_0000–0x1F03_FFFF. Any page Linux later puts there (page cache, heap, network buffers) gets overwritten. | Measured on unit A, 2026-09-26 (`sys.audit`) | Fix without a rebuild: `initrd_high=0x18000000` and `fdt_high=0x18000000` in the SD card's `uEnv.txt` (below the lowest carve-out 0x1900_0000); permanent fix in the Tezuka U-Boot environment. The `hwval` windows start at 0x2000_0000 and are unaffected. A plausible contributor to random instability (e.g. the overnight p25-httpd deaths); unproven as a glitch cause. |
| F21 | Reading the AFI (S_AXI_HP bridge) registers of an HP port whose PL clock is not running never completes: the first `sys.audit` read AFI0–3 on the P25 image (HP0/HP3 unclocked), the CPU stalled, and the system watchdog reset unit A (REBOOT_STATUS SWDT bit). | Observed, 2026-09-26 | The agent reads AFI registers only for ports named with `--afi-ports` (P25 image: 1,2; `hwval`: 0–3). Same class as F15: any PL-clocked register block must be known-clocked before a read. |
| F19 | What drives `XTAL_VTC` (40 MHz oscillator pin 1, also JP5 pin 15) is disputed: Andy's notes say RTL8211F CLKOUT (OE gating); the schematic text shows no net on PHY pin 35, and upstream Maia's fishball7020 design treats it as a VCTCXO tuning input fed from JP5. | Conflicting sources | Matters because the bench keeps the Ethernet PHY active (A↔B link): if PHY CLKOUT reaches the reference, Ethernet activity could modulate the AD9361 reference. `rf.refclk_eth` measures it (CW phase/spurs with link up, down, and under load); a scope on JP5 pin 15 settles it (DC level = tuning input, 25 MHz square wave = CLKOUT). |

## 2. Bench topology

### 2.1 Network

```text
PC 192.168.2.10 ──USB (RNDIS)── Unit A usb0 192.168.2.1
                                Unit A eth0 10.25.0.1/24 ──Cat5/6── Unit B eth0 10.25.0.2/24 (gw 10.25.0.1)
```

- Unit A forwards (`net.ipv4.ip_forward=1`, set by `fbench setup net` each session; not
  persistent). No NAT: the PC holds one route `10.25.0.0/24 via 192.168.2.1`.
- Unit B's USB side is moved to `192.168.12.1` (`fw_setenv ipaddr 192.168.12.1;
  fw_setenv ipaddr_host 192.168.12.10`) so its dead `usb0` cannot capture replies to
  192.168.2.10 and so it can be plugged in alone for recovery without colliding with A.
- All of B's services (SSH, HTTP 8080, iiod 30431) are then reachable at 10.25.0.2.
- Persistent settings live in the u-boot environment (`fw_setenv ipaddr_eth /
  netmask_eth / gateway_eth`), consumed by Tezuka `S40network`.
- USB 2.0 RNDIS (≈25–35 MB/s) is shared by both units, so tests arm → run autonomously
  on the board → collect. No host traffic crosses during a measurement window.
- The A↔B GbE link is also a deliberate load knob (`nc` streams exercise GEM DMA into
  DDR) and the coordination channel between the two agents.

### 2.2 RF cabling

```text
A.TX1A (JP1) ──[pad ≥20 dB]── B.RX1A (JP2)      primary A→B link
B.TX1A (JP1) ──[pad ≥20 dB]── A.RX1A (JP2)      reverse link (optional)
X.TX2A (JP3) ──[pad ≥20 dB]── X.RX2A (JP4)      per-board self loopback (optional, needs 2R2T image)
X.TX1A (JP1) ──[pad ≥20 dB]── X.RX1A (JP2)      self loop on one board (rf.freq_sweep --tx X --rx X)
```

`rf.freq_sweep` compares the two boards' receivers and transmitters by moving one padded
cable through B→A, B→B, A→A and A→B, so every comparison goes through the same cable and
pads (`bench/README.md`, "Frequency sweep").

The bench currently has one 20 dB pad per link: with the PGA-102+ TX (+20 dBm worst
case) a TX attenuation of 0 dB would put ≈0 dBm at the receiver, under the AD9361
absolute maximum but with little margin, so the interlock (§3) refuses any setting
above −10 dBm at the RX (TX attenuation < 10 dB). Two chained 20 dB pads (40 dB) are
preferred. Antennas are never connected while a TX test runs. Unused RF ports are terminated
(50 Ω) or left open; never connect TX directly to RX.

### 2.3 Storage

Each unit has a 64 GB microSD. Tezuka mounts `/dev/mmcblk0p1` (FAT) at `/mnt/sd`. The
same FAT partition may hold the boot files, so the suite **never reformats** and only
writes under `/mnt/sd/bench/`:

```text
/mnt/sd/bench/
  bin/fbench-agent          static ARM binary (musl), deployed by `fbench setup agent`
  share/*.json              register maps (hwval_regs.json, p25_regs.json, adi_regs.json)
  keys/authorized_keys      host key(s); linked into /root/.ssh by `fbench setup keys`
  images/p25/               BOOT.bin + devicetree.dtb (backup of the production image)
  images/hwval/             BOOT.bin + devicetree.dtb (validation image)
  stimulus/                 SigMF clips (P25 site replay, tones, C4FM known-dibit)
  runs/<run_id>/            per-run raw artifacts written on the board
```

FAT32 caps files at 4 GiB, so captures are written as ≤1 GiB SigMF segments. The
Zynq-7000 SD host runs High Speed (≤25 MB/s bus); sustained writes are expected at
10–20 MB/s, so captures faster than that go to reserved DDR first and are flushed after
the window (RAM-first capture). `store.sd_bench` measures the real number per card.

### 2.4 Boot images (dual image on SD)

The SD image is `BOOT.bin` (FSBL + bitstream + U-Boot), `uImage`, `uramdisk.image.gz`,
`devicetree.dtb`, `uEnv.txt`. The bitstream lives inside `BOOT.bin`. The `hwval` image
reuses the production kernel and rootfs; only `BOOT.bin` and `devicetree.dtb` change.

`fbench boot <unit> hwval|p25|status` swaps those two files from
`/mnt/sd/bench/images/<name>/` with sha256 verification, `sync`, then reboots. The
production pair is backed up to `images/p25/` before the first swap. Recovery if a unit
fails to boot: pull the card, copy `images/p25/*` to the card root on the PC.
Precondition (checked by `sys.identity`): the unit boots from SD (`/proc/cmdline`
contains `uio_pdrv_genirq.of_id`, the SD-boot bootargs marker).

On the `hwval` image the scanner cannot take the radio core: `RadioCore::take` wants the
product ID "rad1". That is intended.

## 3. Safety rules (enforced by the CLI)

1. **Cabled-only TX.** Every test that transmits requires `rf.cabled_confirmed = true`
   in the bench config and a link entry for the TX port; otherwise exit code 4.
2. **Level interlock.** For each TX test the CLI computes the worst-case power at the
   receiving port: `P_rx = P_tx_max − tx_atten − pad_db`, with `P_tx_max = +20 dBm`
   (AD9361 +7 dBm plus PGA-102+ gain; configurable per unit). It refuses when
   `P_rx > rx_abs_max_dbm` (default −10 dBm; AD9361 absolute max is higher, the margin is
   deliberate) and warns above `rx_linear_max_dbm` (default −30 dBm).
3. **TX attenuation floor.** Before enabling any TX source the agent writes the TX
   attenuation first (`out_voltage0_hardwaregain`), then the source, and on exit
   restores max attenuation (−89.75 dB) and stops the source, even on error (the agent
   installs a cleanup path and the host re-issues `tx off` after every TX test).
4. **Maintenance mode.** Tests that replace or corrupt the RX stream (BIST PRBS/tone,
   loopback, digital tune, delay sweeps, LO sweeps) stop the radio daemon first (the
   scanner, `/etc/init.d/S60scanner stop`) and restart it
   afterwards. The run record notes it.
5. **Register allow-lists.** The agent refuses reads/writes outside the per-core maps in
   `share/*.json` (offsets above the radio core's 1 KB window alias, and its sync-domain banks
   read 0 while `sdr_reset` holds them).
6. **No destructive storage ops.** Only `/mnt/sd/bench/**` and files the agent created
   are written or deleted. Boot-file swaps keep verified backups.

## 4. Architecture

```text
 ┌──────────────── host (Windows, this repo) ────────────────┐
 │ fbench CLI (Python)                                        │
 │  config/bench.toml ─ units, links, pads, safety limits     │
 │  runner ─ test registry, suites, run dirs, FINDINGS.md     │
 │  transports ─ SSH (OpenSSH, BatchMode) · HTTP · libiio     │
 │  analysis ─ numpy/scipy on pulled artifacts                │
 └───────────────┬────────────────────────────────────────────┘
                 │ ssh unit 'fbench-agent <cmd> --json'
 ┌───────────────▼──────── each Fishball ────────────────────┐
 │ fbench-agent (Rust, static musl, /mnt/sd/bench/bin)        │
 │  reg/audit/telemetry/eyescan/ringcheck/memtest/sdbench/…   │
 │  /dev/mem · UIO · /dev/<rxbuffer> · IIO sysfs + debugfs    │
 └───────────────┬────────────────────────────────────────────┘
                 │ AXI-Lite / DMA
 ┌───────────────▼──────── PL ───────────────────────────────┐
 │ P25 image:  axi_ad9361 + IIO DMA (HP2) + p25_core (HP1)    │
 │ hwval image: axi_ad9361 (+DDS) + IIO DMA (HP2)             │
 │              + hwval_core: ring v2 + legacy ring (HP1),    │
 │                memtesters (HP0, HP3), census, ingest, evt  │
 └────────────────────────────────────────────────────────────┘
```

### 4.1 Tiers

- **Tier 0 (`base`)** runs on the current production image. It uses the ADI built-in test
  hardware (AD9361 BIST PRBS/tone/loopback, `axi_ad9361` PN monitors, IDELAY registers,
  `CLK_FREQ` counter, DAC pattern/PN sources), XADC, the IIO DMA path, the P25 wideband
  ring read directly by the agent, and PS-side memory/SD/network tests. No bake needed.
- **Tier 1 (`hwval`)** needs the validation image (§6): true BER counters, clock census,
  ring v2 and an instrumented replica of the production ring, AXI memory testers on
  HP0/HP3, ingest monitor, CTRL_OUT event recorder.
- **Tier 2 (`p25diag`, future)** ports the ring v2 and instrumentation into the P25 build
  once Tier 1 has proven them.

## 5. CLI and IO contract

### 5.1 Host CLI (`fbench`)

Invocation: `python bench/fbench.py <verb> …` (or `fbench` after `pip install -e bench`).
Every verb accepts `--json` (machine output on stdout, logs on stderr), `--config PATH`
(default `bench/config/bench.toml`), `--timeout S`, `-v`.

| Verb | Purpose |
|---|---|
| `units [--probe]` | List configured units; `--probe` pings, reads IIO `hw_serial`/`fw_version`, SSH reachability, agent version, image type. |
| `setup net\|keys\|agent\|all [--unit U] [--password-prompt]` | One-time and per-session setup (routes, forwarding, key install, agent deploy, share files). Only verb that may ask for a password. |
| `list [--tier T] [--suite S]` | Test catalog. |
| `describe <test>` | Parameters, requirements, pass criteria, artifacts. |
| `run <test\|suite> [--unit U] [--tx U --rx U] [-p key=val …] [--duration S]` | Run; writes a run dir; exit code = verdict. |
| `status` | Last runs, units' current image, session state (maintenance mode, TX state). |
| `analyze <run_dir>` | Re-run analysis offline on pulled artifacts (no hardware). |
| `compare <run_dir> <run_dir> …` | Metric deltas between runs (regression gate). |
| `boot <unit> hwval\|p25\|status` | Dual-image swap (§2.4). |
| `safety [--tx U --rx U --tx-atten DB]` | Print the link budget and whether it passes the interlock. |
| `agent <unit> -- <agent args>` | Raw passthrough to `fbench-agent` (JSON out). |
| `reg <unit> <core> read\|write <REG> [value]` | Allow-listed register access via the agent. |
| `tx <unit> off` | Emergency: max attenuation + all TX sources off. |

Exit codes: `0` pass · `1` fail · `2` error (bug/exception) · `3` precondition
(unreachable unit, wrong image, missing capability) · `4` safety refusal ·
`5` inconclusive (ran, but the result cannot support a verdict).

### 5.2 Run directory and `result.json`

Run dirs follow the diagnostic run-dir convention:
`runs/bench/<YYYY-MM-DD>/bench/run_<YYYYMMDD_HHMMSS>_<test_id>/`
(gitignored), containing `result.json`, `FINDINGS.md` (auto-generated, then editable),
`params.json`, `units.json` (identity snapshot per unit), `log.txt`, and `artifacts/`.

```json
{
  "schema": "fbench.result/1",
  "test": "iface.eye_idelay",
  "run_id": "20260926_153012_iface.eye_idelay",
  "started": "2026-09-26T15:30:12-04:00",
  "ended": "2026-09-26T15:34:40-04:00",
  "verdict": "pass",
  "summary": "RX eye centre tap 17 (lane min window 11 taps = 0.86 ns) at 61.44 MSPS",
  "units": {"A": {"serial": "…", "image": "p25", "build": "…"}},
  "params": {"rates_hz": [61440000], "dwell_ms": 10},
  "metrics": {"window_taps_min": 11, "centre_tap": 17},
  "thresholds": {"window_taps_min": {"min": 6}},
  "maintenance_mode": true,
  "tx_used": false,
  "artifacts": ["artifacts/eye_61M44.json", "artifacts/eye_61M44.png"],
  "warnings": [],
  "errors": []
}
```

### 5.3 On-board agent (`fbench-agent`)

Static ARM binary, no runtime dependencies. Every subcommand prints exactly one JSON
object to stdout (`{"ok": true, …}` or `{"ok": false, "error": "…"}`) unless
`--jsonl` streaming is requested; exit code 0 on `ok`. Long operations write bulk data
to files under `/mnt/sd/bench/runs/<run_id>/` and return the paths.

| Subcommand | Summary |
|---|---|
| `version` | Agent version, build info. |
| `info` | Identity: model, serial (IIO context `hw_serial`), `/proc/cmdline`, uptime, boot id, kernel, bitstream ID (reads known product-ID registers safely), boot medium, SD usage, loaded modules, running services. |
| `audit` | PS config audit (SLCR PLLs, DDRC mode registers and priorities, PL310 control/prefetch, AFI, reboot status, reserved-memory vs `/proc/iomem`), with expected-value checks. |
| `reg read\|write\|dump --core C [--reg R] [--value V]` | Allow-listed register access (`p25`, `adi_adc`, `adi_dac`, `rx_dmac`, `tx_dmac`, `hwval`, `slcr`, `ddrc`, `l2c`). |
| `telemetry --seconds N --interval-ms M [--jsonl FILE]` | XADC temp/rails, AD9361 temp, `CLK_FREQ`, loadavg, per-CPU softirq/irq deltas, `/proc/interrupts` deltas, MemAvailable. |
| `iio attr get\|set --dev D [--chan C --out] --attr A [--value V]` | IIO sysfs access. |
| `iio debug get\|set --dev D --attr A [--value V]` | debugfs access (`bist_prbs`, `bist_tone`, `loopback`, `bist_timing_analysis`, `digital_tune`, `direct_reg_access`). |
| `ad9361 spi read\|write --addr A [--value V]` | AD9361 SPI register via debugfs `direct_reg_access`. |
| `eyescan --mode idelay\|ad9361\|2d --rate HZ --dwell-ms D [--lanes …]` | Runs the PRBS sweep loops on the board; returns pass/fail grids and window/centre per lane. |
| `prbs soak --seconds N --poll-ms P` | Error-interval counting with the ADI PN monitor. |
| `txlink --mode ad9361-loopback\|fpga-loopback [--sweep]` | TX LVDS link test with DAC PN. |
| `ring capture --ring p25-wideband\|hwval-v2\|hwval-legacy --bytes B --mapping cached\|uncached --out FILE` | Raw ring capture (RAM-first), with per-sub-buffer metadata sidecar. |
| `ring check --ring … --pattern pn0fn\|ramp64\|tagged\|iqramp\|tone --seconds N [--stall-ms …]` | Live streaming checker; classifies anomalies (lap, torn, word gap, stale line, splice, bit error). |
| `mem test --anon-mb N \| --phys ADDR --size S --patterns … --passes P [--cpu C]` | PS memory test (walking 1/0, address, inverse, March C−, PRBS, checkerboard). |
| `mem canary fill\|verify --region NAME` | Carve-out foreign-writer canary via `/dev/mem`. |
| `mem bw --size S` | memcpy/read/write bandwidth: cached anon, uncached `/dev/mem`, reserved region. |
| `sd bench --mb N --bs K [--fsync]` | SD write/read throughput and per-write latency histogram. |
| `net serve\|send --port P --mb N` | TCP throughput between agents (A↔B) or to the host. |
| `hwval id\|census\|ingest\|ringv2\|legacy\|mt\|evt …` | Tier 1 operations driven by `share/hwval_regs.json`. |
| `replay stream\|check\|verify --playlist P …` | SD/RAM relay for `rf.p25_corpus`: staged IQ files (`cs16`/`cs12`/`cs8`) through a RAM ring (default 192 MiB) into int16 on stdout for `iio_writedev`; prefill, underrun and read-stall counters in `--status`/`--report`. |
| `tx off` | Max attenuation, DAC sources off, DDS scale 0, loopbacks off. |

## 6. `hwval` validation bitstream

### 6.1 Block design

Built in-tree at `maia-hdl/projects/fishball7020_hwval/` exactly like the P25 project:
source `../pluto/system_bd.tcl` with `fishball`, `LVDS_ENABLE`, `maia_iio`, `with_tx_fir`,
`with_rx_fir_maia`; delete `maia_sdr` and its helpers (same surgery as P25); then:

- `axi_ad9361`: `DAC_DDS_DISABLE=0` (enables the DDS tone generator for TX stimulus).
  Everything else as P25 (LVDS, 1R1T, `ADC_INIT_DELAY=30`).
- PS7: enable `S_AXI_HP0` and `S_AXI_HP3` (64-bit).
- `hwval_core` (Amaranth IP, `fishball-hwval:hwval_core_default:hwval_core:0.1.0`):
  - AXI-Lite at **0x7C46_0000** (4 KiB), IRQ `sys_concat_intc/In11` (DT SPI 55,
    `uio_pdrv_genirq`, UIO name `hwval-core`).
  - `m_axi_ringv2`, `m_axi_legacy` → HP1 interconnect, `clk_out1` (62.5 MHz).
  - `m_axi_mt0` → HP0, `m_axi_mt1` → HP3, both on `clk_out2` (125 MHz).
  - Clocks: `s_axi_lite_clk`=FCLK0 100 MHz, `clk`=`clk_out1` 62.5 MHz (sync),
    `clk2x_clk`=`clk_out2` 125 MHz (mem), `clk3x_clk`=`clk_out3` 187.5 MHz,
    `sampling_clk`=`util_ad9361_divclk/clk_out`, `lclk_clk`=`axi_ad9361/l_clk`,
    `fclk1_clk`=FCLK1 200 MHz (`sys_200m_clk`), `y1_clk`=50 MHz oscillator on pin N18.
  - Data: `re_in`/`im_in` (12-bit slices as P25), `valid_in`=`util_ad9361_adc_fifo/dout_valid_0`,
    `ctrl_out[7:0]` (AD9361 CTRL_OUT, teed from `gpio_status` in `system_top.v`),
    `ad_clkout` (AD9361 CLK_OUT on pin R16, sampled as data in `clk3x`).
- New top-level pins: N18 (`y1_clk`, LVCMOS25 input — bank 34 must stay one IO
  standard family, see F9) and R16 (`ad_clkout`, LVCMOS25 input). JP5 debug pins are
  not driven in v1.
- Constraints: `rx_clk` period **8.138 ns** (real 1R1T LVDS maximum, 122.88 MHz DATA_CLK)
  instead of 4 ns; `y1_clk` 20 ns; the blanket `axi_ad9361/inst/i_tx/*` false path is
  **removed** so the TX path used by stimulus tests is timing-checked; timing failure is
  a hard build failure for `hwval` (no `bad_timing` promotion).

### 6.2 DDR windows (device tree, static `no-map`)

All above `0x2000_0000`, clear of U-Boot's `initrd_high`/`fdt_high` (0x2000_0000) and
below the CMA/lowmem top.

| Window | Base | Size | DT node | Used by |
|---|---|---|---|---|
| `hwval-ringv2` | 0x2000_0000 | 16 MiB | `maia-sdr,rxbuffer` 16 × 1 MiB | ring v2 (runtime base/size inside) |
| `hwval-legacy` | 0x2200_0000 | 16 MiB | `maia-sdr,rxbuffer` 16 × 1 MiB | legacy ring (fixed base, same as P25 wideband) |
| `hwval-memtest` | 0x2400_0000 | 64 MiB | reserved only | memtesters mt0/mt1 |

Every `hwval` master has a hardware address guard: bursts outside
`[GUARD_LO, GUARD_HI)` are not issued and are counted. The agent programs the guard from
`/proc/device-tree/reserved-memory` at session start and sets `GUARD_LOCK`.

### 6.3 Register access rules

- All registers live in the AXI-Lite (100 MHz) domain; the bridge **always responds**.
  Unmapped addresses read `0xDEADBEEF`; writes to them are ignored. No read has side
  effects except `EVT_POP` style registers, which are explicit write pulses.
- Configuration crosses into other domains as quasi-static values: change a block's
  configuration only while that block is disabled/idle.
- Status from other domains is read through a **snapshot**: write the domain mask to
  `SNAP_REQ`, poll `SNAP_ACK` until it equals the mask (timeout 10 ms ⇒ that domain's
  clock is dead — report it, do not hang), then read the snapshot registers.
- `RINGV2_COMMITTED_BURSTS` is additionally gray-code synchronized so it can be read at
  any time without a snapshot (used by the streaming reader).
- Counters never clear on read. They clear on the block's `CLEAR` command, saturate at
  2^32−1, and 64-bit values are `_LO`/`_HI` pairs taken from the same snapshot.
- Snapshot domain bits: bit0 `sync`, bit1 `mem`, bit2 `sampling`.

### 6.4 Register map (names are the contract; offsets are generated)

`hwval_hdl/hwval_top.py` is the single source of truth. It emits
`hwval_regs.json` (consumed by the agent and CLI), `hwval.svd`, and
`doc/hwval_register_map.md`. Blocks are 256 bytes (64 words) each.

**`id` (0x000)** — `ID` (RO, `0x68777631` "hwv1"), `VERSION` (RO, major<<16|minor<<8|patch),
`FEATURES` (RO bitmask: bit0 ringv2, 1 legacy, 2 mt0, 3 mt1, 4 census, 5 ingest, 6 prbs,
7 evt, 8 y1_clk, 9 dna), `DNA_LO`/`DNA_HI`/`DNA_STATUS` (RO, 57-bit PL device DNA from
`DNA_PORT` — the firmware-independent board identity), `SCRATCH` (RW), `CORE_RESET` (RW, default 0; holds all non-AXI-Lite
domains in reset while 1), `SNAP_REQ` (W, domain mask → pulse), `SNAP_ACK` (RO, domain
mask of completed snapshots for the last request), `SNAP_SEQ` (RO), `TS_LO`/`TS_HI` (RO,
sync-domain 64-bit timestamp, snapshot), `IRQ_PENDING` (RO), `IRQ_ENABLE` (RW),
`IRQ_CLEAR` (W, write-1-to-clear), `IRQ_COUNT` (RO), `GUARD_LO`/`GUARD_HI` (RW until
locked), `GUARD_LOCK` (RW, set-once until `CORE_RESET`). IRQ bits: 0 ringv2, 1 legacy
sub-buffer, 2 mt0 done, 3 mt1 done, 4 census done, 5 evt non-empty.

**`census` (0x100)** — `CENSUS_CTRL` (W bit0 start), `CENSUS_GATE` (RW, gate length in
100 MHz cycles), `CENSUS_STATUS` (RO bit0 busy, bit1 done), `CENSUS_GATE_ACTUAL`,
`CENSUS_SYNC`, `CENSUS_MEM`, `CENSUS_CLK3X`, `CENSUS_SAMPLING`, `CENSUS_LCLK`,
`CENSUS_FCLK1`, `CENSUS_Y1`, `CENSUS_CLKOUT` (edges of AD9361 CLK_OUT sampled in clk3x).
Counts are edges during the gate; `f = count / GATE_ACTUAL × 100 MHz`.

**`ingest` (0x200, sampling domain)** — `INGEST_CTRL` (RW: bit0 stats_enable,
bit1 prbs_enable, bit2 prbs_mode (0 = AD9361 BIST PRBS `pn0fn`, 1 = PN9/PN11),
bit3 honor_valid, bit4 clear_on_snap), `INGEST_CMD` (W bit0 clear), `SAMPLES_LO/HI`,
`VALID_GAP_CYCLES`, `VALID_GAP_RUNS`, `CDC_WRERR` (sampling→sync FIFO overflows),
`I_MIN`, `I_MAX`, `Q_MIN`, `Q_MAX` (sign-extended), `I_SUM_LO/HI`, `Q_SUM_LO/HI`,
`I_SUMSQ_LO/HI`, `Q_SUMSQ_LO/HI`, `CLIP_COUNT`, `I_OR_MASK`, `I_AND_MASK`, `Q_OR_MASK`,
`Q_AND_MASK`, `WIN_SAMPLES_LO/HI`, `PRBS_CHECKED_LO/HI`, `PRBS_ERRORS`, `PRBS_OOS_EVENTS`,
`PRBS_STATUS` (bit0 in_sync).

**`legacy` (0x400, sync domain)** — production replica: `radio_core.IQPacker` →
`maia_hdl.DmaStreamRingWrite` at fixed base 0x2200_0000, 16 × 1 MiB, observed by
non-invasive taps. `LEGACY_CTRL` (RW: bit0 dma_enable, bits[2:1] src (0 off, 1 sample
ramp, 2 live rxiq)), `LEGACY_CMD` (W bit0 clear), `LEGACY_RATE_INC` (sample strobe rate =
inc / 2^32 × 62.5 MHz), `LEGACY_BASE`, `LEGACY_SIZE`, `LEGACY_NUM_BUFFERS` (RO consts),
`LEGACY_LAST_BUFFER`, `LEGACY_NEXT_ADDRESS`, `LEGACY_PACKER_OVF` (overflow pulses while
enabled), `LEGACY_PACKER_OVF_DISABLED`, `LEGACY_WORDS_IN`, `LEGACY_WORDS_ACCEPTED`,
`LEGACY_AW`, `LEGACY_B`, `LEGACY_BRESP_ERR`, `LEGACY_SUBBUF_DONE`, `LEGACY_STALL_CYCLES`,
`LEGACY_MAX_STALL`, `LEGACY_MAX_OUTSTANDING`, `LEGACY_LAT_MAX`, `LEGACY_HIST_SEL` (RW),
`LEGACY_HIST_VAL`, `LEGACY_GEN_LO/HI`.

**`ringv2` (0x500, sync domain)** — §7. `RINGV2_CTRL` (RW: bit0 enable, bit1 protect,
bit2 header, bits[5:3] src (0 off, 1 ramp64, 2 tagged, 3 prbs31, 4 live rxiq),
bit8 irq_threshold, bit9 irq_timer), `RINGV2_CMD` (W: bit0 soft_reset, bit1 flush,
bit2 clear), `RINGV2_BASE`, `RINGV2_SIZE_BURSTS`, `RINGV2_SUBBUF_BURSTS`,
`RINGV2_IRQ_EVERY`, `RINGV2_IRQ_TIMEOUT`, `RINGV2_FLUSH_TIMEOUT`, `RINGV2_MAX_OUTSTANDING`,
`RINGV2_RATE_INC`, `RINGV2_CONSUMER_BURSTS` (RW), `RINGV2_COMMITTED_BURSTS` (RO, live),
`RINGV2_STATUS` (RO live: bit0 idle, bit1 enabled, bit2 fifo_empty), snapshot counters
`RINGV2_ISSUED_BURSTS`, `RINGV2_WORDS_IN_LO/HI`, `RINGV2_DROP_FULL`, `RINGV2_DROP_PROTECT`,
`RINGV2_PAD_WORDS`, `RINGV2_FLUSHES`, `RINGV2_BRESP_ERR`, `RINGV2_FIFO_HWM`,
`RINGV2_MAX_OUTSTANDING_SEEN`, `RINGV2_LAT_MAX`, `RINGV2_HIST_SEL` (RW), `RINGV2_HIST_VAL`,
`RINGV2_EPOCH`, `RINGV2_HEADERS`, `RINGV2_GEN_LO/HI`, `RINGV2_GUARD_BLOCKED`.

**`mt0` (0x600) / `mt1` (0x700, mem domain)** — §8. Prefix `MT_` (the JSON carries the
block name): `MT_CTRL` (RW: bits[2:0] mode, bits[6:3] pattern, bits[11:7] burst_len,
bits[15:12] max_outstanding, bit16 stop_on_error), `MT_CMD` (W: bit0 start, bit1 abort,
bit2 clear), `MT_BASE`, `MT_SIZE`, `MT_PASSES`, `MT_IDLE_CYCLES`, `MT_SEED`,
`MT_STATUS` (RO live: bit0 busy, bit1 done, bit2 error), snapshot counters
`MT_PASS_COUNT`, `MT_BYTES_WR_LO/HI`, `MT_BYTES_RD_LO/HI`, `MT_CYCLES_LO/HI`,
`MT_ERR_COUNT`, `MT_FIRST_ERR_ADDR`, `MT_FIRST_ERR_EXP_LO/HI`, `MT_FIRST_ERR_ACT_LO/HI`,
`MT_ERR_LANES_LO/HI`, `MT_BRESP_ERR`, `MT_RRESP_ERR`, `MT_WLAT_MAX`, `MT_RLAT_MAX`,
`MT_HIST_SEL` (RW: bit4 0=write/1=read, bits[3:0] bin), `MT_HIST_VAL`, `MT_GUARD_BLOCKED`.

**`evt` (0x800)** — CTRL_OUT recorder. `EVT_CTRL` (RW: bit0 enable, bits[15:8] mask),
`EVT_LEVEL` (RO FIFO level), `EVT_DATA` (RO head: bit35 heartbeat, bits[34:27] value,
bits[26:0] timestamp in 62.5 MHz cycles, low 27 bits; exposed as `EVT_DATA_LO`/`EVT_DATA_HI`),
`EVT_POP` (W pulse), `EVT_OVERFLOWS`, `EVT_CURRENT` (live CTRL_OUT), heartbeat record
every 2^26 cycles so the host can unwrap timestamps.

## 7. Ring buffer v2 (the redesign)

Ring v2 fixes F2–F6 and is written to replace `DmaStreamRingWrite` + packers in P25 once
Tier 1 proves it.

**Producer (HDL):**

- Input: 64-bit words with `valid`; the source is never stalled (real-time data).
- BRAM FIFO (default 2048 × 64 = 16 KiB, ≈0.5 ms at 32 MB/s, ≈30× the current
  tolerance). If the FIFO is full the word is dropped and `DROP_FULL` counts it.
- Store-and-forward: an AW is issued only when a whole 16-beat burst (128 B) is in the
  FIFO, so no master holds the interconnect waiting for data (F13 head-of-line risk).
- AXI hygiene: AW/W VALID are registered and held until handshake; `enable` takes effect
  at burst boundaries; disabling drains in-flight bursts; `soft_reset` is accepted only
  when idle; BRESP≠OKAY is counted (`BRESP_ERR`).
- Runtime `BASE` (4 KiB aligned) and `SIZE_BURSTS` (≥2, any value; wrap by compare, not
  mask), up to 8 outstanding bursts.
- **Committed pointer:** `COMMITTED_BURSTS` is a 32-bit monotonic count of bursts whose
  write response has returned, gray-synchronized for lock-free reads (2^32 bursts =
  512 GiB before wrap). It advances on every response, including SLVERR/DECERR, so the
  reader's pointer arithmetic stays aligned with the ring; error responses are counted in
  `BRESP_ERR`, and a non-zero `BRESP_ERR` invalidates the run.
- Flush: after `FLUSH_TIMEOUT` idle cycles with a partial burst in the FIFO (or on
  `CMD.flush`), the burst is padded with pad words
  `{0xF1B1, drop_count[15:0], word_count[31:0]}` and written (`PAD_WORDS`, `FLUSHES`).
  This bounds delivery latency for low-rate streams (dibits: one 128 B burst =
  512 dibits ≈ 107 ms) instead of 3.41 s sub-buffers (F4).
- Optional header (`CTRL.header`): the first word of every `SUBBUF_BURSTS`-burst
  sub-buffer is `{0xF1B0, drop_count[15:0], word_count[31:0]}`, so any capture is
  self-describing about gaps even without a test pattern.
- Modes: overwrite (default; reader detects laps) or **protect** (`CTRL.protect`): the
  producer never writes more than `SIZE_BURSTS − 1` bursts past `CONSUMER_BURSTS` (the
  PS-written consumed count) and drops + counts (`DROP_PROTECT`) instead.
- IRQ: every `IRQ_EVERY` committed bursts and/or `IRQ_TIMEOUT` cycles after the last
  IRQ if new bursts are committed (coalescing with a latency bound).

**Consumer protocol (PS):**

```text
R = consumed bursts (u64, reader-owned)   N = SIZE_BURSTS   M = MAX_OUTSTANDING (≤ 8)
S = N − 1 − M                             # safe window: up to M bursts past COMMITTED
                                          # may already be writing into ring slots
loop:
  wait IRQ (or timeout)                   # no lost wakeups: re-read W each wake
  W = COMMITTED_BURSTS (u32, extend to u64)
  avail = W − R
  if avail > S:                           # lapped (overwrite mode)
      lost = avail − S; emit GAP(lost); R = W − S
  invalidate + copy bursts [R, W) (wrapping)    # at burst granularity, not sub-buffer
  W2 = COMMITTED_BURSTS
  drop copied bursts k with W2 + M ≥ k + N, emit GAP   # torn-copy check
  R = W; write CONSUMER_BURSTS = R (protect mode)
```

After a disable drains to idle, `WORDS_IN == data words written + DROP_FULL +
DROP_PROTECT` exactly; tests assert it. Pad and header words carry the index of the next
data word in bits [31:0], so a checker can realign after any marker.

Readers must invalidate L2 before L1 (Linux device-to-CPU order) or use an uncached
mapping; the agent measures both (`mem.cache_coherency`).

## 8. Memory testing

Three layers, from pure hardware up:

1. **PL AXI testers (`hwval` mt0 on HP0, mt1 on HP3, 125 MHz, ≈1 GB/s per direction
   each).** Modes: 0 write-only, 1 read-verify, 2 write-then-verify, 3 read-only
   (bandwidth), 4 byte-lane (DM lines: fill zeros, then ones with a rotating single-byte
   WSTRB, verify). Patterns: 0 address `{addr, ~addr}`, 1 walking-1, 2 walking-0,
   3 checkerboard, 4 PRBS (xorshift hash of address and seed), 5 all-0, 6 all-1,
   7 toggle (max simultaneous switching). Burst lengths 1/2/4/8/16 (aligned, never
   cross 4 KiB), 1–8 outstanding, `IDLE_CYCLES` throttle for aggressor duty cycle.
   Counters: bytes, cycles, errors, first-error address/expected/actual, OR of error
   lanes (points at a DQ line), BRESP/RRESP errors, write (AW→B) and read (AR→RLAST)
   latency max and log2 histograms.
2. **PS tests (agent).** `mem test` on anonymous memory (pinned CPU, walking bits,
   address/inverse, March C−, PRBS) and on reserved windows through `/dev/mem`;
   `mem canary` for foreign writers; `mem bw` for cached vs uncached bandwidth.
3. **System tests.** Ring integrity (§7), contention matrix (PS memcpy × SD writes ×
   IIO DMA × PL aggressor duty × spectrometer), and `sys.audit` of the live DDR
   configuration (F12). A pre-Linux full-array test (U-Boot `mtest` or the Vitis DRAM
   test over JTAG) is documented as an optional deeper step.

Pass criteria: zero data errors in every mode; write latency max under the ring
tolerance at the tested load; bandwidth within 10 % of the 64-bit × clock ceiling for
long bursts on an idle system.

## 9. Test catalog

`fbench list` is authoritative; this table is the initial set. "M" = maintenance mode.

| ID | Tier | Units | M | What it measures | Pass criteria (default) |
|---|---|---|---|---|---|
| `sys.identity` | 0 | any | | serial, image, bitstream ID, boot medium, SD layout, agent version | identity readable; roles match config |
| `sys.audit` | 0 | any | | PLLs, DDR mode/timing, DDRC priorities, PL310 prefetch, reserved memory vs `/proc/iomem`, kmod provenance, leftover services, reboot status | live values match the expected table |
| `sys.telemetry` | 0 | any | | XADC temp/rails, AD9361 temp, `CLK_FREQ`, IRQ/softirq rates | rails within ±5 %, temps < 85 °C |
| `sys.soak` | 0 | any | | telemetry + error events over hours; periodicity search (10 s dropout) | no unexplained periodic events |
| `iface.clk_freq` | 0 | any | | DATA_CLK frequency vs 2×fs | within 1 kHz resolution of expected |
| `iface.prbs_soak` | 0 | any | M | AD9361 BIST PRBS through LVDS, PN error intervals over time/rate | 0 error intervals |
| `iface.eye_ad9361` | 0 | any | M | AD9361 clock/data delay 16×16 pass grid per rate; boot-chosen delays | chosen point ≥3 steps from window edge |
| `iface.eye_idelay` | 0 | any | M | FPGA IDELAY 0–31 per lane × AD9361 delay, 2-D eye per rate | ≥6-tap window on every lane at 61.44 MSPS |
| `iface.tx_link` | 0 | any | M | DAC PN via AD9361 digital loopback; TX delay sweep; clock polarity | 0 PN errors at the chosen TX delay |
| `iface.fpga_loopback` | 0 | any | M | FPGA DAC→ADC internal loopback with PN | 0 errors |
| `xport.iio_capture` | 0 | any | | reference libiio capture: sample count vs wall clock, gaps | count within 0.01 % of expected |
| `xport.p25_ring_prbs` | 0 | any | M | AD9361 PRBS (or BIST tone) through the **production** wideband ring, read by the agent; anomaly classes | 0 anomalies at nominal reader timing |
| `xport.p25_ring_lap` | 0 | any | M | same with injected reader stalls; proves lap blindness and measures threshold | loss onset at (N−1)×T_buf |
| `mem.ps_memtest` | 0 | any | | PS memtest on N MB anonymous memory | 0 errors |
| `mem.canary` | 0 | any | | foreign writes into disabled carve-outs under load | canary intact |
| `mem.ps_bw` | 0 | any | | cached/uncached memcpy bandwidth | report only |
| `store.sd_bench` | 0 | any | | SD sequential write/read MB/s, write-latency p50/p99/max, fsync time | report; flags < 8 MB/s |
| `net.link` | 0 | A,B | | ping RTT and TCP throughput host↔A, A↔B | report |
| `rf.cw_ppm` | 0 | tx,rx | | CW frequency offset between boards, drift over time; cross-check against the difference of the stored p25-httpd corrections (`/mnt/jffs2/p25-ppm-cal.json`, absent = 0) | report ppm; drift < 0.5 ppm/10 min; calibration residual ≤ 0.2 ppm when either unit has a stored correction |
| `rf.level_sweep` | 0 | tx,rx | | TX attenuation sweep: RSSI, ADC power, clipping, linearity | monotonic, slope 1 dB/dB ±0.5 |
| `rf.spur_scan` | 0 | any | | noise floor and spurs with TX off/terminated | report |
| `rf.isolation` | 0 | tx,rx | | leakage with the cable removed | ≥ 60 dB below cabled level |
| `rf.freq_sweep` | 0 | tx,rx | M | One cabled direction across the tuning range (default 70 MHz–6 GHz, 84 points, log-spaced below 1.1 GHz and 100 MHz apart above, with the AD9363's 325 MHz and 3.8 GHz edges). The TX LO follows 1 MHz above the RX LO and the tone sits 0.5 MHz above the TX LO, so tone, TX LO leakage, TX image, RX image and DC land on separate frequencies. Every stimulus method the TX unit supports (dds, pattern, cyclic) sweeps in turn at fixed RX gain. Per point: RX and TX synthesizer lock bits (SPI 0x247/0x287), LO read-back, tone level and SNR, TX vs RX reference offset, RX image, TX LO leakage and TX image after a TX quadrature calibration, strongest spur, RSSI. `--tx A --rx A` is a self loop. `compare=<run dirs>` overlays runs and plots the RX difference of runs sharing a TX unit and the TX difference of runs sharing an RX unit | every point tunes, both synthesizers lock, LOs read back as set, tone found (SNR ≥ 10 dB), tone frequency on the reference offset; the rest reported |
| `rf.refclk_eth` | 0 | tx,rx | | RX-side CW phase continuity, frequency and spurs (25/125 MHz products) with the RX unit's Ethernet link up, down, bounced, and under `net` load (F19) | no phase steps, no Ethernet-correlated spurs or frequency shift |
| `rf.p25_replay` | 0 | tx,rx | | P25 site clip replay into a DUT. A Tezuka TX board streams the clip gap-free from its own RAM (its radio daemon stopped); other boards take one cyclic buffer. The TX LO is trimmed by `units.<recorder>.ref_ppm − units.<tx>.ref_ppm`, and traffic is scored against SDRTrunk's per-call `.mbe` decode of the same air (`rf.p25_truth_dir`). | TSBK CRC-ok ≥ 1/s; IMBE recovery ≥ 90 % of SDRTrunk; per-build regression score |
| `rf.p25_corpus` | 0 | tx,rx | | Many-recording replay corpus (manifest from `tools/p25_corpus_index.py`): `mode=A` the real wideband captures with `.mbe` truth (9 of 18), whole and single pass from the TX board's SD card through `fbench-agent replay stream` (RAM ring, underrun counters; `a_unit=window` for RAM windows); `mode=B` synthetic full system (CC + concurrent traffic recordings up-converted from 50 kSPS and mixed, aligned to their SDRTrunk log clocks ±30 ms); `mode=C` traffic only (every traffic recording with `.mbe` truth back to back on one channel after a CC primer, follower locked with `/api/traffic?lock=on&follower=off`, restored afterwards). Per-transmission scoring: raw IMBE frames from `/api/imbe_dump` matched to SDRTrunk's `.mbe` frames; p25-httpd calls matched by TG and time; `/ws/audio` tone continuity for focus calls (the 2026-05-03 two-tone alert). Resumable, stoppable | clear-voice frame recovery ≥ 90 % of SDRTrunk over followable transmissions; 0 relay underruns; 0 focus-tone dropouts |
| `hw.id` | 1 | any | | hwval ID/version/features, snapshot of all domains | all clocks alive |
| `hw.census` | 1 | any | | frequency of every clock vs FCLK0; implied reference ppm; drift | within expected ppm windows |
| `hw.ingest` | 1 | any | | valid gaps, CDC overflows, ADC stats, stuck bits | 0 gaps, 0 overflows, no stuck bits |
| `hw.prbs_ber` | 1 | any | M | per-sample PRBS BER, optionally across delay settings | BER < 1e-12 (or 0 errors in soak) |
| `hw.ringv2_rate` | 1 | any | | ring v2 rate sweep with ramp/tagged pattern, agent checker | 0 loss up to the tested rate |
| `hw.ringv2_protocol` | 1 | any | | flush latency, protect mode, header, lap detection, soft reset | all protocol assertions hold |
| `hw.legacy_ring` | 1 | any | | production-replica ring at 8 MSPS-equivalent rate under load | 0 packer overflows, 0 anomalies |
| `hw.memtest` | 1 | any | | mt0/mt1 all patterns over the 64 MiB window | 0 errors |
| `hw.mem_bw` | 1 | any | | bandwidth/latency vs burst length × outstanding × mode | report; write p-max < 16 µs idle |
| `hw.contention` | 1 | any | | ring loss/latency under PS/SD/IIO/PL aggressor matrix | 0 ring loss in production-like cells |
| `hw.ctrl_out` | 1 | any | | AD9361 CTRL_OUT transitions during rate/LO changes and soak | no unexplained lock drops |

Suites: `smoke` (identity, audit, telemetry 10 s, clk_freq), `interface`, `transport`,
`memory`, `rf`, `hwval` (all Tier 1), `soak`.

## 10. Operator checklist (Andy)

Before the first RF test, on **both** boards:

1. DMM: VCCO_34 (C90–C94), VCCO_35 (C99/C100/C105/C106), VDD_INTERFACE (C177),
   40 MHz oscillator VDD (C164), `XTAL_VTC` (JP5 pin 15, with nothing attached),
   VCC1V35 (DDR). Record in `bench/config/boards.md`.
2. Known from photos (F16–F18): both OpenSDRLab variant with PGA-102+; DRAM
   MT41K256M16TW-107 DDR3L; newer unit AD9363, original unit AD9361. Label the two
   boards physically (A = original, B = newer) since firmware serials follow the SD
   card. Confirm the BOOT1 DIP is at SD boot (both switches OFF).
   If a scope is available, probe JP5 pin 15 (`XTAL_VTC`) on both boards: a DC level
   means a VCTCXO tuning input, a 25 MHz square wave means PHY CLKOUT (F19).
   Optional: plug each unit's DEBUG USB-C (FT2232 console) into the PC during soaks so
   kernel panics and watchdog resets are captured.
3. Network: cable A.eth0 ↔ B.eth0, then `fbench setup all` (asks for the root password
   once per unit whose image lacks the host key).
4. RF: pads on every TX→RX path, antennas removed, `rf.cabled_confirmed = true`.
5. Unit A's P25 SD card fails to boot cleanly (F14) and A is currently on its factory
   card. Before anything else: connect A's DEBUG USB-C (FT2232 UART, 115200 8N1), boot
   the P25 card and capture the console log to see why RNDIS/iiod fail. Factory-card
   units support Tier 0 read-only tests over libiio; the full suite needs a Tezuka
   image (P25 or `hwval`) on each unit's card. Keep the factory cards as known-good
   references.

## 11. Build and delivery

| Piece | Location | Built by |
|---|---|---|
| HDL (`hwval_hdl`, sims) | `scanner-hdl/hwval_hdl/`, `scanner-hdl/test/test_hwval_*.py` | Claude (sims run locally in `.venv-hdl`) |
| IP + Vivado project | `maia-hdl/ip/hwval-core/`, `maia-hdl/projects/fishball7020_hwval/` | Andy: `./build_fpga_hwval_pretty.sh` (wraps `build_fpga.bat --hwval`) |
| Device tree + SD layout | Tezuka `board/tezuka/fishball7020/dts/fishball-hwval.dts(i)`, `post-image.sh` | Andy: Tezuka build |
| Agent | `bench/agent/` (Rust, `armv7-unknown-linux-musleabihf`, `cargo zigbuild`) | Claude (`bench/scripts/build_agent.sh`), tested under qemu |
| Host CLI | `bench/fbench/` (Python 3.11+) | Claude |

## 12. Phasing

1. **Now (Tier 0, no bake):** agent + CLI + Tier 0 tests. First runs: `smoke`,
   `sys.audit` on both boards, `store.sd_bench`, `mem.ps_memtest`, `iface.*` in
   maintenance mode, `xport.p25_ring_prbs` — the first real measurement of production
   ring loss (F1).
2. **hwval bake:** Tier 1 tests; ring v2 and memtester characterisation; contention
   matrix; clock census and board-to-board reference stability.
3. **p25diag:** port ring v2 (flush-bounded latency, committed counter, lap-safe reader,
   counters instead of Rsticky) into P25 and fix F4's delivery-time gating; re-run
   `rf.p25_replay` as the regression gate.
