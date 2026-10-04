# Data mode: a study

Andy, 2026-10-04: a third mode beside the scanner and ATSC TV, a data scanner and data capture
mode. In it the unit watches the open-protocol data bands (315, 433.92 MHz and the like), finds
the devices transmitting nearby and what protocols they use, logs and captures them, and streams
their data. Unit A does this work. This is a study only: nothing is built and no code changes
with it. Andy's decisions are in section 12.

## 1. What the mode does

- **Scan:** step through the data bands, listen long enough in each to hear the slow
  transmitters, and list every device heard: what it is, its ID, frequency, signal, how often it
  sends and its latest reading.
- **Identify:** decode known devices by protocol. For signals nothing decodes, give the
  modulation (OOK or FSK), the pulse timing and the repeat interval, so an unknown emitter is
  still a row in the list rather than noise. The user can name any emitter during a scan.
- **Log:** every burst and decode goes into a data log, per identified device or, until one is
  identified, per frequency.
- **Monitor:** park on one band and decode everything in it as it arrives.
- **Capture:** keep the IQ of a burst (or of the whole window) for the PC: inspectrum, URH,
  rtl_433 with `-r`.
- **Stream:** decoded data and raw IQ, over WebSocket and REST first.

Like ATSC mode, it has the radio to itself: the scanner stops following calls until the unit
goes back to scanner mode.

## 2. The bands

US devices sit mostly in a few spots. The device names below are from rtl_433's protocol list.

| Band | What transmits there |
|------|----------------------|
| 310-319.5 MHz | Car key fobs (313.8, 314.9, 315.0, 315.1), most US TPMS (315), garage remotes (310, 315), Interlogix/GE security sensors (319.5) |
| 345 MHz | Honeywell 5800 and 2GIG door/window and motion sensors |
| 390 MHz | Older garage remotes |
| 433.92 MHz | Weather and temperature sensors (Acurite, LaCrosse, Oregon Scientific, Ambient F007TH), doorbells, remote outlets, Govee leak sensors, some TPMS and fobs |
| 902-928 MHz | Utility meters (ERT SCM/IDM, Neptune R900, hopping 910-920), Ecowitt/Fine Offset and LaCrosse View sensors (915), Z-Wave (908.42, 916), LoRa and Meshtastic |

- 868 MHz (wM-Bus, EU sensors) is a European band, so it is unlikely here; it costs nothing to
  have in the plan.
- **The 16 MSPS window** (±7.2 MHz usable) covers a band at a time:

  | Window | LO (DC kept off the hot spots) | Covers |
  |--------|--------------------------------|--------|
  | 315 | 316.5 | 309.3-323.7 |
  | 345 | 345.5 | 338.3-352.7 |
  | 433 | 435.0 | 427.8-442.2 |
  | 915 low / high | 909.0 / 921.0 | 902-916 / 914-928 |

  These are a starting plan. The first scan on unit A refines it.

## 3. What the radio already gives (no gateware change)

Three paths reach the PS today:

| Path | What it carries | State |
|------|-----------------|-------|
| **Lanes** (3, one lane ring) | Each a Maia 3-stage DDC with a 28-bit NCO, in tagged 4 KB packets carrying the sample index, power, peak and ADC clips | Every preset decimates to 50 kSPS, 7.25 kHz passband |
| **Spectrometer** | 4096-point FFT of the whole window; integrations 1-1023, average or peak-hold | 65.5 ms a frame at 16 MSPS with 256 integrations; each frame can be read once |
| **Capture ring** (`/dev/p25-wideband-iq`, 16 × 1 MB) | Raw AD9361 IQ (12-bit in i16) | A continuous DMA ring while enabled; the scanner only snapshots it (ATSC naming). 0.26 s deep at 16 MSPS, 2.1 s at 2 MSPS |

**50 kSPS lanes are too narrow for these devices.** Cheap OOK transmitters drift ±100 kHz or
more around their nominal frequency, and FSK sensors deviate ±30-80 kHz. rtl_433 listens at
250 kSPS by default and 1 MSPS for the wider ones.

