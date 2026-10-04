# fbench — Fishball hardware validation bench

Host CLI for the two-Fishball validation bench. Contract and test catalog:
[doc/HW_VALIDATION_SUITE.md](../doc/HW_VALIDATION_SUITE.md). The on-board agent
(`fbench-agent`, Rust, static ARM) lives in [agent/](agent/); this directory's
`fbench/` package is the host side.

Design goals: fully drivable from a shell by an AI session — one JSON object on stdout
per invocation (`--json`), deterministic exit codes, explicit timeouts, and no prompts
except `setup keys --password-prompt`.

## Layout

```text
bench/
  fbench.py                launcher: python bench/fbench.py <verb> ...
  pyproject.toml           pip install -e bench  ->  `fbench` console script
  config/bench.toml        the bench (units, links, pads, safety limits)
  config/bench.example.toml  annotated copy of every key
  share/*.json             register maps (fbench.regmap/1) for the agent allow-lists
  fbench/                  CLI, config, transports, agent adapter, runner, safety
  fbench/analysis/         eye, tone, ring, periodicity, sigmf, bootlog, memtest,
                           sdrtrunk/p25_corpus/p25_score/p25_dsp (replay corpus)
  fbench/corpus.py         corpus items, rendering, SD/RAM staging, relay, taps
  fbench/tests/            one module per test family (sys/iface/xport/mem/store/net/rf/hw)
  tests_host/              pytest suite (simulated bench, no hardware)
  agent/                   fbench-agent (separate crate)
```

## Quick start (Andy)

Both interpreters work: `.venv-hdl` (Python 3.11) and the system Python 3.14.

```bash
cd /c/Users/Andy/Projects/MAIA_SDR/maia-sdr
PY=.venv-hdl/Scripts/python.exe          # or: python
$PY bench/fbench.py units                # offline: what the config says
$PY bench/fbench.py units --probe        # ping, IIO identity, SSH, agent, image, DNA
```

Before the first session:

1. Edit `bench/config/bench.toml`: unit addresses, `known_serials`, pads under
   `[[rf.links]]`. Leave `rf.cabled_confirmed = false` until the RF checklist below is done.
2. Host route and forwarding: `fbench setup net` (adds `10.25.0.0/24 via 192.168.2.1`;
   needs an administrator shell once, or add it yourself with `route -p add …`).
3. Keys: a unit whose image does not authorize `~/.ssh/id_ed25519` yet (factory
   firmware) needs the root password once: `fbench setup keys --unit A --password-prompt`.
   The key is stored on the SD card under `/mnt/sd/bench/keys/`.
