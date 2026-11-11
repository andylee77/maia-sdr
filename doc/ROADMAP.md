# Fishball scanner: goals, capabilities, features and the order of work

The numbered changes are taken from this plan. Each change gets its `CHANGELOG_FORK.md` entry and
a `doc/changes/NNN_*.md`, and takes the next free number when it starts.

Andy, 2026-11-11, after the UI's rebuild (090 to 094): "I like the new layout and progress", then
a list of what comes next, ending "a larger part is organizing and structuring around goals,
capability, features". This document is that structure (change 095). It is for Andy to correct:
the questions in "Decisions for Andy" are the parts only he can settle.

How each item is marked:

| Mark | Meaning |
|------|---------|
| **exists** | Built and on unit A's image (`2026-11-10-pages2`) |
| **decided** | Andy has said what he wants; not built |
| **open** | Needs a decision; the question's number follows (Q1 …) |
| **idea** | Written down, not scheduled |

## Goals

| # | Goal | Andy's words |
|---|------|--------------|
| G1 | **A trunked monitoring station**, now. The radio has a control tuner and two traffic tuners; one screen runs it | "grounded into a trunked monitoring station that has 2 traffic tuners and allows easier control of what to listen, record, etc." |
| G2 | **A stand-alone handheld**, later. The same radio with its own controls and speaker, no browser | "future builds as a stand alone handheld with a few knobs, buttons and speaker output and 3d printed shell" |
| G3 | **More modes.** TV is one; data monitoring and ADS-B come next | "add the data monitoring mode, adsb mode, etc." |

What holds for every goal:

1. **A fast UI over one WebSocket.** Everything on a page is pushed as it happens (`/ws/live`); no
   page polls.
2. **The radio is fully API controllable.** Every control on a page is an API call, so a script,
   an agent or the handheld's panel can do what the page does. A page holds no rule the daemon
   does not serve.
3. **Access is controlled.** Nothing is today (see Security); "need to add auth/security".
4. **One radio, one mode at a time.** A mode has the radio to itself; the other modules' pages
   stay readable meanwhile.
5. **The core knows no protocol, and the gateware serves every mode,** so a new mode is a
   software change (079, "Every mode's needs").
6. **Evidence first.** SDRTrunk is the baseline for P25 and DMR; nothing migrates; a gateware
   change passes 079's gates.

## Capabilities

What each goal needs from each layer, and where that layer stands.

### Gateware and the radio core

| Capability | State | For |
|------------|-------|-----|
| Three DDC lanes at 50 kSPS (the control channel and two traffic channels), the spectrometer, the capture ring (core 2.0.0, bake A) | **exists** | G1 |
| The 8-VSB receiver (core 2.1.0, bake D) | **exists** | G3, TV |
| Bake A's burst-timing gate (`tools/burst_timing.py`) | waits for Andy's key fob or the ESP32-DIV | every later bake |
| Wide lanes, 250 kSPS and 1 MSPS: new filter sets and a bench check, no bake (`DATA_MODE.md` §3) | **decided** | G3, Data |
| A continuous stream of the window at 2 to 4 MSPS to the PS (the capture ring read without gaps, or a lane with only its first stage) | **open** (Q11): measured first | G3, ADS-B |
| More than three lanes (bake B) | waits for a mode that needs them; two built designs to read first (`UPSTREAM_REVIEW.md` R5) | G1 on busier systems, Data |
| The lanes' mixer holding a full-scale input: Maia's DDC wraps on one, and one more bit stops it (`UPSTREAM_REVIEW.md` R4) | with the next bake | G1 |
| The two Vivado projects standing on their own, without upstream's `projects/pluto` design (`UPSTREAM_REVIEW.md` R3) | with the next bake | every bake |
| A burst detector in the PL (bake C) | after data mode's scan and captures | G3, Data |
| I²S audio out to the panel's amplifier (`PANEL_ADDON_BOARD.md` §9.4): a small block and pins on JP5, with 079's gates | **decided** in the panel's design, not built | G2 |

### The daemon and its API