**A lane can run wider without a bake.** Each lane's decimations (`decimation1-3`), filter taps,
and `bypass2` / `bypass3` are runtime registers (`maia-hdl/p25_hdl/p25_top.py`, the lane bank).
The PS always clears the bypass bits today. A wide lane is a new coefficient set:

| Lane rate | At 16 MSPS | Stage limits that apply |
|-----------|------------|-------------------------|
| 250 kSPS | stage 1 /16 (to 1 MSPS), stage 2 /4, stage 3 bypassed | FIR2 ≤ 128 taps at 1 MSPS: about ±100 kHz flat |
| 1 MSPS | stage 1 /16, stages 2 and 3 bypassed | FIR4 ≤ 256 taps, ≤ 11 operations per input sample at clk3x 187.5 MHz: about ±350 kHz flat |

- The rates are set per lane, so lanes can differ.
- Three lanes at 1 MSPS are 12 MB/s into the 2 MB lane ring: 0.17 s of slack against the 20 ms
  poll.
- No preset like this exists and none has been tested. The filter-design tool
  (`tools/p25_ddc_filter_design.py`) would make them, and a bench check proves them (section 9).
- This lasts only while the lanes are DDCs. 079 step 3b would replace them (section 11).

**The CPU is free in this mode.** With the live site paused, both A9 cores are available. The
live scanner itself uses 15-17 % of one.

## 4. How the mode would work

```text
AD9361, 16 MSPS window (LO from the band plan)
 ├─ spectrometer, peak-hold, 65 ms frames → burst detector → emitters by frequency + waterfall
 ├─ lane 0, 250 kSPS or 1 MSPS at a hot spot   ┐
 ├─ lane 1, 250 kSPS or 1 MSPS at a hot spot   ├→ pulse front end → decoders → data log → history,
 ├─ lane 2, roaming: moves to busy unknowns    ┘                                  /ws/live, REST
 └─ capture ring: raw window snapshots on request
```

- **Burst detector** (software, on the spectrometer frames):
  - per-bin floor from a running median;
  - a burst is bins over the floor: start, end, centre, bandwidth, peak;
  - bursts are grouped by frequency into emitters, with their repeat interval.
  - This sees the whole 14 MHz, including what no lane covers. Peak-hold keeps a 5 ms key-fob
    press visible in a 65 ms frame.
- **Lanes:**
  - two sit on the band's hot spots (433.92; 315.0 and 319.5 in the 315 window);
  - the third roams to the busiest emitter no lane covers, so unknowns get IQ and a pulse
    analysis.
- **Pulse front end** (software, per lane):
  - AM envelope and FM discriminator;
  - pulse and gap lengths;
  - the modulation (OOK or FSK) and coding (PWM, PPM, Manchester) from their histograms.
  - Every decoder works from these pulses (section 5). For a burst nothing decodes, they are
    its signature.
- **Scan:**
  - steps through the band plan's windows, dwelling in each (default 2 minutes: most sensors
    send every 16-60 s);
  - a window change costs the 200 ms `LO_SETTLE`;
  - cycles until stopped.
  - **Park** is the same with one window.
- **Gain:** ATSC found the slow-attack AGC best, but that was continuous TV. Bursts may be
  clipped or chopped by an AGC that is still settling. Measure fast attack against manual gain
  on bursts before choosing a default.

## 5. Decoders, all in Rust

Andy wants every decoder in Rust. rtl_433 decodes 386 device protocols. It is the reference these
decoders are checked against, as SDRTrunk is for DMR, but it does not run on the unit.

**Port the protocols, not the code.**

- rtl_433 is GPL-2.0-or-later; the scanner is MIT.
- Translating its C into Rust line by line would make the decoders a derivative of GPL code.
  The decoders, and the binary that carries them, would then have to be GPL.
- 081's rule for philburr/atsc applies: take the facts and write our own code. The facts are:
  - frequencies;
  - pulse and gap timings;
  - preambles and sync words;
  - bit layouts;
  - checksum and CRC parameters;
  - field scaling.
