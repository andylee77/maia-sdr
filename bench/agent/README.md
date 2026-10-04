# fbench-agent

On-board test agent for the Fishball Z7020 hardware validation suite
(`doc/HW_VALIDATION_SUITE.md`, section 5.3). A static ARM binary (Rust, musl,
no runtime dependencies) that the host CLI (`bench/fbench/`) runs over SSH
from `/mnt/sd/bench/bin/fbench-agent`.

## Contract

- Every invocation prints **exactly one JSON object** on stdout, on one line
  (`--pretty` indents it). The only exception is JSONL streaming to stdout
  (`telemetry --jsonl -`): one line per sample, then the summary object as the
  last line.
- Logs and warnings go to stderr (`-v` for more).
- Success: `{"ok": true, "cmd": "<subcommand>", ..., "elapsed_s": 1.234}`,
  exit code 0. Non-fatal problems are listed in `"warnings": [...]`.
- Failure: `{"ok": false, "cmd": "...", "code": "<code>", "error": "<message>", "detail"?: {...}}`.

| `code` | Exit | Meaning |
|---|---|---|
| `usage` | 2 | bad command line (unknown option, missing value) |
| `error` | 2 | runtime failure |
| `interrupted` | 2 | SIGINT/SIGTERM/SIGHUP received (restore guards ran) |
| `unknown_command` | 3 | unknown subcommand |
| `precondition` | 3 | state precondition not met (p25-httpd running, ring disabled, no /dev/mem, ...) |
| `unsupported` | 3 | not supported here (for example Linux-only feature on the host) |
| `no_device` | 3 | required IIO / rxbuffer device missing |
| `wrong_image` | 3 | the running bitstream lacks the core (UIO / product ID mismatch) |
| `not_found` | 3 | named core / region / file does not exist |
| `safety` | 4 | refused by a safety rule (allow-list, vacant bank, read-to-clear, TX interlock, write path) |

These codes match `REFUSED_CODES` / `UNSUPPORTED_CODES` in `bench/fbench/agent.py`.
Test verdicts are data, not errors: a memory test that finds errors returns
`ok: true, pass: false`.

## Build and deploy

```bash
bench/scripts/build_agent.sh            # cargo test + ARM build + qemu smoke test
bench/scripts/build_agent.sh --no-smoke # build only
```

The script runs `cargo zigbuild --release --target armv7-unknown-linux-musleabihf`
with the zig and cargo-zigbuild from `.venv-hdl`, copies the binary to
`bench/agent/dist/fbench-agent` (the host config also accepts
`target/armv7-unknown-linux-musleabihf/release/fbench-agent`), then, when docker
and `arm32v7/debian:bullseye-slim` are available, runs about 45 invocations
under qemu (`dist/smoke/run.sh`) and validates every reply on the host.
`fbench setup agent` copies the binary and `bench/share/*.json` to the board.

Host tests: `cargo test` in `bench/agent/` (unit tests of all pure logic plus
`tests/cli.rs`, which runs the host binary end to end).

## Global options

| Option | Default | Purpose |
|---|---|---|
| `--share DIR` | `/mnt/sd/bench/share` (or `$FBENCH_SHARE`) | register maps (`*.json`) |
| `--run-id ID` | `adhoc_<UTC stamp>` | run directory `/mnt/sd/bench/runs/<ID>/` (falls back to `/tmp/fbench_runs/<ID>/` when the SD layout is missing) |
| `--run-dir DIR` | | explicit artifact directory (must be writable, see below) |
| `-v`, `--verbose` | | log to stderr |
| `--pretty` | | indented JSON |
| `--json` | | accepted and ignored (host compatibility) |

## Safety behaviour

- **Register allow-lists** (rule 5, F15): every register access goes through a
  map. Unknown registers and anything at or above the radio core's 1 KB window
  (0x400: the bus aliases there) are refused before the device is even opened.
  The radio core's sync-domain banks are refused while `control.sdr_reset = 1`
  (they read 0 and drop writes until it clears). Read-to-clear registers (the
  radio core's `interrupts` 0x0C, `laneN_status` 0x3C/0x5C/0x7C, `spec_status`
  0xA4, `wideband_iq_dma_status` 0xC0; axi_dmac PARTIAL_TRANSFER_ID) need
  `--force-side-effects`. PS cores (`slcr`, `ddrc`,
  `l2c`, `afi`) are read-only; `rx_dmac`/`tx_dmac` only allow SCRATCH writes;
  core resets (`RSTN`) need `--force`; TX-affecting DAC registers need `--tx-ok`.
- **Maintenance mode** (rule 4): commands that replace or corrupt the RX
  stream (eyescan, prbs soak, txlink, BIST via `iio debug set` / `ring --bist`,
  AD9361 SPI writes, enabling the production ring) refuse while p25-httpd runs,
  unless `maint enter` was run, or `--auto-maint` (enter and exit around the
  command) / `--ignore-maint` is given.
- **TX** (rule 3): `txlink` arms a TX guard that writes -89.75 dB attenuation
  before any DAC source is enabled and runs `tx off` on every exit path
  (normal, error, panic, SIGINT/SIGTERM/SIGHUP). A SIGBUS/SIGSEGV handler also
  writes the attenuation with async-signal-safe syscalls before `_exit(2)`.
  TX-enabling IIO writes (`out_voltage0_hardwaregain` above -89.75,
  DDS scale, `bist_prbs 1`, `bist_tone 1 ...`) need `--tx-ok`, which the host
  passes only after its link-budget interlock.
- **Restore guards**: every changed setting (IDELAY taps, AD9361 0x006/0x007,
  PN_SEL/DATA_SEL, DAC_CLKSEL, sample rate, BIST, loopback, ring enables) is
  restored on exit, also on error or signal.
- **Writes** (rule 6): only `/mnt/sd/bench/**` and `/tmp/fbench*` (symlinks
  resolved). The one exception is `boot select`, which replaces exactly
  `/mnt/sd/BOOT.bin` and `/mnt/sd/devicetree.dtb` from a hash-verified source.
  `FBENCH_EXTRA_WRITE_ROOT` adds one more root; it exists for host tests only.
- **/dev/mem windows** for `mem test --phys`, `mem canary`, `mem bw` and the
  uncached ring mapping must lie inside a `no-map` reserved-memory region and
  never overlap System RAM.
- PL cores with a UIO device (`p25-core`, `hwval-core`) are mapped through
  `/dev/uioN` (as p25-httpd does); other registers through `/dev/mem` with
  `O_SYNC`. A core is only touched when its UIO/IIO device exists and its
  product ID matches.

## Register maps

Schema `fbench.regmap/1` (as emitted by `scanner-hdl/hwval_hdl/regmap.py`):

```json
{"schema": "fbench.regmap/1", "core": "hwval", "base": "0x7C460000", "size": 4096,
 "id_reg": "ID", "id_value": "0x68777631", "snapshot_domains": {"sync": 1, "mem": 2, "sampling": 4},
 "blocks": [{"name": "id", "offset": "0x000", "regs": [
   {"name": "TS_LO", "offset": "0x02C", "access": "ro", "width": 32, "reset": "0x0",
    "snapshot": "sync", "desc": "...", "fields": [{"name": "...", "lsb": 0, "width": 1, "desc": "..."}]}]}]}
```

Accepted extras: `read_side_effect`, `domain`, `dangerous`, `tx_affecting` per
register; `requires {uio|iio}`, `reset_gate {reg, bit, domains}`,
`vacant [[lo, hi]]`, `decode_limit`, `readonly`, `writable [names]` per core.
Offsets are absolute within the core (a value below its block offset is taken
as block-relative). A file may hold one map, an array of maps, or
`{"cores": [...]}`.

