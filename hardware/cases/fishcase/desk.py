"""The desk unit's case: a radio in the shape of a mobile transceiver, in a tilt bracket.

Owns the desk unit's layout. It is built from the handheld's own parts: the same display,
knobs, keys, speaker and battery pack. The front panel has the display in the middle with
the four soft keys in a row under it, the volume knob and the power button on the left, and
the large dial with the status light on the right. The speaker is in the top cover. The
board lies flat in the right rear with its Ethernet, USB and SD connectors looking out of
the rear wall, which also has the fan, the charge port and two antenna jacks. Two screws in
each side hold the radio in a bracket that stands on the desk or is screwed to it, level or
tipped back.

Four printed shells: the tray (floor, rear and side walls), the front panel, the top cover
and the bracket. The frame, in millimetres: X across from the left, Y from the panel's face
to the rear, Z up from the tray's underside.
"""

import math
from dataclasses import dataclass

from . import board, parts
from .shapes import (
    box, chamfer_where, cone_z, cut, cyl_x, cyl_y, cyl_z, d_hole, place, prism_x, rbox, rbox_y,
    slot, slot_grille, to_y, tube_z, union,
)


@dataclass
class Desk:
    """Every size the desk unit is built from."""

    w: float = 160.0
    h: float = 60.0
    depth: float = 140.0
    side_t: float = 3.2
    wall: float = 2.4
    floor: float = 2.4
    cover_t: float = 2.6
    panel_t: float = 3.0
    radius: float = 6.0

    # The board: its centre line, and how far it stands over the floor (the SD card goes in
    # under it).
    board_x: float = 121.0
    board_lift: float = 4.0
    board_top_clear: float = 20.0

    # The handheld's display: 2.0 inch 320x240 SPI (Waveshare's module, 58 x 35 mm).
    lcd_w: float = 58.0
    lcd_h: float = 35.0
    lcd_t: float = 4.5  # assumed
    lcd_win_w: float = 41.8
    lcd_win_h: float = 31.6
    lcd_win_dx: float = 0.0  # assumed: the active area centred on the module
    lcd_x: float = 73.0
    lcd_z: float = 35.5

    # The soft keys stand under their labels: a quarter of the display's width apart.
    key_pitch: float = 10.2
    key_w: float = 7.4
    key_h: float = 5.4
    key_z: float = 9.5
    key_stack: float = 5.5
    key_post_dx: float = 27.0

    # The panel's two ends: (x, z).
    volume_at: tuple = (26.0, 40.0)
    power_at: tuple = (26.0, 14.0)
    dial_at: tuple = (127.0, 31.0)
    led_at: tuple = (112.0, 52.0)
    dial_d: float = 34.0

    # The handheld's speaker in the top cover, held by tabs over its back: (x, y).
    speaker_d: float = 40.0
    speaker_depth: float = 7.0
    speaker_at: tuple = (45.0, 75.0)

    # On the floor: the battery pack (1S2P 18650, lying flat with its cells front to back)
    # and the charge port's board against the rear wall.
    battery_len: float = 69.0
    battery_w: float = 37.0
    battery_t: float = 19.5
    battery_at: tuple = (44.0, 66.0)
    charge_x: float = 24.0
    charge_board_w: float = 13.0  # assumed
    charge_board_l: float = 22.0  # assumed
    charge_board_t: float = 1.6
    charge_seat: float = 3.0

    sma_x: tuple = (54.0, 70.0)
    sma_z: float = 38.0
    fan_size: float = 30.0

    # The bracket: its arms' thickness, the play beside the radio, how far the radio hangs
    # over its base, the height of the side screws, and the tilts it offers.
    arm_t: float = 5.0
    arm_gap: float = 0.5
    hang: float = 8.0
    side_screw_z: float = 22.0
    tilts: tuple = (0.0, 6.0, 12.0)

    @property
    def y_back(self):
        return self.panel_t

    @property
    def top(self):
        """Where the walls end and the cover begins."""
        return self.h - self.cover_t

    @property
    def z_pcb(self):
        return self.floor + self.board_lift

    @property
    def columns(self):
        """The two columns behind the panel's ends: x of each one's middle."""
        return (self.side_t + 4.5, self.w - self.side_t - 4.5)

    @property
    def panel_screws(self):
        """Where the panel's four screws go: (x, z)."""
        return [(x, z) for x in self.columns for z in (14.0, self.top - 14.0)]

    @property
    def cover_screws(self):
        """Where the cover's four screws go: (x, y). The first two are in the columns."""
        front = [(x, self.y_back + 10.0) for x in self.columns]
        return front + [(7.0, self.depth - 7.0), (self.w - 7.0, self.depth - 7.0)]

    @property
    def side_screws(self):
        """Where the bracket's screws go into each side: y of the front one and of the rear
        one, which is the one the radio turns about."""
        return (self.y_back + 10.0, self.depth - 7.0)

    @property
    def fan_xz(self):
        """The fan's middle on the rear wall: over the board's connectors."""
        return (self.board_x, self.z_pcb + 15.65 + 2.0 + self.fan_size / 2)

    @property
    def charge_z(self):
        """The middle of the charge port's receptacle."""
        return self.floor + self.charge_seat + self.charge_board_t + 1.63


