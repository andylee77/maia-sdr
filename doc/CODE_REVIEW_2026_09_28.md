# Fishball P25 — Code Review (2026-09-28)

## Document metadata

| Field | Value |
|-------|-------|
| Review date | 2026-09-28 |
| Branch | `fishball-p25` |
| HEAD at review | `db84455` (change 070b) |
| Scope | `p25-httpd/` (~55k lines of Rust, ~3.9k lines of UI JS, `p25-json`, `p25-pac` use) |
| Method | Four read-only reviews in parallel: hardware + boot + DSP; `app/`; protocol + vocoder + audio; httpd + services + UI. Merged here. |
| Spot-checked by hand | 0x3C adjacent-status offsets, `Mapping` clone/unmap, wall-clock call timers, boundary-lag close, GET writes |
| Previous review | `doc/CODE_REVIEW_2026_04_16.md` |
| Purpose | Prepare 071 (find local systems), 072 (per-site history) and the refactor toward a multi-protocol scanner (`doc/ROADMAP.md`) |

## Summary

The radio pipeline is sound. Decode quality matches SDRTrunk on every path measured, and the
newer modules are well factored and tested: dibit air-time, ring math, settings/profiles, the
window planner, recording storage and the IMBE port.

The risk is in the glue that grew around them over ~70 changes:

- **No owner for tuning:** six code paths retune the radio with no common lock. 071 would add a
  seventh and remote libiio an eighth.
- **Timers on the wall clock:** calls are timed with the wall clock, which change 067 can now
  step.
- **Weak event plumbing:** a lossy boundary broadcast, and grants as the only typed protocol
  event.
- **Shared DMA ring cursors:** consumers steal each other's frames, which blocks 20 Hz plots and
  the 071 sweep.
- **An open, untyped HTTP API:** no auth, GETs that write, hand-built JSON.
- **Dead weight:** about 1,600 lines of retired or dormant app code, a C4FM decoder that is never
  fed, and comments that are 22–38 % of the code, over 700 of them dated narrative, some of them
  false.

Two defects bear directly on 071:

- The adjacent-site broadcast is decoded 4 bits off.
- PPM calibration searched around the boot control channel after a site switch (fixed in 070b).

## High findings

