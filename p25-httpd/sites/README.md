# P25 site baseline configs

Per-site starter JSON files seeded from
`C:\Users\Andy\SDRTrunk\playlist\default.xml` plus runtime observations
on the Fishball board.

## Layered model

Two layers feed each saved site:

1. **Seed (this directory, checked into the repo)** — site name, control
   frequency, alt CCs, traffic-LCN list, modulation. Comes from the
   SDRTrunk playlist. Lets the LO snap know the freq spread before
   the receiver has heard the air.
2. **Runtime overlay (target board, `/mnt/data/p25/<site>.json`)** —
   NAC, WACN, system_id, rfss_id, site_id, lra, IDEN bands, last-CC.
   Written by p25-httpd as on-air state changes; loaded at startup if
   present and merged with the seed.

## Layout

```text
p25-httpd/sites/
  clay.json             — Clay County (CC 860.9625 MHz, NAC 0x8A1)
  duval.json            — Jacksonville/Duval (CC 855.4875 MHz, NAC 0x3BA)
```

`name` is the API key (`POST /api/site?name=clay`).
`label` is the display string for the dashboard.
`control_freq_hz` is the primary CC.
`alt_control_freqs_hz` are secondary CCs the system has historically
used (per SDRTrunk's `order=1` channels for that site). The follower
can rotate to one if the primary goes silent.
`traffic_freqs_hz` is the *known* downlink channel set. Change 070: the
window planner (`services::lo_plan`) places the LO, and with preset
`auto` picks the preset, so the window holds the CC and as many of these
channels as possible, weighted by the grants seen per frequency (saved in
`/mnt/jffs2/p25-plans/<name>.json`). Clay (8.5 MHz span) gets 12M,
Duval (6.0 MHz span) 8M centred on its traffic. `cc_position` is only used
for a site with no known channels (e.g. `"Top"` puts the CC near the top
of the IF window). At start-up the radio tunes to the active site's CC
and planned window; the init script's `--control-freq` / `--rx-lo` /
`--preset` are the fallback when no site loads.
`iden_bands` is empty in seed files — populated at runtime from
IDEN_UPDATE TSBKs. Same shape as `protocol::p25::tsbk::FrequencyBand`.

## TODO — full pass (deferred)

### Site config (this directory)

- [ ] Add `serde::{Serialize, Deserialize}` derives on `FrequencyBand`,
      `Site`, `IdenBand` types in `p25-httpd/src/protocol/p25/`.
- [ ] `services::sites::Site` struct, hydrate from
      `/mnt/data/p25/<name>.json` ∪ `p25-httpd/sites/<name>.json`,
      atomic-replace save on IDEN/CC change.
- [ ] `GET /api/sites` (list known sites), `GET /api/sites/<name>`,
      `POST /api/site?name=<name>` (apply preset + LO snap to CC at
      configured position + seed `bands`).
- [ ] Boot path: pick last-active site from
      `/mnt/data/p25/active.json`, fall back to `clay.json`.
- [ ] Tezuka rootfs: confirm `/mnt/data` is the persistent ubifs/jffs2
      mount on Fishball Z7020 (`/etc/fstab` lookup needed).
- [ ] Dashboard: site selector dropdown wiring `POST /api/site`.
- [ ] CHANGELOG_FORK entry + memory note when this lands.

### Talkgroups + aliases (also deferred)

SDRTrunk's `playlist.xml` already carries TG → alias mappings
(`<alias list="CCFRAliases" name="CCFR Dispatch"><id ... value="300"/>`).
Mirror this onto the board so the dashboard's call list shows
"CCFR Dispatch" instead of bare TG=300.

- [ ] Define `TalkgroupAlias { tg: u32, name: String, group: Option<String>,
      record_audio: bool, color: Option<i32> }` and a per-site
      `aliases: HashMap<u32, TalkgroupAlias>`.
- [ ] Seed each `<site>.json` with the `<alias>` rows pulled from the
      same SDRTrunk playlist (Clay = `CCFRAliases` list, Duval =
      `DUVAL_PS-P25` list).
- [ ] Runtime overlay at `/mnt/data/p25/<site>_aliases.json`; merge
      keeps user edits over the repo seed.
- [ ] Dashboard talkgroup column shows alias name + group; falls back
      to bare TG when no alias.
- [ ] `GET /api/talkgroups/aliases?site=<name>`,
      `POST /api/talkgroups/alias` (CRUD).
- [ ] Source-ID aliases follow the same shape (subscriber RIDs to
      "Unit 1014 — Dispatcher Console" labels). Lower priority.
