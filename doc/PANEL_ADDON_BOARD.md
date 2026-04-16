# P25 Scanner Panel — Add-On Board Design

**Status:** Future project / design sketch
**Host board:** OpenSDRLab-7020 (Fishball Z7020) — JP5 20-pin expansion header
**Author's goal:** Standalone P25 scanner UX without a browser, via a plug-in daughterboard with LCD, rotary encoder, buttons, and a speaker. PCB fabbed at JLCPCB (or equivalent), mounted in a 3D-printed enclosure.

---

## 1. Goals and Scope

- **Plug-and-play:** single 20-pin 2.54 mm header mates directly with JP5 — no wiring harness.
- **Power:** drawn from JP5 pin 5 (5 V). No external supply.
- **Operator UX:** see currently active talkgroup, cycle/select TG of interest, mute/unmute, speaker audio.
- **Firmware-friendly:** the add-on should look like a serial peripheral to `p25-httpd`, so the Rust side exposes a small `/api/panel` endpoint and everything else is independent of gateware.
- **Hobby-fab friendly:** 2-layer board, JLCPCB-assemblable parts, no BGAs, no 0402-only passives.

### Non-goals

- Not a replacement for the web dashboard — the dashboard stays the "power user" interface.
- Not a transmitter — RX-only, matching the current P25 scope.
- Not a battery-powered portable (at least not in v1).

---

## 2. Feature Set

| Control / Output | Purpose |
|------------------|---------|
| 1.3"–2.0" color LCD or 0.96"–1.3" OLED | Show active TG, NAC, RSSI, call state |
| Rotary encoder with push-switch | Scroll through allowed TGs; press = select / mute toggle |
| 2× tactile buttons | Quick actions (e.g. "return to CC", "cycle site") |
| 1–2 status LEDs | CC lock, call active |
| 8 Ω / 0.5–1 W speaker on I²S DAC+amp | Voice audio |
| USB-C | MCU firmware programming / console |

Optional (v2):

- 1PPS input SMA (for timestamp discipline).
- Second encoder for volume.

---

## 3. Architecture Options

Three realistic partitions between **MCU on the add-on board** vs **PL gateware**.

### Option A — MCU-driven UI + PL I²S audio (recommended)

```
JP5 Bank 13 (3.3V) ──► XIAO ESP32-S3 ──► 1.5" SSD1351 SPI color OLED
                         │                + EC11 encoder + 2 buttons + 2 LEDs
                         │
                         └── UART ◄──► p25-httpd (/api/panel)

JP5 Bank 35 (1.8V) ──► MAX98357A I²S amp ──► 8Ω speaker
   (new Amaranth i2s_tx module in p25_hdl)
```

**Pros**

- LCD driver, font rendering, menu logic all run on the MCU — no new HDL for UI.
- PL side only adds one small I²S transmitter module. Clean responsibility split.
- MCU firmware iterates independently (USB-C reflash on the module itself) without re-baking Vivado.
- XIAO is a pre-assembled module: solder two rows of castellated pads and that's the entire MCU subsystem (no LDO, crystal, USB-C, ESD diode, or external flash on the daughterboard).

**Cons**

- Requires an I²S transmitter module in gateware *and* a vocoder PCM → I²S path in Rust.
- Two firmware targets to maintain (MCU + `p25-httpd`).
- ESP32-S3 has Wi-Fi/BLE radios on board — must be kept disabled by default to avoid coupling into the SDR front-end (see §11).

### Option B — Fully PL-driven

Everything (LCD, encoder, I²S, audio) is hung off JP5 GPIOs and driven from gateware + `p25-httpd`.

**Pros**

- Single firmware codebase.
- No MCU, smaller BOM.

**Cons**

- SPI LCD driver in Amaranth or bit-banged from Rust over a GPIO block is a lot of work for UI features you otherwise get free on a Pico.
- Font/bitmap rendering in Rust on a 650 MHz Cortex-A9 is fine, but every UI tweak needs a `p25-httpd` rebuild.
- Bank 35 is 1.8 V — direct drive of 3.3 V LCDs requires level shifters anyway.

### Option C — MCU-driven UI + MCU audio via USB sound card

Audio streamed from `p25-httpd` over USB (MCU acts as a USB audio class device to the Zynq if USB host is ever wired). Attractive in theory but Fishball doesn't expose USB host on JP5, so it's a non-starter without another board mod.