Built-in maps (embedded; `bench/agent/maps/*.json`): `adi_adc`, `adi_dac`
(both at 0x79020000, DAC offsets 0x4000+), `rx_dmac`, `tx_dmac`, `slcr`
(including DDRIOB 0xB40-0xB74), `ddrc`, `l2c`, `afi`, `p25` (the radio core:
`bench/share/p25_regs.json`, which scanner-hdl's `radio_core.bench_map` writes
from the core's own register banks with their read-to-clear registers, clock
domains and reset gate; `build.rs` embeds it) and an ID-only `hwval`. Every `*.json` in the share directory overrides the
built-in of the same core, but the built-in safety annotations are merged back
as a floor (a share map cannot remove a read side effect, vacant range, reset
gate, UIO requirement or read-only flag).

hwval snapshot protocol: before reading a register whose `snapshot` names a
domain, the agent writes the domain mask to SNAP_REQ and polls SNAP_ACK until
it equals the mask (10 ms timeout); missing bits are reported as
`dead_domains` and the values flagged `stale`.

## Subcommands

JSON examples show representative values; `...` marks repeated entries.

### version

```bash
fbench-agent version
```

```json
{"ok": true, "cmd": "version", "agent": "fbench-agent", "version": "0.1.0", "git": "7fdff4fd279b",
 "build": {"version": "0.1.0", "git": "7fdff4fd279b", "built": "2026-09-26T16:51:00.000Z",
           "target": "armv7-unknown-linux-musleabihf", "profile": "release", "rustc": "rustc 1.93.1 ...", "p25_map": "svd"},
 "schema": "fbench.agent/1", "patterns": ["pn0fn", "ramp64", "tagged", "prbs31", "iqramp", "tone"],
 "memtest_patterns": ["walking1", "..."], "builtin_maps": ["adi_adc", "..."], "elapsed_s": 0.001}
```

### help

`fbench-agent help` returns `{"agent", "version", "commands": [{"usage", "desc"}], "global_options": [...]}`.

### info

```bash
fbench-agent info
```

```json
{"ok": true, "cmd": "info",
 "model": "FISH Ball PlutoSDR Rev.A (Z7020/AD9361)", "serial": "OVHNVI2FJEBXAB4M",
 "hw_model": "...", "fw_version": "...", "hostname": "pluto",
 "image": "p25",
 "bitstream": {"core": "p25", "product_id": "0x72616431", "name": "rad1", "version": "1.0.0", "platform": 0, "sdr_reset": 0},
 "fpga_dna": null,
 "ad936x_compatible": "adi,ad9361", "ad936x_note": "AD9361 and AD9363 cannot be told apart in software; see bench config",
 "sample_rate_hz": 8000000, "rx_lo_hz": 858100000,
 "cmdline": "console=ttyPS0,115200 ... uio_pdrv_genirq.of_id=uio_pdrv_genirq", "boot_medium": "sd",
 "uptime_s": 1234.5, "boot_id": "...", "kernel": "6.1.0-...", "kernel_version": "#1 SMP ...", "cpus": 2,
 "mem": {"total_kb": 1015000, "available_kb": 800000, "cma_total_kb": 0, "cma_free_kb": 0},
 "sd": {"mounted": true, "device": "/dev/mmcblk0p1", "fstype": "vfat", "total_mb": 60000, "free_mb": 59000,
        "bench_dir": true, "agent_installed": true, "share": ["hwval_regs.json"], "images": ["hwval", "p25"]},
 "iio_devices": [{"id": "iio:device0", "name": "xadc"}, "..."], "uio": ["p25-core"],
 "modules": [{"name": "maia_sdr", "size": 16384, "used_by": "-", "srcversion": "...", "version": null}],
 "services": ["p25-httpd", "iiod", "dropbear"], "services_detail": {"p25-httpd": {"running": true, "pids": [312]}, "...": {}},
 "maintenance": false, "agent_path": "/mnt/sd/bench/bin/fbench-agent", "build": {"...": "..."}, "warnings": []}
```

`image` is `p25` / `hwval` (from the UIO core present), `maia`, `factory`
(plutosdr-fw) or `unknown`. With the hwval core, `bitstream` carries
`features`, `map_source`, `fpga_dna` (57-bit PL DNA, when DNA_STATUS.valid)
and `dna_status`, and the top-level `fpga_dna` is set.

### audit

```bash
fbench-agent audit
```

```json
{"ok": true, "cmd": "audit", "pass": false, "failures": 2,
 "checks": [{"name": "ddr_pll_fdiv", "addr": "0xF8000104", "value": "0x00024000", "expected": "FDIV 0x20",
             "ok": false, "severity": "fail", "detail": "overclock FSBL: CL7 at 600 MHz violates tAA for MT41K256M16TW-107"},
            {"name": "ddr_taa", "addr": null, "value": null, "expected": ">= 13.125 ns", "ok": false, "severity": "fail",
             "detail": "tAA = CL7 / 600.0 MHz = 11.667 ns (MT41K256M16TW-107 min 13.125 ns)"},
            {"name": "vccoddr", "expected": "1.35 V +- 5 %", "ok": true, "severity": "fail", "detail": "1.352 V (DDR3L ...)"},
            "..."],
 "derived": {"cpu_6x4x_mhz": 666.667, "ddr_mhz": 600.0, "fclk0_mhz": 100.0, "fclk1_mhz": 200.0,
             "cas_latency": 7, "cas_write_latency": 6, "taa_ns": 11.667,
             "l2c_prefetch": {"data": 1, "instr": 1, "early_bresp": 1, "prefetch_ctrl": "0x..."}},
 "regs": {"ARM_PLL_CTRL": "0x00028008", "DDR_PLL_CTRL": "0x...", "DDRIOB_ADDR0": "0x...", "L2C_CONTROL": "0x00000001", "AFI0_RDCHAN_CTRL": "0x...", "...": "..."},
 "xadc": {"temp_c": 45.1, "vccint": 1.0, "vccaux": 1.8, "vccoddr": 1.352, "...": 0},
 "reserved_memory": [{"node": "p25-wideband-iq-dma@22000000", "base": "0x22000000", "size": "0x1000000",
                      "no_map": true, "label": "p25_wideband_iq_dma", "compatible": null, "overlaps_system_ram": false}],
 "iomem": [{"start": "0x00000000", "end": "0x1FFFFFFF", "name": "System RAM"}, "..."],
 "cmdline": "...", "kmod": {"loaded": ["..."], "files": [{"path": "/lib/modules/.../maia-sdr.ko", "size": 12345, "mtime": 1700000000, "sha256": "..."}], "note": "..."},
 "modules": ["..."], "services": ["..."], "dmesg": ["Reserved memory: created DMA memory pool ..."], "read_errors": []}
```

Checks with `severity: "fail"`: DDR PLL FDIV (0x20 expected; 0x24 gives the
overclock message), DDR_CLK_CTRL = 0x0C200003, DRAM_EMR_MR = 0x00040B30 (CL7),
tAA >= 13.125 ns, XADC vccoddr 1.35 V +- 5 %, no-map reserved regions outside
System RAM. `warn`: ARM/IO PLL, FCLK0/1, DDRC port priorities, L2C enabled,
leftover maia-httpd, SD-boot bootargs. `info`: reboot status. DDRIOB
registers are reported raw in `regs`.

### reg

```bash
fbench-agent reg read  --core p25 --reg product_id
fbench-agent reg read  --core p25 --reg wideband_iq_dma_status --force-side-effects
fbench-agent reg write --core adi_adc --reg IDELAY_3 --value 0x11
fbench-agent reg dump  --core hwval
fbench-agent reg list  [--core C]
```

Options: `--force-side-effects` (read-to-clear), `--force` (dangerous
registers), `--tx-ok` (TX-affecting), `--no-init` (hwval).