def _board_to_case(p):
    """The board lies flat, its connectors at the rear wall, its parts up."""
    origin = (p.board_x, p.depth - board.EDGE_TO_OUTSIDE, p.z_pcb)
    return lambda s: place(s, (-1, 0, 0), (0, -1, 0), (0, 0, 1), origin)


def _behind_panel(x, z, p):
    """Places a part built for a panel at Z = 0 (its body under, its shaft over) on the
    front panel: the body behind it, the shaft out of its face."""
    return lambda s: place(s, (1, 0, 0), (0, 0, 1), (0, -1, 0), (x, p.y_back, z))


def _outline(p, inset, y0, y1, z0, z1, r):
    """A slab of the body's outline between two depths: square at the front, rounded at
    the rear."""
    slab = rbox(inset, y0 - 30.0, z0, p.w - inset, y1, z1, r)
    return cut(slab, [box(-1, y0 - 31.0, z0 - 1, p.w + 1, y0, z1 + 1)])


def tray(p):
    """The floor, the rear wall and the side walls, and everything that stands on the floor."""
    lay = _board_to_case(p)
    y_in = p.depth - p.wall

    shell = _outline(p, 0, p.y_back, p.depth, 0, p.top, p.radius).cut(
        _outline(p, p.side_t, p.y_back, y_in, p.floor, p.h, p.radius - p.wall)
    )
    adds, cuts = [], []

    # A column behind each end of the panel takes the panel's screws from the front and
    # the cover's from above; a post in each rear corner takes the cover's other two.
    for x in p.columns:
        adds.append(box(x - 4.5, p.y_back, p.floor, x + 4.5, p.y_back + 14.0, p.top))
    for x, z in p.panel_screws:
        cuts.append(cyl_y(x, z, parts.INSERT_D / 2, p.y_back - 1, p.y_back + parts.INSERT_DEPTH))
    for x, y in p.cover_screws[2:]:
        adds.append(cyl_z(x, y, 4.5, p.floor, p.top))
    for x, y in p.cover_screws:
        cuts.append(cyl_z(x, y, parts.INSERT_D / 2, p.top - parts.INSERT_DEPTH, p.top + 1))

    # The bracket's screws go into the same columns and posts, through the side walls.
    for y in p.side_screws:
        cuts.append(cyl_x(y, p.side_screw_z, parts.INSERT_D / 2, -1, parts.INSERT_DEPTH))
        cuts.append(cyl_x(y, p.side_screw_z, parts.INSERT_D / 2, p.w - parts.INSERT_DEPTH, p.w + 1))

    # The board: its rails, the posts of its clamps, the openings of its connectors.
    adds += [lay(s) for s in board.rails(-p.board_lift)]
    posts, holes = board.clamp_posts(-p.board_lift, parts.PILOT_D)
    adds += [lay(s) for s in posts]
    cuts += [lay(s) for s in holes]
    cuts += [lay(s) for s in board.connector_cutter()]

    # The battery pack: a fence at each corner.
    bx, by = p.battery_at
    for sx, ex in ((1, bx - 0.4), (-1, bx + p.battery_w + 0.4)):
        for sy, ey in ((1, by - 0.4), (-1, by + p.battery_len + 0.4)):
            x0, x1 = sorted((ex - sx * 1.6, ex + sx * 10.0))
            y0, y1 = sorted((ey - sy * 1.6, ey))
            adds.append(box(x0, y0, p.floor, x1, y1, p.floor + 8.0))
            x0, x1 = sorted((ex - sx * 1.6, ex))
            y0, y1 = sorted((ey - sy * 1.6, ey + sy * 10.0))
            adds.append(box(x0, y0, p.floor, x1, y1, p.floor + 8.0))

    # The charge port: its board lies on two rails against the rear wall, a stop behind it
    # and a clamp tab on a post beside it.
    cx, half = p.charge_x, p.charge_board_w / 2
    seat = p.floor + p.charge_seat
    for sx in (-1, 1):
        x0, x1 = sorted((cx + sx * (half - 1.4), cx + sx * (half + 0.2)))
        adds.append(box(x0, y_in - p.charge_board_l, p.floor, x1, y_in, seat))
        x0, x1 = sorted((cx + sx * (half + 0.2), cx + sx * (half + 1.8)))
        adds.append(box(x0, y_in - p.charge_board_l, p.floor, x1, y_in, seat + p.charge_board_t + 0.8))
    y_stop = y_in - p.charge_board_l - 0.3
    adds.append(box(cx - half, y_stop - 1.6, p.floor, cx + half, y_stop, seat + p.charge_board_t + 0.8))
    post_x, post_y = cx + half + 1.8 + 3.2, y_in - p.charge_board_l / 2
    post_top = seat + p.charge_board_t
    adds.append(cyl_z(post_x, post_y, parts.POST_D / 2, p.floor, post_top))
    cuts.append(cyl_z(post_x, post_y, parts.PILOT_D / 2, post_top - 6.0, post_top + 1))
    cuts.append(rbox_y(cx - 4.7, p.charge_z - 1.85, cx + 4.7, p.charge_z + 1.85, 1.2, y_in - 1, p.depth + 1))
    cuts.append(rbox_y(cx - 6.6, p.charge_z - 3.8, cx + 6.6, p.charge_z + 3.8, 2.0, y_in + 1.0, p.depth + 1))

    # The rear wall: two antenna jacks, and the fan over the board's connectors, blowing
    # along the board.
    for x in p.sma_x:
        cuts.append(to_y(d_hole(p.sma_z, x, 3.25, 2.65, y_in - 1, p.depth + 1)))
    fx, fz = p.fan_xz
    cuts += [to_y(s) for s in slot_grille(fz, fx, p.fan_size / 2 - 2.0, y_in - 1, p.depth + 1, along_y=True)]
    for sx in (-1, 1):
        for sz in (-1, 1):
            cuts.append(cyl_y(fx + sx * 12.0, fz + sz * 12.0, 1.65, y_in - 1, p.depth + 1))

    # Under the floor: recesses for four stick-on feet, for use without the bracket.
    for x in (16.0, p.w - 16.0):
        for y in (22.0, p.depth - 14.0):
            cuts.append(cyl_z(x, y, 5.5, -1, 1.0))

    return cut(union([shell] + adds), cuts)


