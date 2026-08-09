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
    "known_good": 'b.get("known_good"',
    "known_good_note": 'b["known_good_note"]',
}
# Fields this script itself honours, rather than the fetch.
CHECKED_HERE = {"expect", "axes", "license_note"}
# The MCU-family axes, kept in step with the `# mcu family` line of AXES below by
# _check_release_gate_axes, which refuses any name this vocabulary does not
# define. It gates a manifest consistency warning only, never a grade: the
# release gate's own exemption is a single `no-mcu` axis precisely so that no
# allowlist can go stale and quietly switch a rule off.
MCU_FAMILY_AXES = {
    "avr", "stm32", "esp32", "nrf", "rp2040", "riscv", "samd", "imx",
}
NO_MCU_AXIS_LOCAL = "no-mcu"

# Fields the RELEASE GATE honours, rather than the fetch or this script. The
# fetch has nothing to do for them: the file they name is already in the tree it
# pulled, and the gate resolves it at discovery time. Verified against the
# gate's source the same way HONOURED_FIELDS is verified against the fetch, so a
# rename there cannot leave a field here that only looks load-bearing.
# Needles chosen so a mere mention in a comment cannot satisfy them: each is the
# exact expression the gate uses to READ the field.
HONOURED_BY_RELEASE_GATE = {
    "firmware": 'entry.get("firmware")',
    "firmware_expect": 'entry.get("firmware_expect")',
}
RELEASE_GATE_SOURCE = "qc/unseen_boards.py"
NOT_KNOWN_GOOD_MARKER = ".hauksbee-not-known-good"
# Every axis an entry may claim. Spelling is fixed so coverage can be counted
# rather than guessed at, and a typo cannot quietly invent a new axis.
AXES = {
    # format and version
    "kicad5", "kicad6", "kicad7", "kicad8", "kicad9", "kicad10",
    "eagle", "eagle-binary", "altium-binary", "protel-ascii", "gerber-only",
    "odbpp", "ipc2581",
    # A format hauksbee documents that it does not read. The entry exists to hold
    # the refusal to its word on real files, and must never be counted as board
    # coverage.
    "unreadable-by-design",
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


def _check_release_gate_axes(bad):
    """The release gate keys a rule off one axis; keep the two in step.

    `qc/value_grading.NO_MCU_AXIS` is the only axis that exempts a Gerber package
    from the reconstruction floor. Renaming it here without renaming it there
    would leave an exemption nothing can trigger, or worse, a spelling that no
    longer exempts the passive boards it exists for.
    """

    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    sys.path.insert(0, root)
    try:
        from qc.value_grading import NO_MCU_AXIS
    except Exception as error:
        # Fail CLOSED. Silently passing when the gate cannot be imported is the
        # opposite of what this check is for.
        bad.append(f"cannot read the release gate to cross-check its axes: {error}")
        return
    finally:
        sys.path.remove(root)
    if NO_MCU_AXIS not in AXES:
        bad.append(
            f"qc/value_grading.py exempts the reconstruction floor on axis "
            f"`{NO_MCU_AXIS}`, which this manifest vocabulary does not define"
        )
    stale = sorted(MCU_FAMILY_AXES - AXES)
    if stale:
        bad.append(
            "MCU_FAMILY_AXES names axes this vocabulary does not define: "
            + ", ".join(stale)
        )


def _release_gate_source():
    """The release gate's source, or None when it is not beside this script."""

    path = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                        RELEASE_GATE_SOURCE)
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read()
    except OSError:
        return None