```json
{"ok": true, "cmd": "reg read", "core": "p25", "reg": "product_id", "offset": "0x000",
 "value": 1918985265, "hex": "0x72616431", "fields": {"product_id": 1918985265},
 "snapshot": [{"mask": 1, "ack": 1, "ok": true, "dead_domains": [], "elapsed_us": 3}], "stale": false}
```

`snapshot`/`stale` appear only for snapshot registers; `side_effect` for
read-to-clear registers. `reg write` returns
`{"core", "reg", "offset", "written": "0x11", "value", "hex", "fields"}` (the
read-back is omitted for write-only, read-to-clear and snapshot registers).
`reg dump` returns
`{"core", "base", "source", "reset_gate_asserted", "regs": {"NAME": int}, "hex": {"NAME": "0x..."}, "fields": {"NAME": {...}}, "skipped": [{"reg", "reason", "code"}], "snapshot": [...]}`.
`reg list` returns `{"cores": [{"core", "base", "size", "regs", "source", "readonly", "snapshot_domains", "requires"}], "share_dir", "share_files"}`
or, with `--core`, `{"core": {...}, "regs": [{"name", "offset", "access", "snapshot", "domain", "read_side_effect", "dangerous", "tx_affecting"}]}`.

### telemetry

```bash
fbench-agent telemetry --seconds 60 --interval-ms 1000 [--jsonl /mnt/sd/bench/runs/R/telemetry.jsonl | --jsonl -]
```

```json
{"ok": true, "cmd": "telemetry", "seconds": 60.0, "interval_ms": 1000, "count": 60,
 "samples": [{"t": 1.0, "ts": 1790455956.452,
              "xadc": {"temp_c": 45.2, "vccint": 1.001, "vccaux": 1.797, "vccbram": 1.0, "vccpint": 1.0, "vccpaux": 1.8, "vccoddr": 1.351, "vrefp": 1.25, "vrefn": 0.0},
              "ad9361_temp_c": 38.5, "clk_freq_hz": 16000000, "clk_freq_raw": 10486, "loadavg": [0.5, 0.4, 0.3],
              "irq_deltas": {"28:p25-core": 30}, "softirq_deltas": {"TIMER": 100},
              "cpu": {"cpu0": {"irq": 1, "softirq": 2, "busy_pct": 12.5}}, "mem_available_kb": 800000}],
 "samples_truncated": false,
 "summary": {"xadc.temp_c": {"min": 45.0, "max": 45.4, "mean": 45.2, "n": 60}, "clk_freq_hz": {"...": 0}},
 "events": [{"t": 12.0, "kind": "rail_out_of_range", "detail": "vccoddr 1.290 V (nominal 1.35 V +- 5 %)"}],
 "jsonl": null, "jsonl_lines": 0}
```

With `--jsonl FILE` the samples go to FILE (one per line), the reply keeps the
last 10 and sets `samples_truncated`. `--jsonl -` streams the samples to
stdout followed by the reply line. Events: `over_temp` (>= 85 C),
`rail_out_of_range` (+- 5 % of nominal; vccoddr 1.35 V for DDR3L),
`clk_freq_change` (> 1 %).

### profile

```bash
fbench-agent profile --pid scanner [--seconds 30] [--hz 1000] [--top 40] [--symbols /tmp/scanner.unstripped]
```

```json
{"ok": true, "cmd": "profile", "pid": 32171, "samples": 240116, "pct_core": 44.8, "idle_pct_core": 152.9,
 "threads": [{"name": "tokio-rt-worker", "pct_core": 27.8,
              "functions": [{"name": "scanner::dsp::fsk4::folded_run [scanner]", "pct_core": 2.6}]}],
 "functions": [{"name": "scanner::dsp::fsk4::folded_run [scanner]", "pct_core": 5.4,
                "threads": [{"name": "p25-cc", "pct_core": 2.8}, {"name": "tokio-rt-worker", "pct_core": 2.6}]}],
 "others": [{"pid": 30358, "comm": "fbench-agent", "pct_core": 0.5}],
 "symbols": {"/usr/bin/scanner": "symtab", "/lib/libc.so.6": "dynsym"},
 "exe": "/usr/bin/scanner", "seconds": 120.058, "hz": 1000.0, "cpus": 2, "lost": 0}
```

The kernel samples every CPU on its clock (`perf_event_open`, software CPU
clock, `--hz` a second); each sample of the process counts for its thread and
for the function it was in, `pct_core` being the share of one core. Functions
come from the ELF symbol table of the executable or library mapped at the
address (`symtab`, else `dynsym`): a stripped executable shows as `[scanner]`.
`--symbols` names an unstripped build of the same code, which must hold the
running executable's code segment byte for byte (`precondition` otherwise; a
build in another target directory or with other profile settings differs).
Inlined code counts for the function it was inlined into; `[kernel]` is time in
the kernel on the process's behalf. `--pid` takes a PID or a process name.

### iio attr / iio debug

```bash
fbench-agent iio attr get --dev ad9361-phy --chan voltage0 --out --attr hardwaregain
fbench-agent iio attr set --dev ad9361-phy --attr in_voltage_sampling_frequency --value 8000000
fbench-agent iio debug get --dev ad9361-phy --attr bist_prbs
fbench-agent iio debug set --dev ad9361-phy --attr bist_prbs --value 2 --auto-maint
```

```json
{"ok": true, "cmd": "iio attr get", "dev": "ad9361-phy", "id": "iio:device1",
 "attr": "out_voltage0_hardwaregain", "value": "-89.750000 dB", "written": null}
```

`--dev` takes a name or `iio:deviceN`; `--chan C [--out]` builds
`{in|out}_<C>_<attr>`. Debugfs is mounted on demand. RX-corrupting debug
attributes need maintenance; TX-enabling values need `--tx-ok`.

### ad9361 spi

```bash
fbench-agent ad9361 spi read  --addr 0x006
fbench-agent ad9361 spi write --addr 0x006 --value 0x4A --auto-maint
```

```json
{"ok": true, "cmd": "ad9361 spi read", "addr": "0x006", "value": 74, "hex": "0x4A", "written": null}
```

### eyescan

```bash
fbench-agent eyescan --mode idelay --rate 61440000 --dwell-ms 10 [--lanes 0,1,2|all|each] [--auto-maint]
fbench-agent eyescan --mode ad9361 --dwell-ms 10
fbench-agent eyescan --mode 2d --lanes 0,5 --dwell-ms 5
fbench-agent eyescan --mode ad9361-driver
```

Setup: BIST PRBS injected into RX (`bist_prbs 2`), PN_SEL = 0 (pn0fn) on
channels 0-1; each point sets the delay, settles 1 ms, clears CHAN_STATUS,
dwells, then requires STATUS.bit0 and no PN_ERR/PN_OOS on both channels.
`--rate` switches `in_voltage_sampling_frequency` for the scan and restores it.

```json
{"ok": true, "cmd": "eyescan", "mode": "idelay", "dwell_ms": 10,
 "chosen": {"clk": 4, "data": 10}, "current_taps": [30, 30, 30, 30, 30, 30, 30],
 "saved": {"rate_hz": 61440000, "original_rate_hz": 8000000, "reg_0x006": "0x4A", "clk_delay": 4, "data_delay": 10, "idelay_taps": [30, "..."]},
 "baseline": {"pass": true, "if_status": true, "ch0": {"pn_oos": 0, "pn_err": 0}, "ch1": {"pn_oos": 0, "pn_err": 0}},
 "clk_freq_hz": 122880000,
 "lanes": [{"lane": 0, "pass": [0, 0, 1, "..."], "ascii": "..oooooooooooo..................", "passing": 12,
            "window": {"start": 2, "end": 13, "len": 12, "centre": 7.5}, "centre": 7.5, "chosen": 30,
            "chosen_passes": false, "margin_lo": null, "margin_hi": null, "margin_min": null}, "..."],
 "window_taps_min": 11,
 "points": 192, "final_check": {"pass": true, "...": "..."}, "restored": [{"action": "bist_prbs=0", "ok": true, "detail": 0}, "..."],
 "seconds": 4.2}
```