4. Agent: `bench/scripts/build_agent.sh`, then `fbench setup agent`.
5. Every boot (Tezuka's rootfs is a ramfs): `fbench setup session` re-links the key,
   enables `ip_forward` on unit A, marks the agent executable and reports leftover
   maintenance mode.

RF checklist (design doc §10): pads (≥30 dB) on every TX→RX path, antennas removed,
unused ports terminated with 50 Ω. Then set `rf.cabled_confirmed = true`.

## Quick start (Claude)

Always pass `--json` and branch on the exit code. Never guess hardware state: probe it.

```bash
F="python bench/fbench.py"
$F units --probe --json                       # identity, reachability; exit 3 if a unit is down
$F setup session --json                       # per-boot steps
$F list --json                                # catalog + suites
$F describe iface.eye_idelay --json           # params, pass criteria, artifacts
$F run smoke --unit A --json                  # suite; exit = worst verdict
$F run rf.cw_ppm --tx A --rx B --dry-run --json   # params + interlock, no hardware
$F run rf.cw_ppm --tx A --rx B -p span_s=600 --json
$F analyze runs/bench/2026-09-26/bench/run_20260926_153012_iface.eye_idelay --json
$F status --json                              # last runs, maintenance/TX alerts
```

Read `result.json` in the run dir for metrics; `FINDINGS.md` is the human summary (the
`## Notes` section survives re-analysis).

## Verbs

Every verb takes `--json`, `--config PATH`, `--timeout S` (default per remote call) and
`-v`, before or after the verb.

| Verb | Example | Notes |
|---|---|---|
| `units` | `fbench units --probe --unit A` | Offline list; `--probe` adds ping, IIO context, SSH, agent, image, firmware family, serial hint, DNA. |
| `setup` | `fbench setup net\|keys\|agent\|session\|all [--unit U]` | `--password-prompt` / `--password-env VAR` (keys only), `--apply-env` (persistent u-boot network vars), `--skip-route`, `--binary PATH`. |
| `list` | `fbench list --tier 0 --suite interface` | Catalog and suites. |
| `describe` | `fbench describe rf.level_sweep` | Test or suite. |
| `run` | `fbench run <test\|suite> [--unit U[,V]] [--tx U --rx V] [-p k=v …] [--duration S] [--dry-run]` | Writes one run dir per test; exit = verdict (worst for suites). |
| `status` | `fbench status [--limit N] [--probe]` | Last runs, session state, alerts (maintenance left on, TX flagged active). |
| `analyze` | `fbench analyze <run_dir> … [-p k=v …]` | Re-runs the analysis on pulled artifacts, no hardware; `-p` changes an analysis parameter and the run keeps it (e.g. `rf.freq_sweep`'s `compare`). |
| `compare` | `fbench compare <baseline_dir> <run_dir> …` | Metric deltas; exit 1 on regression (worse verdict or newly violated threshold). |
| `boot` | `fbench boot A status\|p25\|hwval [--no-reboot] [--wait S]` / `boot A install --image hwval --from DIR` | Dual-image swap with sha256 manifest, reboot, session setup, image check. |
| `safety` | `fbench safety --tx A --rx B --tx-atten 0` | Link budget and interlock verdict (exit 4 if refused). |
| `agent` | `fbench agent A -- maint status` / `fbench agent --contract` | Raw passthrough; `--contract` prints the assumed agent CLI/JSON. |
| `reg` | `fbench reg A p25 read product_id` | Allow-listed; read-to-clear registers need `--allow-side-effect`; writes to `ro` registers refused (exit 4). |
| `tx` | `fbench tx B off` | Emergency: agent `tx off`, or libiio fallback (hardwaregain −89.75, DDS scale 0). |
| `regmaps` | `fbench regmaps build` / `regmaps show --core p25` | Regenerates `share/adi_regs.json` and `ps_regs.json`. `share/p25_regs.json`, the radio core's map, is written by the FPGA build (scanner-hdl's `radio_core.bench_map`). |
| `console` | `fbench console A --seconds 120 --until "login:"` / `console --list` | FT2232 DEBUG UART (115200 8N1); log in `run_*_console_A/artifacts/console.log`. Needs pyserial. |

### Exit codes

| Code | Meaning |
|---|---|
| 0 | pass |
| 1 | fail (a threshold was violated; `compare`: regression) |
| 2 | error (bug, exception, bad usage) |
| 3 | precondition (unit unreachable, wrong image, agent missing, file missing) |
| 4 | safety refusal (interlock, register allow-list) |
| 5 | inconclusive (ran, but the data cannot support a verdict) |

Suites aggregate by severity: safety > error > fail > precondition > inconclusive > pass.

### JSON output

Every document starts with `ok`, `verb`, `exit_code`; errors carry `error` and `kind`.

```json
{
  "ok": false,
  "verb": "safety",
  "exit_code": 4,
  "allowed": false,
  "tx_atten_db": 0.0,
  "cabled_confirmed": false,
  "budgets": [{"tx": "A.TX1A", "rx": "B.RX1A", "pad_db": 30.0, "p_rx_dbm": -10.0,
               "level_ok": true, "linear_ok": false, "allowed": false,
               "reasons": ["rf.cabled_confirmed is false: ..."],
               "warnings": ["worst-case P_rx -10.00 dBm above the linear limit ..."]}]
}
```

`run` returns one entry per test:

```json
{
  "ok": true, "verb": "run", "exit_code": 0, "suite": null, "verdict": "pass",
  "runs": [{"test": "iface.clk_freq", "run_id": "20260926_153000_iface.clk_freq",
            "run_dir": "runs/bench/2026-09-26/bench/run_20260926_153000_iface.clk_freq",
            "verdict": "pass", "exit_code": 0,
            "summary": "interface clock 16.0001 MHz vs expected 16.0000 MHz ..."}]
}
```

## Run directories

`runs/bench/<YYYY-MM-DD>/bench/run_<YYYYMMDD_HHMMSS>_<test_id>/` containing
`result.json` (schema `fbench.result/1`, exactly the keys of design doc §5.2; unit
identity adds `transceiver` and `label`), `params.json`, `units.json` (roles, identity,
agent info, IIO context), `log.txt`, `artifacts/`, `FINDINGS.md`. Suites also write
`suite_<ts>_<name>.json` next to the run dirs. Board-side bulk data goes to
`/mnt/sd/bench/runs/<run_id>/` and is pulled into `artifacts/`.

## Safety rules (enforced)

1. **Cabled-only TX**: TX tests need `rf.cabled_confirmed = true` and an `[[rf.links]]`
   entry for the TX unit/port, else exit 4 before any hardware access.
2. **Level interlock**: `P_rx = tx_max_dbm − tx_atten − pad_db` (worst case, `tx_max_dbm`
   +20 dBm for the PGA-102+ boards). Refused when `P_rx > rx_abs_max_dbm` (strict: −10.0
   exactly is allowed), warned above `rx_linear_max_dbm` (−30 dBm). Sweeps are checked at
   their smallest attenuation. Example: A→B at 0 dB → 20 − 0 − 30 = −10 dBm → level OK,
   warning; still refused while `cabled_confirmed = false`.
3. **Attenuation first**: stimulus code writes `out_voltage0_hardwaregain` before
   enabling any source; after every TX test the runner issues `tx off` in a `finally`
   (agent, or libiio fallback), before leaving maintenance mode. A failed `tx off` turns
   the verdict into `error` and raises an alert in `status`.
4. **Maintenance mode**: tests marked M run `maint enter` (stops the scanner) and always
   `maint exit` afterwards;
   `rf.refclk_eth` enters it itself only when it must change the RX rate.
5. **Register allow-lists**: `reg` only touches registers listed in `share/*.json`
   (offsets above the radio core's 1 KB window are refused: the bus aliases there).
6. **Storage**: the bench only writes under `/mnt/sd/bench/**` (and `/tmp` on the board
   for RAM-first captures). `setup net --apply-env` is the only persistent config
   change and needs the explicit flag.

Unit identity: the IIO `hw_serial` follows the SD card, so it is only a hint
(`known_serials`); `fpga_dna` (hwval image) is authoritative. Nothing is keyed by IP.

## Tests

`fbench list` is authoritative. Tier 0 runs on the production image; Tier 1 (`hw.*`)
needs `fbench boot <unit> hwval`. RF tests record the direction and both transceivers
(AD9361 vs AD9363) — run both directions (`--tx A --rx B` and `--tx B --rx A`).
`rf.isolation` is a two-step test (`-p phase=cabled`, then remove the cable and
`-p phase=open -p reference=<first run dir>`). `sys.boot_log` records the UART while you
power-cycle the unit.

### Adding a test

1. Pick the family module in `fbench/tests/` (or add one and import it in
   `fbench/tests/__init__.py`).
2. Write an `analyze_x(a: AnalysisContext) -> Outcome` that reads only
   `a.load_json(...)`/captures and `a.params`, records `a.metric(name, value, min=,
   max=, eq=, severity=)`, and returns `a.outcome(summary)` (verdict from thresholds
   unless given).
3. Write the acquisition function and register it:

   ```python
   @bench_test("xport.my_test", tier=0, units="any", maintenance=False, tx=False,
               params={"seconds": 10.0}, description="...", pass_criteria="...",
               artifacts=("my.json",), suites=("transport",), analyze=analyze_x,
               duration_param="seconds")
   def xport_my_test(ctx: TestContext) -> Outcome:
       unit = ctx.roles["dut"]
       ctx.require_agent(unit)
       ctx.save_json("my.json", ctx.agent.run(unit, ["...", "--seconds", "10"]))
       return analyze_x(ctx)
   ```

4. Add agent fixtures under `tests_host/fixtures/agent/` and, if needed, a scenario in
   `tests_host/test_catalog.py::SCENARIOS`. Every registered test is run on the simulated
   bench by `test_each_test_runs_green_on_simulated_bench`.

Raise `PreconditionError` (exit 3) for missing capabilities, `Inconclusive` (exit 5)
when data cannot support a verdict; the runner maps everything else to `error`.

## Frequency sweep (`rf.freq_sweep`)

One cabled direction across the tuning range: 84 points from 70 MHz to 6 GHz, with every
stimulus method the TX unit supports (`pattern` and `cyclic` on the scanner image, `dds` on
the hwval and factory images), an estimated 3 s per point per method (not yet timed on
the units). Both units go into maintenance mode (the scanner stops). Per point it records:

- both synthesizers' lock bits and the LO read-backs;
- the tone's level and SNR at a fixed RX gain;
- the TX vs RX reference offset (a jump means an LO that did not land);
- the RX image, TX LO leakage and TX image (after a TX quadrature calibration);
- the strongest spur.

Results inside and outside each unit's specified range (the AD9363's 325 MHz-3.8 GHz) are
reported separately.

A run's absolute level mixes the TX board (its PGA-102+ gain block rolls off above about
1.5 GHz), the cable and pads, and the RX board. To separate them, move **one** padded cable
through four positions and run the sweep at each. Before each move, set the `[[rf.links]]`
entry in `bench.toml` to the path you cabled: the interlock refuses a TX that is not listed.

