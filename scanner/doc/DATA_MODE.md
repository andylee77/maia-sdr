# Data mode: a study

Andy, 2026-10-04: a third mode beside the scanner and ATSC TV. In it the unit watches the
open-protocol data bands (315, 433.92 MHz and the like), finds the devices transmitting nearby and
what protocols they use, captures them, and streams their data. This is a study only: nothing is
built and no code changes with it.

## 1. What the mode does

- **Survey:** step through the data bands, listen long enough in each to hear the slow
  transmitters, and list every device heard: what it is, its ID, frequency, signal, how often it
  sends and its latest reading.
- **Identify:** decode known devices by protocol. For signals nothing decodes, give the
  modulation (OOK or FSK), the pulse timing and the repeat interval, so an unknown emitter is
  still a row in the list rather than noise.
- **Monitor:** park on one band and decode everything in it as it arrives.
- **Capture:** keep the IQ of a burst (or of the whole window) for the PC: inspectrum, URH,
  rtl_433 with `-r`.
- **Stream:** a device's readings live in the browser, and every decode out of the unit (a
  WebSocket or HTTP stream, MQTT for Home Assistant).

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
- **"415 MHz"** is not a device band in the US (410-420 MHz is federal). Andy most likely means 315 MHz;
  418 MHz carries some UK key fobs.
- **The 16 MSPS window** (±7.2 MHz usable) covers a band at a time:

  | Window | LO (DC kept off the hot spots) | Covers |
  |--------|--------------------------------|--------|
  | 315 | 316.5 | 309.3-323.7 |
  | 345 | 345.5 | 338.3-352.7 |
  | 433 | 435.0 | 427.8-442.2 |
  | 915 low / high | 909.0 / 921.0 | 902-916 / 914-928 |

  These are a starting plan. The first survey on unit A refines it.

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
 ├─ spectrometer, peak-hold, 65 ms frames → burst detector → activity (unknown emitters) + waterfall
 ├─ lane 0, 250 kSPS or 1 MSPS at a hot spot   ┐
 ├─ lane 1, 250 kSPS or 1 MSPS at a hot spot   ├→ pulse front end → decoder → device inventory → history,
 ├─ lane 2, roaming: moves to busy unknowns    ┘   /ws/live, streams out, MQTT
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
  - This is what identifies a protocol that nothing decodes.
- **Survey:**
  - steps through the band plan's windows, dwelling in each (default 2 minutes: most sensors
    send every 16-60 s);
  - a window change costs the 200 ms `LO_SETTLE`;
  - cycles until stopped.
  - **Park** is the same with one window.
- **Gain:** ATSC found the slow-attack AGC best, but that was continuous TV. Bursts may be
  clipped or chopped by an AGC that is still settling. Measure fast attack against manual gain
  on bursts before choosing a default.

## 5. Decoding: the main decision

rtl_433 (GPL-2.0+) decodes 386 device protocols. Buildroot 2025.05, which the Tezuka firmware
uses, packages version 23.11. It reads IQ from a pipe (`-r cs16:-`, with `-s` for the rate) and
writes JSON lines (`-F json`, `-M level` for RSSI and SNR).

| | A. rtl_433 on the unit | B. Rust decoders only | C. Both, staged |
|-|------------------------|-----------------------|-----------------|
| How | One rtl_433 process per lane, fed the lane's IQ through a pipe; the scanner reads its JSON | Write the pulse front end and each device decoder in the crate | Detection, the pulse front end, the inventory and captures in Rust; rtl_433 as the decoder, on the unit and as the PC reference; Rust decoders later where measured worth it |
| Coverage on day one | 386 protocols | The few written | 386 protocols |
| Licence | A separate GPL program in the image beside the MIT scanner, as BusyBox and the kernel are now; nothing of it in the crate | Only from public protocol descriptions: most device formats are documented only in rtl_433's GPL code, so it cannot be ported (the philburr/atsc rule) | As A |
| CPU | Unmeasured on the A9; rtl_433 runs at 250 kSPS on Raspberry Pi Zero-class boards | Lowest | As A until measured |
| Fits the project's pattern | The reference runs on the board itself | SDRTrunk-style: own code, checked against a reference | Own front end checked against rtl_433, as DMR is against SDRTrunk |

**Recommendation: C.** The parts that need the radio core (windows, lanes, burst detection,
captures, the inventory) are ours. The protocol zoo is rtl_433's, which already exists and is
tested. Decodes are stamped with the lane's air time (the packet sample index), not rtl_433's
clock.

## 6. Streaming and capture

**Decoded data out** (the first meaning of "stream the data"):

- the browser: `/ws/live` sends a message per decode, and the device's own panel updates live
  (section 7);
- `GET /api/v1/devices/stream`: newline-delimited JSON for scripts, held open;
- MQTT: a publisher configured in Settings, with rtl_433-style topics and Home Assistant
  discovery, so sensors show up in Andy's Home Assistant.