def lcd_posts(p):
    """Where the display's clamp posts stand on the panel: (x, z)."""
    dx = p.lcd_w / 2 + 0.3 + 3.8
    return [(p.lcd_x + sx * dx, p.lcd_z + sz * 10.0) for sx in (-1, 1) for sz in (-1, 1)]


def panel(p):
    """The front panel: a plate the size of the radio's front, with what holds the display
    and the keys on its back."""
    y1 = p.y_back
    plate = chamfer_where(box(0, 0, 0, p.w, y1, p.top), 1.0, lambda q: abs(q.y) < 1e-4)
    adds, cuts = [], []

    # The display: its window, fences at the module's corners, posts for four clamp tabs.
    wx = p.lcd_x + p.lcd_win_dx
    cuts.append(rbox_y(wx - p.lcd_win_w / 2, p.lcd_z - p.lcd_win_h / 2,
                       wx + p.lcd_win_w / 2, p.lcd_z + p.lcd_win_h / 2, 1.0, -1, y1 + 1))
    for sx in (-1, 1):
        for sz in (-1, 1):
            ex = p.lcd_x + sx * (p.lcd_w / 2 + 0.3)
            ez = p.lcd_z + sz * (p.lcd_h / 2 + 0.3)
            x0, x1 = sorted((ex - sx * 6.0, ex + sx * 1.6))
            z0, z1 = sorted((ez, ez + sz * 1.6))
            adds.append(box(x0, y1, z0, x1, y1 + 3.0, z1))
            x0, x1 = sorted((ex, ex + sx * 1.6))
            z0, z1 = sorted((ez - sz * 6.0, ez + sz * 1.6))
            adds.append(box(x0, y1, z0, x1, y1 + 3.0, z1))
    for x, z in lcd_posts(p):
        post, hole = parts.post_y(x, z, y1, y1 + p.lcd_t)
        adds.append(post)
        cuts.append(hole)

    # The soft keys: a hole for each cap, and two posts for the strip their switches are on.
    for i in range(4):
        x = p.lcd_x + (i - 1.5) * p.key_pitch
        cuts.append(rbox_y(x - p.key_w / 2, p.key_z - p.key_h / 2,
                           x + p.key_w / 2, p.key_z + p.key_h / 2, 1.5, -1, y1 + 1))
    for sx in (-1, 1):
        post, hole = parts.post_y(p.lcd_x + sx * p.key_post_dx, p.key_z, y1, y1 + p.key_stack, 6.5)
        adds.append(post)
        cuts.append(hole)

    # The volume knob and the power button on the left, the dial and the status light on
    # the right.
    for x, z in (p.volume_at, p.power_at, p.dial_at):
        cuts.append(cyl_y(x, z, 3.6, -1, y1 + 1))
    cuts.append(cyl_y(p.led_at[0], p.led_at[1], 1.55, -1, y1 + 1))

    # Its screws, sunk into the face.
    for x, z in p.panel_screws:
        cuts.append(cyl_y(x, z, parts.CLEAR_D / 2, -1, y1 + 1))
        cuts.append(to_y(cone_z(z, x, 3.3, parts.CLEAR_D / 2, -0.01, 1.7)))

    return cut(union([plate] + adds), cuts)