`--mode ad9361`: `"grid"` 16x16 (rows = 0x006[7:4] clock delay, cols =
0x006[3:0] data delay, 1 = pass), `"summary"` =
`{"grid", "ascii", "passing", "rows": [{"ascii", "window"}], "max_row_window", "chosen": {"row", "col", "passes", "row_margin": [lo, hi], "col_margin": [lo, hi], "margin_min"}}`,
`"axes"`. `--mode 2d`: `"lanes": [{"lane", "grid": 16x32 (rows AD9361 data delay, cols IDELAY tap), "summary"}]`.
`--mode ad9361-driver`: the driver's `bist_timing_analysis` grid (`grid`,
`summary`, `raw`). Lane `"all"` sweeps all data lanes together.

### prbs soak

```bash
fbench-agent prbs soak --seconds 600 --poll-ms 100 [--rate 61440000] [--auto-maint]
```

```json
{"ok": true, "cmd": "prbs soak", "seconds": 600.0, "poll_ms": 100, "polls": 5990,
 "error_intervals": 0, "oos_events": 0, "if_status_bad": 0, "adc_overflow_intervals": 0,
 "per_channel": {"ch0": {"oos": 0, "err": 0}, "ch1": {"oos": 0, "err": 0}},
 "first_error_s": null, "last_error_s": null, "error_times_s": [],
 "initial_lock": {"pass": true, "...": "..."}, "clk_freq_hz": 16000000, "pass": true,
 "saved": {"...": "..."}, "restored": ["..."]}
```

### txlink

```bash
fbench-agent txlink --mode ad9361-loopback [--sweep] [--dwell-ms 10] [--intervals 20] [--auto-maint]
fbench-agent txlink --mode fpga-loopback
```

ad9361-loopback: TX attenuation to max first, AD9361 digital loopback,
DAC DATA_SEL 9 (PN9 on I, PN11 on Q), DAC SYNC, ADC PN_SEL 9; `errors` counts
failing dwell intervals at the current TX delay. `--sweep` scans SPI 0x007
16x16 for DAC_CLKSEL 0 and 1.

```json
{"ok": true, "cmd": "txlink", "mode": "ad9361-loopback", "dwell_ms": 10, "saved": {"...": "..."},
 "dac_clksel": 0, "tx_atten_db": -89.75, "chosen_delay": "0x00", "errors_at_chosen": 0, "errors": 0,
 "intervals": 20, "first_error": null,
 "sweep": [{"delay": "0x00", "clk": 0, "data": 0, "clksel": 0, "errors": 0}, "..."],
 "grids": {"clksel0": {"grid": [[1, "..."]], "chosen": {"...": "..."}}, "clksel1": {"...": "..."}},
 "axes": {"rows": "AD9361 0x007[7:4] TX clock delay", "cols": "AD9361 0x007[3:0] TX data delay"},
 "restored": ["..."], "pass": true, "seconds": 60.1}
```

fpga-loopback: ADC CHAN_CNTRL_3.DATA_SEL = 1 (DAC data looped inside the
FPGA). The ADI PN monitor taps the ADC input *before* that mux, so the data is
verified in software on one fresh wideband sub-buffer (p25 image) or by the
hwval ingest checker in PN9/PN11 mode (hwval image):
`"verify": {"method", "subbuf", "samples_checked", "errors", "i": {"pairs", "errors", "alignment"}, "q": {...}}`,
plus `errors` and `samples_checked` at top level.

### ring capture

```bash
fbench-agent ring capture --ring p25-wideband --bytes 64M --mapping cached --out /mnt/sd/bench/runs/R/wb1 \
    [--enable|--reenable] [--release-reset] [--bist prbs|tone[:HZ]] [--pattern pn0fn] [--timeout-s 10] [--auto-maint]
fbench-agent ring capture --ring hwval-v2 --bytes 32M --mapping uncached --out /tmp/fbench_cap/v2
```

RAM-first: the buffer is allocated, touched and mlock'd before the window; the
reader copies each completed sub-buffer (legacy rings) or committed burst range
(ring v2) into RAM, then writes `<out>.sigmf-data`, `<out>.sigmf-meta` (SigMF
1.0: `core:datatype` `ci16_le` for IQ rings, `ru32_le` for 64-bit ring v2
words; `core:sample_rate`, `core:frequency`, `core:hw` with the serial;
`fbench:*` ring geometry, mapping, pattern, build, BIST) and
`<out>.subbuf.jsonl` (one line per sub-buffer:
`{"index", "seq", "wake_ts_ns", "last_buffer", "backlog", "epoch", "offset", "bytes"}`;
ring v2 lines also carry `committed`, `from`, `to`, `lost_bursts`,
`discarded_bursts`, `declared_gap_units`, `boundary`).

```json
{"ok": true, "cmd": "ring capture", "path": "/mnt/sd/bench/runs/R/wb1.sigmf-data",
 "meta_path": "/mnt/sd/bench/runs/R/wb1.sigmf-meta", "sidecar_path": "/mnt/sd/bench/runs/R/wb1.subbuf.jsonl",
 "bytes": 67108864, "requested_bytes": 67108864, "complete": true, "sample_rate_hz": 8000000.0,
 "format": "ci16_le", "ring": "p25-wideband", "mapping": "cached", "mlocked": true,
 "capture_s": 2.1, "write_s": 5.4, "subbuf_period_ms": 32.768,
 "reader": {"polls": 4000, "wakes": 64, "chunks": 64, "bytes": 67108864, "busy_frac": 0.05, "backlog_max": 0,
            "hw_overflow_flags": 0, "last_buffer_delta_hist": [0, 64, 0, "..."], "v2_lost_bursts": 0, "v2_discarded_bursts": 0},
 "hw": {"wideband_iq_next_address": "0x22345000"}, "notes": ["enabled wideband_iq DMA (epoch)"],
 "cleanup": [{"action": "restore wideband_iq_enable", "ok": true, "detail": 0}], "warnings": []}
```

Rings: `p25-wideband` (`/dev/p25-wideband-iq`, 16 x 1 MiB at 0x22000000,
`last_buffer` from status 0xC0), `hwval-legacy` (`/dev/hwval-legacy`,
LEGACY_LAST_BUFFER via snapshot), `hwval-v2` (`/dev/hwval-ringv2`, window
0x20000000, runtime RINGV2_BASE/SIZE_BURSTS, committed-pointer protocol).
`--dev` / `--phys` override the device and base. Mappings: `cached`
(maia-kmod rxbuffer mmap + `_IOW('M', 0, int)` cache invalidate per
sub-buffer; the kmod allows one mapping per device, so p25-httpd must be
stopped) and `uncached` (`/dev/mem` `O_SYNC` of the reserved window).
For hwval rings, `hw` holds the `LEGACY_*` / `RINGV2_*` counters (legacy: true
loss = WORDS_IN - WORDS_ACCEPTED; PACKER_OVF reported but never used as loss;
`lat_max_us` vs the 16.6 us cliff; `b_cap_hit` when MAX_OUTSTANDING reached 5).

### ring check

Live streaming check:

```bash
fbench-agent ring check --ring p25-wideband --pattern pn0fn --seconds 30 --bist prbs --auto-maint \
    [--mapping cached|uncached] [--stall-ms 100,300,480,600,1000] [--stall-every-s 1.5] [--poll-us 500]
fbench-agent ring check --ring hwval-v2 --pattern ramp64 --seconds 10 --stall-ms 600
```

