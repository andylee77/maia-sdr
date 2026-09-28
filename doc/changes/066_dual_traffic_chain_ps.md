# 066 — p25-httpd follows two calls at once (second traffic chain)

**Date:** 2026-09-27. **Branch:** fishball-p25. **Bake required:** no (uses core 0.3.0, change
064). **Needs** for chain 2: core 0.3.0 and the `p25-traffic2-lsm-dibit` device-tree node.
Every other core runs one chain, exactly as before.

Core 0.3.0 has a second traffic decode chain. p25-httpd now uses it: chain 1 follows the left
speaker's talkgroup groups and chain 2 the right's, so a TG 300 call on the left no longer
stops a TAC or hospital call on the right, and the browser plays both at once.

| Phase | What |
|---|---|
| A | Hardware layer: version-gated chain-2 registers, DMA ring, interrupt, bring-up endpoint |
| B | Per-chain ("lane") objects; the lifecycle, recorder, grant stats, heartbeat and reader serve any chain |
| C | Two lanes: chain choice per grant (`choose_lane`), cross-chain rules, `--traffic-chains` |
| D | Live audio of both chains (`/ws/audio?v=2`, two mixed rings), one call card per chain |
| E | Bench |

## The safety rule (phase A)

On a core older than 0.3.0 the `traffic2_*` addresses (0x120–0x168) are vacant, and an AXI
read of a vacant address stalls the CPU until a power cycle. So no code may touch them unless
the core has the chain:

- `CoreVersion::has_traffic2_chain()` (version ≥ 0.3.0).
- `IpCore.traffic2: Option<Traffic2Hw>` is `Some` only when the version says so **and** the
  DMA node `p25-traffic2-lsm-dibit` opened. A missing node logs a warning and leaves one chain;
  it does not fail start-up.
- The chain-2 register methods (`t2_*`) are private. The only public way to them is
  `IpCore::lane(Lane::Two)`, which returns `None` without the chain. `LaneRegs` is the
  per-chain register view; lane One delegates to the existing `traffic_*` methods, which are
  unchanged.
- `DibitRing::Traffic2` is inert without the chain: geometry `{0, 0}` (invalid, so a reader
  would pin itself to legacy mode), a zero snapshot, no register reads.
- Interrupt bit 8 is read inside the existing `interrupts` read. It reads 0 on older cores, so
  counting it is safe everywhere.

## How the two chains share the work

- **Chain choice** (`app/lane_policy.rs`, pure, host-tested). Left groups → chain 1, right
  groups → chain 2, talkgroups on "both" (the "other talkgroups" setting) → either chain,
  preferring the chain already tuned to the grant's frequency, then the side that carries no
  groups, then chain 1. Within the grant's candidate chains the pre-066 rules apply: take an
  idle chain, stay with a talkgroup the chain already follows (calls never move between
  chains), pre-empt at the locked call's end marker (059) or for a higher-priority group
  (063), else reject. A busy side does not borrow the other side's chain: that chain stays
  free for its own side, and one speaker never carries two calls. With one chain everything
  goes to it and the decisions are the pre-066 ones.
- **One call lifecycle, one slot per chain** (`grant_follower.rs`). One call-id space, one
  list of not-followed calls, one grant dedup map. Voice events (HDU, NIDs, link control, end
  markers) and audio carry their chain; a grant update refreshes the call of its talkgroup on
  whichever chain. Cross-chain rules: a followed grant on a frequency another chain's call
  holds ends that call (the channel moved); a not-followed grant ends the call on its
  frequency, whichever chain; stream lag closes both.
- **One follower** (`grant_follower_routing.rs`, moved out of `grant_follower.rs`) drives both
  chains: channel-reuse and encrypted teardown on every chain, then `choose_lane`, then the
  retune or same-frequency resume on the chosen chain through `IpCore::lane`. Per-chain
  memory: last frequency, last call quality, timeout and sticky-reject re-follow.
- **Per chain** (`app/traffic_lane.rs::build_lane`): `TrafficChain`, framer / voice decoder,
  `ImbeForwarder` (lane-stamped), dibit reader, LSM heartbeat (`app/traffic_heartbeat.rs`,
  moved out of `main.rs`), vocoder thread (`p25-vocoder`, `p25-vocoder2`), audio pacer,
  recorder task, AGC reading. Shared (`ForwarderShared`): encrypted-talkgroup history,
  per-frequency AGC cache, per-call counters.
- **grant_stats** keeps one open summary per chain; a call on chain 2 no longer forces the
  chain-1 summary closed as "timeout".