| Cable | Run |
|---|---|
| B.TX1 → pads → A.RX1 | `fbench run rf.freq_sweep --tx B --rx A --json` |
| B.TX1 → pads → B.RX1 | `fbench run rf.freq_sweep --tx B --rx B --json` |
| A.TX1 → pads → A.RX1 | `fbench run rf.freq_sweep --tx A --rx A --json` |
| A.TX1 → pads → B.RX1 | `fbench run rf.freq_sweep --tx A --rx B -p compare=<dir1>,<dir2>,<dir3> --json` |

`compare` (or `fbench analyze <run_dir> -p compare=...` afterwards) writes `compare.json`
and `compare_diff.png`. Two runs with the same TX unit give the RX difference (`RX B - RX
A`), and two runs with the same RX unit give the TX difference. The four positions give
each difference twice, through different boards, and the two estimates should agree. The
two cross directions alone still give the lock, read-back, image, leakage and SNR results
for both units' TX and RX; only their levels stay mixed. The comparison warns when the runs
used different pads, gain, attenuation or sample rate.

## Replay corpus (`rf.p25_corpus`)

Many recordings replayed from the TX board (B) into the DUT (A, the scanner), each
scored per transmission against SDRTrunk's decode of the same air. Design and
inventory: [doc/changes/058_replay_corpus.md](../doc/changes/058_replay_corpus.md).

