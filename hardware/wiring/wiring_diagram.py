"""Draw the wiring diagram of the handheld and the desk unit: run with Python.

    python wiring_diagram.py

Writes wiring_diagram.svg beside this file. The diagram is one harness for both units: the
power chain from the charge port to the board's 5 V rail, the panel's microcontroller with
the display and the controls, the amplifier on the board's I2S pins, and what stays on the
board's own connectors. The pin tables in ../README.md say the same thing as text; change
both together.
"""

import os
from xml.sax.saxutils import escape

W, H = 1700, 1300

INK = "#1c2430"
MUTED = "#5d6a7a"
PAPER = "#ffffff"
BLOCK = "#f4f6f9"
BOARD = "#eaf3ec"
RAIL_5V = "#d23c2a"
RAIL_BAT = "#e08a1e"
RAIL_3V3 = "#c2255c"
UART = "#1f6fd1"
I2S = "#7b3fc4"
SPI = "#1d8a4e"
GPIO = "#4a5563"
AUDIO = "#0e8f9c"
COAX = "#8a6d1c"

out = []


def text(x, y, s, size=13, colour=INK, anchor="start", weight="normal"):
    out.append(
        f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" fill="{colour}" text-anchor="{anchor}" '
        f'font-weight="{weight}">{escape(s)}</text>'
    )


def pin_mark(x, y):
    out.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="3.4" fill="{PAPER}" stroke="{INK}" stroke-width="1.4"/>')


def block(x, y, w, h, title, sub=None, pins=None, fill=BLOCK, title_size=14):
    """A module as a box with its pins on its edges: returns each pin's point on the edge."""
    pins = pins or {}
    out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="8" fill="{fill}" stroke="{INK}" stroke-width="1.6"/>')
    drop = 20 if "top" in pins else 0
    text(x + w / 2, y + 20 + drop, title, title_size, anchor="middle", weight="600")
    head = 28 + drop
    for i, line in enumerate(sub if isinstance(sub, list) else [sub] if sub else []):
        text(x + w / 2, y + 37 + drop + i * 15, line, 11.5, MUTED, anchor="middle")
        head = 44 + drop + i * 15
    foot = 20 if "bottom" in pins else 0
    at = {}
    for side, names in pins.items():
        n = len(names)
        for i, name in enumerate(names):
            if side in ("left", "right"):
                py = y + head + (h - head - foot) * (i + 0.5) / n
                px = x if side == "left" else x + w
                text(px + 9 if side == "left" else px - 9, py + 4, name, 12, anchor="start" if side == "left" else "end")
            else:
                px = x + w * (i + 0.5) / n
                py = y if side == "top" else y + h
                text(px, py + 17 if side == "top" else py - 8, name, 12, anchor="middle")
            pin_mark(px, py)
            at[name] = (px, py)
    return at


def wire(points, colour, width=2.2, dash=None):
    d = " ".join(f"{x:.1f},{y:.1f}" for x, y in points)
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    out.append(f'<polyline points="{d}" fill="none" stroke="{colour}" stroke-width="{width}" stroke-linejoin="round"{extra}/>')


def via(p, q, lane_x, colour, width=2.2):
    """A wire between two pins that runs level from each to a lane and turns there."""
    wire([p, (lane_x, p[1]), (lane_x, q[1]), q], colour, width)


def dot(p, colour):
    out.append(f'<circle cx="{p[0]:.1f}" cy="{p[1]:.1f}" r="4.2" fill="{colour}"/>')


def ground(p, side="right", length=22):
    """A ground symbol on a short stub from a pin."""
    x, y = p
    gx = x + {"right": length, "left": -length}.get(side, 0)
    gy = y + {"down": length}.get(side, 0)
    out.append(f'<polyline points="{x:.1f},{y:.1f} {gx:.1f},{gy:.1f} {gx:.1f},{gy + 8:.1f}" fill="none" stroke="{INK}" stroke-width="1.8"/>')
    for i, half in enumerate((8, 5, 2)):
        yy = gy + 8 + i * 4
        out.append(f'<line x1="{gx - half:.1f}" y1="{yy:.1f}" x2="{gx + half:.1f}" y2="{yy:.1f}" stroke="{INK}" stroke-width="1.8"/>')


def part(x, y, w, label):
    """A small part in a wire (a diode, a divider) as a lozenge on it."""
    out.append(f'<rect x="{x - w / 2:.1f}" y="{y - 12:.1f}" width="{w}" height="24" rx="12" fill="{PAPER}" stroke="{INK}" stroke-width="1.4"/>')
    text(x, y + 4, label, 11.5, anchor="middle", weight="600")


def tag(x, y, s, colour, anchor="start"):
    text(x, y, s, 11.5, colour, anchor=anchor, weight="600")