| Capability | State | For |
|------------|-------|-----|
| P25 Phase 1 (LSM and C4FM) and DMR Tier III trunking; one follower with two lanes, priorities, pre-emption, a site hold and a hold per lane | **exists** | G1 |
| Aliases (SDRTrunk's model, D15): name, group, colour, IDs and ranges, priority, do-not-monitor, record, speaker; a system's listening settings | **exists** in the configuration and the API | G1 |
| Radios' aliases, edits to many at once, a colour or group set from a page | **exists** (097): the Aliases page replaces the list whole, guarded by its revision (`rev`), so a list changed elsewhere is not written over; one talkgroup's controls take a colour too. No route for one alias by itself: nothing needs one yet | G1, Aliases |
| How a talkgroup is treated (followed, its tuner, priority, recorded), served by the daemon | **exists** (096: `GET /api/v1/talkgroups`, by the follower's own rule) | G1, G2 |
| The live site's talkgroups pushed (each with its name, treatment and when it was last heard) | **exists** (096: `talkgroups` on `/ws/live`) | G1, G2 |
| A changed radio setting shown in every browser | half built: the daemon announces it (`changed`, `what: radio`) and no page acts on it | G1 |
| Recording: every followed call, or the calls of talkgroups marked `record`; by day on the card; locks; retention | **exists**; Now's Record sets which (096) | G1 |
| The history on the card, the activity routes, CSV | **exists** | G1 |
| RadioReference CSV import (`POST …/radioreference`, `/preview`) | **exists**, on the Aliases page (097) | G1, Aliases |
| SDRTrunk playlist import and export | **exists** (098: `/api/v1/playlist`), held against SDRTrunk's own reader | G1, Aliases |
| A system or a site added by hand | **decided** (UI_BRIEF), not built | G1 |
| Modes: `scanner` and `atsc`. `Modes::set` assumes two, and `status.live.state: away` does not say which mode has the radio | **exists** for two; a third needs both fixed (`DATA_MODE.md` §8) | G3 |
| What is on the card as files to take away: recordings in bulk, the history, TV recordings, captures | not built: one recording at a time, and the history as CSV | G1 |
| Login, tokens, the sockets and the unit's other services | not built (see Security) | G1, G2 |
| Listening on the unit itself: the two lanes mixed to an output, each lane's volume, the leveller, alerts-only listening | not built: all of it runs in the browser (`js/audio/`) | G2 |
| The panel's service: the knobs' and buttons' actions as the same operations the API serves | not built (`PANEL_ADDON_BOARD.md` §9.2 was written for p25-httpd) | G2 |
| Agent control (an MCP server over the API) | **idea** | — |

### The UI

| Capability | State | For |
|------------|-------|-----|
| The module rail; a module is an entry in `js/modules.js`; viewing a module does not move the radio | **exists** (090) | all |
| Scanner: Now, Activity, Systems, Aliases. TV: Watch, Channels. Tools: Diagnostics, Settings | **exists** (090 to 094, 097) | G1, G3 |
| Now as a monitoring station's front | **exists** (096: layout A, the tuners first) | G1, G2 |
| The Aliases tab, with the RadioReference import | **exists** (097) | G1 |
| The SDRTrunk playlist's export and its import, on Systems | **exists** (098) | G1 |
| A system or a site added by hand | **decided**, not built | G1 |
| Settings and Diagnostics split: the whole radio, and a part per module | **decided**; how is **open** (Q5) | G1, G3 |
| Files: what is on the card, to download | **decided**, not built (Q6) | G1 |
| Login, and who is signed in | not built (Q8) | G1 |
| The first-load welcome and radio check (UI_BRIEF) | **decided**, not built | G1 |
| Data's module (Scan, Devices, Captures), a waterfall | **decided** (`DATA_MODE.md` §7), not built | G3 |
| ADS-B's module | **open** (Q11) | G3 |
| The handheld's face as a page: the prototype of the panel, and a remote control | **idea**; it follows Now's model | G2 |
| A phone layout beyond the rail becoming a bottom bar | later (UI_BRIEF) | — |

### Storage

On unit A's card on 2026-11-11 (58 GB, 7.3 GB used):

| What | Where | Size |
|------|-------|------|
| Recordings | `p25_recordings/`: 40 UTC day folders, locked ones under `locked/` | 101,485 WAVs, 5.1 GB (3,803 locked) |
| The history | `scanner-history.sqlite` | 205 MB |
| TV recordings | `tv/rf<N>.ts` | five channels |
| The configuration and what the radio learned | the flash, `/mnt/jffs2/scanner/` | 244 KB of 896 KB used |
| Left from before the scanner, read by nothing | `p25-history.sqlite*`, `p25_iq_captures/`, `p25_forensics/`, `bench/`, eighteen `boot_*` folders | — |

- **exists:** a recording's WAV by its id (with its alert tones as cue points), the history's
  calls as CSV, the configuration as one document.
- **not built:** many recordings as one download; the history's database as a file; a TV
  recording as a file (its route plays it at the pace of its clock); data mode's captures.
- A download must not cost the scanner a call: one at a time, at a pace the card and the A9 can
  give while two lanes decode. To measure when it is built.

### Security

What a client on the network can do today, from the code and unit A's open ports:

| Door | Today |
|------|-------|
| HTTP on 8080 | Open to anyone who can reach the unit. No login |
| HTTPS on 8443 | The same API under a self-made certificate, which names 192.168.x.1 and not the unit's Ethernet address, so a browser warns twice |
| Reads | All open: the live audio, every recording, the history, the whole configuration, the control channel's IQ |
| Writes | Refused only when a browser says the page came from another host (`Origin` against `Host`, `api/mod.rs`). A client that sends no `Origin` (curl, a script) may write anything, a factory reset included. A page whose name was re-pointed at the unit (DNS rebinding) passes, because `Host` is not checked against the unit's own names |
| `/ws/live`, `/ws/audio`, `/ws/events` | No check at all. A web page on any site can open them in the user's browser and read the state and the audio |
| libiio (`iiod`, port 30431) | Open: anyone can retune the AD9361 under the scanner |
| SSH (dropbear, port 22) | Root, by password or key |
| The legacy `/api/*` routes for the bench | Open, as the rest |

What G1 and G2 need (the depth is Q7 to Q10):

- a password set at first load, kept as a slow salted hash; a session cookie for the browser; a
  token for scripts, the bench, agents and the panel;
- the same check on every route and at each socket's upgrade, with the page's origin checked at
  the upgrade and `Host` against the unit's own names;
- a certificate remade when the unit's addresses change, from the unit's own CA
  (`S50p25-httpd-certificates` in tezuka_fw), so HTTPS can be the one way in. HTTPS also gives
  the browser the better audio path (the worklet needs a secure page);
- the unit's other doors settled with it: `iiod`, SSH, the legacy routes;
- a way back in when the password is lost that needs the unit in hand (the card or the serial
  console), and what a factory reset does with the password;
- the tools that drive the API on tokens: `fbench` and the board agent, `tools/*.py`,
  `runs/ui_check/*.mjs`.

### Hardware controls

- **Today:** none. The unit is run from a browser or the API.
- **Written down:** `PANEL_ADDON_BOARD.md`, a board on JP5 with a XIAO ESP32-S3, a 1.5" OLED,
  one encoder, two buttons, two LEDs, and an I²S amplifier and speaker driven from the PL. It was
  written for p25-httpd (a `/api/panel` route, its own serial messages) and says "not a
  battery-powered portable (at least not in v1)".
- **What G2 needs:** Q13 settles the hardware. Whatever it is, its actions are the API's
  operations (rule 2), and what its knob steps through is Now's talkgroup list (see "Now's model").

## Features by module

### Scanner

| Page | Feature | State |
|------|---------|-------|
| **Now** | A monitoring station's front (layout A): the site bar with what is recorded; the two tuners, each with its call, its hold, the talkgroup it is kept on and its last calls to play again; the talkgroups with their tuner, Rec mark and Listen switch, one selected with its controls in a bar; the calls with the picked call's player | **exists** (096) |
| | Sound by tuner: Traffic 1 left, Traffic 2 right, with a switch for both on both | **exists** (096) |
| | The day's totals in the calls' heading; an alias's colour and group | not built |
| **Activity** | Who talks, the totals and chart, calls, alerts, packet data, the picked call | **exists** (093) |
| | Time per speaker in a call (072's open item); packet data kept in the history and its contents decoded (074b, parked) | **idea** |
| **Systems** | The scan for systems; the systems and their sites edited in place | **exists** (093) |
| | SDRTrunk's playlist: the export, and an import shown system by system before it is made (where each goes, its channels as sites, its names) | **exists** (098) |
| | Add a system or a site by hand; the first-load welcome | **decided** |
| **Aliases** | Per system, the alias list as a table, the talkgroups' and the radios': colour, name, numbers and ranges, group, and for a talkgroup its priority, tuner, Rec mark and Listen switch; each edited in its row, the picked rows together; found by name, group or number, sorted, shown by group | **exists** (097) |
| | What was heard in the last week and has no alias, named in place | **exists** (097) |
| | A system's listening settings: talkgroups with no priority followed or not, their tuner, pre-emption | **exists** (097) |
| | The RadioReference import with its preview | **exists** (097) |
| | A playlist's names | **exists** (098): they come in with its systems, on Systems |
| Later | Encrypted calls followed for their details on a free lane (072b); Harris and Motorola vendor grants (SLERS, FPL); Phase 2 TDMA if a nearby system uses it; a notebook per system | **idea** |

#### Now's model

The page and the handheld's panel share one model, so the page's work is not done twice:

- **Two tuners.** Traffic 1 and Traffic 2 each show their call or what they wait for. A tuner can
  be kept on one talkgroup (`PUT /api/v1/hold` with `lane`).
- **A list of talkgroups with one selected.** A knob steps through it; on the page a click or the
  arrow keys do. The selection is the client's own (a browser's, the panel's).
- **A few actions on the selected talkgroup,** each one API call: follow or skip it
  (`do_not_monitor`), hold the radio on it (`PUT /api/v1/hold`), record it (`record`), give it a
  tuner (`speaker`) and a priority, name it. These are the handheld's buttons.
- **A second knob is the volume.** On the page it is the browser's own; on the handheld it is the
  unit's listening path.
- **The site** is picked from a menu (`POST /api/v1/sites/{id}/activate`).

How the follower treats a talkgroup's `speaker` today (`trunking/follow/mod.rs`): `left` goes to
Traffic 1 only, `right` to Traffic 2 only, `both` to either, and a busy tuner does not borrow the
other. So the field is the talkgroup's tuner. Since 096 the page calls it that, and the browser plays
each tuner on its own speaker (Q3).

Recording today (`services/recordings/mod.rs`): a followed call is recorded when recording is on
and either `recording.every_call` is on (the default) or its talkgroup's alias says `record`.
Now's Record sets which, since 096 (Q2).

### TV

| Page | Feature | State |
|------|---------|-------|
| **Watch** | Any named channel live, in three views; its stream for VLC | **exists** (085, 091) |
| **Channels** | The TV scan, kept across restarts; the picked channel | **exists** (092, 094) |
| Known limits | A channel where every codeword fails holds the live thread at a core (085). The kept scan has no spectra and nothing marks it as kept. The receiver has one stop for every player | in the review's list |
| Later | Recording a programme to the card; the guide (PSIP's events) | **idea** |

### Data (new module)

Andy's decisions are in `scanner/doc/DATA_MODE.md` §12; its steps and gates in §9.

| Page | Feature | State |
|------|---------|-------|
| **Scan** | The band's window live with a waterfall; the emitters heard, named there | **decided** |
| **Devices** | Every device and frequency heard, each with its data log | **decided** |
| **Captures** | Bursts and window snapshots kept as files (cs16 and SigMF), to download | **decided** |
| Outputs | `/ws/live` and REST first; then NDJSON, MQTT, an rtl_tcp server | **decided** |
| Decoders | In Rust, from the protocols' facts, checked against rtl_433 on the PC | **decided** |

### ADS-B (new module)

Nothing is decided beyond "add it" (Q11). What is known: 1090 MHz, 1 Mbit/s pulse-position
frames of 56 or 112 µs, so it needs the window's samples at 2 MSPS or more without gaps, which no
lane gives today. `DATA_MODE.md` §9 step 7 and 079's "Rates by mode" list it as a later use of a
first-stage-only lane. dump1090 is the reference on the PC and, like rtl_433, is GPL: its facts
are used, not its code.

### The whole radio's tools

| Tool | Feature | State |
|------|---------|-------|
| **Diagnostics** | The spectrum, the event log, the carriers heard, the API's routes, the control channel, the receive window, the tuning, the board | **exists** (094), as one page for every mode |
| | Split: the radio's own part (spectrum, board, tuning, the log, the routes), and each module's (the scanner's control channel, window and lanes; TV's receiver; Data's lanes) | **decided**; how is Q5 |
| | Eye and IQ plots; the spectrum, a waterfall and the plots at 20 Hz or more on a socket of their own | **idea** (below) |
| **Settings** | Receiver, radio, clock, configuration, recording, crystal, packet data | **exists** (094), as one page |
| | Split: the radio's own part (gain, crystal, clock, storage, network and access, the configuration), and each module's (the scanner's call timings, lanes, packet data, recording and listening defaults; TV's and Data's) | **decided**; how is Q5 |
| **Files** (new) | What is on the card, and taking it away | **decided**; Q6 |
| **Access** (new) | The password, tokens, who is signed in, the certificate | Q7 to Q10 |

### The handheld

| Feature | State |
|---------|-------|
| The display: the site, each tuner's talkgroup and radio, the selected talkgroup, signal, time | **idea** (`PANEL_ADDON_BOARD.md`; the roadmap's old "Handheld page") |
| Knobs and buttons: the list, the volume; hold, skip, record, menu | follows Now's model |
| The speaker: the unit's own listening path | not built (Capabilities) |
| The shell, the power, the board | Q13 |

## The order of work

Andy's order, with what each step needs first. A step starts when its questions are answered.

| # | Step | Needs first |
|---|------|-------------|
| 1 | **The structure, the code review, its clear defects** (095) | — |
| 2 | **Now, redone:** done in 096 from layout A. Left of this step: `/ws/live` sending only what changed, the hold's two routes | Andy's use of the page. Q2 to Q4 are built as recommended, and his to change |
| 3 | **Aliases:** done. The tab, the alias model's gaps and the RadioReference import's page in 097; the SDRTrunk playlist's import and export in 098 | Andy's own playlist imported on unit A (his to press), and Q4, which 098 found needs another answer |
| 4 | **Settings and Diagnostics split** into the radio's and each module's | Q5. `js/modules.js` gains a module's settings and diagnostics; each settings section reads and saves its own part, and follows a change made elsewhere. Before steps 7 and 8, so the new modules arrive with their own |
| 5 | **Files.** Recordings in bulk, the history, TV recordings, later captures | Q6. An archive route that streams; the history copied whole and consistent; the pace measured against two lanes decoding |
| 6 | **Access.** Login, tokens, both sockets, the certificate, the unit's other doors | Q7 to Q10. The bench reading `/api/v1` so the legacy routes can go; every tool and check on a token; tezuka_fw's certificate script |
| 7 | **Data mode** (`DATA_MODE.md` steps 0 to 5) | The modes made general (three and more; who has the radio); the `packet_data` rename; wide lanes proven on the bench (Andy wires B into A, or the ESP32-DIV); the RTL-SDR's ground truth (Andy); a waterfall |
| 8 | **ADS-B** | Q11. Step 7's mode plumbing. First a capture at 1090 MHz from unit A decoded on the PC, to learn what its antenna hears; then the sample path it needs |
| 9 | **The handheld** | Q13. The unit's own listening path; the I²S block (a bake, with 079's gates); the panel's service; step 6's tokens. Its face as a page can come any time after step 2 |

Until step 6, what step 5 adds is as open as single recordings are now.

Not tied to a step, waiting on Andy:

- **Pushes.** Nothing since 082 is on GitHub, in maia-sdr or tezuka_fw: this PC holds the only
  copy of 083 to 095 (Q14).
- **Bake A's burst-timing gate:** one key-fob or ESP32-DIV burst (`tools/burst_timing.py`).
- **The P25 replay corpus on the radio core:** needs the bench link wired (B into A).
- **Unit B** needs the current image before anything runs on it.

From the upstream review (`doc/UPSTREAM_REVIEW.md`, 2026-11-11: Maia SDR, Tezuka, their forks and
the other P25 and DMR work on FPGAs), which has six questions of its own for Andy (Q15):

- **Now:** a read-only check of unit A's running kernel against the image it was built from
  (R1). Tezuka found a self-extracting `zImage` corrupting the kernel's read-only data once it
  unpacks past about 14.5 MB, and this fork's unpacks larger. Upstream saw it on a boot path
  this fork does not use, so it is not proven either way.
- **With the next image:** the three `maia-kmod` patches that close races in the driver the
  rings go through (R2).
- **With the next bake,** whichever step brings it (bake B, or step 9's I²S block): the lanes'
  mixer given its extra bit (R4), and the Vivado projects made to stand on their own (R3).
  Upstream's working branch deletes the design they are built on.
- **Before bake B is designed:** two built designs read (R5): time-multiplexed DDC lanes, and a
  512-bin filter bank, both on a Zynq-7010. No open P25 receiver for an FPGA was found.
- **Watched, not scheduled:** Tezuka's kernel 6.12 and Buildroot 2026.05 (a change of its own),
  the bitstream loaded from the card by U-Boot, chrony and PPS for the unit's clock.

## Decisions for Andy

Each has a recommendation first. Short answers are enough.

**Now (step 2)**

- **Q1. Which layout?** Answered 2026-11-11: A, the tuners first. Built in 096.
- **Q2. What decides whether a call is recorded?** Recommended: one setting with three positions,
  on Now and in Settings: *off*, *marked talkgroups* (a talkgroup's Record mark decides),
  *every followed call* (the marks are shown lit and locked). It needs no new field. The
  alternative: keep "every call" and add a per-talkgroup "never record", which SDRTrunk's alias
  model has no place for. **Built as recommended in 096** (layout A was drawn with it); say if it
  should be the other.
- **Q3. Does sound follow the tuner?** Recommended: Traffic 1 always plays left and Traffic 2
  right, with a switch in the header for both ears; a talkgroup's setting then reads "Traffic 1,
  Traffic 2 or either". It is a change in the browser's player only. The alternative: a talkgroup
  set to left or right is panned, and an "either" one plays in both ears, as before 096.
  **Built as recommended in 096,** with Left / Right in the header to turn it off.
- **Q4. What is in the talkgroup list once an import adds hundreds of names?** Recommended: the
  talkgroups heard in the last week, plus those with a setting of their own (a priority, a tuner,
  Record, or switched off); a name alone does not list one, and everything is on the Aliases tab.
  The alternative: a list picked by hand (a scan list), which needs a new field. **Built in 097
  with one difference:** switched off alone does not list a talkgroup either, because the
  RadioReference import switches off every fully encrypted one. A talkgroup switched off is listed
  while it is heard (within a week), and is always on Aliases.

  **098 found that not enough.** Andy's own playlist marks 274 aliases Record (60 of Duval's,
  all 118 of Alachua's, all 73 of FPL's), and each would list its talkgroup on Now whether it is
  ever heard there or not. Recommended now: the list is what was heard at the site in the last
  week, and nothing else; every setting is on Aliases, and a talkgroup shows on Now when it
  speaks. It needs Now's check run against a daemon that has heard something. Not changed yet:
  his to say.

**The tools (steps 4 to 6)**

- **Q5. Where do a module's settings and diagnostics live?** Recommended: among the module's own
  tabs (Scanner: Now, Activity, Systems, Aliases, then its Diagnostics and Settings), with the
  rail's two tools becoming the radio's own. The alternative: the two tools stay single pages with
  a tab per module inside.
- **Q6. How should recordings leave the card?** Recommended: from a Files page, one archive (ZIP,
  not compressed) of a date range, a talkgroup, a radio, or the locked ones, plus a command-line
  tool that does the same through the API. Also wanted, or not: a copy to a PC or NAS share on a
  schedule?
- **Q7. Who must be kept out?** Recommended: the unit is a LAN appliance (other people and
  devices on the network, and web pages in a browser, are kept out; reaching it from outside is
  through a VPN of yours). The alternative, a unit that can face the Internet, asks much more.
- **Q8. One password, or two levels?** Recommended: an administrator (everything) and a listener
  (see and hear, change nothing), plus tokens for tools and the panel.
- **Q9. Does plain HTTP stay?** Recommended: once a password is set, port 8080 only sends the
  browser to HTTPS, and the certificate is remade when the unit's address changes. The browser
  warns once unless the unit's CA is installed on the PC.
- **Q10. libiio and SSH.** `iiod` lets anything on the LAN retune the radio. Recommended: off
  unless a switch in Settings turns it on (SDR++ or SDRTrunk using the board as a tuner need it).
  SSH: keys only, or as it is?

**Modes (steps 7 and 8)**

- **Q11. ADS-B.** Recommended: a module of its own ("Aircraft"): a table of the aircraft heard and
  a plot with range rings, no map tiles from the Internet; its messages also served in the Beast
  format on a port, for tar1090 or Virtual Radar on a PC. First a capture at 1090 MHz on unit A.
  Is there a 1090 MHz antenna for A, or is it the scanner's?
- **Q12. Which other modes are behind "etc.", and in what order?** Written down so far: NOAA
  weather radio and other conventional FM channels (Clay Fire's pager channel on 154.205 MHz is
  one), pagers (POCSAG), APRS, AIS, the 24-hour spectrum survey. Recommended: none before Data
  and ADS-B run.

**The handheld (step 9)**

- **Q13. Is it the Fishball board with the panel add-on in a printed shell?** If so: battery or
  wall power; one encoder or two (the list and the volume); which buttons (recommended: Hold,
  Skip, Menu).

**Housekeeping**

- **Q14. Push now?** `fishball-p25` and tezuka_fw's `fishball-dev`, so 083 to 095 have a second
  copy.
- **Q15. The upstream review's six questions** (`doc/UPSTREAM_REVIEW.md`, "Open questions for
  Andy"): whether `main` goes on mirroring F5OEO's frozen `main`; where the ADI HDL pick lives so
  that a fresh clone builds; the speed grade printed on the units' Zynqs; whether either unit is
  the Fishball with the slow DRAM; whether to report findings upstream; and whether a GPS or PPS
  input is planned, which bears on Q13. And its first recommendation: may unit A's kernel be
  checked now (read only)?

## From the code review (095)

What the review found that a fix on the spot does not settle, by the step that takes it. The
defects fixed at once and the whole ranked list are in `doc/changes/095_structure_and_review.md`;
what it recommends for each API quirk the pages lean on is in `scanner/doc/API_INVENTORY.md`.

**Step 2, Now** (096 took the first five and measured the sixth; two are left):

- **Done:** the talkgroups are pushed with their treatment, by the follower's own rule, and the
  page asks the history nothing by itself; the calls kept are bounded and the closed rows are
  drawn only when they change; a lost connection shows; a talkgroup's alias can be removed
  again, and `PUT /radio/recording` takes any of its fields.
- **Measured: one click, one small write.** A talkgroup write through the API, its save to the
  flash included, took 14 to 18 ms on unit A (five writes, 096). It still runs inside its handler,
  on the two workers that also run trunking and live audio; a write that meets the flash's
  garbage collection may take longer, which five writes did not show.
- **Left: the site's hold and a lane's share a route.**
- **Left: `/ws/live` sends what changed.** Per client per second it builds and compares the
  status, the scan and the kept TV scan (85 KB), and sends the traffic because one age in it
  moved. The talkgroups come whole at each change (6 KB for unit A's 42).

**Step 3, Aliases:**

- **Done (097):** the trap where a talkgroup's edit lands on an alias the follower does not
  read. The talkgroup leaves the alias that named it among others, and no list may name a
  talkgroup or a radio by itself twice.
- **Measured (097): a whole-list save.** The Aliases page writes the whole list for each edit.
  With 508 aliases that took 0.10 to 0.33 s on unit A, inside its handler on the two workers
  that also run trunking and live audio (14 to 18 ms with A's eight). Before a system's list
  reaches thousands: the save moved off the workers, or one alias written by itself.
- **Left:** a call by its id; Activity's one loader for every change (five requests a click, none
  aborted); Systems' own copy of the systems; the scan's cards repeating the daemon's rule for
  "the same site".

**Step 4, the split:** Settings is one read and cards closed over what was loaded, and saves
whole objects. Each section needs its own read, save and re-read. Diagnostics is the scanner's:
on TV its spectrum has no frames and three panes say "No site is live", while `atsc_live`
carries the receiver's state that no page shows.

**Step 5, Files:** a factory reset queues a delete per recording and restarts half a second
later, so with 101,000 recordings most stay on the card while the answer counts them as removed.
Deleting by day folder, and waiting for it, belongs with the work on what is on the card.

**Step 6, Access:** the origin check covers writes over HTTP and, since 095, HTTPS; not the three
sockets, and not a name re-pointed at the unit. The missing bounds (a recordings list without a
limit, a stalled CSV reader, sockets without a send timeout or a cap) go with it.

**TV:** a cancelled scan replaces the channel list until a restart; one stream for the receiver's
life, so a player can be fed another RF channel's programme; full screen hides every failure;
Watch between 1000 and 1359 px wide (to render and check).

**With the next page built:** a page has no signal that it was left and no boundary for its
errors. A per-mount abort signal passed to its requests would replace the "gone" flags three
pages now carry.

## Ideas written down, not scheduled

### Encrypted calls: details without audio (072b)

Today an encrypted call is never followed, so all that is known of it comes from the control
channel: the radio granted, and the grant time (which includes hang time and any other radio that
keyed up; on Clay's clear calls decoded voice is about 0.7 of grant time).

An option to follow encrypted calls for their details only, with no audio and no recording. The
voice channel gives every radio that keys up, each transmission's real length, the algorithm and
key ID, and the end. The design:

- only a free lane follows an encrypted call, and only while another lane is free too;
- any clear grant pre-empts it at once, so a clear call loses only the retune;
- the IMBE path stays off;
- the history stores measured voice time and every speaker for encrypted calls, marked as such.

Measure first: on Clay, clear grants overlapped as two calls 0.6 % of a day and as three 0.008 %
(unit A, 2026-10-04).

### Packet data, next steps (074b, parked, DESIGN D11)

- Keep data records in the history, so Activity shows data per radio, hour and day.
- Decode the contents: LRRP, ARS, TMS (SDRTrunk has decoders, `module/decode/ip/mototrbo`).
- Follow data channel grants to other data channels when a site uses more than one.

### Vendor grants

Harris (MFID 0xA4) and Motorola (0x90) vendor TSBKs: SLERS and FPL send many Harris ones. Decode
the voice-grant and patch ones so those systems can be followed.

### Remote libiio control (shared use of the board)

`iiod` gives network libiio access, so SDR++, GQRX or SDRTrunk can retune the AD9361 under the
scanner (Q10 decides whether it is on at all).

- **Detect it:** compare the AD9361's LO, rate and gain each second with what the scanner last
  set; watch `iiod`'s connections to show who is connected.
- **React:** if the control channel is still inside the new window at a known preset, keep
  decoding and mark the radio "shared"; otherwise pause following, say so, and offer "Take back
  control".
- **A setting:** yield to remote clients, or keep the radio.
- The recentre task and the preset handler stay passive while a remote client is connected.

### Agent control (MCP)

- An MCP server in the scanner (Streamable HTTP, for example at `/mcp`), behind step 6's tokens.
- **Read tools:** status and site health; systems, sites, aliases and the coverage plan; recent
  calls with their audio and transcripts; history queries; the event log; the spectrum.
- **Write tools:** switch site; hold a talkgroup or edit its alias; follow or skip one; recentre;
  tune, preset and gain; find systems.
- Notifications from the socket: a call on a talkgroup, a site lost, a system found.
- A document for agents: the radio's concepts, safe defaults (confirm retunes; no site switch
  while someone listens), the read and write split.
- The API describes itself (`GET /api/v1/routes`, `scanner/doc/API.md`); the tools wrap it.

### Transcription and summaries

The A9 cannot run Whisper usefully, so this lives off the board:

- a LAN service (whisper.cpp or faster-whisper on a PC with a GPU) takes finished recordings,
  pushed by the radio or pulled over the API, and returns transcripts to the history;
- summaries, locations and incident grouping are designed in SDRTrunk's
  `doc/design/006d_whisper_transcription.md` and `006e_llm_and_incidents.md`;
- signal classification on the FPGA is its own track: `_shared/FPGA_ML_INFERENCE_GUIDE.md`.

### Spectrum survey: identify everything on air

Andy, 2026-10-01: sweep everything the radio can tune and identify what is there. Done so far:
ATSC (080, 081), the live window's survey; the ISM bands are data mode's.

1. A tool drives the API (tune and `/api/v1/spectrum`) for a 24-hour occupancy map: frequency by
   time, steady or bursty, bandwidth. No firmware change.
2. Targeted captures of the interesting carriers, decoded on the PC (dump1090, rtl_433,
   multimon-ng, SDRTrunk), give an inventory.
3. The classifiers worth having move on board as probes in the scan.

Limits: unit A's antenna is not flat across the range; one 16 MHz window at a time; a survey
takes the radio away from following calls.

### Real-time diagnostics plots (20 Hz or more)

- **`/ws/spectrum`:** every completed spectrometer integration as a binary frame (one byte a bin
  is 4 KB a frame, about 100 KB/s at 25 Hz), drawn as a spectrum and a waterfall with the window,
  the channels and the live calls marked. Data mode needs the same socket and the same waterfall:
  build them once.
- **Constellation and eye:** a lane's symbols from its software receiver as binary frames, drawn
  with persistence.
- **Cost:** stream only while a page subscribes; measure the PS load with the streams on (the
  scanner uses 15 to 17 % of one A9 core).

### A notebook per system

Andy, 2026-10-03: a scratch pad per system in Markdown, kept on the radio with the system's
configuration: its talkgroups' meanings, 10-codes, station information, procedures, notes
(UI_BRIEF, "Later: a system notebook").

## Done

| Change | What |
|--------|------|
| 071, 072, 074, 075 | Find local systems; the activity history; P25 packet data; Clay Electric DMR Tier III |
| 076 | The scanner as a fresh multi-band, multi-protocol crate (`scanner/doc/DESIGN.md`) |
| 079, 082 | The general radio core: every demodulator in software; bake A (core 2.0.0) |
| 080, 081, 083 to 085 | ATSC TV: the mode, station names, the 8-VSB model, our own player, bake D (core 2.1.0, live TV) |
| 086 to 089 | Recordings by date; the live audio's interpolator; one volume; Activity's dates |
| 090 to 094 | The UI rebuilt in the module rail: Now, Watch, the kept TV scan, Activity and Systems, Channels, Diagnostics and Settings |
| — | The cleanup of what the scanner and the radio core replaced (`doc/CLEANUP_INVENTORY.md`, every batch) |