- rtl_433 itself runs unmodified on the PC as the reference.

**The decoder structure:**

- **One pulse front end** for every decoder (section 4). Each burst is sliced once; every
  decoder then tries the pulses, as rtl_433 does.
- **A table-driven decoder for the simple devices.** Many OOK sensors and remotes differ only in
  facts: the modulation (OOK PWM, PPM or Manchester; FSK PCM), the short, long, gap and reset
  times, the bit count and a check. Each is a row in a table (the idea of rtl_433's flex decoder).
  Adding such a device is a row and a test capture.
- **Hand-written decoders for the rest.** These are devices with layered framing or real
  CRCs: TPMS (FSK, Manchester, CRC-8), Honeywell (CRC-16), ERT meters (BCH). Each is a module
  with its captures as tests.
- **What is ported first:** what unit A actually hears. Captures (step 4) decoded by rtl_433 on
  the PC give the list and the test vectors. Families come in the order they appear in the
  corpus.
- **Key fobs:** the fixed parts (serial, button) decode; rolling codes stay opaque, as they are in
  rtl_433.

**The reference harness:** `tools/rtl433_reference.py` decodes a corpus of the unit's captures
with rtl_433 and compares the result with the scanner's decodes on the same files. It writes
`runs/data/<time>/`, as `tools/sdrtrunk_dmr_reference.py` does for DMR, and exits 3 on a
difference. A decoder is done when it matches rtl_433 on the corpus and makes no false decodes on
noise. Departures are fine where the evidence shows ours decodes better.

## 6. Streaming and capture

Every output is designed in from the start. WebSocket and REST are built first.

**Decoded data:**

- **First:**
  - `/ws/live` sends each data-log entry as it is made;
  - REST serves the devices, their logs and the scan.
- **Later:**
  - `GET /api/v1/data/stream`, newline-delimited JSON for scripts;
  - MQTT with rtl_433-style topics and Home Assistant discovery.

**Raw IQ:**

- **First:** `/ws/iq?lane=N`, a lane's IQ live as binary cs16 frames on a socket of its own, as
  `/ws/audio` is.
- **Later:** an rtl_tcp-compatible server, so SDR++, URH or rtl_433 on the PC can tune the unit's
  lane.

**Captures:**

- **Per burst:** the lane IQ around a decode or an unknown burst, kept as cs16 with a SigMF
  `.sigmf-meta` (frequency, rate, time, the device and decode if any). A 100 ms burst at
  250 kSPS is about 100 KB.
- **Arm a capture:** "keep the next N bursts from this device or frequency".
- **Window snapshot:** the capture ring, up to 0.26 s at 16 MSPS.
- **Where:** files under `/mnt/sd`, indexed in the history database (listing a FAT directory is
  slow, as the recordings showed), and downloadable from the API.
- Each decoded burst plus its IQ is a labelled example. That is the corpus for the decoders and
  the training data that `_shared/FPGA_ML_INFERENCE_GUIDE.md` (method 4) calls for.

## 7. The data log and the tabs

**The data log** (Andy: "per frequency or identified device, user set during the scan").

- **The key:** every burst is logged against a device.
- **Kinds of device:**
  - **decoded:** a model and ID from a decoder;
  - **named:** an emitter the user named during the scan, bound to its frequency (with a
    tolerance) and its pulse signature;
  - **a frequency:** until either of those applies.
- **Naming moves the history:** naming an emitter or a frequency moves its log under the new
  device.
- **An entry:**
  - the time (air time from the lane's sample index);
  - the frequency;
  - RSSI and SNR;
  - the decoded fields, or the pulse summary when nothing decodes;
  - a capture, if one was kept.
- **What the user can set on a device:** a name, a kind (weather, TPMS, security, remote, meter,
  other), notes, ignore.
- **The Diagnostics event log** gets the mode's events (scan steps, a device first heard), not
  every entry.

**Tabs.** Following 080, each mode has its tabs, and Diagnostics and Settings are shared.

| Mode | Tabs |
|------|------|
| scanner | Now, Activity, Systems |
| atsc | Channels, Viewer |
| data | **Scan** (the mode's home), **Devices**, **Captures** |
| all | Diagnostics, Settings |

**Scan**: the window live, and where things get named.

```text
┌ Scan: 433 window (2 of 5), 1:12 left ───────────────── [Park here] [Next window] [Stop] ┐
│ waterfall + spectrum of 427.8-442.2 MHz, lanes shaded (drag to move), bursts marked     │
├ Emitters in this window ───────────────────────────────────────────────────────────────┤
│ MHz      Device                    Bursts  Every  RSSI  Signature             Action    │
│ 433.92   Acurite-Tower 12345       41      16 s   -61   OOK PWM, decoded      [Open]    │
│ 433.81   (frequency)               12      30 s   -70   OOK PWM 400/1200 µs   [Name]    │
│ 434.42   "Back fence doorbell"     2       —      -58   OOK PPM               [Open]    │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

- The band plan with Scan and Park; the window, dwell and lanes in use.
- **Name** on an emitter makes it a named device; **Open** goes to its log on Devices.
- A lane's pulse view: envelope or discriminator, the sliced pulses and the decoded bits.

**Devices**: every device and frequency heard, each with its data log.

```text
┌ 14 devices, 3 new this hour ──────────────────── kind [all] band [all] seen [24 h] ───┐
│ Kind     Device                    ID      MHz      Last    Every  RSSI  Latest         │
│ weather  Acurite-Tower             12345   433.92   0:21    16 s   -61   22.4 °C 71 %   │
│ TPMS     Toyota                    0x1A2B  314.98   3 h     —      -78   230 kPa        │
│ security Honeywell 5800            0x9C11  345.00   4 m     70 m   -66   closed         │
│ remote   "Back fence doorbell"     —       434.42   2 h     —      -58   pressed        │
└────────────────────────────────────────────────────────────────────────────────────────┘
┌ Acurite-Tower 12345: data log ─────────────────────── [Name] [Ignore] [Capture next] ──┐
│ temperature_C ▁▂▃▅▆▇▇▆  humidity ▇▇▆▅▅▄   (live: entries arrive as they are logged)     │
│ time      MHz      RSSI  SNR  fields                                        capture     │
│ 14:02:11  433.921  -61   18   temperature_C 22.4, humidity 71, battery_ok 1  —          │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Filters:** kind, band, seen within.
- **Export:** a device's log as CSV or JSON (REST).

**Captures**: the files kept, with their time, frequency, rate, length, device and decode;
download (cs16 and SigMF) and delete; window snapshots.

**Shared tabs:**

- **Settings** gains a Data card:
  - the band plan (windows, dwell, lane spots and rates);
  - decoder families on or off;
  - retention and capture storage;
  - later, MQTT.
- **Header:** the mode selector gains Data; in data mode the header's line shows the window and
  the device count, as ATSC's shows the scan.

## 8. What changes in the code

**The mode:**

- `Mode` gains `Data` (`"data"`, label "Data").
- `Modes::set` assumes the mode it leaves is the scanner's or ATSC's. It becomes "leave the
  current mode (which gives back the site to return to), then enter the new one", so ATSC to
  data works.
- `Lease::Atsc` becomes one lease for any non-scanner mode, or gains a sibling.
- The other places a mode touches:
  - `Deps` and `AppState`;
  - boot's match;
  - `/ws/live`'s snapshot and diff;
  - the route text;
  - in the UI, `index.html`'s selector and tabs, `main.js`'s `PAGES`, `HOME` and header, and
    `store.js`.

**P25 packet data takes the name `packet_data`.** That frees `data` for the mode. The name
matches `services::packet_data` and `views/packet_data.js`, and stays right if DMR packet data
joins. The rename covers:

- the route: `/api/v1/data` becomes `/api/v1/packet_data` (`api/mod.rs`, `api/v1/data.rs`);
- the UI: `api.js` and the comment in `views/packet_data.js`;
- the event class: `data` becomes `packet_data` (`services/packet_data.rs`);
- the field meanings: `tools/api_fields_meanings.py`;
- the docs: `API.md` and `API_FIELDS.md` (regenerated), `API_INVENTORY.md`, and `DESIGN.md`
  §11 and its status log.

**New modules:**

| Module | Owns |
|--------|------|
| `protocol::ism` | The pulse front end (envelope, discriminator, slicer, timing histograms, modulation and coding guess); the table-driven decoder and its device table; a module per hand-written decoder |
| `services::data` | The mode: lease, band plan, scan and park, lanes, burst detector, emitters, the data log, captures, streams |
| `hardware/presets` | Wide-lane presets; the lane reader learns each lane's rate (it assumes 50 kSPS: `radio/streams`) |
| history | `data_devices` (key, kind, model, ID or frequency and signature, name, notes, first and last heard), `data_log` (the entries), `data_captures` (the files). Retention by age and size: today's is keyed on calls, so the new tables join `prune`, `trim_to` and `clear` |
| API | `/api/v1/data/scan` (as ATSC's scan routes), `/data/plan`, `/data/devices[/{key}[/log]]`, `/data/emitters`, `/data/captures[/{id}]`; `/ws/live` feeds `data`; `/ws/spectrum` (binary rows; the roadmap's real-time diagnostics item proposes the same socket); `/ws/iq` |
| UI | `views/scan.js` (the systems scan has a `scan.js` already: one gets a new name), `views/devices.js`, `views/captures.js`; a shared waterfall component |
| tools | `tools/rtl433_reference.py` (section 5); `tools/data_check.py` for a scan against a ground truth (step 0) |

**Gaps this mode exposes:**

- **No waterfall exists in the UI.** The roadmap's "real-time diagnostics" item wants one too;
  build it once.
- **The UI spectrum is blank outside scanner mode** (it waits for a normal lease). In data mode
  the data service owns the spectrometer, because each frame can be read once. The UI's
  spectrum and waterfall come from the service.
- **The systems scan's carrier test** (12 dB over the floor in 80 % of frames) misses bursty
  transmitters by design. The burst detector is new, not a discovery probe.

## 9. Steps and gates

Each step is checked before the next starts.

| # | Step | Gate |
|---|------|------|
| 0 | **Ground truth, no unit code.** Andy's RTL-SDR on this PC runs rtl_433 for a day across the band plan (`-f` per band, `-H` to hop, `-F json`), as the HDHomeRun did for ATSC. Its indoor antenna gives a first look; for the comparison itself it shares unit A's antenna through a splitter, so a miss is the unit's and not the antenna's | A list of the devices really there, so `tools/data_check.py` can say what the unit finds, misses and adds |
| 1 | **Wide lanes.** 250 kSPS and 1 MSPS presets; the lane reader at any rate | On the bench (B into A through the pads; Andy wires it) or over the air from the ESP32-DIV's CC1101 stepped across each lane: flat ±100 / ±350 kHz, alias rejection measured, no lost packets with three 1 MSPS lanes for an hour |
| 2 | **The mode.** `Mode::Data` and the lease, the `packet_data` rename, the three tabs empty, round trips through all three modes | Host tests as 080's; a mode round trip on A |
| 3 | **Scan.** Band plan, scan and park, the burst detector, emitters by frequency with names, the data log per frequency, `/ws/spectrum` and the waterfall, the Scan tab | A key-fob press is seen in its frame; a sensor's interval is right; a named emitter keeps its log. The ESP32-DIV sends bursts of known frequency, length and spacing, so the detector's times can be checked |
| 4 | **Captures and raw IQ.** Per-burst cs16 and SigMF, armed captures, window snapshots, the Captures tab, `/ws/iq`. Then a day of captures on A: the corpus in `runs/data/`, decoded on the PC by `tools/rtl433_reference.py` | The corpus decodes with rtl_433 as live bursts do; it says which families to port first |
| 5 | **Decoders.** The pulse front end, the table-driven decoder, then hand-written decoders in the corpus's order; decoded devices in the data log and on Devices | Each decoder matches rtl_433 on the corpus and makes no false decodes on noise; CPU measured on A |
| 6 | **Later outputs.** NDJSON stream, MQTT with Home Assistant discovery, an rtl_tcp server | A sensor shows up in Home Assistant; SDR++ tunes a lane |
| 7 | **Later, on evidence:** more families; other data protocols on the same machinery (POCSAG, APRS, AIS; ADS-B on a 2 MSPS stage-1-only lane); FPGA classifiers trained on the corpus | |

## 10. Limits and risks

- **Antenna:** unit A's antenna was chosen for 800 MHz and UHF TV. Its response at 315-433 MHz
  is unknown; the first scan shows it.
- **Unit B is not used:** its AD9363 is specified from 325 MHz, so 315 MHz is outside its
  specification, and its indoor antenna rules out signal conclusions anyway.
- **No preselection:** VHF windows took third-harmonic images in 080. Check for images in the
  315 and 345 windows: a 316.5 MHz LO's third harmonic is 949.5 MHz, near 900 MHz paging.
- **Slow transmitters:**
  - TPMS sends while the car moves (rarely parked);
  - fobs send only when pressed;
  - security sensors send a supervision packet about hourly.
  - The log fills over hours; that is the nature of the band, not a fault.
- **Decoder breadth:** ours grow with the corpus. A device not yet captured here is not decoded,
  but its bursts still land in its frequency's log with their signature, and it can be named.
- **Licence:** only facts come from rtl_433 (section 5). Code review checks for translated code
  as it would for any copied code.
- **One mode at a time:** no calls are followed in data mode.
- **The DC spur:** keep every LO off the hot spots, as the plan above does.

## 11. Gateware later

Data mode needs no bake. The gateware changes that would serve it and the other modes are in the
radio core's plan, `doc/changes/079_general_radio_core.md`, section "Every mode's needs" (the
roadmap points there). For data mode:

- **Keep wide lanes when the channelizer comes (item 1).** 079 step 3b replaces the three DDCs,
  which are what make 250 kSPS and 1 MSPS lanes, with 50 kSPS channelizer lanes. Unless 3b keeps
  some DDC lanes, or its synthesizer can join more bins, data mode loses its lanes at that bake.
- **A larger lane ring (item 3)**, **a deeper spectrum ring with average and peak (4, 5)**, and
  **time on spectrum frames and captures (6, 7)**: these make the burst detector and captures
  exact rather than approximate.
- **Later, on evidence:** a 30 MSPS window for 902-928 MHz (8), a burst detector in the PL (9),
  and a classifier tap (10).

## 12. Decisions

**Andy, 2026-10-04:**

1. **Unit A** does the work.
2. **Names:**
   - the mode is `data` (label "Data");
   - P25 packet data's route and event class become `packet_data` (section 8);
   - the tabs are Scan, Devices and Captures.
3. **Decoders:** all in Rust. Ported from the protocols' facts, with rtl_433 as the reference on
   the PC (section 5).
4. **Outputs:** all of them are designed in. WebSocket and REST come first, for decoded data and
   raw IQ alike.
5. **The log:** data goes into a data log per identified device or frequency, and the user names
   devices during the scan. The Diagnostics event log keeps only the mode's events.
6. **Bands:** 315 MHz (not 415), 345, 433.92 and 902-928 MHz, with 390 and 868 as extras.

7. **Ground truth and test signals:**
   - **The ground truth:** Andy's RTL-SDR on this PC, running rtl_433 (step 0). It has an
     indoor antenna now.
   - **Not what the decoders are checked on:** they are checked on the unit's own captures
     (step 4). The ground truth only answers what the unit misses.
   - **Test signals:** the ESP32-DIV's CC1101 sends known bursts. It can also replay captures of
     Andy's own devices, for the gates of steps 1, 3 and 5.

**Open:**

- **The RTL-SDR's antenna for the comparison:** a splitter on unit A's antenna (same signal, about
  3.5 dB less to each), or its own outdoor antenna. With the indoor antenna, the two lists
  differ by antenna as well as by receiver.