def draw():
    out.append(f'<rect x="0" y="0" width="{W}" height="{H}" fill="{PAPER}"/>')
    text(40, 36, "Fishball scanner: one harness for the handheld and the desk unit", 20, weight="700")
    legend = [("5 V rail", RAIL_5V), ("battery side", RAIL_BAT), ("3.3 V", RAIL_3V3), ("serial link", UART),
              ("I2S audio", I2S), ("display SPI", SPI), ("plain signals", GPIO), ("speaker", AUDIO), ("coax", COAX)]
    x = 40
    for name, colour in legend:
        out.append(f'<line x1="{x}" y1="54" x2="{x + 26}" y2="54" stroke="{colour}" stroke-width="4"/>')
        text(x + 32, 58, name, 12, MUTED)
        x += 46 + len(name) * 7.2
    text(x + 10, 58, "Every ground is common.", 12, MUTED)

    # ---- the power chain, left to right along the top ----
    usbc = block(40, 100, 160, 110, "Charge port", "USB-C, 5 V in", {"right": ["VBUS", "GND"]})
    chg = block(280, 90, 230, 180, "Charger with power path", ["bq24074 (Adafruit 4755)", "1.5 A, load output 4.4 V max"],
                {"left": ["VIN", "GND"], "right": ["LOAD", "GND "], "bottom": ["BAT+", "BAT-", "CHG"]})
    batt = block(40, 290, 170, 100, "Battery pack", ["1S2P 18650, 3.7 V", "with protection board"], {"right": ["+", "-"]})
    sw = block(600, 90, 200, 180, "Power switch", ["Pololu 2808 (latching,", "reverse protected)"],
               {"left": ["VIN", "GND"], "right": ["VOUT"], "bottom": ["A", "OFF"]})
    boost = block(880, 90, 200, 180, "5 V booster", ["Pololu U3V70F5", "2.9 to 5 V in, 5 V out"],
                  {"left": ["VIN", "GND"], "right": ["VOUT"]})
    diode = block(1150, 150, 150, 62, "Ideal diode", "2 A or more", {"left": ["IN"], "right": ["OUT"]})

    via(usbc["VBUS"], chg["VIN"], 240, RAIL_5V, 3)
    ground(usbc["GND"])
    ground(chg["GND"], "left")
    via(chg["LOAD"], sw["VIN"], 555, RAIL_BAT, 3)
    tag(518, chg["LOAD"][1] - 9, "3.0 to 4.4 V", RAIL_BAT)
    ground(chg["GND "])
    ground(sw["GND"], "left")
    via(sw["VOUT"], boost["VIN"], 840, RAIL_BAT, 3)
    ground(boost["GND"], "left")
    via(boost["VOUT"], diode["IN"], 1115, RAIL_5V, 3)

    bp, bm = chg["BAT+"], chg["BAT-"]
    wire([batt["+"], (bp[0], batt["+"][1]), bp], RAIL_BAT, 3)
    wire([batt["-"], (bm[0], batt["-"][1]), bm], INK, 2.2)

    # ---- the board ----
    bx, by, bw, bh = 1230, 300, 430, 640
    out.append(f'<rect x="{bx}" y="{by}" width="{bw}" height="{bh}" rx="10" fill="{BOARD}" stroke="{INK}" stroke-width="2"/>')
    text(bx + bw / 2, by + 26, "Fishball Z7020 board (PlutoSky)", 16, anchor="middle", weight="700")
    text(bx + bw / 2, by + 44, "JP5, the 2x10 expansion header: the pins the harness uses", 11.5, MUTED, anchor="middle")
    jp5 = [
        ("5", "VCC5V", "the board's 5 V rail", RAIL_5V),
        ("18, 20", "GND", "", INK),
        ("7", "3V3 IO1 (V10)", "serial, board to panel", UART),
        ("9", "3V3 IO2 (U9)", "serial, panel to board", UART),
        ("11", "3V3 IO3 (U10)", "ALIVE: high while the scanner runs", GPIO),
        ("13", "3V3 IO4 (T9)", "spare", MUTED),
        ("2", "1V8 IO1_P (G19)", "I2S bit clock", I2S),
        ("4", "1V8 IO1_N (G20)", "I2S word clock", I2S),
        ("6", "1V8 IO3_P (J18)", "I2S data", I2S),
        ("8", "1V8 IO3_N (H18)", "amplifier enable", I2S),
    ]
    pin = {}
    y0 = by + 78
    for i, (num, name, use, colour) in enumerate(jp5):
        y = y0 + i * 34
        pin_mark(bx, y)
        text(bx + 12, y + 4, f"JP5-{num}", 12, weight="600")
        text(bx + 92, y + 4, name, 12)
        if use:
            text(bx + 212, y + 4, use, 11.5, colour)
        pin[num] = (bx, y)
    text(bx + 12, y0 + 10 * 34 + 2, "Not used: 1 (1.8 V), 3 (3.3 V), 10, 12, 14, 16 (1.8 V I/O), 15, 17, 19.", 11, MUTED)

    cy = by + 468
    text(bx + 14, cy, "The board's own connectors, each at an opening in the case wall", 12.5, weight="600")
    rows = [
        ("USB-C \"USB\"", "the unit as a USB device", MUTED),
        ("USB-C \"DEBUG\"", "console and JTAG; its VBUS is the 5 V rail itself (note 2)", RAIL_5V),
        ("RJ45", "Ethernet", MUTED),
        ("microSD", "the card", MUTED),
    ]
    for i, (name, use, colour) in enumerate(rows):
        y = cy + 24 + i * 21
        text(bx + 14, y, name, 12, weight="600")
        text(bx + 122, y, use, 11.5, colour)
    fan_pin, sma_pin = (bx + 95, by + bh), (bx + 295, by + bh)
    for p, name in ((fan_pin, "FAN1"), (sma_pin, "SMA RX1")):
        pin_mark(*p)
        text(p[0], p[1] - 9, name, 12, anchor="middle", weight="600")
    fan = block(bx + 20, 990, 150, 66, "Fan 30 mm", "5 V, always on", {"top": ["+"]})
    wire([fan["+"], fan_pin], RAIL_5V, 2.2)
    ant = block(bx + 210, 990, 170, 66, "Antenna jack", "SMA bulkhead", {"top": ["RF"]})
    wire([ant["RF"], sma_pin], COAX, 3, "7 4")
    tag(sma_pin[0] + 10, 968, "coax pigtail", COAX)

    # ---- the 5 V rail: from the diode down the left of the board ----
    rail_x = 1180
    wire([diode["OUT"], (1340, diode["OUT"][1]), (1340, 262), (rail_x, 262), (rail_x, 1130)], RAIL_5V, 4)
    tag(1350, diode["OUT"][1] - 9, "5V_SYS", RAIL_5V)
    wire([(rail_x, pin["5"][1]), pin["5"]], RAIL_5V, 4)
    dot((rail_x, pin["5"][1]), RAIL_5V)
    ground(pin["18, 20"], "left", 24)

    # ---- the panel's microcontroller ----
    left = ["GP4", "GP5", "GP6", "GP7", "GP8", "GP9", "GP10", "GP11", "GP12", "GP13", "GP27", "GP28", "GP22"]
    right = ["VSYS", "GP0 TX", "GP1 RX", "GP2", "3V3 OUT", "GP16", "GP17", "GP18", "GP19", "GP20", "GP21", "GND"]
    pico = block(330, 470, 250, 620, "Raspberry Pi Pico", ["the panel: display, knobs, keys,", "light, power control"],
                 {"left": left, "right": right, "top": ["GP26", "GP3", "GP14", "GP15"]}, title_size=15)
    ground(pico["GND"])

    # battery sense, charge status and power control, from the top of the Pico
    gp26, gp3, gp14, gp15 = pico["GP26"], pico["GP3"], pico["GP14"], pico["GP15"]
    node = (bp[0], 300)
    dot(node, RAIL_BAT)
    wire([node, (gp26[0], 300), gp26], RAIL_BAT, 2)
    part(gp26[0], 425, 100, "220k : 220k")
    tag(gp26[0] - 56, 429, "battery volts, halved", RAIL_BAT, "end")
    cg = chg["CHG"]
    wire([cg, (cg[0], 320), (gp3[0], 320), gp3], GPIO, 2)
    tag(gp3[0] + 7, 452, "charging", GPIO)
    a, off = sw["A"], sw["OFF"]
    btn = block(625, 345, 135, 62, "Power button", "momentary", {"left": ["1"], "right": ["2"]})
    wire([a, (a[0], 300), (585, 300), (585, btn["1"][1]), btn["1"]], GPIO, 2)
    ground(btn["2"])
    dot((585, 330), GPIO)
    wire([(585, 330), (gp14[0], 330), gp14], GPIO, 2)
    part(gp14[0], 395, 76, "BAT54")
    tag(gp14[0] - 44, 380, "anode down", MUTED, "end")
    wire([off, (off[0], 290), (830, 290), (830, 440), (gp15[0], 440), gp15], GPIO, 2)
    tag(838, 432, "high: power off", GPIO)

    # the Pico's own supply from the rail
    vsys = pico["VSYS"]
    wire([vsys, (rail_x, vsys[1])], RAIL_5V, 2.6)
    dot((rail_x, vsys[1]), RAIL_5V)
    part(700, vsys[1], 86, "1N5817")
    tag(752, vsys[1] - 8, "anode at the rail", MUTED)

    # the serial link and ALIVE
    for name, num, lane, colour, label in (("GP0 TX", "9", 1040, UART, "panel to board"),
                                           ("GP1 RX", "7", 1060, UART, "board to panel"),
                                           ("GP2", "11", 1080, GPIO, "ALIVE")):
        via(pico[name], pin[num], lane, colour)
        tag(600, pico[name][1] - 7, label, colour)

    # ---- the display ----
    lcd = block(770, 715, 230, 300, "Display 320x240 SPI", ["2.0 in, ST7789V", "the same in both units"],
                {"left": ["VCC", "BL", "CS", "CLK", "DIN", "DC", "RST", "GND"]})
    ground(lcd["GND"], "left")
    runs = [("3V3 OUT", "VCC", RAIL_3V3, 715), ("GP16", "BL", GPIO, 700), ("GP17", "CS", SPI, 685), ("GP18", "CLK", SPI, 655),
            ("GP19", "DIN", SPI, 655), ("GP20", "DC", SPI, 670), ("GP21", "RST", SPI, 685)]
    for a_, b_, colour, lane in runs:
        via(pico[a_], lcd[b_], lane, colour)
    tag(600, pico["3V3 OUT"][1] - 7, "3.3 V", RAIL_3V3)

    # ---- the controls ----
    enc1 = block(40, 470, 180, 110, "SELECT knob", "EC11 encoder with switch", {"right": ["A", "B", "SW"]})
    enc2 = block(40, 600, 180, 110, "VOLUME knob", "EC11 encoder with switch", {"right": ["A", "B", "SW"]})
    keys = block(40, 730, 180, 140, "Soft keys", "4 tactile switches", {"right": ["F1", "F2", "F3", "F4"]})
    led = block(40, 890, 180, 120, "Status light", ["RGB, common cathode,", "330 ohm in each leg"], {"right": ["R", "G", "B"]})
    text(40, 1030, "Each knob's common pin and switch return,", 11, MUTED)
    text(40, 1045, "each key's other leg and the light's cathode", 11, MUTED)
    text(40, 1060, "go to ground. The pull-ups are the Pico's own.", 11, MUTED)
    groups = [(enc1, ["A", "B", "SW"], ["GP4", "GP5", "GP6"]), (enc2, ["A", "B", "SW"], ["GP7", "GP8", "GP9"]),
              (keys, ["F1", "F2", "F3", "F4"], ["GP10", "GP11", "GP12", "GP13"]), (led, ["R", "G", "B"], ["GP27", "GP28", "GP22"])]
    for blk, names, gps in groups:
        for k, (n, gname) in enumerate(zip(names, gps)):
            via(blk[n], pico[gname], 300 - k * 14, GPIO, 2)

    # ---- the audio ----
    amp = block(770, 1085, 200, 190, "I2S amplifier", ["MAX98357A (Adafruit 3006)", "3 W class D, mono"],
                {"right": ["VIN", "BCLK", "LRC", "DIN", "SD", "GND"], "left": ["SPK+", "SPK-"]})
    spk = block(500, 1160, 170, 86, "Speaker", "4 or 8 ohm", {"right": ["+", "-"]})
    via(amp["SPK+"], spk["+"], 715, AUDIO, 2.4)
    via(amp["SPK-"], spk["-"], 735, AUDIO, 2.4)
    ground(amp["GND"])
    wire([amp["VIN"], (rail_x, amp["VIN"][1]), (rail_x, 1130)], RAIL_5V, 2.6)
    for i, (name, num) in enumerate((("BCLK", "2"), ("LRC", "4"), ("DIN", "6"), ("SD", "8"))):
        via(pin[num], amp[name], 1150 - i * 14, I2S)
    tag(1000, amp["SD"][1] + 22, "1.8 V logic, driven by the FPGA", I2S)

    # ---- notes ----
    notes = [
        ("1", "Power: the button turns the switch on. The Pico turns it off (OFF high)"),
        ("", "once the scanner has stopped. The charger works on or off."),
        ("2", "DEBUG's VBUS is wired straight to the board's 5 V rail. On battery,"),
        ("", "plug DEBUG in only through a cable without VBUS, or with the unit"),
        ("", "switched off: the PC then powers the board and the panel."),
        ("3", "JP5's pins are FPGA pins: the serial link and ALIVE need the bitstream"),
        ("", "to bring PS UART0 and a GPIO out to them, and the audio needs the"),
        ("", "radio core's I2S block (the roadmap's step 9)."),
    ]
    for i, (n, line) in enumerate(notes):
        y = 1100 + i * 17
        if n:
            text(bx, y, n, 12, weight="700")
        text(bx + 16, y, line, 11.5, INK)


def main():
    draw()
    svg = (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
        f'font-family="Segoe UI, Helvetica, Arial, sans-serif">\n' + "\n".join(out) + "\n</svg>\n"
    )
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wiring_diagram.svg")
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(svg)
    print("wrote", path)


if __name__ == "__main__":
    main()
