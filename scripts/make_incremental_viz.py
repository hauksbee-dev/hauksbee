#!/usr/bin/env python3
"""Render a before / after / diff visualisation of an incremental recompile.

The story: start from a board, make one small edit (here: move a single part to
a new location, exactly the kind of edit a human makes in the Board-as-Code
text), recompile *incrementally* so untouched parts keep their coordinates and
only the changed part is re-placed. The diff panel makes that visible: kept
parts are grey, the moved part is drawn in orange at both its old (ghost) and
new position with an arrow between them, and any genuinely new part is green.

This is deliberately a standalone renderer (not kicad-cli) so it can colour the
changed region; kicad-cli cannot highlight individual footprints.

Run via board_as_code_assets.sh, or directly:

    python3 make_incremental_viz.py --bin <galvani> --board <storm.board> \
        --assets <docs/assets> --work <tmpdir> --kicad-cli <kicad-cli>
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

try:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.patches import FancyArrowPatch, Rectangle
except Exception as e:  # pragma: no cover
    print(f"matplotlib required for the incremental viz: {e}", file=sys.stderr)
    sys.exit(0)  # soft-skip so the asset script does not hard-fail


FP_RE = re.compile(
    r'\(footprint\s+"[^"]*"\s*\(layer\s+"([^"]+)"\)\s*\(at\s+([\d.-]+)\s+([\d.-]+)(?:\s+([\d.-]+))?\)'
)
REF_RE = re.compile(r'\(property\s+"Reference"\s*"([^"]*)"')


def parse_board(path: Path):
    """Return {reference: (x, y, rot, layer, [(px,py,w,h),...])} for a board."""
    text = path.read_text()
    comps = {}
    # Split into footprint chunks.
    idxs = [m.start() for m in re.finditer(r"\(footprint ", text)]
    idxs.append(len(text))
    for a, b in zip(idxs, idxs[1:]):
        chunk = text[a:b]
        mat = re.search(
            r'\(layer "([^"]+)"\)\(at ([\d.-]+) ([\d.-]+)(?: ([\d.-]+))?\)', chunk
        )
        if not mat:
            mat = re.search(
                r'\(layer "([^"]+)"\)\s*\(at ([\d.-]+) ([\d.-]+)(?: ([\d.-]+))?\)',
                chunk,
            )
        if not mat:
            continue
        layer, x, y, rot = mat.group(1), float(mat.group(2)), float(mat.group(3)), float(mat.group(4) or 0)
        rm = REF_RE.search(chunk)
        ref = rm.group(1) if rm else f"?{a}"
        pads = []
        for pm in re.finditer(
            r"\(pad [^()]*\(at ([\d.-]+) ([\d.-]+)(?: [\d.-]+)?\)\(size ([\d.-]+) ([\d.-]+)\)",
            chunk,
        ):
            pads.append(
                (float(pm.group(1)), float(pm.group(2)), float(pm.group(3)), float(pm.group(4)))
            )
        comps.setdefault(ref, (x, y, rot, layer, pads))
    return comps


def draw_board(ax, comps, highlight=None, ghosts=None, title=""):
    highlight = highlight or {}
    ghosts = ghosts or {}
    import math

    def rot_pad(px, py, rot):
        a = math.radians(rot)
        return px * math.cos(a) - py * math.sin(a), px * math.sin(a) + py * math.cos(a)

    for ref, (x, y, rot, layer, pads) in comps.items():
        color = highlight.get(ref, "#cfd3d8")
        for (px, py, w, h) in pads or [(0, 0, 1, 1)]:
            rx, ry = rot_pad(px, py, rot)
            ax.add_patch(
                Rectangle(
                    (x + rx - w / 2, y + ry - h / 2),
                    w,
                    h,
                    facecolor=color,
                    edgecolor="none",
                    zorder=2,
                )
            )
    # Ghost (old positions) for moved parts + arrows.
    for ref, (ox, oy) in ghosts.items():
        if ref not in comps:
            continue
        nx, ny, *_ = comps[ref]
        ax.add_patch(
            Rectangle((ox - 2, oy - 2), 4, 4, facecolor="none", edgecolor="#e8a33d", lw=1.2, ls="--", zorder=3)
        )
        ax.add_patch(
            FancyArrowPatch((ox, oy), (nx, ny), arrowstyle="->", color="#e8a33d", lw=1.5, mutation_scale=12, zorder=4)
        )
    # add_patch does not autoscale; compute bounds from component extents.
    xs, ys = [], []
    for (x, y, _r, _l, pads) in comps.values():
        xs.append(x)
        ys.append(y)
    for (ox, oy) in ghosts.values():
        xs.append(ox)
        ys.append(oy)
    if xs:
        pad_m = 6.0
        ax.set_xlim(min(xs) - pad_m, max(xs) + pad_m)
        # y inverted below, so set ascending then invert.
        ax.set_ylim(min(ys) - pad_m, max(ys) + pad_m)
    ax.set_title(title, fontsize=11)
    ax.set_aspect("equal")
    ax.invert_yaxis()  # KiCad y grows downward
    ax.axis("off")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--board", required=True)
    ap.add_argument("--assets", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--kicad-cli", default="kicad-cli")
    args = ap.parse_args()

    work = Path(args.work)
    assets = Path(args.assets)
    base_board = Path(args.board)
    code = base_board.read_text()

    # --- the edit: move one resistor far across the board ---------------------
    # Pick the first `comp R...` with an `at`, and shift it by +12, -10 mm. This
    # is the textual edit a human would make; incremental recompile must then
    # re-place only this part (and keep everyone else fixed).
    m = re.search(r'(comp (R\d+)[^\n]* at )([\d.-]+) ([\d.-]+)( rot)', code)
    if not m:
        m = re.search(r'(comp (\w+)[^\n]* at )([\d.-]+) ([\d.-]+)( rot)', code)
    if not m:
        print("could not find a component to edit; skipping incremental viz", file=sys.stderr)
        return
    moved_ref = m.group(2)
    ox, oy = float(m.group(3)), float(m.group(4))
    nx, ny = ox + 12.0, oy - 10.0
    edited = code[: m.start()] + f"{m.group(1)}{nx} {ny}{m.group(5)}" + code[m.end():]
    edited_board = work / "storm_edited.board"
    edited_board.write_text(edited)
    print(f"   edited: moved {moved_ref} from ({ox:.1f},{oy:.1f}) to ({nx:.1f},{ny:.1f})")

    # --- recompile: original (no relayout) and edited (incremental) -----------
    orig_pcb = work / "inc_orig.kicad_pcb"
    inc_pcb = work / "inc_recompiled.kicad_pcb"
    subprocess.run([args.bin, "from-code", str(base_board), "--out", str(orig_pcb)], check=True)
    subprocess.run(
        [args.bin, "from-code", str(edited_board), "--incremental", "--out", str(inc_pcb)],
        check=True,
    )

    orig = parse_board(orig_pcb)
    inc = parse_board(inc_pcb)

    # --- classify: kept / moved / new ----------------------------------------
    moved, new, ghosts = {}, {}, {}
    for ref, (x, y, *_rest) in inc.items():
        if ref in orig:
            ox2, oy2, *_ = orig[ref]
            if abs(x - ox2) > 0.05 or abs(y - oy2) > 0.05:
                moved[ref] = "#e8a33d"  # orange
                ghosts[ref] = (ox2, oy2)
        else:
            new[ref] = "#3da35d"  # green
    highlight = {**moved, **new}
    print(f"   incremental: {len(moved)} moved, {len(new)} new, {len(inc) - len(moved) - len(new)} kept")

    # --- render 3 panels ------------------------------------------------------
    fig, axes = plt.subplots(1, 3, figsize=(16, 5.2))
    draw_board(axes[0], orig, title="original")
    draw_board(axes[1], inc, title="edited + incrementally recompiled")
    draw_board(axes[2], inc, highlight=highlight, ghosts=ghosts, title="diff: moved (orange), new (green), kept (grey)")
    fig.suptitle(
        f"Incremental recompile: moved {moved_ref}; untouched parts stay put",
        fontsize=13,
    )
    fig.tight_layout(rect=(0, 0, 1, 0.96))
    out_png = assets / "incremental_recompile.png"
    fig.savefig(out_png, dpi=130)
    print(f"   wrote {out_png}")


if __name__ == "__main__":
    main()