Check of a capture file (SigMF meta and sidecar are picked up next to it):

```bash
fbench-agent ring check --file /mnt/sd/bench/runs/R/wb1.sigmf-data [--pattern P] [--subbuf-bytes 1M --num-subbufs 16] \
    [--sidecar F] [--anomalies-out F] [--iq-swap auto|yes|no] [--tone-tol 0] [--bit-err-max 3] [--max-records 32]
```

```json
{"ok": true, "cmd": "ring check", "pattern": "pn0fn", "unit_bytes": 4,
 "units_checked": 1048576, "units_ok": 1048561, "units_unsynced": 0, "chunks": 16, "resyncs": 6,
 "lost_units": 1048583, "lost_bytes": 4194332, "declared_lost_units": 0, "torn_units": 32768,
 "meta_words": 0, "ringv2_headers": 0, "ringv2_pads": 0, "last_ringv2_header": null,
 "meta_index_checked": 0, "meta_index_mismatch": 0,
 "counts": {"word_gap": 1, "lap": 1, "torn": 1, "stale_line": 1, "splice": 1, "bit_error": 1, "repeat": 1},
 "anomalies_total": 7,
 "model": {"iq_swapped": false, "period_samples": 65535, "reference": "adi-hdl axi_ad9361_rx_pnmon.v pn0fn (Q_OR_I_N=0)"},
 "synced": true,
 "anomalies": [{"class": "lap", "t": 0.004, "offset": 1048576, "chunk": 4, "subbuf": 4, "unit": 0,
                "expected": "0xFC8EF871", "actual": "0xF92503A4", "expected_pos": 12516, "actual_pos": 12532,
                "delta_units": 1048576, "laps": 1}, "..."],
 "anomaly_file": "/mnt/sd/bench/runs/R/wb1.anomalies.jsonl",
 "bytes_checked": 4194304, "subbuffers": 16, "subbuf_bytes": 262144, "num_buffers": 16,
 "check_seconds": 0.05, "check_mbs": 80.0, "pass": false}
```

The live form adds `ring`, `mapping`, `seconds`, `sidecar_path`,
`subbuf_period_ms`, `ring_depth_ms`, `lap_threshold_ms` ((N-1) x T_sub),
`reader` (as in capture), `hw`, `bist`, `epoch_at_start`, `notes`, `cleanup`
and `stalls`:

```json
"stalls": [{"stall_ms": 600, "t_start": 1.5, "last_buffer_before": 7, "produced_subbufs_est": 18.31,
            "lap_expected": true, "laps_est": 1.0, "last_buffer_after": 9, "seen_new_subbufs": 2,
            "overflow_flag": true, "anomalies_after": {"lap": 1, "torn": 0, "...": 0}, "lost_units_after": 4194304}]
```

`--file` output adds `file`, `subbuf_bytes`, `sidecar_records`,
`check_seconds` and `check_mbs` (checker throughput; on the board it must
exceed 32 MB/s for the 8 MSPS wideband ring). Every anomaly goes to the
JSONL anomaly file; the reply keeps the first `--max-records` (32).

Patterns:

| Pattern | Unit | Source | Position |
|---|---|---|---|
| `pn0fn` | 32-bit IQ sample | AD9361 BIST PRBS (`bist_prbs 2`) | 16-bit LFSR state S with I = S[15:4], Q = bitrev12(S[11:0]); next = `{S[14:0], ^S[15:4] ^ ^S[2:1]}` (maximal, period 65535). Ported from `axi_ad9361_rx_pnmon.v` / `ad_pnmon.v`; I/Q orientation auto-detected (`--iq-swap`) |
| `ramp64` | 64-bit word | ring v2 mode 1 | `data = seq` |
| `tagged` | 64-bit word | ring v2 mode 2 | `{tag4, 0000, seq56}`; a tag change is a `splice` |
| `prbs31` | 64-bit word | ring v2 mode 3 | `{seq32, prbs32}`; the PRBS half must be `step32(prev & 0x7FFFFFFF)` of `hwval_hdl/pattern.py` (x^31 + x^28 + 1, MSB first, seed 0x7FFFFFFF) |
| `iqramp` | 32-bit IQ sample | legacy ring sample ramp | counter c with re = c[15:0], im = c[31:16] |
| `tone` | 32-bit IQ sample | AD9361 BIST tone at k*fs/32 | period-32 reference from the first 32 samples; breaks re-locked by phase |

Anomaly classes (design doc F3 / section 7):

| Class | Rule |
|---|---|
| `word_gap` | forward jump inside a sub-buffer (or at a boundary when not a whole number of rings) |
| `lap` | forward jump of k x ring size at a sub-buffer boundary: the writer lapped the reader |
| `torn` | backward jump into one-lap-old data inside a sub-buffer; ends when the stream returns to the original sequence (not double counted as a lap) |
| `stale_line` | one 32-byte aligned chunk exactly k laps old, then the stream resumes (stale cache line) |
| `splice` | any discontinuity inside the epoch window (one sub-buffer after an enable epoch), or a tag change |
| `bit_error` | value within `--bit-err-max` bits of the expected one and the stream continues, a payload-only mismatch, or an undecodable unit |
| `repeat` | small backward jump (up to one sub-buffer): duplicated block |

Periodic patterns classify by residue: pn0fn laps are k x (ring mod 65535)
(64 samples for the 16 MiB wideband ring), the tone pattern cannot see laps.
Ring v2 laps are declared by the consumer protocol (`declared_lost_units`,
`reader.v2_lost_bursts`, `v2_discarded_bursts`: safe window N - 1 -
max_outstanding, post-copy torn check), header/pad words
(`0xF1B0`/`0xF1B1` in bits 63:48) are skipped and their next-word index is
cross-checked (`meta_index_mismatch`); after the run the drained-idle
accounting `WORDS_IN == data words written + DROP_FULL + DROP_PROTECT` is in
`hw.accounting`.

### ring synth

```bash
fbench-agent ring synth --pattern ramp64 --out /tmp/fbench_smoke/r64 [--subbufs 24] [--subbuf-bytes 64k] [--ring-subbufs 16] \
    [--inject default|none|SPEC] [--tone-k 3] [--tone-amp 1500]
```

SPEC is a comma list of `gap@sb:at:len`, `lap@sb:k`, `torn@sb:at`,
`stale@sb:at`, `splice@sb:len`, `biterr@sb:at:mask`, `repeat@sb:at:len`
(`sb` = sub-buffer index in the capture, `at`/`len` in units). Writes the same
three files as `ring capture`.

```json
{"ok": true, "cmd": "ring synth", "path": "/tmp/fbench_smoke/r64.sigmf-data", "meta_path": "...", "sidecar_path": "...",
 "bytes": 1048576, "pattern": "ramp64", "subbuf_bytes": 65536, "num_subbufs": 4, "subbufs": 16,
 "injections": [{"class": "word_gap", "spec": "Gap { sb: 2, at: 2730, len: 7 }"}, "..."],
 "expected_counts": {"word_gap": 1, "lap": 1, "torn": 1, "stale_line": 1, "splice": 1, "bit_error": 1, "repeat": 1}}
```

### mem test

```bash
fbench-agent mem test --anon-mb 256 [--patterns walking1,marchc,prbs] [--passes 2] [--cpu 1] [--no-mlock]
fbench-agent mem test --phys 0x24000000 --size 64M --patterns address,marchc
```

Patterns: `walking1`, `walking0` (32 rotations), `address`,
`inverse-address`, `marchc` (March C-: up w0; up r0 w1; up r1 w0; down r0 w1;
down r1 w0; r0), `prbs` (xorshift32 per pass), `checkerboard`
(0x55555555/0xAAAAAAAA and inverted). `--phys` windows must be inside a
no-map reserved region (uncached `/dev/mem`).

