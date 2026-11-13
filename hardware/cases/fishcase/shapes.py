"""Solid-building helpers for the case models.

Owns the primitives every part is made of (boxes, prisms with rounded corners, slots, slot
grilles), the booleans with their clean-up, and the rigid placement of a solid built in one
frame into another. Everything is in millimetres.
"""

import math

import Part
from FreeCAD import Matrix, Vector


def box(x0, y0, z0, x1, y1, z1):
    """An axis-aligned box between two corners."""
    return Part.makeBox(x1 - x0, y1 - y0, z1 - z0, Vector(x0, y0, z0))


def cyl_z(x, y, r, z0, z1):
    """A cylinder along Z."""
    return Part.makeCylinder(r, z1 - z0, Vector(x, y, z0), Vector(0, 0, 1))


def cone_z(x, y, r0, r1, z0, z1):
    """A cone frustum along Z, radius r0 at z0 and r1 at z1."""
    return Part.makeCone(r0, r1, z1 - z0, Vector(x, y, z0), Vector(0, 0, 1))


def rbox(x0, y0, z0, x1, y1, z1, r):
    """A box whose four edges along Z are rounded to radius r."""
    r = min(r, (x1 - x0) / 2 - 1e-3, (y1 - y0) / 2 - 1e-3)
    if r <= 0.01:
        return box(x0, y0, z0, x1, y1, z1)
    corners = [(x1 - r, y1 - r, 0), (x0 + r, y1 - r, 90), (x0 + r, y0 + r, 180), (x1 - r, y0 + r, 270)]
    edges = []
    for cx, cy, a in corners:
        edges.append(Part.makeCircle(r, Vector(cx, cy, z0), Vector(0, 0, 1), a, a + 90))
    lines = [
        ((x1 - r, y1), (x0 + r, y1)),
        ((x0, y1 - r), (x0, y0 + r)),
        ((x0 + r, y0), (x1 - r, y0)),
        ((x1, y0 + r), (x1, y1 - r)),
    ]
    for (ax, ay), (bx, by) in lines:
        if abs(ax - bx) + abs(ay - by) > 1e-6:
            edges.append(Part.makeLine(Vector(ax, ay, z0), Vector(bx, by, z0)))
    wire = Part.Wire(Part.__sortEdges__(edges))
    return Part.Face(wire).extrude(Vector(0, 0, z1 - z0))


def prism_x(outline_yz, x0, x1):
    """A plate along X whose outline is a polygon given as (y, z) corners."""
    pts = [Vector(x0, y, z) for y, z in outline_yz]
    face = Part.Face(Part.makePolygon(pts + [pts[0]]))
    return face.extrude(Vector(x1 - x0, 0, 0))


def slot(x0, y0, z0, x1, y1, z1):
    """A box along Z whose shorter side in XY ends in half circles."""
    return rbox(x0, y0, z0, x1, y1, z1, min(x1 - x0, y1 - y0) / 2)


def union(solids):
    """The fusion of several solids, with the seams between coplanar faces removed."""
    solids = [s for s in solids if s is not None]
    if len(solids) == 1:
        return solids[0]
    return solids[0].multiFuse(solids[1:]).removeSplitter()


def cut(solid, cutters):
    """A solid less every cutter."""
    cutters = [c for c in cutters if c is not None]
    if not cutters:
        return solid
    return solid.cut(Part.makeCompound(cutters)).removeSplitter()


def chamfer_where(solid, size, on_face):
    """A solid with the edges of one flat face chamfered: on_face says whether a point lies
    in that face. A chamfer, not a fillet, so that the face can lie on the printer's bed."""
    edges = [e for e in solid.Edges if all(on_face(v.Point) for v in e.Vertexes)]
    return solid.makeChamfer(size, edges)


