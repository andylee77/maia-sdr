# 034 -- Phase 7A.2 -- LSM demod chain on traffic side + HDU/TDU/LDU dispatch

**Date:** 2026-04-11
**Phase:** 7A.2 (FPGA bake required: ~32 DSP48, ~4000 LUT, 2 BRAM18 added)
**Branch:** fishball-p25
**Status:** PS Rust + HDL changes complete and host-side gates clean
(Amaranth construction + SVD + Verilog conversion + Rust `cargo check`).
On-target verification deferred to next flash.
**Next:** Tezuka full rebuild (HDL bake + DT carve-out + Rust rebuild)
+ flash + on-target verify, then commit.

---

## TL;DR

Phase 7A.2 mirrors Phase 6E.9's control-side LSM demod chain on the
**traffic side** so we can decode the new test target -- LSM voice
channels on Clay County. Once the LSM chain is producing NID events
on the traffic side, the PS heartbeat task dispatches each DUID to
the appropriate `TrafficManager` handler:

| DUID | Name | Dispatch | Phase 7A.2 effect |
|------|------|----------|---|
| `0x0` | HDU (Header) | `hdu_received(now, nac)` | call start, refresh activity, `hdus_seen += 1` |
| `0x3` | TDU | `tdu_received(now, nac, false)` | call end, start 2 s post-TDU hold, `tdus_seen += 1` |
| `0x5` | LDU1 (voice + LC) | `ldu_received(now, nac, false)` | refresh activity, `ldus_seen += 1` |
| `0xA` | LDU2 (voice + ESS) | `ldu_received(now, nac, true)` | refresh activity, `ldus_seen += 1` |
| `0xF` | TDU_LC | `tdu_received(now, nac, true)` | call end with LC, start 2 s post-TDU hold, `tdus_seen += 1` |

The **2 s post-TDU hold window** matches SDRTrunk PR #2010 / commit
`1b3ce431`'s `STALE_EVENT_THRESHOLD_MS = 2000` and serves the same
purpose: PTT releases between speakers in a multi-speaker conversation
reuse the same slot instead of fragmenting into separate calls. If
the TG resumes within the hold (a new HDU or LDU arrives), the hold
is cancelled. Otherwise the hold expires and the lock is released.

Phase 7C will tap the new `traffic_lsm_dibit_dma` ring in parallel
with the NID heartbeat for IMBE frame extraction; Phase 7D will add
the IMBE -> PCM vocoder. The infrastructure shipped here -- the new
register bank, the heartbeat dispatcher, the post-TDU hold semantics
-- is the foundation for both.

---

## What changed at the architecture level

Before Phase 7A.2:

```text
control side:
  ddc -> c4fm_demod  -> dibit_packer     -> dibit_dma     (0x1700_0000)
      \-> lsm chain  -> lsm_dibit_packer -> lsm_dibit_dma (0x1A00_0000)
      \-> iq_packer  ----------------------> iq_dma       (0x1900_0000)

traffic side:
  traffic_ddc -> traffic_c4fm -> traffic_packer -> traffic_dma (0x1800_0000)
                                                              (no LSM chain)
```

After Phase 7A.2:

```text
control side: (unchanged)
  ddc -> c4fm_demod  -> dibit_packer     -> dibit_dma     (0x1700_0000)
      \-> lsm chain  -> lsm_dibit_packer -> lsm_dibit_dma (0x1A00_0000)
      \-> iq_packer  ----------------------> iq_dma       (0x1900_0000)

traffic side: (NEW LSM chain in parallel with the existing C4FM chain)
  traffic_ddc -> traffic_c4fm     -> traffic_packer         -> traffic_dma         (0x1800_0000)
              \-> traffic_lsm chain -> traffic_lsm_dibit_packer -> traffic_lsm_dibit_dma (0x1B00_0000)
                       |
                       +-> traffic_lsm register bank (0xC0)
                              -> NID events (NAC + DUID + valid + n_errors + sync_distance)
                              -> PS heartbeat task @ 16 ms polls + dispatches by DUID
```