**Raw IQ out** (the second meaning, if wanted):

- a lane's IQ live, as binary frames on a socket of its own, as `/ws/audio` is;
- or an rtl_tcp-compatible server, so SDR++, URH or rtl_433 on the PC can tune the unit's lane.

**Captures:**

- **Per burst:** the lane IQ around a decode or an unknown burst, kept as cs16 with a SigMF
  `.sigmf-meta` (frequency, rate, time, the decode if any). A 100 ms burst at 250 kSPS is about
  100 KB.
- **Arm a capture:** "keep the next N bursts from this emitter".
- **Window snapshot:** the capture ring, up to 0.26 s at 16 MSPS.
- **Where:** files under `/mnt/sd`, listed and downloadable from the API.
- Each decoded burst plus its IQ is a labelled example: the training data that
  `_shared/FPGA_ML_INFERENCE_GUIDE.md` (method 4, rtl_433 cross-labelling) calls for.

## 7. Tabs

Following 080: each mode has its tabs, and Diagnostics and Settings are shared.

| Mode | Tabs |
|------|------|
| scanner | Now, Activity, Systems |
| atsc | ATSC |
| data | **Devices** (the mode's home), **Spectrum**, **Captures** |
| all | Diagnostics, Settings |

**Devices**: the inventory, with a picked device's panel.

```text
┌ Survey: 433 window (2 of 5), 1:12 left ── 14 devices, 3 new this hour ── [Park] [Stop] ┐
│ Kind     Model / emitter        ID      MHz      Last    Every   RSSI  SNR   Reading     │
│ weather  Acurite-Tower          12345   433.92   0:21    16 s   -61   18   22.4 °C 71 % │
│ TPMS     Toyota                 0x1A2B  314.98   3 h     —      -78    9   230 kPa      │
│ security Honeywell 5800         0x9C11  345.00   4 m     70 m   -66   14   closed       │
│ unknown  OOK PWM 400/1200 µs    —       433.81   0:05    30 s   -70   12   [capture]    │
└────────────────────────────────────────────────────────────────────────────────────────┘
┌ Acurite-Tower 12345 ──────────────────────────────────── [Name] [Ignore] [Capture next] ┐
│ temperature_C ▁▂▃▅▆▇▇▆  humidity ▇▇▆▅▅▄  battery_ok 1     live: every reading as it comes │
│ raw JSON of the last decode                                                            │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Filters:** kind, band, seen within.
- **Device actions:** name it (an alias, as the scanner's aliases are), ignore it, capture its
  next bursts.
- **Unknown emitters** are rows too, with their pulse signature in place of a model.

**Spectrum**: the window live.

- A waterfall and spectrum of the window, with the lanes shaded (drag a lane to move it) and the
  bursts marked.
- The band plan with Survey and Park.
- The activity list: emitters by frequency, bandwidth, count, interval and modulation guess.
- A lane's pulse view: envelope or discriminator, the sliced pulses and the decoded bits.

**Captures**: the files kept, with their time, frequency, rate, length and decode; download
(cs16 and SigMF) and delete; window snapshots.

**Shared tabs:**

- **Settings** gains a Data card:
  - the band plan (windows, dwell, lane spots);
  - decoder families on or off;
  - lane rates;
  - MQTT;
  - retention and capture storage.
- **Diagnostics:** the mode's events (survey steps, new devices) go to the one event log.
  Whether every decode also goes there or only to Devices is decision 4.
- **Header:** the mode selector gains Data; in data mode the header's line shows the window and
  the device count, as ATSC's shows the scan.

## 8. What changes in the code

**The mode:**

- `Mode` gains a third variant.
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

**Naming:** "data" already means P25 packet data:

- the `/api/v1/data` route;
- `services::packet_data`;
- the event class `data`;
- `views/packet_data.js`.

The mode needs another identifier (`devices` is used here), whatever the tab says (decision 1).

**New modules:**

| Module | Owns |
|--------|------|
| `protocol::ism` | Pulse front end (envelope, discriminator, slicer, timing histograms, modulation and coding guess) |
| `services::devices` | The mode: lease, band plan, survey and park, lanes, burst detector, rtl_433 processes, inventory, captures, streams |
| `hardware/presets` | Wide-lane presets; the lane reader learns each lane's rate (it assumes 50 kSPS: `radio/streams`) |
| history | `devices`, `device_readings`, `emitters` tables, with retention by age and size. Retention today is keyed on calls, so the new tables join `prune`, `trim_to` and `clear` |
| API | `/api/v1/devices[...]` (inventory, one device, readings, plan, survey, park, activity, captures, stream); `/ws/live` feeds `devices` and `waterfall` |
| UI | `views/devices.js`, `views/spectrum.js` (a name clash with the Diagnostics component, to settle), `views/captures.js`; a shared waterfall component |

**Gaps this mode exposes:**

- **No waterfall exists in the UI.** The roadmap's "real-time diagnostics" item wants one too;
  build it once.
- **The UI spectrum is blank outside scanner mode** (it waits for a normal lease). In data mode
  the devices service owns the spectrometer, because each frame can be read once. The UI's
  spectrum and waterfall come from the service.
- **The systems scan's carrier test** (12 dB over the floor in 80 % of frames) misses bursty
  transmitters by design. The burst detector is new, not a discovery probe.

## 9. Steps and gates

Each step is checked before the next starts.

| # | Step | Gate |
|---|------|------|
| 0 | **Ground truth, no unit code.** rtl_433 on the PC with a receiver at A's location, a day across the band plan (an RTL-SDR, or unit A through libiio when it is free), as the HDHomeRun was for ATSC | A list of the devices really there: what data mode must find |
| 1 | **Wide lanes.** 250 kSPS and 1 MSPS presets; the lane reader at any rate | On the bench (B into A through the pads; Andy wires it): flat ±100 / ±350 kHz, alias rejection measured, no lost packets with three 1 MSPS lanes for an hour |
| 2 | **The mode.** `Mode` and lease work, the Devices tab empty, round trips through all three modes | Host tests as 080's; a mode round trip on A |
| 3 | **Window and bursts.** Band plan, survey and park, burst detector, waterfall | A key-fob press is seen in its frame; a sensor's interval is right |
| 4 | **Decoding.** rtl_433 per lane, the inventory, the history tables, the Devices page | `tools/data_check.py` (as `atsc_check.py`) against step 0's list; CPU measured on A |
| 5 | **Streams.** `/devices/stream`, MQTT and Home Assistant discovery, the live device panel | A sensor shows up in Home Assistant |
| 6 | **Captures and unknowns.** Per-burst cs16 and SigMF, armed captures, window snapshots, the pulse signature, the Captures page | A capture decodes with rtl_433 `-r` on the PC as it did live |
| 7 | **Later, on evidence:** Rust decoders where rtl_433 costs too much or lacks a device; other data protocols on the same machinery (POCSAG, APRS, AIS; ADS-B on a 2 MSPS stage-1-only lane); FPGA classifiers trained on step 6's captures | |

## 10. Limits and risks

- **Antenna:** unit A's antenna was chosen for 800 MHz and UHF TV. Its response at 315-433 MHz
  is unknown; step 0 and the first survey show it.
- **Unit B:** its AD9363 is specified from 325 MHz, so 315 MHz is outside its specification, and
  its indoor antenna rules out signal conclusions anyway. A is the unit for this work.
- **No preselection:** VHF windows took third-harmonic images in 080. Check for images in the
  315 and 345 windows: a 316.5 MHz LO's third harmonic is 949.5 MHz, near 900 MHz paging.
- **Slow transmitters:**
  - TPMS sends while the car moves (rarely parked);
  - fobs send only when pressed;
  - security sensors send a supervision packet about hourly.
  - The inventory fills over hours; that is the nature of the band, not a fault.
- **One mode at a time:** no calls are followed in data mode.
- **rtl_433 on the A9:** CPU unmeasured (step 4). Three processes at 1 MSPS may be too much;
  250 kSPS is the default.
- **The DC spur:** keep every LO off the hot spots, as the plan above does.

## 11. Gateware later

Data mode needs no bake. The gateware changes that would serve it and the other modes are
listed in `doc/ROADMAP.md`, "Gateware for every mode". For data mode:

- **Keep wide lanes when the channelizer comes (item 1).** 079 step 3b replaces the three DDCs,
  which are what make 250 kSPS and 1 MSPS lanes, with 50 kSPS channelizer lanes. Unless 3b keeps
  some DDC lanes, or its synthesizer can join more bins, data mode loses its lanes at that bake.
- **A larger lane ring (item 3)**, **a deeper spectrum ring with average and peak (4, 5)**, and
  **time on spectrum frames and captures (6, 7)**: these make the burst detector and captures
  exact rather than approximate.
- **Later, on evidence:** a 30 MSPS window for 902-928 MHz (8), a burst detector in the PL (9),
  and a classifier tap (10).

## 12. Decisions for Andy

1. **The mode's name.** The tab and selector can say "Data", but `data` is taken by P25 packet
   data in the API and code. Use `devices` inside, or rename packet data's route?
2. **Decoding:** rtl_433 on the unit as a separate program (C, recommended), or Rust decoders
   only (B)?
3. **"Stream the data":** decoded readings (WebSocket, HTTP stream, MQTT), the raw IQ of a lane,
   or both? Decoded first is recommended.
4. **Decodes in the event log:** every decode in the Diagnostics events box (the rule for protocol
   messages), or only the mode's events there and decodes on Devices?
5. **Bands:** the plan above (315, 345, 433.92, 902-928), with 390 and 868 as extras. Is "415"
   315?
6. **Ground truth:** which receiver runs rtl_433 on the PC for step 0?
