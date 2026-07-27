#!/usr/bin/env python3
"""Geometry checks for the draw.io sources under docs/assets/diagrams.

    python3 scripts/lint-diagrams.py [file.drawio ...]

draw.io will happily render boxes stacked on top of each other, labels spilling
past their borders, and edges pointing at cells that no longer exist. All three
look fine in the XML and wrong in the exported SVG, and the export is what ends
up in the documentation, so they are caught here rather than by eye.

Exits non-zero if anything is found.
"""
import sys
import re
import glob
import xml.etree.ElementTree as ET

# draw.io's default font at the sizes used here. Measured against the existing
# diagrams rather than assumed: a 12px label needs a shade over 6px per
# character, and a line of text occupies about 17px of height.
CHAR_W = 6.4
LINE_H = 17.0
PAD_X = 16.0
PAD_Y = 10.0
# Boxes closer than this look like a rendering accident even when they do not
# strictly overlap.
MIN_GAP = 6.0


def cells(path):
    """(id, label, style, x, y, w, h, is_edge, source, target) per mxCell."""
    root = ET.parse(path).getroot()
    out = []
    for c in root.iter("mxCell"):
        cid = c.get("id")
        if cid in (None, "0", "1"):
            continue
        geo = c.find("mxGeometry")
        x = float(geo.get("x", 0)) if geo is not None else 0.0
        y = float(geo.get("y", 0)) if geo is not None else 0.0
        w = float(geo.get("width", 0)) if geo is not None else 0.0
        h = float(geo.get("height", 0)) if geo is not None else 0.0
        out.append({
            "id": cid,
            "label": c.get("value") or "",
            "style": c.get("style") or "",
            "x": x, "y": y, "w": w, "h": h,
            "edge": c.get("edge") == "1",
            "source": c.get("source"),
            "target": c.get("target"),
        })
    return out


def label_lines(label):
    """Label split into rendered lines. draw.io encodes breaks as &#10;."""
    text = re.sub(r"<[^>]+>", "", label).replace("&#10;", "\n")
    return [ln for ln in text.split("\n")]


def font_size(style):
    m = re.search(r"fontSize=(\d+)", style)
    return int(m.group(1)) if m else 12


def check(path):
    problems = []
    cs = cells(path)
    ids = {c["id"] for c in cs}
    boxes = [c for c in cs if not c["edge"] and c["w"] > 0 and c["h"] > 0]

    seen = set()
    for c in cs:
        if c["id"] in seen:
            problems.append(f"duplicate cell id {c['id']!r}")
        seen.add(c["id"])

    for c in cs:
        if not c["edge"]:
            continue
        for end in ("source", "target"):
            ref = c[end]
            if ref is not None and ref not in ids:
                problems.append(f"edge {c['id']!r} {end} points at missing cell {ref!r}")
        if c["source"] is None and c["target"] is None:
            problems.append(f"edge {c['id']!r} is attached to nothing at either end")

    for i, a in enumerate(boxes):
        for b in boxes[i + 1:]:
            # Containment is deliberate (a box drawn inside a group), so only
            # partial overlap is reported.
            ox = min(a["x"] + a["w"], b["x"] + b["w"]) - max(a["x"], b["x"])
            oy = min(a["y"] + a["h"], b["y"] + b["h"]) - max(a["y"], b["y"])
            if ox > 0 and oy > 0:
                contained = (
                    (a["x"] >= b["x"] and a["y"] >= b["y"]
                     and a["x"] + a["w"] <= b["x"] + b["w"]
                     and a["y"] + a["h"] <= b["y"] + b["h"])
                    or (b["x"] >= a["x"] and b["y"] >= a["y"]
                        and b["x"] + b["w"] <= a["x"] + a["w"]
                        and b["y"] + b["h"] <= a["y"] + a["h"]))
                if not contained:
                    problems.append(
                        f"{a['id']!r} and {b['id']!r} overlap by "
                        f"{ox:.0f}x{oy:.0f}px")
            elif -MIN_GAP < ox <= 0 and oy > 0:
                problems.append(
                    f"{a['id']!r} and {b['id']!r} are only {-ox:.0f}px apart horizontally")
            elif -MIN_GAP < oy <= 0 and ox > 0:
                problems.append(
                    f"{a['id']!r} and {b['id']!r} are only {-oy:.0f}px apart vertically")

    for c in boxes:
        lines = label_lines(c["label"])
        if not any(ln.strip() for ln in lines):
            continue
        scale = font_size(c["style"]) / 12.0
        widest = max(len(ln) for ln in lines) * CHAR_W * scale
        if widest + PAD_X > c["w"]:
            problems.append(
                f"{c['id']!r} label needs about {widest + PAD_X:.0f}px "
                f"but the box is {c['w']:.0f}px wide")
        tall = len(lines) * LINE_H * scale
        if tall + PAD_Y > c["h"]:
            problems.append(
                f"{c['id']!r} label needs about {tall + PAD_Y:.0f}px "
                f"but the box is {c['h']:.0f}px tall")

    return problems


def main():
    targets = sys.argv[1:] or sorted(glob.glob("docs/assets/diagrams/*.drawio"))
    if not targets:
        print("no .drawio files found")
        return 1
    bad = 0
    for path in targets:
        try:
            problems = check(path)
        except ET.ParseError as e:
            print(f"FAIL {path}\n      not valid XML: {e}")
            bad += 1
            continue
        if problems:
            bad += 1
            print(f"FAIL {path}")
            for p in problems:
                print(f"      {p}")
        else:
            print(f"ok   {path}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