```json
{"ok": true, "cmd": "mem test", "mode": "anon", "region": {"bytes": 8388608, "mlocked": true, "vaddr": "0x40A00000"},
 "cpu": null, "passes": 1, "errors": 0, "pass": true, "bytes_tested": 8388608, "bytes_moved": 1241513984, "seconds": 0.66,
 "patterns": [{"name": "walking1", "errors": 0}, "..."],
 "results": [{"name": "walking1", "errors": 0, "bytes_written": 268435456, "bytes_read": 268435456, "seconds": 0.281,
              "mbs": 1911.6, "error_bits": "0x00000000", "error_byte_lanes": [], "first_errors": [], "pass_index": 0}, "..."],
 "warnings": []}
```

`first_errors` entries are `{"addr", "expected", "actual", "xor"}` (up to 16);
`error_bits` is the OR of all XORs (a stuck DQ line shows as one bit).

### mem canary

```bash
fbench-agent mem canary fill   --region hwval-memtest [--size 16M] [--seed N]
fbench-agent mem canary verify --region hwval-memtest
```

`--region` is a reserved-memory node name, node stem or label. The fill seed
is kept in `/tmp/fbench_canary_<node>.json`.

```json
{"ok": true, "cmd": "mem canary fill", "region": "hwval-memtest@24000000", "base": "0x24000000", "bytes": 67108864,
 "seed": 12345, "state_file": "/tmp/fbench_canary_hwval-memtest_24000000.json", "seconds": 1.2}
{"ok": true, "cmd": "mem canary verify", "region": "...", "base": "0x24000000", "bytes": 67108864, "seed": 12345,
 "filled": "2026-09-26T16:00:00.000Z", "same_boot": true, "corrupt_words": 0, "intact": true,
 "first_corrupt_addr": null, "first_corrupt": [], "corrupt_page_ranges": [], "seconds": 1.1}
```

### mem bw

```bash
fbench-agent mem bw --size 16M [--reps 3] [--cpu 0] [--region hwval-memtest | --phys 0x24000000] [--write]
```

```json
{"ok": true, "cmd": "mem bw", "size": 16777216, "reps": 3, "cpu": null,
 "results": {"cached_memcpy": 450.0, "cached_write": 900.0, "cached_read": 1100.0,
             "uncached_memcpy_read": 120.0, "uncached_read32": 30.0, "uncached_write32": 60.0},
 "uncached_region": {"base": "0x24000000", "bytes": 16777216, "region": {"node": "...", "base": "...", "size": 0}},
 "unit": "MB/s (1e6 bytes/s, best of reps)"}
```

Uncached writes only with `--write`.

### sd bench

```bash
fbench-agent sd bench --mb 256 --bs 1024 --fsync [--fsync-each] [--dir /mnt/sd/bench/runs/sdbench_tmp] [--keep]
```

`--bs` is in KiB. The file is written with a verifiable pattern, dropped from
the page cache (`posix_fadvise DONTNEED`), read back, verified and deleted.

```json
{"ok": true, "cmd": "sd bench", "path": "/mnt/sd/bench/runs/sdbench_tmp/fbench_sdbench_812.bin", "kept": false,
 "bytes": 268435456, "block_bytes": 1048576, "fsync": true, "fsync_each": false,
 "write_s": 14.2, "write_total_s": 15.0, "write_mbs": 17.9, "write_mbs_no_fsync": 18.9, "fsync_ms": 800.0,
 "read_mbs": 20.1, "read_bytes": 268435456, "verify_mismatches": 0,
 "write_lat_us": {"p50": 50000, "p99": 120000, "max": 300000, "mean": 55000.0},
 "write_lat_hist": [{"ge": 0, "count": 0}, {"ge": 1, "count": 0}, "...", {"ge": 32768, "count": 200}],
 "slow": false, "fs_free_mb": 59000}
```

`slow` flags < 8 MB/s. Histogram bins are log2 microseconds (`ge` = lower bound).

### net serve / net send

```bash
fbench-agent net serve --port 5201 [--timeout-s 60] [--bind 0.0.0.0] [--source --mb 100]
fbench-agent net send  --host 10.25.0.2 --port 5201 --mb 100 [--sink]
```

`serve` accepts one connection and sinks data until EOF (or sources `--mb`
with `--source`); `send` connects and sources `--mb` (or sinks with `--sink`).

```json
{"ok": true, "cmd": "net send", "direction": "source", "peer": "10.25.0.2:5201", "bytes": 104857600,
 "seconds": 1.05, "mbs": 99.9, "mbits": 799.1}
```

### hwval

Tier 1 operations; names come from `share/hwval_regs.json`. All of them run
`hwval init` implicitly once per boot unless `--no-init`
(`/tmp/fbench_hwval_init.json` records the boot id and which clock domains
were alive; init repeats when a previously dead domain comes alive).

```bash
fbench-agent hwval init [--census]
fbench-agent hwval id
fbench-agent hwval census [--gate-ms 1000]
fbench-agent hwval ingest [--seconds 1] [--prbs off|pn0fn|pn9] [--honor-valid] [--clear] [--bist --auto-maint]
fbench-agent hwval ringv2 status|stop
fbench-agent hwval ringv2 setup --src ramp64|tagged|prbs31|live|off [--rate-mbs 32 | --rate-inc N] [--size-bursts N] [--base A] \
    [--subbuf-bursts N] [--protect] [--header] [--tag T] [--max-outstanding N] [--flush-timeout N] [--irq-every N] [--clear] [--enable]
fbench-agent hwval legacy status|stop
fbench-agent hwval legacy setup --src ramp|live|off [--rate-msps 8 | --rate-inc N] [--clear] [--enable]
fbench-agent hwval mt run --mt 0 --mode write-verify --pattern prbs [--burst-len 16] [--outstanding 8] [--passes 1] \
    [--base A] [--size S] [--seed 1] [--idle 0] [--stop-on-error] [--seconds S | --timeout-s 60] [--no-hist]
fbench-agent hwval mt status|abort --mt 1
fbench-agent hwval evt enable [--mask 0xFF] | disable | status | drain [--max 100000]
fbench-agent hwval guard [--lo A --hi B] [--lock]
fbench-agent hwval contention --seconds 5 --idle 0,16,64,256 [--mt-mode read-only] [--burst-len 16]
```

`hwval init`: verifies ID = 0x68777631, snapshots sync/mem/sampling (10 ms
timeout), pulses CORE_RESET (>= 1 ms), programs GUARD_LO/HI from the
`hwval-ringv2` + `hwval-memtest` reserved regions (min..max) and sets
GUARD_LOCK:

```json
{"ok": true, "cmd": "hwval init", "id": "0x68777631",
 "pre_reset_snapshot": {"mask": 7, "ack": 3, "ok": false, "dead_domains": ["sampling"], "elapsed_us": 10000},
 "post_reset_snapshot": {"mask": 7, "ack": 7, "ok": true, "dead_domains": [], "elapsed_us": 4},
 "core_reset_pulsed": true,
 "guard": {"programmed": true, "lo": "0x20000000", "hi": "0x28000000", "locked": 1, "regions": [{"node": "...", "base": "...", "size": "..."}]},
 "census": null, "state_file": "/tmp/fbench_hwval_init.json"}
```

`hwval id`:
`{"id", "id_ok", "version", "features", "feature_names", "scratch_ok", "fpga_dna", "snapshot", "all_clocks_alive", "ts_cycles", "ts_s", "snap_seq", "irq": {"pending", "enable", "count"}, "guard": {"lo", "hi", "locked"}, "core_reset", "init", "map_source"}`.