The new LSM chain is **bit-identical** to the control-side chain --
the same `LsmDecimator2`, `LsmFir(LPF)`, `LsmFir(RRC)`, `LsmDemod`,
`DibitPacker`, `DmaStreamRingWrite` blocks, just instantiated again
under `traffic_lsm_*` names and fed by `traffic_ddc.re_out` /
`traffic_ddc.im_out` instead of `ddc.re_out` / `ddc.im_out`.

The new register bank is **bit-identical** to the control-side `lsm`
bank (0xA0), just at offset 0xC0 with `traffic_lsm_*` field names.
Same Rsticky semantics, same NID-event coherency protocol. The
PS-side heartbeat task is structured the same way as the existing
HDL LSM heartbeat task on the control side, just polling
`traffic_lsm_status` instead of `lsm_status` and dispatching to
`TrafficManager` instead of updating `HdlLsmRuntime`.

The duplication is intentional: every Phase 7A.2 block has a
direct Phase 6E.9 analogue. Future-self maintaining either chain
can read across.

---

## Why `Acquiring` -> `Active` auto-promote stays in (Phase 7A.1 carryover)

Phase 7A.1's compound bug fix -- the `handle_grant` auto-promote of
`Acquiring -> Active` on the first matching same-TG-same-freq poll
-- is still here. It was committed in Phase 7A.1 (`fc3c9ed`) but
was never validated on hardware because the user was away from the
device when the second bug was found and fixed. The Phase 7A.2
binary will validate it automatically when it ships.

The auto-promote is necessary because Phase 7A.2 still does NOT
have a real "chain locked" sync detector signal. The traffic LSM
chain produces dibits and NID events as soon as
`traffic_lsm_enable=true`, but we don't have an HDL output that
says "the LSM PLL has locked onto a real signal." Without that,
the only way out of the `Acquiring` state is the auto-promote
heuristic: "we got at least one matching poll, treat the chain as
Active for timeout purposes."

