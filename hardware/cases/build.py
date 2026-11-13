"""Build the cases: run with FreeCAD's command-line interpreter from this folder.

    "C:/Program Files/FreeCAD 1.0/bin/freecadcmd.exe" build.py

For each unit it writes, under out/<unit>/, every printed part as STEP (as assembled) and as
STL (as it lies on the printer's bed), a mesh of the assembly with the bought parts' block
models for the previews (render.py draws them), and report.json: each part's size and
volume, whether it is one valid solid, and every place where a part and a bought part, or
two bought parts, would share space.
"""

import json
import os
import sys
import traceback

ROOT = os.getcwd()
sys.path.insert(0, ROOT)

# A shared volume under this is a face in contact, not a collision.
TOUCH_MM3 = 0.5

# Bought parts that are meant to share space: the board's own blocks lie inside the room
# kept clear above and below it.
SAME_GROUP = ("board.",)


def mesh_of(solid, deflection=0.15):
    points, facets = solid.tessellate(deflection)
    return {
        "v": [[round(p.x, 3), round(p.y, 3), round(p.z, 3)] for p in points],
        "f": [list(f) for f in facets],
    }


def describe(solid):
    b = solid.BoundBox
    return {
        "valid": bool(solid.isValid()),
        "solids": len(solid.Solids),
        "volume_cm3": round(solid.Volume / 1000.0, 2),
        "size_mm": [round(b.XLength, 2), round(b.YLength, 2), round(b.ZLength, 2)],
    }


def collisions(parts, ghosts):
    found = []
    names = list(ghosts)
    for pname, part in parts.items():
        for gname in names:
            v = part.common(ghosts[gname]).Volume
            if v > TOUCH_MM3:
                found.append({"a": pname, "b": gname, "mm3": round(v, 1)})
    for i, a in enumerate(names):
        for b in names[i + 1:]:
            if any(a.startswith(g) and b.startswith(g) for g in SAME_GROUP):
                continue
            v = ghosts[a].common(ghosts[b]).Volume
            if v > TOUCH_MM3:
                found.append({"a": a, "b": b, "mm3": round(v, 1)})
    return found


def write_stl(solid, path):
    import Mesh

    points, facets = solid.tessellate(0.05)
    mesh = Mesh.Mesh([[tuple(points[i]) for i in f] for f in facets])
    mesh.write(path)


def build_unit(name, module):
    unit = module.build()
    out = os.path.join(ROOT, "out", name)
    os.makedirs(out, exist_ok=True)
    report = {"parts": {}, "small_parts": {}, "collisions": []}
    meshes = {"parts": {}, "ghosts": {}, "meta": unit.get("meta", {})}

    for pname, solid in unit["parts"].items():
        report["parts"][pname] = describe(solid)
        solid.exportStep(os.path.join(out, f"{name}_{pname}.step"))
        posed = unit["pose"](pname, solid)
        report["parts"][pname]["print_size_mm"] = describe(posed)["size_mm"]
        write_stl(posed, os.path.join(out, f"{name}_{pname}.stl"))
        meshes["parts"][pname] = mesh_of(solid)
    for pname, (solid, count) in unit["small"].items():
        report["small_parts"][pname] = dict(describe(solid), count=count)
        solid.exportStep(os.path.join(out, f"{name}_{pname}.step"))
        write_stl(solid, os.path.join(out, f"{name}_{pname}.stl"))
    for gname, solid in unit["ghosts"].items():
        meshes["ghosts"][gname] = mesh_of(solid, 0.4)

    report["collisions"] = collisions(unit["parts"], unit["ghosts"])
    with open(os.path.join(out, "report.json"), "w", newline="\n") as fh:
        json.dump(report, fh, indent=2)
    with open(os.path.join(out, "meshes.json"), "w", newline="\n") as fh:
        json.dump(meshes, fh)
    return report


def main():
    from fishcase import handheld

    units = {"handheld": handheld}
    try:
        from fishcase import desk

        units["desk"] = desk
    except ImportError:
        pass
    wanted = os.environ.get("FISHCASE_UNITS")
    failed = False
    for name, module in units.items():
        if wanted and name not in wanted.split(","):
            continue
        report = build_unit(name, module)
        print(f"== {name}")
        for pname, d in list(report["parts"].items()) + list(report["small_parts"].items()):
            ok = d["valid"] and d["solids"] == 1
            failed = failed or not ok
            print(f"  {pname:12s} {'ok ' if ok else 'BAD'} solids={d['solids']} "
                  f"{d['volume_cm3']:7.2f} cm3  {d['size_mm']}")
        for c in report["collisions"]:
            failed = True
            print(f"  COLLISION {c['a']} / {c['b']}: {c['mm3']} mm3")
        if not report["collisions"]:
            print("  no collisions")
    print("BUILD " + ("FAILED" if failed else "OK"))


try:
    main()
except Exception:
    traceback.print_exc()
    print("BUILD FAILED")