### Verdict

**Go with Option A.** It splits the problem cleanly: HDL adds one new module (I²S TX), Rust adds one PCM router + one small serial protocol, and the add-on becomes an independent embedded project that can be developed and iterated on its own cadence.

---

## 4. JP5 Pin Allocation (Option A)

12 usable signals on JP5, split by bank:

### Bank 13 (3.3 V, single-ended) → XIAO control plane

| JP5 Pin | Signal | XIAO Pin (typical) | Role |
|---------|--------|--------------------|------|
| 7 | 3V3 IO1 (V10) | D7 (RX) | UART TX (PS → XIAO) |
| 9 | 3V3 IO2 (U9) | D6 (TX) | UART RX (XIAO → PS) |
| 11 | 3V3 IO3 (U10) | D5 (input) | "Attention" / wake line, optional |
| 13 | 3V3 IO4 (T9) | EN | XIAO reset, optional (pull high in normal operation) |

Rationale: UART is slow, low-pin, trivially bridged in Linux (`/dev/ttyPSx` or PL AXI-UART → `/dev/ttyULx`). Two data lines + optional reset/wake is plenty. XIAO ESP32-S3 has fully assignable peripherals so the actual D-pin choice can be re-routed in firmware to whatever simplifies the PCB layout.

### Bank 35 (1.8 V, LVDS-capable) → I²S audio

| JP5 Pin | Signal | Role |
|---------|--------|------|
| 2 (IO1_P, G19) | I²S BCLK | Bit clock |
| 4 (IO1_N, G20) | I²S LRCLK | Word select / frame |
| 6 (IO3_P, J18) | I²S SDIN | Data in to DAC |
| 8 (IO3_N, H18) | I²S MCLK (optional) | Master clock for DAC (MAX98357A does NOT need this — leave as spare) |
| 10, 12, 14, 16 | Spare | Reserved for v2 (second channel, 1PPS, etc.) |

MAX98357A accepts 1.8 V logic on its I²S inputs (Vih min ≈ 1.3 V at 5 V VDD per datasheet). No level shifting required. Good fit for Bank 35 without extra parts.

### Power / other

| JP5 Pin | Use |
|---------|-----|
| 5 (VCC5V) | Primary 5 V supply for the add-on (feeds both MAX98357A and MCU LDO) |
| 18, 20 (GND) | Ground |
| 17 (PTT), 19 (SYNC), 15 (XTAL_VTC) | **Not used** — leave the header pass-through or NC on the board so a future TX add-on can still use them |
| 1 (1.8V), 3 (3.3V) | Optional: tap 3.3 V directly from the base board instead of running an LDO — saves a part, but watch the total Bank 13 / 3V3 current budget first |

---

## 5. Schematic Outline

### 5.1 Power tree

```
JP5 pin 5 (5V) ─┬── MAX98357A VDD (decoupled 10µF + 0.1µF)
                ├── XIAO ESP32-S3 5V pin (module has its own onboard 3.3V LDO)
                │      └── XIAO 3V3 out ── color OLED VCC, encoder pull-ups, LEDs
                └── Speaker return path (via amp output)
```

- Decouple 5 V with 10 µF bulk + 0.1 µF at each IC.
- Add a 0 Ω jumper or SPDT switch between JP5-5V and the add-on 5 V rail so you can measure current draw during bring-up — and so the XIAO can be powered from its own USB-C during dev without backfeeding the Fishball.
- Budget: XIAO with Wi-Fi off ≈ 30–50 mA, color OLED ≈ 30–80 mA depending on size and brightness, MAX98357A peak ≈ 400 mA at full output into 8 Ω. Worst case ≈ 500 mA from JP5. **Verify the Fishball 5 V rail can source this** before finalizing v1 — if not, add a barrel jack for external 5 V and jumper-select.
- The XIAO's onboard 3.3 V LDO is rated for ~700 mA, so it can also supply the OLED + encoder pull-ups. Keep the OLED VCC tied to the XIAO 3.3 V output rather than running a separate LDO on the daughterboard.

### 5.2 MCU

**Selected part: Seeed Studio XIAO ESP32-S3.** User already has these on hand, which collapses the MCU subsystem to a single hand-soldered module.

