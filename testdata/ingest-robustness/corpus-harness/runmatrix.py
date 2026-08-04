#!/usr/bin/env python3
"""Run every corpus file through the four hauksbee ingest surfaces and classify.

Modes: --check --plain | --report --plain | --drc | --json

Classification (per file, worst mode wins):
  OK              every mode produced present-and-sane output
  HONEST-REFUSAL  refused with exit 1 and a message that names the file/reason
  CRASH-OR-PANIC  Rust panic, abort, or a fatal signal
  HANG            exceeded the timeout
  SILENTLY-WRONG  exit 0 but the output contradicts the source file
  SUSPECT         exit 0 with output that looks wrong but is not proven so

Re-runnable: `python3 runmatrix.py [--jobs N] [--timeout S] [--only substr]`.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time
import zipfile
from concurrent.futures import ThreadPoolExecutor

ROOT = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(ROOT, "ingest-corpus")
BIN = os.environ.get(
    "HAUKSBEE_BIN",
    "/Users/hauksbee-user/Tarski/Tarski-Repos/hauksbee-dev/target/release/hauksbee",
)
RESULTS = os.path.join(ROOT, "matrix-results.jsonl")

MODES = {
    "check": ["--check", "--plain"],
    "report": ["--report", "--plain"],
    "drc": ["--drc"],
    "json": ["--json"],
    "nets": ["--list-nets", "--plain"],
}

PANIC_RE = re.compile(
    r"panicked at|thread '.*' panicked|stack backtrace|RUST_BACKTRACE|"
    r"fatal runtime error|memory allocation of|SIGSEGV|SIGABRT|attempt to subtract with overflow",
    re.I,
)


# ── expectations read straight out of the source file ────────────────────────
COPPER_EXT = (".gtl", ".gbl", ".cmp", ".sol", ".g1", ".g2", ".g3", ".g4")


def dir_expectations(path):
    """A gerber JOB folder: what fab files are actually in it."""
    names = sorted(os.listdir(path))
    exp = {"family": "gerber_dir", "entries": len(names), "sample": names[:12]}
    low = [n.lower() for n in names]
    exp["copper_files"] = sum(
        1 for n in low
        if n.endswith(COPPER_EXT) or ("cu" in n and n.endswith((".gbr", ".ger")))
    )
    exp["drill_files"] = sum(1 for n in low if n.endswith((".drl", ".xln", ".txt", ".nc", ".drd")))
    # A pick-and-place file is what turns pads into parts, so its presence is
    # what decides whether "no components" is an honest answer for this job.
    pnp = [
        n for n in names
        if n.lower().endswith((".pos", ".csv"))
        or any(k in n.lower() for k in ("-pos", "pnp", "placement", "position", "pick"))
    ]
    exp["pnp_files"] = len(pnp)
    exp["pnp_sample"] = pnp[:3]
    return exp


def expectations(path):
    """Cheap independent count of components/nets from the raw source."""
    if os.path.isdir(path):
        return dir_expectations(path)
    exp = {}
    try:
        with open(path, "rb") as fh:
            raw = fh.read()
    except OSError:
        return exp
    exp["file_bytes"] = len(raw)
    head = raw[:2048].decode("utf-8", "replace")
    if raw[:4] == b"PK\x03\x04":
        exp["family"] = "zip"
        try:
            with zipfile.ZipFile(path) as z:
                names = z.namelist()
            exp["zip_entries"] = len(names)
            exp["zip_sample"] = names[:12]
        except Exception as e:
            exp["zip_error"] = str(e)[:120]
        return exp
    if raw[:4] == b"\xd0\xcf\x11\xe0":
        exp["family"] = "ole2"
        return exp
    text = raw.decode("utf-8", "replace")
    if "(kicad_pcb" in head:
        exp["family"] = "kicad_pcb"
        m = re.search(r"\(version\s+(\d+)", head)
        exp["format_version"] = m.group(1) if m else None
        exp["footprints"] = len(re.findall(r"\n\s*\((?:footprint|module)\s", text))
        # DISTINCT reference designators, which is what the bind table shows
        # (four MountingHole footprints all named REF** are one row, so raw
        # footprint blocks are not a fair comparison).
        # KiCad 4/5 write the ref unquoted (`(fp_text reference REF** ...)`),
        # KiCad 6+ quote it, KiCad 7+ use `(property "Reference" "R1")`.
        # Only footprints that HAVE a pad are electrical parts, and only those
        # appear in the bind table: a silkscreen logo, a kibuzzard text label and
        # a mounting hole are padless artwork that the extractor drops on
        # purpose. Counting them made a correctly-read keyboard plate look like
        # a half-failed parse. So slice the file into footprint blocks and take
        # the ref only from blocks containing a pad.
        refs = set()
        for block in re.split(r"\n\s*\((?:footprint|module)\s", text)[1:]:
            # Cut the block at the start of the next top-level form; the split
            # already did that for footprints, and a trailing tail is harmless.
            if not re.search(r"\n\s*\(pad\s", block):
                continue
            m = re.search(r'\(fp_text\s+reference\s+(?:"([^"]*)"|([^\s\)]+))', block) \
                or re.search(r'\(property\s+"Reference"\s+"([^"]*)"()', block)
            if m:
                ref = (m.group(1) or m.group(2) or "").strip()
                if ref:
                    refs.add(ref)
        exp["refs"] = len(refs)
        # Distinct NON-ZERO net numbers anywhere in the file (declarations and
        # pad references share the same `(net N "name")` syntax, so counting
        # occurrences overcounts; the distinct-id set is the honest floor).
        ids = {int(n) for n in re.findall(r"\(net\s+(\d+)[\s\"\)]", text)}
        exp["net_ids"] = len(ids - {0})
        exp["pads"] = len(re.findall(r"\n\s*\(pad\s", text))
    elif "(kicad_sch" in head:
        exp["family"] = "kicad_sch"
        m = re.search(r"\(version\s+(\d+)", head)
        exp["format_version"] = m.group(1) if m else None
        exp["symbols"] = len(re.findall(r'\(property "Reference" "', text))
    elif head.lstrip().startswith("(export"):
        exp["family"] = "kicad_net"
        exp["comps"] = len(re.findall(r"\(comp\s+\(ref", text))
        exp["nets"] = len(re.findall(r"\(net\s+\(code", text))
    elif "<eagle" in head:
        exp["family"] = "eagle_brd"
        m = re.search(r'<eagle version="([^"]+)"', head)
        exp["format_version"] = m.group(1) if m else None
        exp["elements"] = len(re.findall(r"<element\s", text))
        exp["refs"] = len(set(re.findall(r'<element[^>]*\sname="([^"]*)"', text)))
        exp["signals"] = len(re.findall(r"<signal\s", text))
    elif re.search(r"(?m)^3[126]7", text):
        exp["family"] = "ipc356"
        nets = set(re.findall(r"(?m)^3[126]7(.{17})", text))
        exp["ipc_nets"] = len({n.strip() for n in nets})
        exp["ipc_records"] = len(re.findall(r"(?m)^3[126]7", text))
    elif "|RECORD=" in head and "Protel_Advanced_PCB" in text[:200000]:
        exp["family"] = "protel_ascii"
        exp["comps"] = len(re.findall(r"\|RECORD=Component\|", text))
    elif "|RECORD=" in head:
        exp["family"] = "protel_ascii_other"
    elif head.startswith("version https://git-lfs"):
        exp["family"] = "lfs_pointer"
    elif re.search(r"(?m)^%FS|^G04|^%MO(MM|IN)", text[:4000]):
        exp["family"] = "gerber_loose"
    elif re.search(r"(?m)^M48", text[:200]):
        exp["family"] = "excellon"
    elif not raw.strip():
        exp["family"] = "empty"
    else:
        exp["family"] = "other"
    return exp


def run_one(path, mode_args, timeout):
    t0 = time.time()
    try:
        p = subprocess.run(
            [BIN, "run", path] + mode_args,
            capture_output=True, timeout=timeout,
            stdin=subprocess.DEVNULL,
            env={**os.environ, "NO_COLOR": "1", "TERM": "dumb", "RUST_BACKTRACE": "0"},
        )
        return {
            "exit": p.returncode,
            "secs": round(time.time() - t0, 2),
            "stdout": p.stdout.decode("utf-8", "replace"),
            "stderr": p.stderr.decode("utf-8", "replace"),
            "timeout": False,
        }
    except subprocess.TimeoutExpired:
        return {"exit": None, "secs": round(time.time() - t0, 2),
                "stdout": "", "stderr": "", "timeout": True}


def classify_mode(mode, res, exp, path):
    """Return (class, reason)."""
    name = os.path.basename(path)
    if res["timeout"]:
        return "HANG", f"{mode}: exceeded timeout after {res['secs']}s"
    out, err = res["stdout"], res["stderr"]
    if PANIC_RE.search(err) or PANIC_RE.search(out):
        snippet = (PANIC_RE.search(err) or PANIC_RE.search(out)).group(0)
        return "CRASH-OR-PANIC", f"{mode}: {snippet} :: {err.strip()[:300]}"
    rc = res["exit"]
    if rc is not None and (rc < 0 or rc > 128):
        return "CRASH-OR-PANIC", f"{mode}: killed by signal (exit {rc})"
    if rc == 2:
        return "CRASH-OR-PANIC", f"{mode}: usage/arg failure on a plain board path: {err.strip()[:200]}"
    if rc in (1, 3):
        # 1 = refused to read/analyse, 3 = read it but it is not analysable.
        # Both are loud, non-zero outcomes; they pass only if they say why.
        msg = (err + out).strip()
        if not msg:
            return "CRASH-OR-PANIC", f"{mode}: exit {rc} with NO message at all"
        return "HONEST-REFUSAL", msg[:400]
    if mode == "json":
        try:
            doc = json.loads(out)
        except Exception as e:
            return "CRASH-OR-PANIC", f"json: exit {rc} but stdout is not JSON ({e}): {out[:200]}"
        # `ok: false` is a VERDICT (the board has findings), not a refusal:
        # only the error object printed on the failure path carries `error`.
        err_field = doc.get("error")
        if isinstance(err_field, str) and err_field.strip():
            return "HONEST-REFUSAL", err_field[:400]
        return "OK", ""
    # `--list-nets` prints the "N net(s):" header on stderr so stdout stays a
    # bare pipeable list, so a zero-net board legitimately has empty stdout.
    if not (out.strip() or err.strip()):
        return "CRASH-OR-PANIC", f"{mode}: exit {rc} but produced no output at all"
    return "OK", ""


# A bind-table data row: "│ <ref> │ <value> │ ...". Reference designators in
# real boards include `*`, `?`, `/` and unicode, so take the whole first cell
# rather than a guessed character class (REF** used to slip through and make an
# all-mounting-holes board look like a zero-component parse).
REF_ROW = re.compile(r"^│([^│]+)│")


def observed_counts(outs):
    """What the tool actually reported: components from the bind table, nets
    from --list-nets. Returns (components, nets), either possibly None."""
    comps = None
    rep = outs.get("report", "")
    if rep.strip():
        refs = set()
        for line in rep.splitlines():
            m = REF_ROW.match(line)
            ref = m.group(1).strip() if m else ""
            if ref and ref != "Ref" and not ref.startswith("RAIL"):
                refs.add(ref)
        m = re.search(r"(\d+) of (\d+) non-ignored components resolved", rep)
        comps = len(refs) if refs else (int(m.group(2)) if m else 0)
    nets = None
    ns = outs.get("nets_err", "") + outs.get("nets", "")
    m = re.search(r"(?m)^\s*(\d+) net\(s\)", ns)
    if m:
        nets = int(m.group(1))
    return comps, nets


def sanity(exp, comps, nets):
    """Evidence-based sanity of a successful run against the source file."""
    fam = exp.get("family")

    def wrong(msg):
        return "SILENTLY-WRONG", msg

    def suspect(msg):
        return "SUSPECT", msg

    if fam == "kicad_pcb":
        fp = exp.get("refs") or exp.get("footprints", 0)
        if fp > 0 and comps == 0:
            return wrong(f"source names {fp} distinct reference designators but the run reports 0 components")
        if fp > 5 and comps is not None and comps < fp * 0.4:
            return suspect(f"source names {fp} distinct refs, the bind table shows {comps}")
        nd = exp.get("net_ids", 0)
        if nd > 1 and nets == 0:
            return wrong(f"source carries {nd} distinct non-zero net ids but --list-nets reports 0")
        if nd > 4 and nets is not None and nets < nd * 0.5:
            return suspect(f"source carries {nd} distinct net ids, --list-nets reports {nets}")
    elif fam == "eagle_brd":
        el = exp.get("refs") or exp.get("elements", 0)
        if el > 0 and comps == 0:
            return wrong(f"eagle .brd has {el} <element> entries but the run reports 0 components")
        sg = exp.get("signals", 0)
        if sg > 1 and nets == 0:
            return wrong(f"eagle .brd has {sg} <signal> entries but --list-nets reports 0")
    elif fam == "kicad_net":
        c = exp.get("comps", 0)
        if c > 0 and comps == 0:
            return wrong(f"netlist has {c} (comp (ref ...)) entries but the run reports 0 components")
        n = exp.get("nets", 0)
        if n > 1 and nets == 0:
            return wrong(f"netlist has {n} (net (code ...)) entries but --list-nets reports 0")
    elif fam == "ipc356":
        n = exp.get("ipc_nets", 0)
        if n > 1 and nets == 0:
            return wrong(f"IPC-D-356 has {exp.get('ipc_records')} test records / ~{n} nets but --list-nets reports 0")
    elif fam == "kicad_sch":
        s = exp.get("symbols", 0)
        if s > 2 and comps == 0:
            return suspect(f"schematic has {s} Reference properties but the run reports 0 components")
    elif fam in ("zip", "ole2", "protel_ascii"):
        if comps == 0 and not nets:
            return suspect(f"{fam} input accepted but reports 0 components and 0 nets")
    if not comps and not nets:
        return suspect("accepted the file but reported neither components nor nets")
    return "OK", ""


SEVERITY = ["OK", "HONEST-REFUSAL", "SUSPECT", "FALSE-REFUSAL", "SILENTLY-WRONG",
            "HANG", "CRASH-OR-PANIC"]


def refusal_is_true(reasons, exp):
    """Cross-check a refusal against the source. Returns None when the refusal
    checks out, or a reason string when the file contradicts it."""
    msg = " ".join(reasons)
    fam = exp.get("family")
    if "has no components" in msg:
        # Honest only if the file really carries no electrical parts. A pad on a
        # net is an electrical part, whatever the footprint is called.
        if fam == "kicad_pcb" and exp.get("pads", 0) > 0 and exp.get("net_ids", 0) > 0:
            return (f"refused as 'no components' but the source has {exp['pads']} pads "
                    f"across {exp['net_ids']} nets")
        if fam == "eagle_brd" and exp.get("elements", 0) > 0 and exp.get("signals", 0) > 0:
            return (f"refused as 'no components' but the source has {exp['elements']} "
                    f"elements and {exp['signals']} signals")
        if fam == "kicad_net" and exp.get("comps", 0) > 0:
            return f"refused as 'no components' but the netlist has {exp['comps']} comps"
        if fam == "ipc356" and exp.get("ipc_records", 0) > 0:
            return (f"refused as 'no components' but the file has "
                    f"{exp['ipc_records']} IPC test records")
        if fam == "gerber_dir" and exp.get("pnp_files", 0) > 0:
            return ("refused as 'no components' but the job ships a pick-and-place "
                    f"file ({exp.get('pnp_sample')})")
        if fam in ("gerber_dir", "zip") and exp.get("copper_files", 0) >= 2:
            return (f"refused as 'no components' but the job has "
                    f"{exp['copper_files']} copper layer file(s)")
    if "unrecognized board format" in msg or "Unrecognized" in msg:
        if fam in ("kicad_pcb", "kicad_sch", "kicad_net", "eagle_brd", "ipc356",
                   "protel_ascii"):
            return f"refused as an unrecognized format, but the content is {fam}"
    if "Git LFS pointer" in msg and fam != "lfs_pointer":
        return f"claimed a Git LFS pointer, but the content is {fam}"
    return None


def process(path, timeout):
    exp = expectations(path)
    row = {"file": os.path.relpath(path, CORPUS), "abs": path, "exp": exp, "modes": {}}
    worst, reasons = "OK", []
    outs = {}
    for mode, args in MODES.items():
        res = run_one(path, args, timeout)
        cls, why = classify_mode(mode, res, exp, path)
        outs[mode] = res["stdout"]
        outs[mode + "_err"] = res["stderr"]
        row["modes"][mode] = {
            "exit": res["exit"], "secs": res["secs"], "class": cls,
            "why": why,
            "stderr_tail": res["stderr"].strip()[-400:],
        }
        if res["secs"] > 60:
            row["modes"][mode]["slow"] = True
        if SEVERITY.index(cls) > SEVERITY.index(worst):
            worst = cls
        if why:
            reasons.append(f"[{mode}] {why}")
    # Cross-mode sanity: only meaningful when the tool claimed success.
    if worst == "OK":
        comps, nets = observed_counts(outs)
        row["observed"] = {"components": comps, "nets": nets}
        cls, why = sanity(exp, comps, nets)
        if cls != "OK":
            worst = cls
            reasons.append(f"[sanity] {why}")
    if worst == "HONEST-REFUSAL":
        bad = refusal_is_true(reasons, exp)
        if bad:
            worst = "FALSE-REFUSAL"
            reasons.append(f"[false-refusal] {bad}")
    row["class"] = worst
    row["reasons"] = reasons
    return row


def main():
    global CORPUS
    ap = argparse.ArgumentParser()
    ap.add_argument("--jobs", type=int, default=6)
    ap.add_argument("--timeout", type=int, default=60)
    ap.add_argument("--only", default=None)
    ap.add_argument("--out", default=RESULTS)
    ap.add_argument("--root", default=CORPUS)
    ap.add_argument("--limit", type=int, default=0)
    args = ap.parse_args()
    CORPUS = args.root

    files = []
    # A gerber job folder is ONE input (the directory), not N loose layers.
    job_roots = [os.path.join(CORPUS, b) for b in ("gerber_dir", "gerber_pnp")]
    for root in job_roots:
        if not os.path.isdir(root):
            continue
        for d in sorted(os.listdir(root)):
            full = os.path.join(root, d)
            if os.path.isdir(full) and (not args.only or args.only in full):
                files.append(full)
    for dirpath, dirnames, filenames in os.walk(CORPUS):
        if any(dirpath.startswith(r) for r in job_roots):
            dirnames[:] = []
            continue
        dirnames[:] = [d for d in dirnames if not d.startswith(".")]
        for fn in filenames:
            if fn in ("manifest.jsonl",) or fn.startswith("."):
                continue
            p = os.path.join(dirpath, fn)
            if args.only and args.only not in p:
                continue
            files.append(p)
    files.sort()
    if args.limit:
        step = max(1, len(files) // args.limit)
        files = files[::step][: args.limit]
    print(f"{len(files)} files, {len(MODES)} modes, {args.jobs} jobs", flush=True)

    rows = []
    done = 0
    with ThreadPoolExecutor(max_workers=args.jobs) as ex:
        for row in ex.map(lambda p: process(p, args.timeout), files):
            rows.append(row)
            done += 1
            if row["class"] not in ("OK", "HONEST-REFUSAL"):
                print(f"  !! {row['class']} {row['file']}\n     {row['reasons'][:1]}", flush=True)
            if done % 50 == 0:
                print(f"  ... {done}/{len(files)}", flush=True)
    with open(args.out, "w") as fh:
        for r in rows:
            fh.write(json.dumps(r) + "\n")

    from collections import Counter
    by_class = Counter(r["class"] for r in rows)
    by_fam = Counter(r["exp"].get("family") for r in rows)
    print("\n== classification ==")
    for k in SEVERITY:
        if by_class[k]:
            print(f"  {k:16} {by_class[k]}")
    print("\n== detected family ==")
    for k, v in by_fam.most_common():
        print(f"  {str(k):20} {v}")
    good = by_class["OK"] + by_class["HONEST-REFUSAL"]
    print(f"\npass (OK or HONEST-REFUSAL) = {good}/{len(rows)} = {100.0*good/max(1,len(rows)):.2f}%")


if __name__ == "__main__":
    main()
