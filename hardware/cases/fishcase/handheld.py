"""The handheld's case: a tub that holds everything and a back cover.

Owns the handheld's layout. Seen from the front: the display at the top with four soft keys
under it and the speaker below; on the top face the antenna jack, the power button, the
status light and the two knobs; on the bottom face the board's own Ethernet, USB and SD
openings. Inside, from the bottom up: the board standing on its connector edge with its
parts toward the back, a bay for the power modules above its SMA jacks, the battery pack
lying across behind the display, and the knobs' bodies under the top face.

The frame, in millimetres: X across the front from the left, Y from the front face to the
back, Z from the bottom face up.
"""

from dataclasses import dataclass

from . import board, parts
from .shapes import (
    box, chamfer_where, cone_z, cut, cyl_y, cyl_z, d_hole, moved, place, rbox_x, rbox_y, slot,
    slot_grille, to_y, tube_z, union,
)


@dataclass
class Handheld:
    """Every size the handheld is built from."""

    w: float = 75.0
    h: float = 170.0
    d: float = 37.0
    wall: float = 2.4
    cover_t: float = 2.4
    radius: float = 7.0
    edge: float = 1.5  # the chamfer round the front and the back

    # The speaker lies between the front wall and the board: its depth sets where the
    # board stands. top_clear is what the board's parts, the header's plug and the fan
    # need behind it.
    speaker_d: float = 40.0
    speaker_depth: float = 7.0
    speaker_z: float = 42.0
    board_top_clear: float = 20.0

    # A 2.0 inch 320x240 SPI display (Waveshare's module: 58 x 35 mm, 40.8 x 30.6 active).
    lcd_w: float = 58.0
    lcd_h: float = 35.0
    lcd_t: float = 4.5  # assumed
    lcd_win_w: float = 41.8
    lcd_win_h: float = 31.6
    lcd_win_dx: float = 0.0  # assumed: the active area centred on the module
    lcd_z: float = 126.0

    # The soft keys stand under their labels: a quarter of the display's width apart.
    key_pitch: float = 10.2
    key_w: float = 7.4
    key_h: float = 5.4
    key_z: float = 96.0
    key_stack: float = 5.5  # the cap's flange and a 4.3 mm tactile switch
    key_post_dx: float = 27.0

    # The battery pack lies across the case on ribs: 1S2P 18650 with its protection board.
    battery_len: float = 69.0
    battery_w: float = 37.0
    battery_t: float = 19.5
    battery_z: float = 117.0

    # The top face.
    sma_x: float = 59.0
    sma_y: float = 7.5
    power_x: float = 17.0
    power_y: float = 8.0
    led_x: float = 32.0
    led_y: float = 8.0
    knob_y: float = 22.0
    knob_x: tuple = (27.0, 52.0)

    # The charge port: a USB-C receptacle looking out of the left side.
    charge_y: float = 22.0
    charge_z: float = 100.0

    fan_size: float = 30.0

    @property
    def xc(self):
        return self.w / 2

    @property
    def tub_d(self):
        return self.d - self.cover_t

    @property
    def floor_z(self):
        return board.EDGE_TO_OUTSIDE - board.END_GAP

    @property
    def y_pcb(self):
        return self.wall + self.speaker_depth + 1.0 + board.LOWER_CLEAR

    @property
    def bosses(self):
        """Where the cover's six screws go: (x, z)."""
        e = 6.2
        return [(x, z) for z in (e, 80.0, self.h - e) for x in (e, self.w - e)]

    @property
    def fan_xz(self):
        return (self.xc - board.FAN_CENTRE[0], board.EDGE_TO_OUTSIDE + board.FAN_CENTRE[1])


def _board_to_case(p):
    """The board stands on its connector edge, parts toward the back."""
    origin = (p.xc, p.y_pcb, board.EDGE_TO_OUTSIDE)
    return lambda s: place(s, (-1, 0, 0), (0, 0, 1), (0, 1, 0), origin)