- ESP32-S3 dual-core 240 MHz, 8 MB flash, optional 8 MB PSRAM (XIAO ESP32-S3 *Sense* variant).
- 11 GPIOs broken out, all with peripheral matrix routing — assign UART / SPI / I²S / I²C to whichever pins simplify the PCB layout.
- Onboard USB-C, onboard 3.3 V LDO (~700 mA), onboard reset and boot buttons, onboard charging IC for an optional Li-Po (don't connect one here — power comes from JP5).
- Mounts via 2 × 7 castellated pads at 2.54 mm pitch on the long edges, plus optional 2 × 6 inner pads on the underside (USB lines, additional GPIOs). The daughterboard footprint is just two rows of pads — no fine-pitch IC, no crystal, no external flash.
- Flashed over USB-C using esptool / Arduino IDE / ESP-IDF / esp-rs. No external programmer.
- **External antenna variant.** User's modules use the XIAO ESP32-S3 with the onboard u.FL/IPEX connector and the bundled flat 2.4 GHz adhesive antenna. The 0 Ω resistor on the XIAO is configured to route Wi-Fi/BLE RF to the u.FL connector (factory default for the external-antenna kit). This means the radiating element is physically separable from the daughterboard — see §7 and §11 for routing and EMI implications.
- **Wi-Fi/BLE radios are present but kept disabled by default** — see §11 for the EMI rationale.

Mechanical: the XIAO is ~21 × 17.5 mm. Plan to recess it slightly so the USB-C port is accessible at one edge of the daughterboard for re-flashing without removing the panel from the enclosure.

### 5.3 Display — 1.5" color OLED (SSD1351)

**Recommended part: 1.5" SSD1351 RGB OLED, 128 × 128, 16-bit color, SPI.** Sold by Waveshare and many AliExpress sellers; ~$20–25. True OLED (per-pixel emissive, infinite contrast, no backlight) — fits the user's "good OLED" criterion. Very well supported in firmware (`Adafruit_SSD1351` Arduino library, `lvgl` driver, `embedded-graphics` Rust driver via `ssd1351` crate).

Mechanical: Waveshare's breakout PCB is approximately 43 × 38 mm; the active glass area is 27 × 27 mm. The breakout has a 7-pin 0.1" header that we'll mate to with female headers on the daughterboard so the OLED can be removed for service or swap.

Wiring (SPI, 7 signals):

| OLED pin | XIAO pin (typical) | Notes |
|----------|--------------------|-------|
| VCC | 3V3 | From XIAO LDO |
| GND | GND | |
| SCK / D0 | D8 (SCK) | SPI clock, up to 20 MHz |
| MOSI / D1 | D10 (MOSI) | SPI data |
| CS | D2 | GPIO, active-low |
| DC | D3 | Data/command select |
| RST | D4 | Active-low, hardware reset |

Alternative if cost or size matters more than wow-factor:

- **SSD1331 0.96" 96 × 64 SPI color OLED** — ~$10, much smaller display area, otherwise pin-compatible (different controller, library swap only). Acceptable but harder to read at distance.

Note: many "color screens" sold with XIAO ESP32-S3 are actually IPS LCDs (ST7789, GC9A01), not OLEDs. They're brighter and cheaper but have a backlight and worse blacks. The SSD1351 is specifically chosen because the user asked for a *good OLED*, not just any color display.

### 5.4 Audio path

```
I²S (BCLK/LRCLK/SDIN) ──► MAX98357A ── 8Ω 1W speaker
                              │
                              └── GAIN pin ─► resistor-select 3/6/9/12/15 dB
```

- MAX98357A is a complete I²S class-D amp, 3.2 W into 4 Ω at 5 V. For a small internal speaker, 8 Ω / 0.5–1 W at 9 dB gain is plenty loud for voice.
- Bridge-tied output — speaker has two wires, no ground reference to the enclosure. Use a 2-pin JST-PH.
- Optional: series RC snubber (100 nF + 10 Ω) across speaker terminals for EMI on long speaker wires.
- Optional: SHDN pin tied to an MCU GPIO so the Pico can cut power to the amp when the call ends (eliminates idle hiss entirely).

### 5.5 Encoder / buttons / LEDs

- Rotary encoder: EC11 with integrated push-switch, 20 detents. Connect A, B, SW to XIAO GPIOs with 10 kΩ pull-ups. Debounce in firmware (ESP32-S3 has hardware pulse-counter peripheral usable for clean encoder reads).
- Buttons: 2 × 6 × 6 mm tactile, pulled to 3V3, XIAO GPIO to ground when pressed.
- LEDs: 2 × 0603 or 0805, ~1 kΩ series resistor, driven by XIAO GPIO directly (low-side).
- No ESD specific to these — they're inside the enclosure.

### 5.6 USB-C

Built into the XIAO ESP32-S3 module — no separate USB-C receptacle, ESD diode, or D+/D− routing on the daughterboard. Position the XIAO so the USB-C edge faces an enclosure cutout for re-flashing without disassembly. The XIAO's USB 5 V is internally diode-OR'd with the 5 V pin, so plugging in USB-C while JP5 is also connected is safe — but for clean current measurements during bring-up, unplug one or the other.

---

## 6. Bill of Materials (rough)

| Ref | Part | Qty | Source | Approx $ | Hand-solder? |
|-----|------|-----|--------|----------|--------------|
| U1 | Seeed XIAO ESP32-S3 module | 1 | Already owned | — | Yes (castellated) |
| U2 | MAX98357AETE+T (I²S Class-D amp) | 1 | Mouser/DigiKey/LCSC | 2.50 | Yes (TQFN-16, hot air) |
| J1 | 2×10 2.54 mm pin header, straight | 1 | Generic | 0.50 | Yes |
| J2 | JST-PH 2-pin (speaker) | 1 | Generic | 0.20 | Yes |
| OLED1 | Waveshare 1.5" SSD1351 RGB OLED, 128×128 SPI | 1 | Waveshare / AliExpress | 22.00 | Yes (female header) |
| SW1 | EC11 rotary encoder w/ switch | 1 | Generic | 1.00 | Yes |
| SW2/SW3 | 6 × 6 mm tactile | 2 | Generic | 0.10 | Yes |
| SP1 | 8Ω 1W speaker, ~28 mm | 1 | Generic | 1.50 | Wires to JST |
| Passives | 0603 R/C, decoupling, pull-ups | ~12 | LCSC | 0.30 | Yes |
| LEDs | 2 × 0603 + resistors | 2 | LCSC | 0.05 | Yes |

**Target BOM cost: ≈ $28 per board in new parts** (XIAO reused; OLED is the dominant line at $22). Order PCBs from JLCPCB *without* assembly — every part on the board is hand-solderable, so the cheapest "PCB only" tier ($2 for 5 boards) is the right service. This eliminates the ~$30–40 PCBA setup fee and the part-rotation/orientation hassle that would otherwise dominate cost on a small run.

The MAX98357A in TQFN-16 is the only fine-pitch part. It's reflow-friendly with hot air and a stencil, or hand-solderable with a fine tip and flux if you're confident. If hand-soldering is undesirable, swap to a MAX98357A breakout module (Adafruit / generic AliExpress, ~$5) and add a 4-pin header — same circuit, slightly larger footprint, zero fine-pitch work.

---

## 7. PCB Layout Notes

- **Form factor:** ≈ 70 × 50 mm. The Waveshare 1.5" OLED breakout is ~43 × 38 mm and dominates the layout; the encoder, two buttons, two LEDs, and the speaker JST need to share the remaining space. Should easily clear JP5 and sit above the Fishball board with 10–15 mm standoffs. Confirm against the Waveshare drawing before ordering boards.
- **Stack-up:** 2-layer, 1.6 mm FR4, HASL or ENIG. No impedance control needed.
- **Connector orientation:** put J1 (the 2×10 header) on the *underside* edge so the panel plugs straight down onto JP5. OLED / encoder / buttons on the top side.
- **XIAO placement:** mount on the top side with castellated pads; orient so the USB-C edge faces an enclosure cutout. Leave 1.5 mm clear under the XIAO module (no tall components beneath).
- **Ground plane:** continuous ground pour on bottom layer, stitched with vias under the MAX98357A and the XIAO module.
- **Audio amp:** keep the speaker traces short and thick (≥ 20 mil); place the 100 nF bypass right at the VDD pin. Keep the class-D switching node at least 15 mm away from the JP5 header to limit conducted noise back into the SDR.
- **I²S routing:** BCLK, LRCLK, SDIN are 1.8 V signals running alongside switched 5 V to the amp. Ground guard-trace is cheap, add one.
- **Wi-Fi/BLE antenna:** XIAO is the *external-antenna* variant, so RF leaves the module via the u.FL connector. No PCB antenna keep-out is needed on the daughterboard. Plan the enclosure so the u.FL pigtail can route to the *opposite side* of the case from the SDR's RF SMAs, and stick the flat antenna to the inside wall furthest from the AD9361 front end. If Wi-Fi/BLE is not used at all, terminating the u.FL with a 50 Ω dummy load (or simply leaving the antenna disconnected) reduces stray radiation further still.
- **Silkscreen:** label every button, every LED, and the UART polarity on the header footprint so bring-up is trivial. Mark the JP5 pin-1 dot.
- **Test points:** 5 V, 3.3 V (XIAO output), UART TX, UART RX, I²S BCLK. One-pin 0.1" pads are fine.

---

## 8. Enclosure (3D-printed)

Two-part design, PETG or PLA+:

- **Base shell:** screws onto the Fishball board mounting holes with M3 brass inserts / standoffs. Contains a cutout for JP5 so the panel PCB plugs down through it. Leaves a rectangular window above the Fishball SMAs.
- **Front face:** snap-fits or screws onto the base. Has:
  - Rectangular cutout for the LCD visible area.
  - Circular hole for the encoder shaft (knob clearance ≈ 14 mm).
  - Two 7 mm holes for button caps (or just open tactile access).
  - Speaker grille (hex or slot pattern, 60–70% open area).
  - Side cutout for the USB-C port.
- **Materials / finish:** PETG 0.2 mm layers, 3 perimeters, 20% gyroid infill. Sand + paint the front face if you want a polished look.

Fusion 360 / FreeCAD / OpenSCAD, whichever you prefer — parametric so you can retune the LCD cutout for panel version B without rebuilding the whole model.

**Design tip:** leave 3–5 mm of clearance on all sides of the PCB inside the enclosure — 3D prints warp, and "perfect fit" in CAD means "won't close" in reality.

---

## 9. Firmware Plan

### 9.1 MCU (XIAO ESP32-S3)

Toolchain options, in rough order of "least friction first":

- **Arduino IDE + ESP32 Arduino core** — fastest path to a working LCD + encoder loop. `Adafruit_GFX` + a controller-specific driver (e.g. `Adafruit_SSD1351`) and `ESP32Encoder` get you to a usable panel in an evening.
- **PlatformIO + ESP-IDF** — better long-term, proper FreeRTOS tasks for UART parser + UI redraw + encoder polling on different cores.
- **esp-rs (Rust)** — works on ESP32-S3, less mature than ESP-IDF for graphics but viable if Rust everywhere is preferred.

Responsibilities:

- Drive color OLED over SPI (or I²C if that's what the part is).
- Read encoder + buttons with IRQ-driven debounce; ESP32-S3 has a hardware PCNT (pulse counter) peripheral — use it for the encoder rather than GPIO IRQs, much cleaner.
- Render a simple menu: "Active call", "TG list", "Site info", "Settings".
- UART protocol to `p25-httpd` (see §9.3) on UART1; reserve UART0 for the USB-C serial console for debugging.
- **Wi-Fi/BLE OFF** at boot via `WiFi.mode(WIFI_OFF)` and `btStop()` (or equivalent IDF calls). See §11.

Flash layout: default ESP32-S3 partition table + single application binary. No OTA needed; USB-C reflash on the XIAO itself is the dev loop.

### 9.2 `p25-httpd` side

- New module `p25-httpd/src/panel.rs`:
  - Opens the UART device (`/dev/ttyULx` for PL UART, or `/dev/ttyPSx` for PS UART — probably PL given JP5 pins go to Bank 13 PL I/O).
  - Runs an async task that serializes/deserializes the panel protocol.
  - Bridges to the existing grant store / call manager / mute state.
- New HTTP endpoint `/api/panel` for dashboard parity (what TG is selected, mute on/off, LCD text mirror) — useful for remote debugging.
- Expose audio routing toggle: `/api/audio/sink` with values `ws`, `i2s`, `both`. Lets the dashboard still work when the panel is plugged in.

### 9.3 Panel serial protocol (sketch)

Framed newline-delimited JSON over UART @ 115200 8N1 — tiny, human-readable for debugging, cheap to parse on both sides.

PS → panel (every 100 ms, or on change):

```json
{"t":"state","tg":12345,"tg_label":"PD DISPATCH","nac":"8A1","rssi":-72,"call":"active","mute":false}
```

Panel → PS (on user action):

```json
{"t":"tg_next"}
{"t":"tg_prev"}
{"t":"tg_select","tg":12345}
{"t":"mute_toggle"}
{"t":"btn","id":1}
```

Keep it append-only: new fields must not break older firmware on either side.

### 9.4 Gateware

New Amaranth module `p25_hdl/i2s_tx.py`:

- Inputs: 16-bit signed PCM stream, strobe on new sample at 8 kHz (vocoder rate). Upsample to 48 kHz internally (linear interp is fine for voice).
- Outputs: `bclk`, `lrclk`, `sdin` — standard I²S, left-justified, 16-bit.
- Derive `bclk` from a 24.576 MHz PLL output (already present on the board? verify — if not, a low-jitter clock source on the add-on itself, fed back to the FPGA on a spare Bank 35 pin, is an alternative).

Lives in the P25 IP wrapper, driven from the same PCM FIFO that currently feeds the WebSocket audio path.

---

## 10. Milestones

1. **Breadboard prototype** — RP2040 dev board + MAX98357A breakout + OLED + encoder, wired to JP5 with jumper wires. Prove the UART protocol and audio path end-to-end. *No PCB yet.*
2. **Gateware I²S module + audio routing** — land in `p25_hdl/` with its own sim test; land Rust audio router with a `sink=both` mode so the dashboard still works.
3. **Schematic + PCB v1** — KiCad. Review against this doc, order 5 boards + assembly from JLCPCB.
4. **Bring-up** — power-on test, programming test, LCD test, encoder test, audio test in that order. Solder-jumper the USB 5 V so you can test the PCB standalone without risking the Fishball rail.
5. **Enclosure v1** — rough print in PLA, fit-check, iterate. Final in PETG.
6. **Integration test** — plug into real Fishball, scan Clay County, validate audio quality vs. browser reference.
7. **v2 PCB if needed** — silkscreen fixes, footprint tweaks, optional second encoder for volume.

---

## 11. Open Questions / Risks

- **5 V current budget on JP5.** Must measure the Fishball rail's headroom before committing. If tight, plan for an optional barrel jack.
- **Bank 13 PL I/O pin directionality.** Confirm the chosen GPIOs can be safely driven as both inputs and outputs at 3.3 V — they're PL, so device-tree + XDC constraints will set this, not anything on the add-on board.
- **PL UART vs PS UART for the MCU link.** PS UART has Linux driver support out of the box; a PL AXI-UART needs a kernel driver binding. Easier: put the MCU UART on PS UART1 if any of its pins are reachable from JP5 — otherwise PL AXI-UART is standard and well-supported.
- **I²S MCLK requirement.** MAX98357A runs MCLK-less, so no issue for v1. If a future DAC is MCLK-required, reserve an LVDS pair for it now.
- **Fan connector interaction.** FAN1 is on its own connector on the base board — the add-on doesn't touch it, but the enclosure airflow plan should account for the fan.
- **RF coupling — class-D amp.** Keep the MAX98357A class-D switching node physically separated from the JP5 header by ≥ 15 mm and shielded by ground pour. Add an LC filter on the speaker outputs if bench testing shows audible birdies in the SDR passband.
- **RF coupling — ESP32-S3 Wi-Fi/BLE.** Mitigated significantly by the external-antenna XIAO variant: the radiating element is physically off the daughterboard and can be relocated inside the enclosure to the wall furthest from the SDR front end. Conducted leakage from the module's 2.4 GHz oscillators and unintentional radiation from the u.FL pigtail are still possible but small. Mitigations: (1) keep both radios disabled by default in firmware, (2) place the flat antenna on the inside enclosure wall furthest from the AD9361 SMAs, (3) if Wi-Fi/BLE is never planned for use, just leave the antenna disconnected — eliminates intentional radiation entirely, (4) if Wi-Fi is later enabled and degrades 2.4 GHz RX, the next step is a shield can over the XIAO module (the antenna being external means a can is feasible without breaking the radio). Document any enable in firmware as an explicit user opt-in.
- **OLED supply / vendor variation.** SSD1351 1.5" modules are sold by multiple vendors (Waveshare, generic AliExpress) with slightly different breakout PCB outlines and mounting-hole positions despite identical electrical pinouts. Pick a specific vendor SKU before PCB layout and order one in advance to measure, rather than trusting a generic datasheet drawing.

---

## 12. Prior Art / Related Projects

A web survey done 2026-04-16 turned up **no existing public daughterboard project specifically targeting JP5** on the OpenSDRLab-7020 / Fishball board. JP5 is unique to the OpenSourceSDRLab variant of the Pluto+ design (the vanilla Pluto+ / "Fishball" boards don't have it), so no community ecosystem has formed around it yet. This means the project would be the first public JP5 add-on — worth open-sourcing the hardware + STLs when built.

### Conceptually identical, different host hardware

The *exact same UX pattern* (small LCD + rotary encoder + speaker for P25 talkgroup selection and audio) has been built repeatedly on Raspberry Pi rigs sitting next to an SDR. Borrow the UX and menu structure from these — they've already iterated on what a "headless P25 scanner panel" should feel like:

- [VE6BC — P25 scanner using Raspberry Pi and OP25](https://ve6bc.radio/index.php/2025/05/27/a-p25-scanner-using-raspberry-pi-and-op25/) — closest match in spirit. Encoder selects the OP25 stream, LCD shows TG and control channel name. Read their menu structure before designing yours.
- [Hackaday / RTL-SDR — Pi 5 + SDRTrunk portable P25 scanner](https://hackaday.com/2024/02/10/pi-5-and-sdr-team-up-for-a-digital-scanner-you-can-actually-afford/) — uses an HDMI touchscreen rather than a discrete LCD + encoder, so less directly applicable to the panel UX, but useful for portable enclosure ideas.
- [OP25 Bearcat IV headless Pi scanner thread (RadioReference)](https://forums.radioreference.com/threads/op25-sdr-bearcat-iv-headless-raspberry-pi-scanner.478477/) — closest to the "no browser, just a dial and a speaker" philosophy this project is targeting.

### Hardware references

- [OpenSourceSDRLab/PlutoSky_7020_AD936X_SDR](https://github.com/OpenSourceSDRLab/PlutoSky_7020_AD936X_SDR) — vendor's own GitHub. Contains `hardware/` (schematic + PCB source for the host board, useful for confirming JP5 net names against the schematic before PCB layout) and `3D case/` (vendor enclosure STL — reference geometry for cutouts and mounting holes the add-on must clear).
- [Fishball SDR Case by jangrewe (Printables)](https://www.printables.com/model/1662303-fishball-sdr-case/files) — community 3D-printed enclosure for the base board. Good starting point for the base shell of the two-part case; the panel enclosure can extend the same mounting pattern.
- [moritz-meier/fishball-sdr](https://github.com/moritz-meier/fishball-sdr) — alternative Z7020 firmware built from scratch, with GPIO bring-up code patterns worth referencing for the PL UART or GPIO instantiation in our gateware.
- [F5OEO/tezuka_fw discussion #194 — Fishball variants](https://github.com/F5OEO/tezuka_fw/discussions/194) — community context on the OpenSDRLab-specific JP5 addition. Brief mention only ("OpenSDRLab have added some gpio on it") but useful to confirm we're not missing a hidden ecosystem.

### What's *not* useful

- Digilent **PMOD** ecosystem boards — different connector, different pinout convention, different voltage rules. Not mechanically or electrically compatible with JP5. Don't try to use a PMOD adapter.
- ADALM-Pluto add-on shields — the Pluto has a different physical form factor and no equivalent header. Their schematics are not reusable.

---

## 13. Related Docs

- [JP5 pinout + electrical reference](../../../_shared/Hardware/OpenSDRLab-7020/07_EXPANSION_IO.md) — authoritative source for pin assignments and PTT circuit (host-side reference)
- `doc/P25_API.md` — HTTP API surface the panel must extend
- `doc/P25_ADDRESS_MAP.md` — register layout; the new I²S TX module will claim an address range here