def speaker_posts(p):
    """Where the speaker's clamp posts stand under the cover: (x, y)."""
    x, y = p.speaker_at
    r = p.speaker_d / 2 + 0.3 + 1.6 + 3.6
    return [(x + r * math.cos(math.radians(a)), y + r * math.sin(math.radians(a))) for a in (90, 210, 330)]


def cover(p):
    """The top: a plate over the walls and the panel, with the speaker's slots and what
    holds the speaker under it, a lip that drops inside the walls, and vents over the board."""
    y_in = p.depth - p.wall
    plate = chamfer_where(_outline(p, 0, 0.0, p.depth, p.top, p.h, p.radius), 1.0, lambda q: abs(q.z - p.h) < 1e-4)

    g = 0.3
    lip = rbox(p.side_t + g, p.y_back + g, p.top - 2.5, p.w - p.side_t - g, y_in - g, p.top, p.radius - p.wall).cut(
        rbox(p.side_t + g + 1.4, p.y_back + g + 1.4, p.top - 3.5, p.w - p.side_t - g - 1.4, y_in - g - 1.4, p.top + 1, 1.5)
    )
    gaps = [box(x - 4.9, p.y_back - 1, p.top - 3.5, x + 4.9, p.y_back + 14.4, p.top + 1) for x in p.columns]
    gaps += [cyl_z(x, y, 5.0, p.top - 3.5, p.top + 1) for x, y in p.cover_screws[2:]]
    lip = cut(lip, gaps)

    adds, cuts = [], []

    # The speaker: slots through the cover, a ring that centres it, and posts as tall as
    # it is deep, so that three tabs lie over its back.
    sx, sy = p.speaker_at
    cuts += slot_grille(sx, sy, p.speaker_d / 2 - 3.0, p.top - 1, p.h + 1)
    adds.append(tube_z(sx, sy, p.speaker_d / 2 + 1.8, p.speaker_d / 2 + 0.3, p.top - 2.5, p.top))
    for x, y in speaker_posts(p):
        z0 = p.top - p.speaker_depth
        adds.append(cyl_z(x, y, parts.POST_D / 2, z0, p.top))
        cuts.append(cyl_z(x, y, parts.PILOT_D / 2, z0 - 1, z0 + 7.0))

    # Vents over the Zynq.
    vy = p.depth - board.EDGE_TO_OUTSIDE - board.FAN_CENTRE[1]
    for i in range(5):
        y = vy + (i - 2) * 5.0
        cuts.append(slot(p.board_x - 20.0, y - 1.1, p.top - 1, p.board_x + 20.0, y + 1.1, p.h + 1))

    # Its screws, sunk into the top.
    for x, y in p.cover_screws:
        cuts.append(cyl_z(x, y, parts.CLEAR_D / 2, p.top - 1, p.h + 1))
        cuts.append(cone_z(x, y, parts.CLEAR_D / 2, 3.3, p.h - 1.7, p.h + 0.01))

    return cut(union([plate, lip] + adds), cuts)