def tub(p):
    """The front shell: every wall but the back."""
    wall = p.wall
    stand = _board_to_case(p)
    floor_w = -(p.y_pcb - wall)

    outer = chamfer_where(rbox_y(0, 0, p.w, p.h, p.radius, 0, p.tub_d), p.edge, lambda q: abs(q.y) < 1e-4)
    shell = outer.cut(rbox_y(wall, p.floor_z, p.w - wall, p.h - wall, p.radius - wall, wall, p.tub_d + 1))
    adds, cuts = [], []

    # The board: its rails, the posts of its clamps, the openings of its connectors.
    adds += [stand(s) for s in board.rails(floor_w)]
    posts, holes = board.clamp_posts(floor_w, parts.PILOT_D)
    adds += [stand(s) for s in posts]
    cuts += [stand(s) for s in holes]
    cuts += [stand(s) for s in board.connector_cutter()]

    # The cover's screws: bosses the whole depth of the tub, with heat-set inserts.
    for x, z in p.bosses:
        adds.append(cyl_y(x, z, 3.75, wall, p.tub_d))
        cuts.append(cyl_y(x, z, parts.INSERT_D / 2, p.tub_d - parts.INSERT_DEPTH, p.tub_d + 1))

    # The display: its window, fences at the module's corners, posts for four clamp tabs.
    wx = p.xc + p.lcd_win_dx
    cuts.append(rbox_y(wx - p.lcd_win_w / 2, p.lcd_z - p.lcd_win_h / 2,
                       wx + p.lcd_win_w / 2, p.lcd_z + p.lcd_win_h / 2, 1.0, -1, wall + 1))
    for sx in (-1, 1):
        for sz in (-1, 1):
            ex = p.xc + sx * (p.lcd_w / 2 + 0.3)
            ez = p.lcd_z + sz * (p.lcd_h / 2 + 0.3)
            x0, x1 = sorted((ex - sx * 6.0, ex + sx * 1.6))
            z0, z1 = sorted((ez, ez + sz * 1.6))
            adds.append(box(x0, wall, z0, x1, wall + 3.0, z1))
            x0, x1 = sorted((ex, ex + sx * 1.6))
            z0, z1 = sorted((ez - sz * 6.0, ez + sz * 1.6))
            adds.append(box(x0, wall, z0, x1, wall + 3.0, z1))
    for x, z in lcd_posts(p):
        post, hole = parts.post_y(x, z, wall, wall + p.lcd_t)
        adds.append(post)
        cuts.append(hole)

    # The soft keys: a hole for each cap, and two posts for the strip their switches are on.
    for i in range(4):
        x = p.xc + (i - 1.5) * p.key_pitch
        cuts.append(rbox_y(x - p.key_w / 2, p.key_z - p.key_h / 2,
                           x + p.key_w / 2, p.key_z + p.key_h / 2, 1.5, -1, wall + 1))
    for sx in (-1, 1):
        post, hole = parts.post_y(p.xc + sx * p.key_post_dx, p.key_z, wall, wall + p.key_stack, 6.5)
        adds.append(post)
        cuts.append(hole)

    # The speaker: slots through the front wall, and a ring that centres it.
    cuts += [to_y(s) for s in slot_grille(p.speaker_z, p.xc, p.speaker_d / 2 - 3.0, -1, wall + 1, along_y=True)]
    adds.append(to_y(tube_z(p.speaker_z, p.xc, p.speaker_d / 2 + 1.8, p.speaker_d / 2 + 0.3, wall, wall + 2.5)))

    # The battery pack: a rib under and over each end, and two more that keep it off the
    # display. The ribs stop short of the rim, where the cover's tongue goes.
    rib_y1 = p.tub_d - 2.5
    stop_y = p.tub_d - 0.6 - p.battery_t - 0.2
    for x0, x1 in ((wall, wall + 4.6), (p.w - wall - 4.6, p.w - wall)):
        adds.append(box(x0, wall, p.battery_z - 1.4, x1, rib_y1, p.battery_z))
        z = p.battery_z + p.battery_w + 2.0
        adds.append(box(x0, wall, z, x1, rib_y1, z + 1.4))
        for dz in (12.0, 27.0):
            adds.append(box(x0, wall, p.battery_z + dz, x1, stop_y, p.battery_z + dz + 1.4))

    # The top face: the antenna jack, the power button, the status light, the two knobs.
    top0, top1 = p.h - wall - 1, p.h + 1
    cuts.append(d_hole(p.sma_x, p.sma_y, 3.25, 2.65, top0, top1))
    cuts.append(cyl_z(p.power_x, p.power_y, 3.6, top0, top1))
    cuts.append(cyl_z(p.led_x, p.led_y, 1.55, top0, top1))
    for x in p.knob_x:
        cuts.append(cyl_z(x, p.knob_y, 3.6, top0, top1))

    # The charge port: an opening that fits the receptacle, relieved outside for the plug.
    cuts.append(rbox_x(p.charge_y - 4.7, p.charge_z - 1.85, p.charge_y + 4.7, p.charge_z + 1.85, 1.2, -1, wall + 1))
    cuts.append(rbox_x(p.charge_y - 6.6, p.charge_z - 3.8, p.charge_y + 6.6, p.charge_z + 3.8, 2.0, -1, wall - 1.0))

    return cut(union([shell] + adds), cuts)


def lcd_posts(p):
    """Where the display's clamp posts stand: (x, z)."""
    up = p.lcd_z + p.lcd_h / 2 + 0.3 + 3.8
    down = p.lcd_z - p.lcd_h / 2 - 0.3 - 3.8
    return [(p.xc - 9.0, up), (p.xc + 9.0, up), (p.xc - 16.0, down), (p.xc + 16.0, down)]


