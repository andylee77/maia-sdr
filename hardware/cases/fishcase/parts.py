"""The small printed parts both cases share, and block models of the bought parts.

Owns the clamp tab that holds the board, the display and the speaker, the caps over the
tactile switches, and the knob; and the envelopes (LCD module, speaker, battery pack, encoder,
jacks) the cases are checked against. The bought parts' sizes are their makers' where a
drawing was found, and marked "assumed" where not: each is an argument, so a part in hand
that differs is a number to change.
"""

from .shapes import box, cut, cyl_y, cyl_z, rbox, union

# One screw holds everything inside the cases: M3, self-tapping into a printed pilot hole.
PILOT_D = 2.6
CLEAR_D = 3.4
POST_D = 7.0
# The lid and cover screws go into heat-set inserts.
INSERT_D = 4.0
INSERT_DEPTH = 6.5

TAB_T = 2.5
TAB_W = 6.0
TAB_BACK = 3.5
TAB_REACH = 7.0


def post_y(x, z, y0, y1, d=POST_D):
    """A post standing along Y on a wall whose inner face is at y0, with a pilot hole for
    its screw in its free end: (post, hole)."""
    depth = min(y1 - y0 + 1.2, 7.0)
    return cyl_y(x, z, d / 2, y0, y1), cyl_y(x, z, PILOT_D / 2, y1 - depth, y1 + 1)


def clamp_tab():
    """A tab with one screw hole: screwed to a post, its free end reaches over the edge of
    whatever the post stands beside. Built flat, the hole at the origin, reaching along +X."""
    body = rbox(-TAB_BACK, -TAB_W / 2, 0, TAB_REACH, TAB_W / 2, TAB_T, TAB_W / 2 - 0.01)
    return cut(body, [cyl_z(0, 0, CLEAR_D / 2, -1, TAB_T + 1)])


def key_cap(w, h, wall_t, proud=1.2, flange=1.2, play=0.2):
    """A cap over a tactile switch: a plunger that fills its hole in the wall and stands
    proud of it, on a flange that keeps it in. Built flange down, centred on the origin."""
    pw, ph = w - 2 * play, h - 2 * play
    plunger = rbox(-pw / 2, -ph / 2, 0, pw / 2, ph / 2, flange + wall_t + proud, 1.3)
    base = rbox(-w / 2 - 1.0, -h / 2 - 1.0, 0, w / 2 + 1.0, h / 2 + 1.0, flange, 1.6)
    return union([base, plunger])


def knob(d=15.0, h=13.0, shaft_d=6.2, flat=1.6, bore=10.0, dimple=False):
    """A knob for a 6 mm D shaft, with a finger dimple in its face when it is a dial.
    Built bore down."""
    body = union([cyl_z(0, 0, d / 2, 0, h - 2.0), cyl_z(0, 0, d / 2 - 1.2, h - 2.0, h)])
    holes = [cyl_z(0, 0, shaft_d / 2, -1, bore).common(box(-shaft_d, -shaft_d, -1, flat, shaft_d, bore))]
    if dimple:
        holes.append(cyl_z(d / 2 - 8.0, 0, 5.0, h - 1.5, h + 1))
    return cut(body, holes)


def switch_strip(length, hole_pitch, w=10.0, t=1.6):
    """The strip of board the soft keys' tactile switches are soldered to: a block model,
    centred on the origin, the switches on +Z."""
    strip = box(-length / 2, -w / 2, 0, length / 2, w / 2, t)
    return cut(strip, [cyl_z(sx * hole_pitch / 2, 0, CLEAR_D / 2, -1, t + 1) for sx in (-1, 1)])


def lcd_block(w, h, t):
    """An SPI display module: its board's outline and its whole thickness."""
    return box(-w / 2, -h / 2, 0, w / 2, h / 2, t)


def speaker_block(d, depth, magnet_d=None, flange_t=1.5):
    """A round speaker, its face at Z = 0: the frame, and the magnet behind it."""
    if magnet_d is None or depth <= flange_t + 0.5:
        return cyl_z(0, 0, d / 2, 0, depth)
    return union([cyl_z(0, 0, d / 2, 0, flange_t + 4.0), cyl_z(0, 0, magnet_d / 2, flange_t + 4.0, depth)])


def battery_block(length=69.0, width=37.0, t=19.5):
    """A 1S2P pack of 18650 cells with its protection board: two cells side by side."""
    return box(-length / 2, -width / 2, 0, length / 2, width / 2, t)


def encoder_block(body=12.4, depth=10.5, shaft_len=15.0):
    """An EC11 encoder mounted through a panel whose inner face is at Z = 0: the body with
    its pins below, the bushing and the shaft above."""
    return union([
        box(-body / 2, -body / 2, -depth, body / 2, body / 2, 0),
        cyl_z(0, 0, 3.5, 0, 7.0),
        cyl_z(0, 0, 3.0, 7.0, shaft_len + 5.0),
    ])


def jack_block(d=9.0, behind=20.0, front=9.0, thread_d=5.0):
    """A panel jack (SMA bulkhead or push button) through a panel whose inner face is at
    Z = 0: its tail with the cable behind, the thread in front. The thread is drawn thinner
    than it is: its hole is cut to the jack's own drawing, flat and all."""
    return union([cyl_z(0, 0, d / 2, -behind, 0), cyl_z(0, 0, thread_d / 2, 0, front)])


def pico_block():
    """A Raspberry Pi Pico without headers, parts on +Z."""
    return box(-25.5, -10.5, 0, 25.5, 10.5, 3.6)


def fan_block(size=30.0, t=8.0):
    """A square fan, its mounting face at Z = 0."""
    return box(-size / 2, -size / 2, 0, size / 2, size / 2, t)