def tilt_holes(p):
    """Where the bracket's arms are drilled: the rear hole the radio turns about, and a
    front hole for each tilt: (y, z)."""
    y_front, y_rear = p.side_screws
    reach = y_rear - y_front
    holes = [(y_rear, p.side_screw_z)]
    for deg in p.tilts:
        a = math.radians(deg)
        holes.append((y_rear - reach * math.cos(a), p.side_screw_z + reach * math.sin(a)))
    return holes


def bracket(p):
    """The bracket the radio hangs in: a base on the desk and an arm up each side, drawn
    where it sits with the radio level."""
    base_top = -p.hang
    base = box(-p.arm_gap - p.arm_t, 2.0, base_top - 4.0, p.w + p.arm_gap + p.arm_t, p.depth - 2.0, base_top)
    holes = tilt_holes(p)
    rear_top = p.side_screw_z + 9.0
    front_top = max(z for _, z in holes) + 9.0
    y_a, y_b = holes[-1][0] + 12.0, p.depth - 2.0
    outline = [(2.0, base_top - 4.0), (y_b, base_top - 4.0), (y_b, rear_top), (y_a, front_top), (2.0, front_top)]

    def edge(y):
        """The height of the arm's sloping top edge."""
        return front_top + (y - y_a) * (rear_top - front_top) / (y_b - y_a)

    # Each arm is a frame: what is between the screw holes is left open.
    w0, w1 = y_a + 8.0, holes[0][0] - 16.0
    window = [(w0, base_top + 7.0), (w1, base_top + 7.0), (w1, edge(w1) - 8.0), (w0, edge(w0) - 8.0)]
    solids, cuts = [base], []
    for x0, x1 in ((-p.arm_gap - p.arm_t, -p.arm_gap), (p.w + p.arm_gap, p.w + p.arm_gap + p.arm_t)):
        solids.append(prism_x(outline, x0, x1))
        cuts.append(prism_x(window, x0 - 1, x1 + 1))
        for y, z in holes:
            cuts.append(cyl_x(y, z, parts.CLEAR_D / 2, x0 - 1, x1 + 1))

    # Four holes for screwing the base down, their heads sunk, and the middle of the base
    # left open: it carries nothing.
    for x in (16.0, p.w - 16.0):
        for y in (20.0, p.depth - 20.0):
            cuts.append(cyl_z(x, y, 2.25, base_top - 5.0, base_top + 1))
            cuts.append(cone_z(x, y, 2.25, 4.4, base_top - 2.2, base_top + 0.01))
    cuts.append(rbox(32.0, 34.0, base_top - 5.0, p.w - 32.0, p.depth - 34.0, base_top + 1, 8.0))
    return cut(union(solids), cuts)