def check_manifest(doc, script_src):
    bad = []
    seen_ids, seen_dests = {}, {}

    for field, needle in HONOURED_FIELDS.items():
        if needle not in script_src:
            bad.append(
                f"corpus.toml field `{field}` is documented as honoured but "
                f"scripts/fetch-corpus.sh does not read it ({needle!r} absent)"
            )

    _check_release_gate_axes(bad)

    gate_source = _release_gate_source()
    if gate_source is None:
        bad.append(
            f"cannot read {RELEASE_GATE_SOURCE} to cross-check the fields it honours"
        )
    for field, needle in HONOURED_BY_RELEASE_GATE.items():
        if gate_source is not None and needle not in gate_source:
            bad.append(
                f"corpus.toml field `{field}` is documented as honoured by the "
                f"release gate but {RELEASE_GATE_SOURCE} does not read it "
                f"({needle!r} absent)"
            )

    for i, b in enumerate(doc.get("board", [])):
        if "id" not in b:
            bad.append(f"board #{i} has no id")
            continue
        bid = b["id"]
        for field in b:
            if (
                field not in HONOURED_FIELDS
                and field not in CHECKED_HERE
                and field not in HONOURED_BY_RELEASE_GATE
            ):
                bad.append(
                    f"{bid}: field `{field}` is declared and nothing honours it. "
                    f"Either act on it or delete it; a field that only looks "
                    f"load-bearing is how the KiCad qa/ tree got into the gate"
                )
        # An entry covering many boards may legitimately declare both (the CATs
        # Eurosynth entry is 88 modules, a few with an AVR and most purely
        # analogue), so the pair is only ambiguous where the release gate actually
        # READS the exemption: a Gerber entry, whose reconstruction floor it
        # switches off.
        axes = set(b.get("axes", []))
        if (
            "gerber-only" in axes
            and NO_MCU_AXIS_LOCAL in axes
            and (axes & MCU_FAMILY_AXES)
        ):
            bad.append(
                f"{bid}: a gerber-only entry declares both `{NO_MCU_AXIS_LOCAL}` "
                f"and an MCU family ({', '.join(sorted(axes & MCU_FAMILY_AXES))}); "
                "the release gate reads the first as an exemption from the "
                "reconstruction floor, so the pair is ambiguous"
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

        if b.get("known_good", True) is False and not b.get("known_good_note", "").strip():
            bad.append(
                f"{bid}: known_good = false with no known_good_note. Excluding a "
                f"board from the silence gates without saying why is how a gate "
                f"quietly narrows its own input set"
            )
        if b.get("known_good_note") and b.get("known_good", True):
            bad.append(f"{bid}: known_good_note without known_good = false; nothing reads it")

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


def _would_be_floored(path):
    """`(floor, facts)` if the gate would hold this package to a net floor.

    The gate's own two functions, imported rather than reimplemented: asking
    `expected_min_nets` directly means this lint fires on exactly the packages the
    floor would reach, including the ones whose copper cannot be classified and
    which are floored from a lower-bound estimate. A hand-written `>= 2 films and
    >= 500 flashes` copy missed those. Returns None when the package is
    unreadable, carries no Gerber film, or would receive no floor anyway.
    """

    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    sys.path.insert(0, root)
    try:
        from qc.value_grading import expected_min_nets, gerber_input_facts
    except Exception:
        return None
    finally:
        sys.path.remove(root)
    try:
        import pathlib

        facts = gerber_input_facts(pathlib.Path(path))
    except Exception:
        return None
    if not facts.get("gerber_layers"):
        return None
    # Asked WITHOUT the exemption: the question is what the floor would be if the
    # axis were not there.
    floor = expected_min_nets(facts, ())
    return None if floor is None else (floor, facts)


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

        # The exemption is a manifest CLAIM, and it is the one claim that can
        # switch the reconstruction floor off entirely. Where the package it names
        # is readable, measure it: a board dense enough to receive a floor is not
        # the passive low-net board the exemption was written for, and the two
        # uConsole entries showed how easily a socketed compute module gets
        # written down as `no-mcu` on a full mainboard. Unreadable packages (a
        # `.7z`, say) cannot be measured, so that residual stays open and is
        # stated in docs/ci/RELEASE_BOARD_GATES.md rather than implied closed.
        if NO_MCU_AXIS_LOCAL in set(b.get("axes", [])):
            for e in b.get("expect", []):
                target = os.path.join(dest, e)
                if not os.path.exists(target):
                    continue
                measured = _would_be_floored(target)
                if measured is None:
                    continue
                floor, facts = measured
                bad.append(
                    f"{bid}: declares `{NO_MCU_AXIS_LOCAL}`, which exempts {e} "
                    f"from the release gate's reconstruction floor, but that "
                    f"package would otherwise be held to {floor} nets on "
                    f"{facts['aperture_flashes']} aperture flashes across "
                    f"{facts['copper_layers']} copper film(s). The exemption is "
                    f"for passive low-net boards; on a package this dense it "
                    f"would hide a connectivity collapse"
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
        marker = os.path.join(dest, NOT_KNOWN_GOOD_MARKER)
        if b.get("known_good", True) is False:
            if not os.path.isfile(marker):
                bad.append(
                    f"{bid}: known_good = false and {dest_rel}/{NOT_KNOWN_GOOD_MARKER} "
                    f"is missing, so the silence gates would grade themselves on it"
                )
        elif os.path.isfile(marker):
            bad.append(
                f"{bid}: {dest_rel}/{NOT_KNOWN_GOOD_MARKER} is present and the "
                f"manifest does not declare known_good = false"
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