`hwval census`:
`{"gate_cycles", "gate_actual", "gate_s", "resolution_ppm", "clocks": {"sync": {"count", "hz", "alive", "nominal_hz", "ppm_vs_fclk0"}, "sampling": {"count", "hz", "alive", "ratio_to_fs"}, "lclk": {...}, "y1": {...}, "clkout": {...}, ...}, "ad9361_fs_hz", "note"}`.

`hwval ingest`:
`{"ctrl", "snapshot", "stale", "samples", "valid_gap_cycles", "valid_gap_runs", "cdc_wrerr", "cdc_full_cycles", "window": {"samples", "i_min", "i_max", "q_min", "q_max", "i_mean", "q_mean", "i_rms", "q_rms", "clip_count", "i_stuck0", "i_stuck1", "q_stuck0", "q_stuck1"}, "prbs": {"mode", "checked", "errors", "oos_events", "in_sync", "ber", "dropped_samples_estimate", "note"}, "counters_note"}`.
`checked` counts in-sync samples only (BER denominator); a dropped sample in
sync costs 16 errors + 1 OOS event, hence `dropped_samples_estimate`.

`hwval ringv2 status|setup|stop`:
`{"regs": {"RINGV2_*": int}, "accounting": {"idle", "words_in", "data_words_written", "drop_full", "drop_protect", "ok", "note"}, "snapshot"}`;
`setup` adds `ctrl`, `refused` (GUARD_BLOCKED incremented on enable) and
`refused_reason`; `stop` adds `drained_idle`.

`hwval legacy status|setup|stop`:
`{"regs": {"LEGACY_*": int}, "loss_words", "loss_note", "lat_max_us", "lat_cliff_us", "snapshot"}`
(+ `ctrl` / `stop`). `stop` uses the safe order: clear dma_enable, wait
LEGACY_AW == LEGACY_B, then source off.

`hwval mt run`: writes CTRL/BASE/SIZE/PASSES/IDLE_CYCLES/SEED, then
CMD = start|clear in one write, polls STATUS.done, snapshots the mem domain,
reads the latency histograms (HIST_SEL, SNAP_REQ, HIST_VAL per bin):

```json
{"ok": true, "cmd": "hwval mt", "status": {"busy": 0, "done": 1, "error": 0}, "pass_count": 1,
 "bytes_wr": 33554432, "bytes_rd": 33554432, "cycles": 600000, "seconds": 0.0048, "wr_mbs": 6990.5, "rd_mbs": 6990.5,
 "err_count": 0, "first_err": {"addr": "0x00000000", "expected": "0x0000000000000000", "actual": "0x0000000000000000"},
 "err_lanes": "0x0000000000000000", "bresp_err": 0, "rresp_err": 0,
 "wlat_max_cycles": 40, "wlat_max_ns": 320, "rlat_max_cycles": 60, "rlat_max_ns": 480,
 "guard_blocked": 0, "refused": false, "snapshot": ["..."],
 "wlat_hist": [0, 0, 0, 5, "..."], "rlat_hist": ["..."], "hist_bins": "bin k = [2^k, 2^(k+1)) clk2x cycles (8 ns); bin 15 = >= 2^15",
 "config": {"mt": 0, "mode": "write-verify", "pattern": "prbs", "burst_len": 16, "outstanding": 8, "passes": 1,
            "base": "0x24000000", "size": 33554432, "seed": 1, "idle_cycles": 0, "offered_load": 1.0, "ctrl": "0x..."},
 "timed_out": false, "wall_s": 0.01, "pass": true}
```

Bandwidth = bytes / (cycles / 125 MHz). Modes: `write-only`, `read-verify`
(verifies pass index 0), `write-verify`, `read-only`, `byte-lane`; patterns:
`address`, `walking1`, `walking0`, `checkerboard`, `prbs`, `zeros`, `ones`,
`toggle` (names or numbers). A refused start (bad parameters, window outside
the guard) shows `refused: true`. With PRBS errors the reply names the
reference for decoding `first_err`.

`hwval evt drain`:
`{"count", "transitions", "events": [{"t_s", "cycles", "heartbeat", "value": "0xA5"}], "overflows", "current", "remaining"}`
(27-bit timestamps unwrapped with the heartbeat records); the other `evt`
operations return `{"ctrl", "level", "overflows", "current"}`.

`hwval guard`: `{"was_locked", "lo", "hi", "locked", "dt"}`.

`hwval contention` (ring v2 must be enabled; PS/SD/IIO aggressors are
orchestrated by the host):
`{"seconds_per_cell", "mt_mode", "burst_len", "cells": [{"idle_cycles", "offered_load", "mt0_mbs", "mt1_mbs", "ringv2": {"drop_full_delta", "committed_bursts_delta", "fifo_hwm", "lat_max_cycles", "max_outstanding_seen"}}], "note"}`.

### replay stream / check / verify

```bash
fbench-agent replay stream --playlist P.json [--ring-mb 192] [--prefill-mb M] [--chunk-kb 1024] \
    [--status F] [--status-ms 1000] [--report F] [--on-underrun wait|zero] [--zero-after-ms 250] \
    [--stall-ms 500] [--out F] | iio_writedev -u local: -b 262144 cf-ad9361-dds-core-lpc voltage0 voltage1
fbench-agent replay check --playlist P.json
fbench-agent replay verify --file F [--offset B --length B] [--sha256 H]
```

The SD relay behind `rf.p25_corpus`: a reader thread copies the playlist's byte
ranges into a RAM ring (pages faulted in at start, consumed file pages dropped
with `POSIX_FADV_DONTNEED`); after the prefill (default: the whole ring) the
main thread converts to interleaved int16 with each item's gain and writes
stdout, which `iio_writedev` drains at the DAC rate. **stdout carries samples**
(written through fd 1 unbuffered, pipe raised to 1 MiB), so the reply JSON goes
to stderr and to `--report`; `--status` (under `/tmp/fbench*` or
`/mnt/sd/bench/**`) is rewritten every `--status-ms`.

Playlist: `{"format": "cs16"|"cs12"|"cs8", "rate_hz", "gain", "loops"?, "items":
[{"path", "offset"?, "length"? (bytes, 0 = to EOF), "gain"?} | {"zeros": samples}]}`.
`cs12` packs I and Q as 12-bit two's complement in one little-endian 24-bit
word (I in bits 0..11), lossless for AD9361 captures; `cs8` is 2 x int8.

Underruns (ring empty after the first output sample) are counted with their
stream position and length. `wait` keeps every sample (the timeline slips by
whatever the ~0.4 s of pipe + iio blocks could not cover); `zero` writes zeros
after `--zero-after-ms` and then skips as many source samples, so each sample
airs at its nominal time. Read calls slower than `--stall-ms` are listed in
`read_stalls`. Test hooks: `--pace-hz R` (DAC-like pacing when writing a file),
`--inject-stall BYTE:MS,...` (an SD stall inside the read at that input byte).

Reply shape (illustrative values, not a measurement):

```json
{"ok": true, "cmd": "replay stream", "state": "done", "complete": true, "prefill_s": 8.4,
 "t_first_out_unix": 1790502399.504, "samples_out": 1676800000, "seconds_out": 419.2,
 "bytes_in": 5030400000, "ring_bytes": 201326592, "ring_fill_s": 0.0, "ring_min_fill_s": 11.3,
 "read_mbs": 23.1, "read_calls": 4797, "read_max_ms": 2310.4, "read_slow_200ms": 7,
 "read_hist_log2_ms": [4700, 60, "..."], "read_stalls": [{"input_byte": 1811939328, "ms": 2310,
 "ring_fill_bytes": 190000000, "t_unix": 1790502600.1}], "reader_wait_full_s": 190.2,
 "underruns": 0, "underrun_ms": 0.0, "underrun_events": [], "zero_samples": 0, "skipped_samples": 0,
 "playlist": {"format": "cs12", "items": 6, "bytes": 5030400000, "samples": 1676800000, "seconds": 419.2},
 "on_underrun": "wait", "skip_debt_left": 0}
```