def ghosts(p):
    """Block models of what goes inside, where it goes: what the shells are checked against."""
    lay = _board_to_case(p)
    g = {"board." + k: lay(v) for k, v in board.envelope(p.board_top_clear).items()}

    f = p.floor
    y_in = p.depth - p.wall
    bx, by = p.battery_at
    g["battery"] = box(bx, by, f, bx + p.battery_w, by + p.battery_len, f + p.battery_t)
    seat = f + p.charge_seat
    g["charge_board"] = box(p.charge_x - p.charge_board_w / 2, y_in - p.charge_board_l, seat,
                            p.charge_x + p.charge_board_w / 2, y_in, seat + p.charge_board_t + 3.3)
    for i, x in enumerate(p.sma_x):
        g[f"antenna_jack{i + 1}"] = cyl_y(x, p.sma_z, 4.0, y_in - 22.0, y_in)
    fx, fz = p.fan_xz
    g["fan"] = box(fx - p.fan_size / 2, y_in - 8.0, fz - p.fan_size / 2, fx + p.fan_size / 2, y_in, fz + p.fan_size / 2)

    # The power modules lie on the floor behind the panel: charger, booster, power switch,
    # amplifier.
    y = p.y_back + 18.0
    g["charger"] = box(14, y, f, 58, y + 34.0, f + 7.0)
    g["booster"] = box(62, y, f, 102.6, y + 15.2, f + 5.0)
    g["power_switch"] = box(62, y + 19.0, f, 77.2, y + 36.8, f + 4.0)
    g["amplifier"] = box(78, y + 19.0, f, 97.4, y + 36.8, f + 4.0)

    y1 = p.y_back
    g["lcd"] = box(p.lcd_x - p.lcd_w / 2, y1, p.lcd_z - p.lcd_h / 2, p.lcd_x + p.lcd_w / 2, y1 + p.lcd_t, p.lcd_z + p.lcd_h / 2)
    g["pico"] = box(p.lcd_x - 25.5, y1 + p.lcd_t + 1.5, p.lcd_z - 10.5, p.lcd_x + 25.5, y1 + p.lcd_t + 5.1, p.lcd_z + 10.5)
    g["key_strip"] = box(p.lcd_x - 30.0, y1 + p.key_stack, p.key_z - 5.0, p.lcd_x + 30.0, y1 + p.key_stack + 1.6, p.key_z + 5.0)
    g["dial_encoder"] = _behind_panel(p.dial_at[0], p.dial_at[1], p)(parts.encoder_block())
    g["volume_encoder"] = _behind_panel(p.volume_at[0], p.volume_at[1], p)(parts.encoder_block())
    g["power_button"] = _behind_panel(p.power_at[0], p.power_at[1], p)(parts.jack_block(d=9.5, behind=15.0, front=6.0))

    sx, sy = p.speaker_at
    g["speaker"] = cyl_z(sx, sy, p.speaker_d / 2, p.top - p.speaker_depth, p.top)
    return g


def small_parts(p):
    """The loose printed parts, each as it lies on the printer's bed: (shape, how many)."""
    return {
        "clamp_tab": (parts.clamp_tab(), 12),
        "key_cap": (parts.key_cap(p.key_w, p.key_h, p.panel_t), 4),
        "dial": (parts.knob(d=p.dial_d, h=16.0, dimple=True), 1),
        "knob": (parts.knob(d=16.0, h=13.0), 1),
    }


def print_pose(name, solid, p):
    """A part turned as it is printed: the tray and the bracket as they stand, the panel on
    its face, the cover on its top."""
    if name == "panel":
        return place(solid, (1, 0, 0), (0, 0, 1), (0, -1, 0), (0, p.top, 0))
    if name == "cover":
        return place(solid, (1, 0, 0), (0, -1, 0), (0, 0, -1), (0, p.depth, p.h))
    return solid


def build(p=None):
    p = p or Desk()
    return {
        "params": p,
        "parts": {"tray": tray(p), "panel": panel(p), "cover": cover(p), "bracket": bracket(p)},
        "small": small_parts(p),
        "ghosts": ghosts(p),
        "pose": lambda name, solid: print_pose(name, solid, p),
        # For the previews: everything but the bracket turns about the rear side screws.
        "meta": {"pivot": [p.side_screws[1], p.side_screw_z], "tilts": list(p.tilts), "fixed": ["bracket"]},
    }