- **Recorders:** one task per chain on the shared store; each ignores the other chain's audio
  and calls.

## Audio and UI (phase D)

- `/ws/audio` carries chain 1 only, in the old format (the bench's `wsaudio.py` uses it).
  `/ws/audio?v=2` carries every chain: each binary frame starts with `[lane, 0, 0, 0]`, each
  meta frame names its `lane`. `/api/audio?chain=1|2` streams one chain.
- The player (`ui/js/audio/ring.js`, `sources.js`, `player.js`) keeps one jitter ring per chain
  and mixes them into left and right; each sample keeps its talkgroup's pan (change 062), and
  the sum is clipped. The ring class is shared by the AudioWorklet (its source text is
  injected) and the http ScriptProcessor fallback.
- `/api/ui/state` gains `calls[]` and `chains[]` (`call` / `chain` stay chain 1's). The Now
  page shows a card per chain ("Left speaker · chain 1", "Right speaker · chain 2"); chain 2's
  card hides with one chain. Recent calls show "chain 2" under the frequency and a "Traffic
  chain" detail row; the Speakers panel says whether one or two chains run.
- `/api/traffic2`: chain-2 registers and the chain-2 call when the follower runs it.

## Operating it

`--traffic-chains auto|1|2`, default `auto`: every chain the core and device tree offer (two
on core 0.3.0 with the chain-2 node). `1` forces the pre-066 single chain. `/api/traffic2`
answers 409 without the chain; while the follower uses chain 2, its bring-up controls need
`force=1`.

Bring-up without the follower (chain 2 idle):

```sh
curl "http://192.168.2.1:8080/api/traffic2?freq_hz=860962500&probe_ms=10000"
```

## Bench

- **0.3.0 regression, 065 PS:** see doc 064 (identical to 0.2.0: 99.3 %, 0 missed).
- **Chain 2 bring-up** (066d, B replaying Mode B item 1, chain 2 tuned to the control channel
  860.9625 MHz): NID NAC 0x8A1 valid, PLL 530, AGC gain 24.9, interrupt bit 8 twice in 10 s
  (one per 4 KB sub-buffer), and the probe decoded 268 TSBKs (91 frame syncs, opcodes
  0x00 / 0x01 / 0x02) from 8 KB of the chain-2 ring. The whole chain-2 path works: DDC, LSM,
  NID, packer, DMA master, interrupt, device-tree ring.
- **Single chain, 066 vs 065** (first 10 Mode B items, run `run_20260927_204309`): 4968 vs
  4977 of 5121 clear frames; one transmission one LDU short, the rest identical (replay
  noise). The refactor changes nothing with one chain.
- **Two chains, full Mode B corpus** (`--traffic-chains 2`, bench routing: Primary = TG 300
  left; TAC 301-310 and Hospital 315-325 right; other talkgroups right; run
  `run_20260927_205410`), against the single-chain 0.3.0 run the same evening:

| | One chain | Two chains |
|---|---|---|
| Followable clear transmissions | 219 | 230 |
| IMBE frames of SDRTrunk's, followable | 33795 / 34038 (99.3 %) | 35208 / 35361 (99.6 %) |
| IMBE frames of SDRTrunk's, **all** clear transmissions | 95.2 % | **99.1 %** |
| Missed / partial transmissions | 0 / 6 | 0 / 3 |
| Relay underruns | 0 | 0 |

  Of the 12 clear transmissions the single chain lost to an overlapping call, 11 are now
  decoded in full (TG 300 against 301, 850 and 319: left against right). The 12th, TG 318
  against 319, stays one-at-a-time by design: both are Hospital, same side, same rank. One
  transmission scored 81 of 117 instead of 117: a back-to-back hand-over on chain 1 split
  the pair differently (the next talker scored 72 of 72 instead of 9 of 72), not a dual-chain
  effect. CPU with both chains live: about 8 % and 19 % of the two cores.

## Tests

p25-httpd host tests 269 (+22, before 067): core version, lanes available, FIR RAM images equal to the
chain-1 loader for every preset, `choose_lane` (8 cases on the bench routing), two-chain
lifecycle (5), two-chain grant stats, lane objects sharing site state. The Linux-only code
(`fpga.rs`, the follower, `main.rs`) type-checks with `cargo-zigbuild check --target
armv7-unknown-linux-gnueabihf.2.31`. The audio ring and the injected worklet source were run
under node 20 (two lanes panned left and right, clipping, priming).
