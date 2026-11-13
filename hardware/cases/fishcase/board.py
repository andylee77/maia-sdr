"""The Fishball Z7020 board (OpenSourceSDRLab's PlutoSky) as the cases see it.

Owns the board's outline, the openings its connectors need, the rails and posts that hold it,
and a block model of it for checking that nothing collides. The numbers are taken from the
vendor's own printed case (the STEP files in OpenSourceSDRLab/PlutoSky_7020_AD936X_SDR,
"3D case"): the pocket the board lies in, the openings in its end wall and the four spots
where its posts press on the board. What that case does not show is marked "assumed".

The board frame, right-handed, in millimetres:
  u  across the board, zero on its centre line; the Ethernet jack is on the negative side
  v  along the board, zero at the edge with the Ethernet, USB and SD connectors, 79 at the SMAs
  w  through the board, zero at the face with the SD socket, 1.6 at the face with the parts
"""

from .shapes import box, cyl_z, place, rbox

PCB_W = 50.0
PCB_L = 79.0
PCB_T = 1.6

# Play between the board and what locates it.
END_GAP = 0.25
SIDE_GAP = 0.2

# From the connector edge of the board to the outside of the wall the connectors look
# through, and the thickness that wall keeps where the USB and SD connectors are: they
# reach only a millimetre past the board's edge.
EDGE_TO_OUTSIDE = 2.6
THIN_WALL = 1.3

# Openings in the connector wall: (u0, u1, w0, w1).
RJ45 = (-22.74, -6.21, 1.55, 15.65)
USB_DEBUG = (-2.2, 7.3, 1.74, 5.14)
USB_OTG = (13.26, 22.76, 1.74, 5.14)
SD_SLOT = (-4.64, 10.94, -1.9, 0.79)
# Where the wall is thinned for the USB and SD connectors.
THIN_SPAN = (-4.64, 22.76, -2.3, 6.2)

# The strip of the board's lower face that may rest on a rail: from LEDGE_IN to the edge,
# along both long edges. Parts on that face need LOWER_CLEAR under the board elsewhere.
LEDGE_IN = 23.75
LOWER_CLEAR = 2.1

# Where a clamp may press on the upper face: within CLAMP_REACH of each long edge, at these
# places along the board. The posts the clamps screw into stand at CLAMP_POST_U.
CLAMP_V = (24.65, 57.75)
CLAMP_REACH = 2.4
CLAMP_POST_U = 29.6

# The SMA jacks' centre lines, and how far they are taken to stand past the board's edge.
SMA_U = (-19.01, -6.23, 6.56, 19.31)
SMA_REACH = 10.0  # assumed

# The expansion header (JP5, a 2x10 socket) and the fan over the Zynq: placed from the
# vendor's photographs, not measured.
JP5_BOX = (-23.6, -18.4, 22.0, 47.6, 8.6)  # u0, u1, v0, v1, height above the board; assumed
FAN_CENTRE = (5.0, 38.0)  # u, v; assumed


def connector_cutter(inside=1.0):
    """What to take out of the connector wall. The wall's outer face is at v = -EDGE_TO_OUTSIDE;
    the cutter reaches `inside` into the cavity."""
    out = -EDGE_TO_OUTSIDE - 1.0
    u0, u1, w0, w1 = RJ45
    solids = [box(u0, out, w0, u1, inside, w1)]
    for u0, u1, w0, w1 in (USB_DEBUG, USB_OTG, SD_SLOT):
        solids.append(_slot_through(u0, u1, w0, w1, out, 0.0))
    u0, u1, w0, w1 = THIN_SPAN
    solids.append(box(u0, -EDGE_TO_OUTSIDE + THIN_WALL, w0, u1, inside, w1))
    return solids


def rails(floor_w):
    """The two rails the board's long edges rest on, with the fences that centre it and the
    stops past its SMA edge. floor_w is where the surface they stand on is (negative: below
    the board)."""
    solids = []
    edge = PCB_W / 2 + SIDE_GAP
    for side in (-1, 1):
        a, b = sorted((side * LEDGE_IN, side * (edge + 1.6)))
        solids.append(box(a, 1.5, floor_w, b, PCB_L - 2.0, 0.0))
        a, b = sorted((side * edge, side * (edge + 1.6)))
        solids.append(box(a, 1.5, floor_w, b, PCB_L - 2.0, PCB_T + 0.8))
    # The stops stand between the jacks, where the vendor's case has its own.
    for a, b in ((-15.3, -9.9), (10.3, 15.6)):
        solids.append(box(a, PCB_L - 1.5, floor_w, b, PCB_L + END_GAP, 0.0))
        solids.append(box(a, PCB_L + END_GAP, floor_w, b, PCB_L + END_GAP + 1.8, PCB_T + 0.8))
    return solids


def clamp_posts(floor_w, pilot_d, post_d=7.0):
    """The four posts beside the board that its clamp tabs screw into: (solids, holes)."""
    solids, holes = [], []
    for side in (-1, 1):
        for v in CLAMP_V:
            u = side * CLAMP_POST_U
            solids.append(cyl_z(u, v, post_d / 2, floor_w, PCB_T))
            holes.append(cyl_z(u, v, pilot_d / 2, PCB_T - 8.0, PCB_T + 1.0))
    return solids, holes


def clamp_tab_sites():
    """Where the clamp tabs sit when the board is fixed: (u of the screw, v, the sign of the
    direction along u in which the tab reaches over the board)."""
    return [(side * CLAMP_POST_U, v, -side) for side in (-1, 1) for v in CLAMP_V]


def envelope(top_clear):
    """A block model of the board with its connectors: the space it takes, with top_clear
    kept free above the upper face (the header's plug and the fan live there)."""
    blocks = {
        "pcb": box(-PCB_W / 2, 0, 0, PCB_W / 2, PCB_L, PCB_T),
        "rj45": box(-22.5, -2.4, PCB_T, -6.4, 19.0, 15.2),
        "usb_debug": box(-1.9, -1.1, PCB_T, 7.0, 6.5, PCB_T + 3.3),
        "usb_otg": box(13.5, -1.1, PCB_T, 22.5, 6.5, PCB_T + 3.3),
        "sd_socket": box(-4.0, 0.0, -1.9, 10.3, 15.5, 0.0),
        "jp5": box(JP5_BOX[0], JP5_BOX[2], PCB_T, JP5_BOX[1], JP5_BOX[3], PCB_T + JP5_BOX[4]),
        "keep_above": box(-PCB_W / 2 + 3, 20.0, PCB_T, PCB_W / 2 - 3, PCB_L - 4, PCB_T + top_clear),
        "keep_below": box(-LEDGE_IN + 0.3, 2.0, -LOWER_CLEAR, LEDGE_IN - 0.3, PCB_L - 2.5, 0.0),
    }
    for i, u in enumerate(SMA_U):
        blocks[f"sma{i + 1}"] = box(u - 3.2, PCB_L - 4.0, -2.2, u + 3.2, PCB_L + SMA_REACH, 4.2)
    return blocks


def _slot_through(u0, u1, w0, w1, v0, v1):
    """A slot through the wall with rounded corners, as the vendor's openings have."""
    lying = rbox(u0, w0, 0.0, u1, w1, v1 - v0, 1.0)
    # The lying slot's x is u, its y is w, and its length runs back along v.
    return place(lying, (1, 0, 0), (0, 0, 1), (0, -1, 0), (0.0, v1, 0.0))