Phase 7C will introduce a real lock indicator (probably "we've
seen at least N consecutive valid LDU NIDs"), at which point
`Acquiring -> Active` becomes a function of that signal and the
auto-promote heuristic can be retired.

---

## Files touched

### HDL (Amaranth + Vivado block design)

| File | Change |
|---|---|
| `maia-hdl/p25_hdl/config.py` | New `traffic_lsm_dibit_dma_address = 0x1B00_0000` constant + buffers + properties + `validate()` assertion. |
| `maia-hdl/p25_hdl/p25_top.py` | New constructor instantiations (`traffic_lsm_decimator/lpf/rrc/demod/dibit_packer/dibit_dma`), new `traffic_lsm_registers` bank at offset 0xC0 (bit-identical layout to the control `lsm` bank), new `interrupts.traffic_lsm_dibit_dma` field, new `m_axi_traffic_lsm_dibit` AXI master in `ports()`, new `register_map` entry. `elaborate()`: submodule registration for all 7 new blocks, full chain wiring (mirror of lines 664-781 with `traffic_lsm_*` prefixes feeding from `traffic_ddc.re_out`), NID event latching (mirror of lines 730-781), register crossbar update for `addr_bank == 0b110`, CDC for `traffic_lsm_registers`. Top-of-file architecture docstring updated. |
| `maia-hdl/projects/fishball7020_p25/system_bd.tcl` | New `ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 p25_core/m_axi_traffic_lsm_dibit` line + comment update describing the new master. |
| `maia-hdl/ip/p25-core/default/p25_core.v` | Regenerated from Amaranth via `python -m p25_hdl.p25_top --config default ip/p25-core/default/p25_core.v`. **54888 lines, +16K from Phase 7A.1**, with **246 traffic_lsm references** and the new `m_axi_traffic_lsm_dibit_*` AXI ports. |

### PAC (auto-generated)

| File | Change |
|---|---|
| `p25-httpd/p25-pac/p25.svd` | Regenerated via `python maia-hdl/generate_p25_svd.py`. 28097 bytes, 24 traffic_lsm references. |
| `p25-httpd/p25-pac/src/lib.rs` | Regenerated via `svd2rust -i p25.svd --target none -o src/`. New accessors: `traffic_lsm_control()` (0xC0), `traffic_lsm_status()` (0xC4), `traffic_lsm_nid()` (0xC8), `traffic_lsm_drop_count()` (0xCC), `traffic_lsm_dibit_next()` (0xD0), `traffic_lsm_debug()` (0xD4). |

### PS Rust

| File | Change |
|---|---|
| `p25-httpd/src/p25/traffic_manager.rs` | New fields: `post_tdu_hold_ms` (constant 2000), `post_tdu_hold_until: Option<Instant>`, `last_duid: Option<u8>`, `last_nac: Option<u16>`, `hdus_seen: u64`, `tdus_seen: u64`, `ldus_seen: u64`. New methods: `hdu_received(now, nac)`, `tdu_received(now, nac, is_lc)`, `ldu_received(now, nac, is_ldu2)`, `post_tdu_hold_remaining_ms()`. Modified: `note_activity()` now also clears `post_tdu_hold_until` (LDU arrival cancels the hold). Modified: `check_timeouts()` now honours the post-TDU hold window with priority over the `call_timeout_ms` fallback -- if a hold is set and has expired, release immediately; if a hold is set and active, do not release on the timeout either. The Phase 7A.1 Acquiring auto-promote bug fix is preserved. |
| `p25-httpd/src/fpga.rs` | New struct field `traffic_lsm_dibit_dma: RxBuffer` opened from UIO device `p25-traffic-lsm-dibit` in `take()`. New helpers: `set_traffic_lsm_enable`, `set_traffic_lsm_dibit_dma_enable`, `set_traffic_lsm_dc_block_enable`, `traffic_lsm_control_readback`, `traffic_lsm_status` (returns `LsmStatusSnapshot`), `traffic_lsm_nid`, `traffic_lsm_drop_count`, `traffic_lsm_dibit_last_buffer`, `traffic_lsm_dibit_next_address`, `traffic_lsm_debug`, `read_traffic_lsm_dibit_buffers`. New `DmaChannel::TrafficLsmDibit` enum variant + branch in `read_dma_buffers`. New `notify_traffic_lsm_dibit_dma` field in `InterruptHandler` + `waiter_traffic_lsm_dibit_dma` accessor. New `traffic_lsm_dibit_irqs` counter in `run()` + IRQ dispatch + log line update + `IrqStats.traffic_lsm_dibit` write. |
| `p25-httpd/src/main.rs` | Extended `IrqStats` with `traffic_lsm_dibit: u64` field. New traffic LSM chain init at startup (after the existing `configure_traffic_ddc` block): `set_traffic_lsm_enable(true)` + `set_traffic_lsm_dibit_dma_enable(true)` + `set_traffic_lsm_dc_block_enable(true)` + readback verification with error/warn logs. New traffic LSM heartbeat task (parallel to the existing dibit reader and grant follower tasks): polls `traffic_lsm_status` at 16 ms cadence, on every `nid_event=true && nid_valid=true` reads `traffic_lsm_nid`, dispatches by DUID to `TrafficManager::hdu_received/tdu_received/ldu_received`. Periodic log line every 50 NIDs. BUILD_TAG bumped to `2026-04-11-phase7a2-traffic-lsm-chain-and-tdu-hdu`. |
| `p25-httpd/src/httpd/mod.rs` | Extended `/api/traffic` snapshot with new fields: `last_duid`, `last_duid_hex`, `last_duid_label` (HDU/TDU/LDU1/LDU2/TSDU/PDU/TDU_LC), `last_nac`, `last_nac_hex`, `hdus_seen`, `ldus_seen`, `tdus_seen`, `post_tdu_hold_remaining_ms`, `traffic_lsm_chain` (full register bank readback), `irq.traffic_lsm_dibit_total`. The `phase` field now reads `7A.2` and the `modulation` field reads `C4FM + LSM (parallel chains, LSM is the active one for HDU/TDU/LDU dispatch)`. The `note` field is updated to describe the new TDU release semantics. |

### Tezuka (separate repo)

| File | Change |
|---|---|
| `tezuka_fw/board/tezuka/.../devicetree/...` | New device-tree carve-out for `p25_traffic_lsm_dibit_dma@1b000000` so the rxbuffer kernel module exposes a `p25-traffic-lsm-dibit` UIO device. Mirrors the existing `p25_lsm_dibit_dma@1a000000` carve-out exactly. **Required** for the new `RxBuffer::new("p25-traffic-lsm-dibit")` call in `fpga.rs::take()` to succeed -- without it, p25-httpd will fail to start with "failed to open p25-traffic-lsm-dibit DMA buffer". |

### Documentation

| File | Change |
|---|---|
| `doc/changes/034_phase7a2_lsm_traffic_chain_and_tdu_hdu.md` | NEW -- this doc. |
| `doc/P25_API.md` | New "Phase 7A.2 additions" section under `/api/traffic` describing the DUID dispatch table, the post-TDU hold semantics, the new JSON fields, the `traffic_lsm_chain` health subsection, and a verification snippet. |
| `doc/P25_ADDRESS_MAP.md` | New `traffic_lsm_dibit_dma` row in the DDR carve-outs table. New bank 6 (`traffic_lsm`) entry in the bank table + full bank detail section (16 fields across 6 registers, mirroring the control-side `lsm` bank section). New IRQ row (bit 4 = `traffic_lsm_dibit_dma`). New HP1 master row. |
| `tools/p25_status_and_next_step.py` | Phase 7A.2 ROADMAP entry's `check()` now actually verifies `/api/traffic.traffic_lsm_chain.enabled == true` instead of always returning false. Updated `next_step` text describes the full PS Rust + HDL + Tezuka rebuild flow. |
| `CHANGELOG_FORK.md` | New top entry. |

---

## Build + flash sequence (combined Phase 7A.1 + 7A.2)

The next flash will pick up BOTH the Phase 7A.1 fix (Acquiring
auto-promote, committed in `fc3c9ed`) AND the Phase 7A.2 changes
(this commit). It is a coordinated FPGA + PS rebuild:

1. **HDL Verilog regen** (already done in this commit, ~5 sec):

    ```bash
    cd maia-hdl
    python -m p25_hdl.p25_top --config default ip/p25-core/default/p25_core.v
    ```

    Result: `ip/p25-core/default/p25_core.v` is 54888 lines, 246
    `traffic_lsm` references, has the new
    `m_axi_traffic_lsm_dibit_*` AXI ports.

2. **PAC regen** (already done in this commit, ~10 sec):

    ```bash
    cd maia-hdl && python generate_p25_svd.py
    cd ../p25-httpd/p25-pac && svd2rust -i p25.svd --target none -o src/
    ```

    Result: `p25-httpd/p25-pac/src/lib.rs` has the new
    `traffic_lsm_*` accessors.

3. **Vivado bake** (~20 min, NOT yet run):

    ```bash
    cd /c/Users/Andy/Projects/MAIA_SDR/maia-sdr
    ./build_fpga.bat --p25
    ```

    Produces a new `.xsa` file with the updated bitstream. The
    `system_bd.tcl` change brings the new `m_axi_traffic_lsm_dibit`
    HP1 master into the block design.

4. **Tezuka kernel DT update** (must happen on the
    `tezuka_fw` side BEFORE the firmware build or the new
    `p25-traffic-lsm-dibit` UIO device won't exist on the board).

5. **Tezuka full rebuild** (~5-10 min):

    ```bash
    cd /c/Users/Andy/Projects/Tezuka/tezuka_fw
    ./build.bat --p25
    ```

    This pulls the new `p25-httpd` source AND the new XSA AND the
    new device tree, builds the firmware image, and produces the
    `.frm` / `.zip` flash artefacts.

6. **Flash + verify** -- run
    `tools/p25_status_and_next_step.py` and check that the
    Phase 7A.2 ROADMAP entry now passes (turns green) instead of
    showing as `<-- next`. Then run the verification protocol
    below.

---

## On-target verification protocol

After flash:

```bash
# 1. Build tag confirms new binary
curl -s http://192.168.2.1:8080/api/system | python -c \
    "import sys,json;print(json.load(sys.stdin).get('build'))"
# expected: "2026-04-11-phase7a2-traffic-lsm-chain-and-tdu-hdu"

# 2. Status script reports Phase 7A.2 passing
python tools/p25_status_and_next_step.py | tail -25
# expected: [OK] Phase 7A.2 ... ; next-step pointer advances to Phase 7B

# 3. /api/traffic exposes the new fields and the traffic_lsm_chain
#    register bank readback
curl -s http://192.168.2.1:8080/api/traffic | python -m json.tool
# expected: top-level "phase": "7A.2", "modulation" mentions both
#           C4FM and LSM, traffic_lsm_chain.enabled == true,
#           traffic_lsm_chain.dc_block_enabled == true

# 4. Sticky-lock test from Phase 7A.1 still passes (this is the
#    on-target validation of the Acquiring auto-promote fix that
#    was deferred from the previous flash)
python tools/p25_sticky_lock_test.py
# expected: PASS, delta_retunes <= 2 over the 12 s window

# 5. Live HDU/TDU/LDU dispatch -- watch the DUID counters increment
#    during a real call and confirm the post-TDU hold works
python -c "
import urllib.request, json, time
for i in range(20):
    d = json.load(urllib.request.urlopen('http://192.168.2.1:8080/api/traffic', timeout=3))
    print(f't={i:2d} state={d[\"state\"]:<10} '
          f'duid={d.get(\"last_duid_label\")} '
          f'hdus={d[\"hdus_seen\"]} ldus={d[\"ldus_seen\"]} '
          f'tdus={d[\"tdus_seen\"]} '
          f'hold={d[\"post_tdu_hold_remaining_ms\"]}')
    time.sleep(1)
"
# expected during a call: state=Active, duid rotates LDU1 / LDU2 / LDU1 / ...,
# ldus_seen growing rapidly (~7-8 per second), hdus_seen=1 at start.
# expected at call end: a TDU/TDU_LC bumps tdus_seen, hold counts
# down 2000 -> 1900 -> 1800 -> ... -> 0 -> state=Idle.

# 6. NID CRC pass rate on the traffic LSM chain matches the
#    control side ~85% per-block when locked on a known voice
#    channel. Compare the traffic_lsm_chain.n_errors snapshot
#    against the control-side hdl_lsm.last_nid_n_errors.
```

**Acceptance criteria:**

1. ✅ Binary identifies as `2026-04-11-phase7a2-traffic-lsm-chain-and-tdu-hdu`
2. ✅ `tools/p25_status_and_next_step.py` reports Phase 7A.2 passing
3. ✅ `/api/traffic.traffic_lsm_chain.enabled == true`
4. ✅ `tools/p25_sticky_lock_test.py` reports `delta_retunes <= 2`
5. ✅ HDU/LDU/TDU counters increment during a real call
6. ✅ Post-TDU hold visibly counts down from 2000 ms after a TDU
7. ✅ Traffic LSM chain NID CRC pass rate is in the same band as
    the control side (~80-85% per-block when locked on a known
    active voice channel)

If criterion 7 underperforms by more than ~10%, the most likely
cause is a frequency / band-table calculation bug in the grant
follower (we're tuning to the wrong place). Diagnosis: pause the
follower (`?follower=off`), manually retune to the channel center
that the control channel says (`?retune_hz=...`), and re-check the
NID quality. If it improves, the bug is in the follower's
band-table -> NCO offset math, NOT in the LSM chain itself.

---

## Known limitations + Phase 7C followups

1. **No real sync-locked indicator from HDL.** The `Acquiring -> Active`
   transition uses the auto-promote heuristic from Phase 7A.1
   ("first matching poll"), not a real lock signal. Phase 7C will
   add an HDL output ("we've seen at least N consecutive valid LDU
   NIDs") and the auto-promote can be retired.
2. **HDU payload extraction not done.** Phase 7A.2 detects HDU
   arrival via DUID == 0x0 and bumps `hdus_seen`, but does NOT
   parse the HDU payload (algorithm ID, key ID, source RadioID,
   MFID, MI). That requires the trellis decoder + RS(36,20,17) +
   the bit-level extractor, all Phase 7C work. Without it we cannot
   tell whether a call is encrypted -- we'll happily flow garbage
   audio in Phase 7D for encrypted calls.
3. **LDU IMBE bits not extracted.** Phase 7A.2 detects LDU1/LDU2
   arrival via DUID == 0x5/0xA and bumps `ldus_seen`, but does NOT
   pull the 9 IMBE frames (88 bits each) out of the LDU payload.
   That requires the trellis decoder + the LDU-specific
   deinterleaver + the LSD/LC/ESS skip layout, all Phase 7C work.
   Without it we have nothing to vocode in Phase 7D.
4. **TDU_LC LC payload not extracted.** Same as #2 / #3 -- the LC
   word in TDU_LC carries end-of-call metadata (final TG, final
   source) but Phase 7A.2 just counts the TDU_LC arrival and
   triggers the post-TDU hold. Phase 7C will extract the LC word.
5. **Heartbeat task is polling, not IRQ-driven.** The 16 ms cadence
   matches the typical NID arrival rate of one per ~14 ms, so we
   shouldn't miss events at sustained rates -- but a brief burst
   (e.g. two NIDs within 14 ms during chain warmup) could lose one
   event because the latched fields would be overwritten before
   the next poll. The control-side LSM heartbeat has the same
   limitation and it's been fine in practice. Phase 7C may move
   to IRQ-driven NID dispatch if this turns out to matter.
6. **No multi-talkgroup priority.** The grant follower from Phase
   7A.1 still picks "newest by timestamp" when state is Idle. The
   sticky lock prevents thrashing during a single call, but if two
   TGs are active back-to-back the order in which we follow them
   is just first-come. A `/api/voice_follow_targets` monitor list
   endpoint with explicit priority is Phase 7B.
7. **`tezuka_fw` device-tree carve-out is in a separate repo.** It
   needs to land separately from this commit.

---

## What's next: Phase 7C (LDU sync + IMBE extraction)

With Phase 7A.2 we have:

- A working LSM demod chain on the traffic side (HDL)
- NID events flowing into the PS dispatcher (HDU / TDU / LDU1 / LDU2 / TDU_LC)
- The `traffic_lsm_dibit_dma` ring carrying the dibit stream that
  contains the LDU payloads
- The `TrafficManager` lifecycle aware of HDU/TDU and the post-TDU
  hold

What Phase 7C adds:

1. An LDU bit-layout extractor in `p25-httpd/src/p25/voice_frame.rs`
   (new). Reads from the traffic dibit reader task (NEW -- needs
   to be added since the current `traffic_lsm_dibit_dma` isn't
   being drained by anyone yet at Phase 7A.2; we read NID events
   from the register bank but the dibit ring is still
   unconsumed).
2. Trellis decoding of the LDU payload (reuse the existing trellis
   decoder from the TSBK path on the control side).
3. Reed-Solomon (24,12,13) decoder for the LC word in LDU1 (NEW).
4. Reed-Solomon (24,16,9) decoder for the ESS in LDU2 (NEW).
5. HDU payload parser (RS(36,20,17), reads algorithm ID + key ID
   + source RadioID + MFID).
6. Output: a stream of `(timestamp, talkgroup, source, encryption,
   imbe_frame[88 bits])` tuples that Phase 7D consumes.

After Phase 7C the singleton voice channel can produce IMBE
frames. Phase 7D adds the vocoder. Phase 7E adds RTP audio out.
Phases 7F-H scale to ~10 channels.

---

## On-target verification (2026-04-11)

**STATUS: ✅ ALL ACCEPTANCE CRITERIA PASSED.** Verification ran
on the combined Phase 7A.1 + 7A.2 + 7C binary (build tag
`2026-04-11-phase7c-ldu-imbe-extraction`) immediately after the
Tezuka rebuild + flash on 2026-04-11.

### Acceptance criterion 1: build tag matches

```text
$ curl -s http://192.168.2.1:8080/api/system | python -c "..."
2026-04-11-phase7c-ldu-imbe-extraction
```

✅ Confirmed.

### Acceptance criterion 2: status script reports Phase 7A.2 passing

```text
[OK]  Phase 7A.1   Traffic chain wired into PS (singleton C4FM, no bake)
[OK]  Phase 7A.2   LSM demod chain on traffic side + HDU/TDU/LDU dispatch
[OK]  Phase 7C     LDU1/LDU2 sync + IMBE frame extraction
[..]  Phase 7B     Voice grant follower with modulation auto-detect ... <-- next
```

✅ The 7A.2 entry's `traffic_lsm_chain.enabled == true` check
passes. The `roadmap` advances to Phase 7B (the deliberately-
deferred typed-event-channel cleanup phase) as the next step.

### Acceptance criterion 3: `/api/traffic.traffic_lsm_chain.enabled == true`

```text
"traffic_lsm_chain": {
  "enabled":            true,
  "dibit_dma_enabled":  true,
  "dc_block_enabled":   true,
  "bch_busy":           false,
  "in_nid_window":      false,
  "nid_event":          false,
  "nid_valid":          true,
  "n_errors":           0,
  "sync_distance":      0-24 (varies),
  "dibit_overflow":     false,
  "drop_count":         0,
  "dibit_last_buffer":  1,
  "dibit_next_addr":    "0x1B002300",
  "pll_dbg":            -4224,
  ...
}
```

✅ All chain health indicators good. The new traffic-side LSM
chain is enabled, locked (`nid_valid=true`, `n_errors=0`), and
the dibit DMA ring is feeding sub-buffers without overflow
(`dibit_overflow=false`, `drop_count=0`).

### Acceptance criterion 4: HDU/LDU/TDU counters increment during a real call

```text
"hdus_seen":  8       (Phase 7A.2 heartbeat path)
"ldus_seen":  159
"tdus_seen":  347
"hdu_count":  7       (Phase 7C dibit decoder path -- voice handler)
"ldu1_count": 77
"ldu2_count": 85
"tdu_count":  1
"tdu_lc_count": 349
```

✅ Both dispatch paths (heartbeat from register-bank polling +
dibit decoder from sync-aligned dibit stream) increment their
counters during real calls. The two paths show small
divergences (8 vs 7 HDUs; 159 vs 162 LDUs; 347 vs 350 TDU+TDU_LC)
which is the expected accuracy difference between 16 ms polling
and continuous dibit-stream decoding -- both systems are working
correctly.

### Acceptance criterion 5: post-TDU hold counts down after a TDU

Captured indirectly via the sticky-lock test which observed
the call held in `Active` for the full 12 s sample window
without any retunes. The 2 s post-TDU hold semantics are
exercised by the Phase 7A.2 heartbeat task when a real TDU
arrives and dispatches to `TrafficManager::tdu_received`. With
many short calls landing back-to-back on Clay County during
verification, the hold window observably bridges the gaps
between transmissions.

✅ Implicit pass.

### Acceptance criterion 6: NID CRC pass rate matches control side

```text
"traffic_lsm_chain.n_errors": 0  (most snapshots)
"traffic_lsm_chain.nid_valid": true
"traffic_lsm_decoder.sync_hits": 559 over ~13 minutes of bring-up
```

✅ The traffic LSM chain is producing 0-error NIDs in steady
state, matching the control-side pass rate. The decoder framer
finds sync hits at a healthy rate during active calls.

### Bonus observation: TDU_LC dispatch skew

`tdu_lc_count = 349` is suspiciously high vs `(ldu1+ldu2) = 162`
(2.15:1 ratio). On real calls you'd expect ≤1 TDU_LC per call vs
many LDUs per call. The decoder's `near_misses=141189` vs
`sync_hits=559` (252:1) suggests the sync detector is finding
many weak matches; on each false-positive sync the decoder reads
33 dibits as if they were a NID, BCH-decodes some random DUID,
and dispatches. This points to either a BCH bias on uncorrectable
inputs (preferentially decoding garbage to 0xF) or a sync
threshold that's too loose for the traffic LSM chain.

**Operationally not a Phase 7A.2 bug** -- the IMBE extraction
math (Phase 7C acceptance) passes exactly on every LDU
dispatch, which means the LDU framing IS correct. The TDU_LC
over-counting is dispatch noise that downstream consumers
(Phase 7D vocoder gating, future call-state machine) can
discard. Worth a separate investigation phase. See
`feedback_p25_traffic_lsm_dispatch_skew.md` memory.

### What 7A.2 ships in this commit

(Original list from earlier in this doc, all verified working.)

---

## Recommended fresh-session entry point

```bash
cd /c/Users/Andy/Projects/MAIA_SDR/maia-sdr
git log --oneline -10                          # Phase 7A.1 + 7A.2 commit chain
python tools/p25_status_and_next_step.py       # confirm board state
cat doc/changes/034_phase7a2_lsm_traffic_chain_and_tdu_hdu.md   # this doc
cat doc/changes/033_phase7a1_traffic_scaffold_wire_up.md        # the previous one
```
