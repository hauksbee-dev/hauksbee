#!/usr/bin/env python3
"""Corpus manifest self-check: does corpus.toml describe the corpus that lands?

Two independent failures made the nightly board gate meaningless, and both were
invisible because nothing compared the manifest against reality.

  1. `subdir = "demos"` was declared on the KiCad entry from the day it was added
     and `fetch-corpus.sh` never acted on it. The fetch pulled KiCad's whole
     repository, including the `qa/` tree, and the zero-shorts gate graded itself
     on boards whose entire purpose is to reproduce KiCad bugs.
  2. Five sweeps resolved board paths through a `famous/` level that exists only
     in the hand-built corpus, so on a fetched corpus they matched nothing and
     reported the empty match as a pass.

Both are the same shape: a declaration nobody honours. So this script checks the
manifest against the script that reads it, and against the tree that lands.

  --manifest-only   the static half: field names, pin shapes, duplicate ids.
                    Needs no corpus, so it can run on every push.
  --dir DIR         also the landed half: every entry's `expect` paths exist,
                    every transform actually happened.

Exit 0 clean, 1 with one line per problem.
"""

import argparse
import os
import re
import sys
import tomllib

# Every field `fetch-corpus.sh` reads and acts on. A field in corpus.toml that is
# not in here is a declaration nothing honours, which is failure (1) above; a
# field in here that the script does not mention is a check that has rotted.
HONOURED_FIELDS = {
    "id": "b[\"id\"]",
    "name": 'b.get("name"',
    "url": 'b["url"]',
    "rev": 'b.get("rev"',
    "kind": 'b.get("kind"',
    "sha256": 'b.get("sha256"',
    "dest": 'b.get("dest"',
    "subdir": 'b.get("subdir"',
    "drop": 'b.get("drop"',
    "hoist": 'b.get("hoist"',
    "unpack": 'b.get("unpack"',
    "license": 'b.get("license"',
    "license_confirmed": 'b.get("license_confirmed"',
}
# Fields this script itself honours, rather than the fetch.
CHECKED_HERE = {"expect", "axes", "license_note"}
# Every axis an entry may claim. Spelling is fixed so coverage can be counted
# rather than guessed at, and a typo cannot quietly invent a new axis.
AXES = {
    # format and version
    "kicad5", "kicad6", "kicad7", "kicad8", "kicad9", "kicad10",
    "eagle", "altium-binary", "protel-ascii", "gerber-only", "odbpp", "ipc2581",
    # board class
    "dev-board", "keyboard", "power-electronics", "motor-driver",
    "flight-controller", "audio", "rf", "industrial-sensor", "sbc", "hat",
    "wearable", "handheld", "instrument", "regression-fixture",
    # mcu family
    "avr", "stm32", "esp32", "nrf", "rp2040", "riscv", "samd", "imx", "no-mcu",
    # scale
    "tiny", "small", "medium", "large",
}
PIN_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")

DESIGN_EXT = (
    ".kicad_pcb", ".kicad_sch", ".brd", ".sch", ".net", ".pcbdoc", ".schdoc",
)


def landed_design_files(path):
    n = 0
    for _dp, _dn, fns in os.walk(path):
        n += sum(1 for f in fns if f.lower().endswith(DESIGN_EXT))
    return n


def check_manifest(doc, script_src):
    bad = []
    seen_ids, seen_dests = {}, {}

    for field, needle in HONOURED_FIELDS.items():
        if needle not in script_src:
            bad.append(
                f"corpus.toml field `{field}` is documented as honoured but "
                f"scripts/fetch-corpus.sh does not read it ({needle!r} absent)"
            )

    for i, b in enumerate(doc.get("board", [])):
        if "id" not in b:
            bad.append(f"board #{i} has no id")
            continue
        bid = b["id"]
        for field in b:
            if field not in HONOURED_FIELDS and field not in CHECKED_HERE:
                bad.append(
                    f"{bid}: field `{field}` is declared and nothing honours it. "
                    f"Either act on it or delete it; a field that only looks "
                    f"load-bearing is how the KiCad qa/ tree got into the gate"
                )
        if bid in seen_ids:
            bad.append(f"{bid}: duplicate id (also board #{seen_ids[bid]})")
        seen_ids[bid] = i

        dest = b.get("dest", bid)
        if dest in seen_dests:
            bad.append(
                f"{bid}: dest `{dest}` collides with {seen_dests[dest]}; "
                f"one board would overwrite the other"
            )
        seen_dests[dest] = bid

        kind = b.get("kind", "git")
        if kind not in ("git", "zip"):
            bad.append(f"{bid}: kind `{kind}` is neither git nor zip")
        if kind == "git":
            rev = b.get("rev", "")
            if not rev:
                bad.append(f"{bid}: kind=git needs a rev")
            elif not PIN_RE.match(rev):
                bad.append(
                    f"{bid}: rev `{rev}` is not a full 40-character sha. An "
                    f"abbreviated pin that happens to be a branch head hides "
                    f"drift, and a tag can be moved"
                )
            if b.get("sha256"):
                bad.append(f"{bid}: sha256 on a git entry is not checked by anything")
        if kind == "zip":
            if not SHA256_RE.match(b.get("sha256", "")):
                bad.append(f"{bid}: kind=zip needs a 64-character sha256; it is the pin")
            if b.get("rev"):
                bad.append(f"{bid}: rev on a zip entry is not checked by anything")

        if not b.get("license"):
            bad.append(f"{bid}: no license")
        if b.get("license") == "unconfirmed" and b.get("license_confirmed", True):
            bad.append(f"{bid}: license says unconfirmed but license_confirmed is not false")

        for a in b.get("axes", []):
            if a not in AXES:
                bad.append(f"{bid}: unknown axis `{a}`; add it to AXES or fix the spelling")
        if not b.get("axes"):
            bad.append(
                f"{bid}: no axes. The corpus is only diverse if the diversity is "
                f"recorded, so every entry says what it is there to stress"
            )

        exp = b.get("expect", [])
        if not isinstance(exp, list) or not all(isinstance(e, str) for e in exp):
            bad.append(f"{bid}: expect must be a list of relative paths")
        elif not exp:
            bad.append(
                f"{bid}: no expect paths. Without one, a fetch that lands the "
                f"wrong tree looks identical to one that lands the right tree"
            )
        for e in exp:
            if e.startswith("/") or ".." in e.split("/"):
                bad.append(f"{bid}: expect path `{e}` must be relative and inside the board")
    return bad