| # | Finding | Where | Effect | Fix |
|---|---------|-------|--------|-----|
| H1 | Adjacent Status Broadcast (0x3C) decoded 4 bits off (verified against SDRTrunk `AdjacentStatusBroadcast.java`) | `protocol/p25/tsbk.rs:998-1017` | System, RFSS, site and channel of every neighbour are wrong; 071 depends on them | Parse at SDRTrunk offsets (flags 24-27, system 28-39, RFSS 40-47, site 48-55, band/channel 56-71, service class 72-79); add a test vector |
| H2 | Radio retunes are not serialised | `api/tuning.rs` preset/tune/ppm, `api/sites.rs`, `app/recentre_task.rs`, `app/autoppm.rs` | Atomics (LO, rate, preset) and DDC writes can interleave; a grant followed during a preset change is lost; 071 and remote control add more writers | One `Tuner` service owning AD9361 + DDC state behind one async mutex, publishing a `TuningSnapshot`; a `RadioLease` (normal / moving / sweep / external) that the follower, recentre and autoppm honour |
| H3 | Call timers use the wall clock | `app/grant_follower.rs:616` (`close_due`, sweeps, queue expiry), follower `LaneMem`, `imbe_forwarder.rs:914`, `autoppm.rs:620`, `recentre_task.rs:190` | A clock step (067's first site-time step, NTP, `set_time`) closes every open call (forward) or leaves a lane sticky for the step's length (back) | `Instant` for all internal timing; stamp unix ms only on emitted events |
| H4 | Lag on the call-boundary broadcast closes every call | `audio/mod.rs:258` (`broadcast(64)`, one subscriber), `grant_follower.rs:1160-1174` | Every traffic NID and every grant TSBK goes through it; a 1–2 s stall (blocking file I/O in async tasks exists) drops all calls | mpsc with one consumer; grants lossless, voice NIDs `try_send` + counter; send only voice NIDs; resync instead of closing everything |
| H5 | A missed CallClose leaves a lane locked | follower `routing.rs:916-922`; `TrafficChain` has no timeout | The lane keeps its talkgroup and rejects every other one as sticky | Periodic check of each lane against its active call; release when there is none |
| H6 | `Mapping` is `Clone` and unmaps on `Drop` (verified) | `hardware/uio.rs:23-28,153-159`, `fpga.rs:170-171` | If the interrupt task exits, its copy unmaps the registers under `IpCore`: next register access segfaults (double munmap too) | `Registers(Arc<Mapping>)`, drop `Clone`; supervise the IRQ task |
| H7 | No task supervision | every `tokio::spawn` in `main.rs` / `app/` | A panic in the follower or lifecycle stops trunking while the web UI keeps serving | `spawn_supervised`: log to the event log and exit so `S60p25-httpd` restarts the daemon |
| H8 | No auth; GETs that write | e.g. `GET /api/rx_gain?mode=` (verified), `/api/traffic?follower=`, `/api/monitor?add=` (persists), `/api/decoder_reset`, `/api/sync_tune`; query-only POSTs (`/api/site`, `/api/set_time`, `/api/wideband_iq_capture`) | Any page open in the operator's browser can change the radio (CSRF); MCP and remote access would inherit this | GETs side-effect free; writes via POST/PUT with JSON bodies; token middleware (read / write scopes) off-localhost; Origin/Host check |
| H9 | The C4FM decoder is never fed but stays selectable | `main.rs:486, 975-984, 1736-1792`; `PUT /api/modulation` (only changes which decoder the UI reads) | Choosing C4FM blanks site panels, bands and the site clock; auto-detect compares a delta that is always 0 | Remove the decoder, the mode and the UI option until a real C4FM chain exists |
| H10 | Vocoder per-call accumulators grow without bound | `app/vocoder_task.rs:121,232,319` | A lane that only ever hears one talkgroup never flushes: ~100 MB/day | Key flushes and resets on `call_id`; cap or histogram the stage times |
| — | PPM calibration used the boot control channel | `app/autoppm.rs:156` | After a site switch it searched around the old site's CC | **Fixed in 070b** |

## Medium findings, by theme

### Events and the trunking model

- `P25Event` has one variant (`Grant`). Identity, IDEN bands, neighbours, secondary CCs,
  affiliations and registrations exist only as JSON strings in the TSBK feed, and
  `SystemIdentity` fields have no timestamps (`control_channel/tsbk_handlers.rs:216-252`).
  071 and 072 have nothing typed to subscribe to.
- Missing or wrong TSBK coverage:
  - unit-to-unit and interconnect grants (0x04/0x06/0x08) are listed but not decoded;
  - 0x38, 0x29, 0x27, 0x21, 0x2D, 0x1D and 0x1F are not decoded;
  - every manufacturer TSBK is opaque (Motorola 0x90 patch grants are neither followed nor
    counted);
  - the opcode name tables disagree in three places (`tsbk.rs:185`, `api/history.rs:380-451`,
    `tsbk_handlers`).
- TDMA IDEN bands resolve with the FDMA formula (`tsbk.rs:916-940`): a Phase 2 grant announced on
  the Phase 1 CC goes to the wrong carrier. Clay has TDMA bands.
- Voice FEC failures are used as if valid (`voice_frame.rs:619,724,984,1133`). This is a likely
  source of the "garbage RID" and phantom-encryption workarounds.
- The follower is one 650-line `select!` arm (`grant_follower_routing.rs:258-912`) with side
  effects between gates.
  - There are five different chain-release sequences.
  - Two grant tallies (`TrafficChain::grant_map`, `lo_plan`) and a lifecycle dedup each count
    grants; `lo_plan` counts each TSBK repeat of a TSDU (about ×2–3).
  - Lifecycle and follower coordinate through forwarder atomics.
- Per-site state is global: the encrypted-talkgroup history blocks a TG on every site after one
  encrypted grant anywhere, and `grant_map` mixes sites.

### Hardware access and streams

- DMA ring reads advance a cursor inside `IpCore`, so autoppm and `/api/spectrum_wide` take each
  other's spectra, and `/ws/iq`, `/api/spectrum` and the dumps split IQ buffers
  (`fpga.rs:1860-1969`, acknowledged at `ws.rs:252`).
  - This blocks 20 Hz plots, multiple viewers and the sweep.
  - Fix: one producer per stream, fanned out with `watch` / `broadcast`.
- Chain register code is written three times, about 900 lines:
  - the control FIR loader and the lane-1 FIR loader duplicate `ddc_fir_ram.rs`, which lane 2
    already uses;
  - lane-1 `traffic_lsm_*`, lane-2 `t2_*` and `LaneRegs` repeat each other.
  - The root cause is the prefixed SVD names; an SVD cluster or `dim` array, or a macro, removes
    it. 37 call sites bypass `LaneRegs` for lane 1.
- The wideband IQ DMA has three owners with no reference count, and its rate is hard-coded
  differently in two places (4 vs 8 MSPS). Either is wrong for most presets.
- `TrafficChain` computes the NCO from an LO that is never updated (`traffic_chain.rs:377-412`).
  After a preset change the NCO-skip decision can be wrong (masked most of the time; confirm on
  target).
- `notify_waiters()` in the IRQ handler can drop a wakeup (use `notify_one()`).
- There is undefined behaviour in `wideband_iq_task.rs:219`: an unaligned `&[i16]` cast.

### API, state and layering

- `AppState` has 54 fields; about 9 handles would do: Tuner, PPM, ControlChannel, Chains, Calls,
  Audio, Sites, Events, Diag.
- Four chain-1 alias fields and several endpoints still show chain 1 only (since 066).
- The app layer calls httpd handlers: `recentre_task` → `api::tuning::apply_preset`,
  `rec_storage` → `api::system::fs_usage`. `apply_settings_patch` and `release_chains_on` are
  domain operations living in handlers.
- Site activation is split between server and browser: `POST /api/site`, then the page calls
  `/api/preset auto`. `applied_preset` in the reply is not truthful. MCP, the handheld page and
  071's "Add site" would each have to repeat the sequence.
- The API is untyped: 7 of about 85 handlers return `p25-json` types, with about 214 ad-hoc
  `json!` sites.
  - The error model is inconsistent: some failures return 200 with `ok:false`; bodies mix plain
    text and JSON.
  - The endpoint catalogue is missing 20 routes and lists a retired one.
  - `P25_API.md` is missing 11 routes.
- Blocking work runs on the 2-core runtime:
  - settings are written to JFFS2 while a lock is held;
  - `list_sites` reads files on every request;
  - the FFT runs inline;
  - the `recordings/{id}/events` endpoint clones a 16k-entry log.
- Site names are not validated before being joined into a path (`services/sites.rs:215`).
  `switch_site` is accepted in a settings PUT.

### Dead code, duplication, comments

- **Retired or dormant, about 1,600 lines in `app/` alone:**
  - the seed-snapshot chain (built at 60 Hz, ignored by the retune);
  - `sw_demod_task` (off by default, wrong rate);
  - `wideband_iq_task`;
  - forensics (broken since 065/066);
  - the traffic PLL watchdog (cores before 0.2.0 only).
- **Dead items (grep-verified by the reviewers):**
  - about 20 `fpga.rs` functions, `iio::set_rx_rssi`, `lsm::{LsmStats, LastSync, ring}`, and the
    `sw_demod` re-exports behind the target warnings;
  - lifecycle fields and events nobody reads;
  - always-zero stats (`TrafficStats`, several `IrqStats` fields, `RecorderDiag` counters);
  - 15 routes with no consumer, including `/api/traffic_bins` and `/api/traffic_iq_dump`.
- **Duplication:**
  - `now_unix_ms` has 7 named copies and about 46 inline `UNIX_EPOCH` computations;
  - the NCO offset formula is written 7 times;
  - preset-index lookups appear 6 times;
  - NAC state is kept in 4 places;
  - the Golay tables are duplicated between jmbe and `voice_frame`;
  - there are 3 atomic JSON writers;
  - the TDULC FEC chain is written twice.
- **Comments:**
  - 22 % of httpd/services, 24 % of `app/`, 36 % of protocol, 34 % of audio;
  - over 700 dated, "Phase N" or "Change 0NN" narrative references.
  - Some are false:
    - `fpga.rs:509-514` says the traffic DDC API was removed; it exists;
    - `recorder.rs:28` says 15 s; the constant is 45 s;
    - `sites.rs:10-16` says the overlay is saved on every IDEN update, at `/mnt/data/p25` (it is
      never saved, and the path is wrong);
    - `lsm/mod.rs:10-38` says there is no consumer; `sw_demod` uses it.
  - Keep intent comments such as the ones in `dibit_ring.rs`; move history to the CHANGELOG.

## Target architecture

The four proposals agree. Merged:

```text
p25-httpd/src/                      (rename later: the scanner daemon)
  util/time.rs                      unix_ms(), monotonic helpers
  hw/                               drivers only; SimBackend on the host (removes the ~150 cfg stubs)
    mmio.rs  ad9361.rs (with a shadow of commanded values)
    p25core/{ddc,lsm,rings,irq}.rs  one bank impl per block (SVD cluster), notify_one
    presets/                        ddc_presets (generated), ddc_fir_ram, ddc_rate
  radio/                            the only way to move hardware
    tuner.rs                        apply(TuningPlan), nco_offset(), retune_lane(), watch<TuningSnapshot>
    lease.rs                        Normal | Moving | Sweep | External
    streams.rs                      SpectrumProducer (watch), IqStream (broadcast), DMA leases
    remote_watch.rs                 AD9361 read-back vs shadow; iiod clients on :30431
    sweep.rs                        071 band sweep
  protocol/
    mod.rs                          ControlDecoder / TrafficDecoder traits (pure, synchronous)
    fec/                            golay, hamming, bch (+ locked-NAC fast path), rs_gf64, trellis, crc
    p25/{framer,tsbk/{parse,opcodes,osp},control,voice,lc,diag}.rs
    dmr/                            later (reuses fec/, the dibit rings, the software LSM lane)
  trunking/                         protocol-agnostic, host-tested
    ids.rs                          TalkgroupId(u32), UnitId(u32), SiteKey, LogicalChannel{freq, slot}
    events.rs                       TrunkingEvent (grants, identity, channel plan, neighbours, affiliations, …), VoiceEvent
    site.rs                         SiteState with observed_at
    call/  follow/{gates,lane_policy,reset_policy}  engine.rs (one actor: follower + lifecycle)
    lane.rs                         LaneController trait: one release sequence
  voice/                            VoiceCodec trait; imbe/ (jmbe); ambe2/ later
  recording/                        chunk, recorder, storage, wav
  services/
    settings (profiles), site_store (seed + overlay + plan + profiles per site), history (072),
    discovery (071), sample_hub (real-time plots), clock, ntp, event_log
  api/v1/                           thin handlers, typed DTOs (schemars), ApiError, one operation
                                    table → router + catalogue + MCP tools; auth scopes
    radio sites calls talkgroups settings status diag history discovery streams mcp
  boot/                             args, state, tasks (main.rs split)
dsp-lab/ (dev crate)                lsm/, sw_demod/ multistage DDC, golden dumps
```

Protocol trait sketch (from the protocol review):

```rust
pub trait ControlDecoder: Send {
    fn push(&mut self, dibits: &[u8], out: &mut Vec<TrunkingEvent>);
    fn retune(&mut self);
    fn new_system(&mut self);
    fn site(&self) -> &SiteState;
}
pub enum TrunkingEvent {
    VoiceGrant(VoiceGrant), DataGrant { .. }, Identity(IdentityUpdate), ChannelPlan(BandEntry),
    Neighbour(Neighbour), SecondaryCc(LogicalChannel), SiteTime(SiteSync),
    Registration { .. }, Affiliation { .. }, Deny { .. }, Emergency { .. }, Raw { opcode, summary },
}
pub trait VoiceCodec: Send {
    fn decode(&mut self, frame: &[u8]) -> [i16; 160];
    fn reset(&mut self);
}
```

## Plan

### Stage 0: fixes before 071 (small, independent)

1. 0x3C adjacent status at the right offsets, with flags and service class, plus a test (H1).
2. `Registers(Arc<Mapping>)`; `notify_one` (H6).
3. Boundary channel → mpsc; send only voice NIDs; resync on lag (H4).
4. Monotonic time in the lifecycle, follower, autoppm and recentre (H3).
5. Release a lane when no call is open (H5); supervise tasks (H7).
6. Vocoder flush keyed on `call_id`, capped (H10).
7. Remove the C4FM decoder, mode and UI option (H9).
8. TDMA band frequency maths, and tag grants with phase/slot (M, affects Clay).

### Stage 1: with 071

- `Tuner` + `RadioLease` (H2): recentre, autoppm, site switch and the sweep all go through it.
- `streams` / `sample_hub`: one producer per DMA ring and the spectrometer (also the base for the
  20 Hz plots).
- `TrunkingEvent` for identity, channel plan, neighbours and secondary CCs, with timestamps.
- `SiteService::activate(name)` on the server, and `SiteName` validation.
- Discovery job API: start, progress over `/ws/events`, results, "add site" (the first use of
  `save_site`).

### Stage 2: with 072

- `services::history` (SQLite on the SD card, one writer thread fed by an mpsc).
- Per-site encrypted history, grant counts and the monitor roster move into it. This retires
  `grant_map` and the `lo_plan` duplicate count.
- Affiliation, registration, private and interconnect grants become typed events.

### Stage 3: the refactor proper (after 072)

- Module moves to the layout above.
- Split `AppState`.
- `api/v1` with typed DTOs, `ApiError`, auth scopes and a generated catalogue; `/mcp` from the
  same operation table.
- Split `main.rs` into `boot/`.
- SVD cluster for the chain banks.
- Delete dead code and move the DSP lab into its own crate.
- Comment pass: intent comments stay, history goes to the CHANGELOG.

Each step is behaviour-preserving. Before each commit it is checked by:

- the host tests;
- the replay corpus bench (`fbench.py run rf.p25_corpus --tx B --rx A`);
- a live run on unit A.

### Quick wins (any time; safe deletions and merges)

1. `util::time::now_unix_ms()` and one `nco_offset(freq, rx_lo, shift)`.
2. Replace the control and lane-1 FIR loaders with `ddc_fir_ram` (saves ~230 lines; existing tests
   prove equivalence).
3. Delete the dead `fpga.rs` / `iio` / `lsm` items and the `sw_demod` re-exports. This clears the
   target warnings.
4. Delete `seed_snapshot`, its 60 Hz producer in `main.rs` and the ignored retune parameter.
5. Delete the retired routes (`traffic_bins`, `traffic_iq_dump`, the empty traffic branches of
   `/ws/iq` and `/api/deviation`, `dibit_dump`), `DdcConfig`, and the unused `api.js` wrappers.
6. Delete the always-zero stats (`TrafficStats`, dead `IrqStats` and `RecorderDiag` fields) after
   a UI check.
7. Stop enabling the traffic pre-diff IQ DMA nobody reads (`main.rs:892`).
8. One opcode name table; one atomic JSON writer; shared Golay/Hamming tables; a `VecDeque` for
   recent messages; GF tables built once.
9. Fix the false comments listed above; remove the orphan section banners and the
   `allow(unused_imports)` preludes.
10. Make `POST /api/site`'s `applied_preset` truthful.
11. `lo_plan` flush: clear `dirty` only after a successful write.

## Readiness

### 071: find local systems

| Need | Today |
|------|-------|
| Stable NAC | Available (`nac_tracker`) |
| WACN, system (0x3B) | Available; LRA and service class not parsed |
| RFSS, site, CC (0x3A) | Available; system, active flag and service class not parsed |
| IDEN bands | Available; TDMA maths wrong; cleared by `new_system` (correct) |
| Neighbours (0x3C / 0x3E) | Wrong offsets (H1), not stored; 0x3E not parsed |
| Secondary CCs (0x39 / 0x29) | Last A/B pair only; 0x29 missing |
| Services (0x38) | Missing |
| Hardware | Spectrometer frames (shared cursor), 3 LSM chains usable as parallel probes, preset/LO apply in < 0.2 s (measured 070b) |

### 072: per-site history

| Need | Today |
|------|-------|
| Group grants, calls, airtime | Available (`CallTrackerEvent` open/close); no site key |
| Encryption | Grant options; ALGID/KID only in forwarder atomics |
| Affiliation, registration | Parsed to JSON only |
| Private and interconnect calls, deny, queue | Missing |
| Motorola patches (0x90) | Opaque |