1. Inventory + manifest (read-only on the SDRTrunk dirs, ~15 s; ffmpeg decodes the
   focus call's MP3 once for the reference tone):

   ```bash
   $PY tools/p25_corpus_index.py            # -> bench/.state/corpus/manifest.json + report
   ```

2. Deploy the agent with `replay stream` on B: `bench/scripts/build_agent.sh`, then
   `$PY bench/fbench.py setup agent --unit B --json`.
3. Stage a mode on B's SD card once (rendered while uploading, ~9 MB/s, no local
   copies; cached by name/size, checked with `ls -ln`, sha256 in
   `bench/.state/corpus/staged_B.json` and next to each file on the card):

   ```bash
   $PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=B -p stage_only=true --json
   ```

4. Run it (the DUT is A, so always `--tx B --rx A`):

   ```bash
   $PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=B -p items=focus --json
   $PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=A --json
   $PY bench/fbench.py run rf.p25_corpus --tx B --rx A -p mode=A -p a_unit=window -p source=ram --json
   ```

| Mode | Item | Content | Notes |
|---|---|---|---|
| `A` | one wideband capture (`a_unit=whole`, default) or window (`a_unit=window`) | the real air, `cs12` (lossless 12-bit), 4 MSPS | TX LO trimmed by `units.A.ref_ppm - units.B.ref_ppm` (captures carry A's uncorrected reference) |
| `B` | one scene: a CC recording + the traffic recordings overlapping it | 50 kSPS channel recordings up-converted to their RF offsets and mixed (`cs8`, 3.5/4/5 MSPS, AWGN `noise_db` below each channel; TX centre keeps IQ images and LO leakage >= 100 kHz off every channel) | placed on their SDRTrunk log clocks (+-30 ms) with each recording's SDRTrunk session frequency offset removed (`correct_hz`); TX LO trimmed by `-units.B.ref_ppm` |

Selection: `items=all|focus|<id>,<id>`, `limit=N`, `max_minutes=M`. Stop
gracefully with `touch bench/.state/corpus/STOP` (checked every tap poll; the
current relay is stopped, TX off, maintenance exited, completed items analysed);
continue later with `-p resume=<run dir>` or `-p resume=auto` (the latest run of
that mode). `-p purge=true` with `stage_only` deletes corpus files the current
selection does not use.

On the TX board each item runs `fbench-agent replay stream --playlist … |
iio_writedev -u local: -b 262144 cf-ad9361-dds-core-lpc voltage0 voltage1` in
its own session (`setsid`, pid file, `pkill` stop). The relay's RAM ring
(`ring_mb`, default 192 MiB, prefilled before the first sample airs) holds 16 s
of `cs12` at 4 MSPS (29 s of `cs8` at 3.5 MSPS): the card's ~23.6 MB/s refills it
at ~11.6 MB/s net, so multi-second SD stalls cost nothing. Any underrun is
counted (`relay_underruns`, fail above `max_underruns`), with the stream
position, in `items/<id>.json` `relay`.

On the DUT the test polls `/api/imbe_dump` (every `tap_period_s` = 0.5 s; the ring
holds the last 128 frames, 2.56 s of voice, after a baseline dump taken before the
stream starts) and `/api/ui/calls` (15 s), and reads `/ws/audio`.

Scores (`scores.json`, metrics in `result.json`):

- **Recovery (the verdict):** the DUT's own per-call counts. Each `.mbe`
  transmission is matched to the `/api/ui/calls` call with the same TG and source
  whose open interval covers it (DUT clock offset voted from the call starts), and
  the call's `imbe` (exact per call_id since 057) is credited to its transmissions
  in time order, each up to its truth frame count (`excess` keeps the rest).
  Clear-voice recovery is taken over *followable* transmissions: not encrypted,
  and not the loser of two overlapping calls, since one traffic chain follows one call.
- **Bit accuracy (report only, never changes the recovery):** the tapped raw
  144-bit codewords aligned in order with SDRTrunk's (`hex_aligned_*`,
  `hex_exact_pct_of_aligned`, `hex_mean_bit_diff`; two receivers differ in the
  bits the IMBE FEC corrects, about 2 bits per frame on the 05:44 scene).