def check_landed(doc, root, include_unconfirmed):
    bad = []
    for b in doc.get("board", []):
        bid, dest_rel = b["id"], b.get("dest", b["id"])
        dest = os.path.join(root, dest_rel)
        if not b.get("license_confirmed", True) and not include_unconfirmed:
            continue
        if not os.path.isdir(dest):
            bad.append(f"{bid}: nothing at {dest_rel}/ (the fetch did not land it)")
            continue
        if not os.path.isfile(os.path.join(dest, ".hauksbee-rev")):
            bad.append(
                f"{bid}: {dest_rel}/.hauksbee-rev is missing, so what landed "
                f"cannot be traced to a pin"
            )
        else:
            got = open(os.path.join(dest, ".hauksbee-rev")).read().split("\n")[0].strip()
            want = b.get("rev") or ""
            if want and got != want:
                bad.append(f"{bid}: pinned {want} but {dest_rel}/ holds {got}")

        for e in b.get("expect", []):
            if not os.path.exists(os.path.join(dest, e)):
                bad.append(
                    f"{bid}: expected {dest_rel}/{e} and it is not there. This is "
                    f"the path a gate resolves, so the gate would match nothing "
                    f"and report the empty match as a pass"
                )

        # The transforms have to have happened, not merely been declared.
        sub = b.get("subdir")
        if sub:
            if not os.path.isdir(os.path.join(dest, sub)):
                bad.append(f"{bid}: subdir `{sub}` is declared and {dest_rel}/{sub} does not exist")
            else:
                # `subdir` may be nested, in which case its FIRST component is
                # what survives at the top level.
                top = sub.split("/")[0]
                strays = [
                    n for n in os.listdir(dest)
                    if n != top and not n.startswith(".hauksbee")
                    and not re.match(r"(?i)(licen[sc]e|copying|readme)", n)
                ]
                if strays:
                    bad.append(
                        f"{bid}: subdir `{sub}` is declared but {dest_rel}/ still "
                        f"carries {sorted(strays)[:6]}. This is the KiCad qa/ "
                        f"failure: boards outside the wanted subtree reach the gate"
                    )
        for d in b.get("drop", []):
            if os.path.exists(os.path.join(dest, d)):
                bad.append(f"{bid}: drop `{d}` is declared and {dest_rel}/{d} is still there")
        hoist = b.get("hoist")
        if hoist and os.path.isdir(os.path.join(dest, hoist)):
            bad.append(
                f"{bid}: hoist `{hoist}` is declared and {dest_rel}/{hoist} is "
                f"still there, so its contents were never lifted"
            )
        unpack = b.get("unpack")
        if unpack and os.path.isfile(os.path.join(dest, unpack)):
            bad.append(f"{bid}: unpack `{unpack}` is declared and the archive is still packed")

        if landed_design_files(dest) == 0 and "gerber-only" not in b.get("axes", []):
            bad.append(f"{bid}: not one design file landed under {dest_rel}/")
    return bad


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--manifest", default=os.path.join(root, "corpus.toml"))
    ap.add_argument("--dir", help="a fetched corpus to check the manifest against")
    ap.add_argument("--manifest-only", action="store_true")
    ap.add_argument("--include-unconfirmed", action="store_true")
    args = ap.parse_args()

    with open(args.manifest, "rb") as f:
        doc = tomllib.load(f)
    with open(os.path.join(here, "fetch-corpus.sh"), encoding="utf-8") as f:
        script_src = f.read()

    bad = check_manifest(doc, script_src)
    n_entries = len(doc.get("board", []))
    checked = "manifest"
    if args.dir and not args.manifest_only:
        bad += check_landed(doc, args.dir, args.include_unconfirmed)
        checked = f"manifest and the corpus at {args.dir}"

    for line in bad:
        print(f"corpus.toml: {line}")
    if bad:
        print(f"\n{len(bad)} problem(s) across {n_entries} entries ({checked})")
        return 1
    print(f"corpus self-check: {n_entries} entries clean ({checked})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