def place(solid, cu, cv, cw, origin):
    """A copy of a solid moved rigidly: its X, Y and Z axes become cu, cv and cw, its origin
    lands on origin. The three axes must form a right-handed set."""
    det = (
        cu[0] * (cv[1] * cw[2] - cv[2] * cw[1])
        - cu[1] * (cv[0] * cw[2] - cv[2] * cw[0])
        + cu[2] * (cv[0] * cw[1] - cv[1] * cw[0])
    )
    if abs(det - 1.0) > 1e-6:
        raise ValueError("place: the axes are not a right-handed rotation")
    m = Matrix(
        cu[0], cv[0], cw[0], origin[0],
        cu[1], cv[1], cw[1], origin[1],
        cu[2], cv[2], cw[2], origin[2],
        0, 0, 0, 1,
    )
    moved = solid.copy()
    moved.transformShape(m)
    return moved


def moved(solid, dx=0.0, dy=0.0, dz=0.0):
    """A copy of a solid shifted by a vector."""
    return place(solid, (1, 0, 0), (0, 1, 0), (0, 0, 1), (dx, dy, dz))


def unplace(solid, cu, cv, cw, origin):
    """The inverse of place: a copy of a solid brought back into the frame whose axes and
    origin are given in the frame the solid is in."""
    ou = -(cu[0] * origin[0] + cu[1] * origin[1] + cu[2] * origin[2])
    ov = -(cv[0] * origin[0] + cv[1] * origin[1] + cv[2] * origin[2])
    ow = -(cw[0] * origin[0] + cw[1] * origin[1] + cw[2] * origin[2])
    return place(solid, (cu[0], cv[0], cw[0]), (cu[1], cv[1], cw[1]), (cu[2], cv[2], cw[2]), (ou, ov, ow))


def to_y(solid):
    """Turn a solid built along Z so that it runs along Y: what was built at (z, x, y) in
    its own X, Y and Z lands at (x, y, z)."""
    return place(solid, (0, 0, 1), (1, 0, 0), (0, 1, 0), (0, 0, 0))


def to_x(solid):
    """Turn a solid built along Z so that it runs along X: what was built at (y, z, x) in
    its own X, Y and Z lands at (x, y, z)."""
    return place(solid, (0, 1, 0), (0, 0, 1), (1, 0, 0), (0, 0, 0))


def cyl_y(x, z, r, y0, y1):
    """A cylinder along Y."""
    return to_y(cyl_z(z, x, r, y0, y1))


def cyl_x(y, z, r, x0, x1):
    """A cylinder along X."""
    return to_x(cyl_z(y, z, r, x0, x1))


def rbox_y(x0, z0, x1, z1, r, y0, y1):
    """A box whose four edges along Y are rounded."""
    return to_y(rbox(z0, x0, y0, z1, x1, y1, r))


def rbox_x(y0, z0, y1, z1, r, x0, x1):
    """A box whose four edges along X are rounded."""
    return to_x(rbox(y0, z0, x0, y1, z1, x1, r))


def slot_grille(cx, cy, radius, z0, z1, slot_w=2.2, pitch=4.2, along_y=False):
    """Parallel slots that fill a circle: the cutters for a speaker or fan opening. The slots
    run along X, or along Y when asked."""
    cutters = []
    n = int(radius // pitch)
    for i in range(-n, n + 1):
        off = i * pitch
        half = math.sqrt(max(radius * radius - off * off, 0.0)) - slot_w / 2
        if half < slot_w:
            continue
        if along_y:
            cutters.append(slot(cx + off - slot_w / 2, cy - half, z0, cx + off + slot_w / 2, cy + half, z1))
        else:
            cutters.append(slot(cx - half, cy + off - slot_w / 2, z0, cx + half, cy + off + slot_w / 2, z1))
    return cutters


def tube_z(x, y, r_out, r_in, z0, z1):
    """A ring along Z."""
    return cyl_z(x, y, r_out, z0, z1).cut(cyl_z(x, y, r_in, z0 - 1, z1 + 1))


def d_hole(x, y, r, flat, z0, z1):
    """A round hole with one flat, as an SMA bulkhead jack wants: the cutter. flat is the
    distance from the centre to the flat."""
    return cyl_z(x, y, r, z0, z1).common(box(x - r - 1, y - r - 1, z0, x + flat, y + r + 1, z1))