- Missed transmissions, worst items, close reasons, relay health, and for focus
  items the tone check (per-tone mean / std / max deviation, dropouts, `/ws/audio`
  arrival gaps and lag events).

## Host tests

```bash
cd /c/Users/Andy/Projects/MAIA_SDR/maia-sdr/bench && ../.venv-hdl/Scripts/python.exe -m pytest -q tests_host
```

The suite never touches the network: `tests_host/conftest.py` simulates both boards
(IIO attributes, DAC registers, a CW whose frequency and level follow the TX settings,
the DUT's calls and IMBE tap, UART boot log) behind fake SSH/HTTP/libiio/agent objects.

## Agent contract

`fbench agent --contract --json` prints the subcommands and reply keys the host
assumes (`fbench/agent.py::CONTRACT`, checked against `agent/src/cmd/*.rs`). Error
replies `{"ok": false, "error": …, "code": …}` map `safety`/`refused` → exit 4,
`unknown_command`/`precondition`/`unsupported`/`no_device`/`wrong_image`/`not_found` →
exit 3, anything else → exit 2. The agent rejects unknown options, so the host adds none
beyond what each command reads (`[agent] extra_args` exists for emergencies).

Integration points:

- The runner passes `--run-id <run_id>` on every agent call during a run, so agent-side
  bulk files land in `/mnt/sd/bench/runs/<run_id>/`.
- `--tx-ok` is added only by TX tests after the host interlock passed (the agent refuses
  TX-enabling `iio attr set` values and TX-affecting register writes without it); a
  non-TX test asking for it is refused by the runner.
- Ring tests let the agent drive the stimulus (`ring check --bist prbs --enable
  --release-reset`); the lap test sends one `--stall-ms` list and reads `stalls[]`.
- `boot install` always stages to `images/.incoming_<name>/` and passes `--from` plus
  both sha256 values: without `--from` the agent treats `install` as `select`.
- `share/*.json` replace the agent's built-in core maps of the same name, so
  `regmaps build` merges onto `agent/maps/*.json` (agent names kept, host names as
  `aliases`, host `expected` values and extra registers added). Re-run it after the
  agent's maps change; `tests_host/test_regmaps.py` flags a stale `share/`.
- `mem.canary -p regions=auto` resolves regions from `audit.reserved_memory` (no-map, not
  an rxbuffer ring), because the agent needs real node names.