def cover(p):
    """The back: a plate with a tongue that drops into the tub, the fan's opening over the
    Zynq and a vent above it."""
    wall = p.wall
    plate = chamfer_where(rbox_y(0, 0, p.w, p.h, p.radius, p.tub_d, p.d), p.edge, lambda q: abs(q.y - p.d) < 1e-4)
    g = wall + 0.25
    tongue = rbox_y(g, p.floor_z + 0.25, p.w - g, p.h - g, p.radius - g, p.tub_d - 2.0, p.tub_d).cut(
        rbox_y(g + 1.4, p.floor_z + 1.65, p.w - g - 1.4, p.h - g - 1.4, p.radius - g - 1.4, p.tub_d - 3.0, p.tub_d + 1)
    )
    # The tongue gives way to the screw bosses, and to the battery pack, which is as wide
    # as the tub.
    gaps = [cyl_y(x, z, 4.4, p.tub_d - 3.0, p.tub_d + 1) for x, z in p.bosses]
    gaps.append(box(-1, p.tub_d - 3.0, p.battery_z - 2.0, p.w + 1, p.tub_d + 1, p.battery_z + p.battery_w + 4.0))
    tongue = cut(tongue, gaps)

    cuts = []
    for x, z in p.bosses:
        cuts.append(cyl_y(x, z, parts.CLEAR_D / 2, p.tub_d - 1, p.d + 1))
        cuts.append(to_y(cone_z(z, x, parts.CLEAR_D / 2, 3.3, p.d - 1.7, p.d + 0.01)))
    fx, fz = p.fan_xz
    cuts += [to_y(s) for s in slot_grille(fz, fx, p.fan_size / 2 - 2.0, p.tub_d - 1, p.d + 1, along_y=True)]
    for sx in (-1, 1):
        for sz in (-1, 1):
            cuts.append(cyl_y(fx + sx * 12.0, fz + sz * 12.0, 1.65, p.tub_d - 1, p.d + 1))
    for i in range(4):
        z = 66.0 + i * 4.2
        cuts.append(to_y(slot(z - 1.1, p.xc - 16.0, p.tub_d - 1, z + 1.1, p.xc + 16.0, p.d + 1)))
    return cut(union([plate, tongue]), cuts)


def ghosts(p):
    """Block models of what goes inside, where it goes: what the shells are checked against."""
    wall = p.wall
    stand = _board_to_case(p)
    g = {"board." + k: stand(v) for k, v in board.envelope(p.board_top_clear).items()}
    wx = p.xc
    g["lcd"] = box(wx - p.lcd_w / 2, wall, p.lcd_z - p.lcd_h / 2, wx + p.lcd_w / 2, wall + p.lcd_t, p.lcd_z + p.lcd_h / 2)
    g["pico"] = box(p.xc - 25.5, wall + p.lcd_t + 1.5, p.lcd_z - 10.5, p.xc + 25.5, wall + p.lcd_t + 5.1, p.lcd_z + 10.5)
    g["speaker"] = cyl_y(p.xc, p.speaker_z, p.speaker_d / 2, wall, wall + p.speaker_depth)
    g["key_strip"] = box(p.xc - 30.0, wall + p.key_stack, p.key_z - 5.0, p.xc + 30.0, wall + p.key_stack + 1.6, p.key_z + 5.0)
    back = p.tub_d - 0.6
    g["battery"] = box(p.xc - p.battery_len / 2, back - p.battery_t, p.battery_z + 1.0,
                       p.xc + p.battery_len / 2, back, p.battery_z + 1.0 + p.battery_w)
    top = p.h - wall
    for i, x in enumerate(p.knob_x):
        g[f"encoder{i + 1}"] = moved(parts.encoder_block(), x, p.knob_y, top)
    g["antenna_jack"] = moved(parts.jack_block(d=8.0, behind=22.0, front=10.0), p.sma_x, p.sma_y, top)
    g["power_button"] = moved(parts.jack_block(d=9.5, behind=15.0, front=6.0), p.power_x, p.power_y, top)
    # The fan hangs from the cover in the room kept clear behind the board.
    fx, fz = p.fan_xz
    g["board.fan"] = box(fx - p.fan_size / 2, p.tub_d - 8.0, fz - p.fan_size / 2, fx + p.fan_size / 2, p.tub_d, fz + p.fan_size / 2)
    return g


def small_parts(p):
    """The loose printed parts, each as it lies on the printer's bed: (shape, how many)."""
    return {
        "clamp_tab": (parts.clamp_tab(), 8),
        "key_cap": (parts.key_cap(p.key_w, p.key_h, p.wall), 4),
        "knob": (parts.knob(), 2),
    }


def print_pose(name, solid, p):
    """A part turned as it is printed: the tub on its front face, the cover on its back."""
    if name == "tub":
        return place(solid, (1, 0, 0), (0, 0, 1), (0, -1, 0), (0, p.h, 0))
    if name == "cover":
        return place(solid, (1, 0, 0), (0, 0, -1), (0, 1, 0), (0, 0, p.d))
    return solid


def build(p=None):
    p = p or Handheld()
    if p.y_pcb + board.PCB_T + p.board_top_clear > p.tub_d:
        raise ValueError("handheld: the board and what stands on it do not fit the depth")
    return {
        "params": p,
        "parts": {"tub": tub(p), "cover": cover(p)},
        "small": small_parts(p),
        "ghosts": ghosts(p),
        "pose": lambda name, solid: print_pose(name, solid, p),
    }
