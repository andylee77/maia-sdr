"""Draw the previews of the cases from the meshes build.py wrote: run with Python.

    python render.py

Writes out/<unit>/preview_*.png: each unit closed, open with the bought parts' block models
in place, and each main part alone. The drawing is a plain painter's rendering, good for
seeing where things are and not for more.
"""

import json
import os

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
from mpl_toolkits.mplot3d.art3d import Poly3DCollection

ROOT = os.path.dirname(os.path.abspath(__file__))

SHELLS = [(0.80, 0.82, 0.85), (0.62, 0.68, 0.76), (0.70, 0.74, 0.80), (0.45, 0.49, 0.55)]
GHOSTS = {
    "board.pcb": (0.15, 0.50, 0.30),
    "board.": (0.55, 0.58, 0.60),
    "battery": (0.25, 0.45, 0.80),
    "lcd": (0.10, 0.10, 0.12),
    "speaker": (0.30, 0.30, 0.32),
    "": (0.85, 0.60, 0.25),
}
HIDDEN = ("board.keep_above", "board.keep_below")


def ghost_colour(name):
    for prefix, colour in GHOSTS.items():
        if prefix and (name == prefix or name.startswith(prefix)):
            return colour
    return GHOSTS[""]


def triangles(mesh):
    v = np.array(mesh["v"], dtype=float)
    f = np.array(mesh["f"], dtype=int)
    return v[f] if len(f) else np.zeros((0, 3, 3))


def tilted(tris, pivot, degrees):
    """Triangles turned about the line across the unit through pivot (y, z): front up."""
    a = np.radians(-degrees)
    y = tris[..., 1] - pivot[0]
    z = tris[..., 2] - pivot[1]
    out = tris.copy()
    out[..., 1] = pivot[0] + y * np.cos(a) - z * np.sin(a)
    out[..., 2] = pivot[1] + y * np.sin(a) + z * np.cos(a)
    return out


def draw(items, path, elev, azim, size=(9, 9)):
    """items: (triangles, colour) pairs. One collection, so the faces sort among themselves.
    The background is left clear, so a picture sits on a light page or a dark one."""
    tris = np.concatenate([t for t, _ in items])
    base = np.concatenate([np.tile(np.array(c), (len(t), 1)) for t, c in items])
    n = np.cross(tris[:, 1] - tris[:, 0], tris[:, 2] - tris[:, 0])
    n /= np.maximum(np.linalg.norm(n, axis=1, keepdims=True), 1e-9)
    az, el = np.radians(azim), np.radians(elev)
    eye = np.array([np.cos(el) * np.cos(az), np.cos(el) * np.sin(az), np.sin(el)])
    light = eye + np.array([0.3, -0.2, 0.6])
    light /= np.linalg.norm(light)
    shade = 0.35 + 0.65 * np.abs(n @ light)
    colours = np.clip(base * shade[:, None], 0, 1)

    fig = plt.figure(figsize=size)
    ax = fig.add_subplot(111, projection="3d")
    ax.add_collection3d(Poly3DCollection(tris, facecolors=colours, edgecolors="none", linewidths=0))
    lo, hi = tris.reshape(-1, 3).min(axis=0), tris.reshape(-1, 3).max(axis=0)
    ax.set_xlim(lo[0], hi[0])
    ax.set_ylim(lo[1], hi[1])
    ax.set_zlim(lo[2], hi[2])
    ax.set_box_aspect(tuple(np.maximum(hi - lo, 1.0)))
    ax.view_init(elev=elev, azim=azim)
    ax.set_axis_off()
    fig.subplots_adjust(left=0, right=1, bottom=0, top=1)
    fig.savefig(path, dpi=110, transparent=True)
    plt.close(fig)


def unit_views(name, views):
    """views: (file, the shells to draw, whether to draw what is inside, the tilt in the
    bracket, elevation, azimuth, what it shows)."""
    out = os.path.join(ROOT, "out", name)
    with open(os.path.join(out, "meshes.json")) as fh:
        meshes = json.load(fh)
    meta = meshes.get("meta", {})
    fixed = set(meta.get("fixed", []))
    order = list(meshes["parts"])
    part = {k: triangles(v) for k, v in meshes["parts"].items()}
    ghost = [(triangles(v), ghost_colour(k)) for k, v in meshes["ghosts"].items() if k not in HIDDEN]
    for fname, which, with_ghosts, tilt, elev, azim, title in views:
        def turn(t, key=None):
            return t if not tilt or key in fixed else tilted(t, meta["pivot"], tilt)

        items = [(turn(part[w], w), SHELLS[order.index(w) % len(SHELLS)]) for w in which]
        if with_ghosts:
            items += [(turn(t), c) for t, c in ghost]
        draw(items, os.path.join(out, fname), elev, azim)
        print("wrote", os.path.join(name, fname), "-", title)


def main():
    unit_views("handheld", [
        ("preview_closed_front.png", ["tub", "cover"], False, 0, 12, -62, "Handheld, front"),
        ("preview_closed_back.png", ["tub", "cover"], False, 0, 12, 118, "Handheld, back"),
        ("preview_open.png", ["tub"], True, 0, 24, 112, "Handheld, cover off, parts in place"),
        ("preview_tub.png", ["tub"], False, 0, 30, 105, "Handheld tub, from the back"),
        ("preview_cover.png", ["cover"], False, 0, 24, -70, "Handheld cover, inside"),
        ("preview_bottom.png", ["tub", "cover"], False, 0, -42, -75, "Handheld, bottom face: Ethernet, USB, SD"),
    ])
    body = ["tray", "panel", "cover"]
    unit_views("desk", [
        ("preview_closed_front.png", body + ["bracket"], False, 6, 16, -58, "Desk unit in its bracket, tipped back 6 degrees"),
        ("preview_closed_rear.png", body + ["bracket"], False, 0, 14, 122, "Desk unit, rear: Ethernet, USB, SD, fan, antennas, charge port"),
        ("preview_open.png", ["tray", "panel"], True, 0, 46, -66, "Desk unit, cover off, parts in place"),
        ("preview_body.png", body, False, 0, 20, -58, "Desk unit without the bracket"),
        ("preview_panel_back.png", ["panel"], False, 0, 20, 110, "Desk front panel, from behind"),
        ("preview_cover_under.png", ["cover"], False, 0, -38, -68, "Desk cover, from below"),
    ])


if __name__ == "__main__":
    main()