`state`: `done` | `stopped` (SIGTERM) | `downstream_closed` (EPIPE) | `error`
(reader failure, e.g. a file shorter than its range). `check` resolves the
playlist (sizes, missing files: `not_found`, short files: `precondition`) and
reports `MemAvailable`; `verify` hashes a staged file (about 45 s per GiB on the
SD card).

### tx off

```bash
fbench-agent tx off [--lo-powerdown]
```

```json
{"ok": true, "cmd": "tx off", "tx_atten_db": -89.75,
 "actions": [{"action": "tx_atten=-89.75dB", "ok": true, "detail": -89.75},
             {"action": "dac_data_sel=zero", "ok": true, "detail": {"data_sel": [3, 3]}},
             {"action": "dds_scale=0", "ok": true, "detail": ["out_altvoltage0_TX1_I_F1_scale", "..."]},
             {"action": "loopback=0", "ok": true, "detail": 0},
             {"action": "bist_prbs=0", "ok": true, "detail": 0},
             {"action": "bist_tone=off", "ok": true, "detail": "0 0 0 0"}]}
```

`ok` requires the attenuation at maximum (or no TX device) and DAC DATA_SEL =
3 on channels 0-1 (or no DAC core); otherwise the reply is
`{"ok": false, "code": "error", "detail": {...same object...}}`.

### maint

```bash
fbench-agent maint enter | exit | status
```

Maintenance mode stops the image's radio daemon: the scanner (`/etc/init.d/S60scanner`)
or, on older cards, p25-httpd (`/etc/init.d/S60p25-httpd`), whichever init script is
installed. `enter` records `/tmp/fbench_maint.json` first (with the daemon and its init
script), runs `<init> stop` and waits for the process to exit (escalating to
SIGTERM/SIGKILL); `exit` resets BIST/loopback, restarts the recorded daemon if it was
running and removes the state file. A state file from another boot is ignored.
`S60scanner stop` flushes the history and recordings (up to 25 s) and then tries to
unmount the SD card, which stays mounted while the agent runs from it.

```json
{"ok": true, "cmd": "maint enter", "maintenance": true,
 "state": {"entered": "2026-10-03T16:00:00.000Z", "boot_id": "...", "was_running": true, "daemon": "scanner",
           "init": "/etc/init.d/S60scanner", "pids": [312], "agent_pid": 900},
 "services": {"scanner": {"running": false, "pids": []}, "...": {}}, "daemon": "scanner", "daemon_pids": [],
 "steps": [{"cmd": "/etc/init.d/S60scanner stop", "rc": 0, "stdout": "Stopping scanner: OK", "stderr": ""}], "stopped": true}
```

`exit` returns `{"maintenance": false, "state": null, "services", "daemon", "daemon_pids", "was_in_maintenance", "restarted", "steps"}`;
`status` returns `{"maintenance", "state", "services", "daemon", "daemon_pids"}`.

### boot

```bash
fbench-agent boot status
fbench-agent boot select hwval [--reboot]          # also: boot select --image hwval
fbench-agent boot install hwval                    # same as select
fbench-agent boot install hwval --from /tmp/fbench_upload [--sha256-boot HEX --sha256-dtb HEX]   # stage only
```

Image directories are `/mnt/sd/bench/images/<name>/` with `BOOT.bin`,
`devicetree.dtb` and a `SHA256SUMS` manifest (`sha256sum` format).
`select` refuses unless both files match the manifest, backs the current root
pair up to `images/p25/` once (only if that backup is absent, with its own
manifest), copies each file to a temp name with fsync, verifies the hash,
renames it over `/mnt/sd/<file>`, fsyncs the directory, re-verifies and runs
`sync`. It never reboots unless `--reboot` is given. `install --from DIR`
stages DIR into `images/<name>/` (verified against `DIR/SHA256SUMS` or the
given hashes) without touching the boot pair.

```json
{"ok": true, "cmd": "boot status", "active": "p25",
 "images": {"hwval": {"present": true, "sha256_ok": true, "manifest": true, "active": false,
                      "files": {"BOOT.bin": {"present": true, "size": 4194304, "sha256": "...", "manifest": "...", "sha256_ok": true},
                                "devicetree.dtb": {"...": "..."}}},
            "p25": {"...": "..."}},
 "root": {"BOOT.bin": {"present": true, "size": 4194304, "sha256": "..."}, "devicetree.dtb": {"...": "..."}},
 "sd_mounted": true, "sd_boot": true, "backup_present": true}
{"ok": true, "cmd": "boot select", "image": "hwval", "changed": true, "reboot_required": true,
 "steps": [{"backup": "/mnt/sd/bench/images/p25", "sha256": {"...": "..."}}, {"installed": "/mnt/sd/BOOT.bin", "sha256": "..."}, "..."],
 "status": {"...": "..."}}
```

## Source layout

| Path | Contents |
|---|---|
| `src/main.rs` | entry point, JSON envelope, exit codes, panic capture |
| `src/cli.rs` | dependency-free argument parser (unknown options are errors) |
| `src/err.rs` | error codes |
| `src/regmap.rs`, `maps/*.json`, `build.rs` | allow-list maps (built-in + share, safety floor, SVD -> p25 map) |
| `src/access.rs`, `src/regio.rs` | safe register access (allow-list, gates, snapshot protocol), `/dev/mem` / UIO mapping |
| `src/checker/` | ring checker engine, pattern models (pn0fn, prbs31, ...), tone checker, PN9/PN11 (`pn1fn`) |
| `src/synth.rs` | synthetic captures with injected anomalies |
| `src/rings.rs` | ring geometry, cached/uncached mappings, legacy tracker, ring v2 consumer protocol |
| `src/eye.rs`, `src/memtest.rs`, `src/hist.rs`, `src/sigmf.rs`, `src/sha256.rs` | pure helpers |
| `src/safety.rs` | signals, `tx off`, TX guard, restore guards, maintenance mode |
| `src/sys.rs`, `src/iio.rs` | /proc, /sys, DT, IIO sysfs/debugfs, syscall wrappers |
| `src/cmd/*.rs` | one module per subcommand |
| `tests/cli.rs`, `tests/fixtures/` | end-to-end host tests; `hwval_regs.json` generated from `scanner-hdl/hwval_hdl/regmap.py`, `hwval_small.json` hand-written |

## Not yet verified on hardware

Everything that touches the board (register maps against the live bitstream,
maia-kmod mmap/ioctl, `/dev/uioN` mapping, AD9361 debugfs formats, the
hwval core) has only been compiled and exercised through mocks and qemu. Known assumptions to confirm there:

- `pn0fn` / `tone` through the P25 ring need a transparent ADC data path
  (DC filter off, IQ correction at unity, which is the ADI driver default);
  if every unit is undecodable, check `CHAN0_CNTRL` / `CHAN0_CNTRL_2` with
  `reg dump --core adi_adc` and try `--iq-swap yes`.
- `/dev/mem` must be usable for the ADI and PS registers
  (`CONFIG_IO_STRICT_DEVMEM` off); P25/hwval cores go through `/dev/uioN`.
- The hwval PN9/PN11 ingest channel mapping used by
  `txlink --mode fpga-loopback` follows the ADI convention (I = PN9, Q = PN11).

The first board session should run, in order: `version`, `info`, `audit`,
`reg dump --core p25`, `reg dump --core adi_adc`, `telemetry --seconds 5`,
`maint enter`, `prbs soak --seconds 10`, `eyescan --mode idelay`,
`ring check --ring p25-wideband --pattern pn0fn --bist prbs --enable --seconds 10`,
`maint exit`.
