#!/usr/bin/env python3
"""Scenario QC: run simulated engineering sessions against the real binaries.

Every scenario under qc/scenarios/ is a directory holding a scenario.toml (what
the session is, who is running it, and what has to be true) and an EXPECT.md
(what a real user should experience, in prose). This runner executes the steps,
checks the assertions against real stdout/stderr/exit codes, and writes a
transcript so a failure can be read without re-running anything.

    qc/run.sh                      run every scenario
    qc/run.sh --scenario 04        run the ones whose id contains "04"
    qc/run.sh --bin-dir target/x   run a different build

Exit code is 0 only when every non-skipped scenario passed.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import time
import tomllib
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from pathlib import Path

QC_DIR = Path(__file__).resolve().parent
REPO = QC_DIR.parent

# A step that overruns this is a failure even if its assertions hold: the flows
# here are sub-second flows, and a thirty-second one means something regressed
# into a place no assertion looks.
DEFAULT_MAX_SECONDS = 30.0


# --------------------------------------------------------------------------
# results


@dataclass
class StepResult:
    name: str
    kind: str
    argv: list[str] = field(default_factory=list)
    exit_code: int | None = None
    stdout: str = ""
    stderr: str = ""
    seconds: float = 0.0
    failures: list[str] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not self.failures


@dataclass
class ScenarioResult:
    ident: str
    title: str
    persona: str
    goal: str
    steps: list[StepResult] = field(default_factory=list)
    skipped: str | None = None
    error: str | None = None

    @property
    def seconds(self) -> float:
        return sum(s.seconds for s in self.steps)

    @property
    def status(self) -> str:
        if self.skipped:
            return "SKIP"
        if self.error:
            return "ERROR"
        return "PASS" if all(s.ok for s in self.steps) else "FAIL"

    @property
    def failures(self) -> list[str]:
        out = []
        if self.error:
            out.append(self.error)
        for s in self.steps:
            out.extend(f"{s.name}: {f}" for f in s.failures)
        return out


# --------------------------------------------------------------------------
# a small JSON-Schema draft-07 subset, so the schema check needs no dependency


def validate_subset(instance, schema: dict, path: str = "$") -> list[str]:
    """Check type / required / properties / items / enum, and nothing else.

    Deliberately a subset: the point is to catch a report that dropped a
    required field or changed a field's type, which is what breaks a consumer.
    A dependency on jsonschema for that would not survive a CI runner without
    network.
    """
    errs: list[str] = []
    types = schema.get("type")
    if types:
        want = types if isinstance(types, list) else [types]
        py = {
            "object": dict,
            "array": list,
            "string": str,
            "boolean": bool,
            "null": type(None),
        }
        ok = False
        for t in want:
            if t == "number" and isinstance(instance, (int, float)) and not isinstance(instance, bool):
                ok = True
            elif t == "integer" and isinstance(instance, int) and not isinstance(instance, bool):
                ok = True
            elif t in py and isinstance(instance, py[t]):
                ok = True
        if not ok:
            return [f"{path}: expected type {want}, got {type(instance).__name__}"]
    if "enum" in schema and instance not in schema["enum"]:
        errs.append(f"{path}: {instance!r} not in enum {schema['enum']}")
    if isinstance(instance, dict):
        for key in schema.get("required", []):
            if key not in instance:
                errs.append(f"{path}: missing required property '{key}'")
        for key, sub in (schema.get("properties") or {}).items():
            if key in instance:
                errs.extend(validate_subset(instance[key], sub, f"{path}.{key}"))
    if isinstance(instance, list) and isinstance(schema.get("items"), dict):
        for i, item in enumerate(instance):
            errs.extend(validate_subset(item, schema["items"], f"{path}[{i}]"))
    return errs


def json_path_get(doc, dotted: str):
    """Fetch `a.b.0.c` out of a decoded JSON document, or raise KeyError."""
    cur = doc
    for part in dotted.split("."):
        if isinstance(cur, list):
            cur = cur[int(part)]
        else:
            cur = cur[part]
    return cur


# --------------------------------------------------------------------------
# step execution


class Session:
    """One scenario's working directory and the substitutions its steps use."""

    def __init__(self, bin_dir: Path, work: Path, scenario_dir: Path):
        self.bin_dir = bin_dir
        self.work = work
        self.scenario_dir = scenario_dir

    def expand(self, text: str) -> str:
        return (
            text.replace("{bin}", str(self.bin_dir))
            .replace("{work}", str(self.work))
            .replace("{repo}", str(REPO))
            .replace("{scenario}", str(self.scenario_dir))
        )

    def redact(self, text: str) -> str:
        """Make a transcript comparable between machines and runs."""
        return text.replace(str(self.work), "<WORK>").replace(str(REPO), "<REPO>")


def uncomment_toml_block(path: Path, marker: str) -> None:
    """Uncomment the commented-out TOML block containing `marker`.

    This is what a user does to a `hauksbee-ci init` scaffold: find the block
    they want, strip the leading `# ` from it, and leave the prose comments
    alone. Walks back to the block's opening `[[...]]` / `[...]` header and
    forward to the first blank line.
    """
    lines = path.read_text().splitlines()
    hit = next((i for i, l in enumerate(lines) if marker in l and l.lstrip().startswith("#")), None)
    if hit is None:
        raise RuntimeError(f"marker {marker!r} not found in {path.name}")
    start = hit
    while start > 0:
        stripped = re.sub(r"^\s*#\s?", "", lines[start])
        if stripped.startswith("["):
            break
        start -= 1
    else:
        raise RuntimeError(f"no table header above {marker!r} in {path.name}")
    i = start
    while i < len(lines):
        bare = lines[i].strip()
        if bare in ("", "#"):
            break
        if bare.startswith("#"):
            lines[i] = re.sub(r"^(\s*)#\s?", r"\1", lines[i])
        i += 1
    path.write_text("\n".join(lines) + "\n")


def run_step(step: dict, sess: Session, idx: int) -> StepResult:
    kind = step.get("do", "run")
    name = step.get("name") or f"step {idx}"
    res = StepResult(name=name, kind=kind)

    if kind == "write":
        target = sess.work / sess.expand(step["file"])
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(sess.expand(step["text"]))
        return res

    if kind == "copy":
        src = Path(sess.expand(step["src"]))
        if not src.is_absolute():
            src = REPO / src
        dst = sess.work / sess.expand(step["dst"])
        dst.parent.mkdir(parents=True, exist_ok=True)
        if src.is_dir():
            shutil.copytree(src, dst, dirs_exist_ok=True)
        else:
            shutil.copy2(src, dst)
        return res

    if kind == "patch":
        target = sess.work / sess.expand(step["file"])
        text = target.read_text()
        find = sess.expand(step["find"])
        if find not in text:
            res.failures.append(f"patch target {find!r} not present in {step['file']}")
            return res
        count = text.count(find) if step.get("all") else 1
        target.write_text(text.replace(find, sess.expand(step["replace"]), count))
        return res

    if kind == "uncomment":
        try:
            uncomment_toml_block(sess.work / sess.expand(step["file"]), sess.expand(step["marker"]))
        except RuntimeError as exc:
            res.failures.append(str(exc))
        return res

    if kind == "truncate":
        target = sess.work / sess.expand(step["file"])
        data = target.read_bytes()[: int(step["bytes"])]
        target.write_bytes(data)
        return res

    if kind != "run":
        res.failures.append(f"unknown step kind {kind!r}")
        return res

    argv = [sess.expand(a) for a in step["cmd"]]
    res.argv = argv
    env = dict(os.environ)
    # Stable output: no colour codes, no locale-dependent formatting, and never
    # the GitHub annotation surface (a scenario asserts on the human report).
    env.update({"NO_COLOR": "1", "CLICOLOR": "0", "TERM": "dumb", "LC_ALL": "C"})
    env.pop("GITHUB_ACTIONS", None)
    for key, value in (step.get("env") or {}).items():
        env[key] = sess.expand(str(value))

    started = time.monotonic()
    try:
        proc = subprocess.run(
            argv,
            cwd=sess.work,
            env=env,
            capture_output=True,
            text=True,
            timeout=float(step.get("timeout_seconds", 120)),
        )
        res.exit_code, res.stdout, res.stderr = proc.returncode, proc.stdout, proc.stderr
    except FileNotFoundError:
        res.seconds = time.monotonic() - started
        res.failures.append(f"binary not found: {argv[0]}")
        return res
    except subprocess.TimeoutExpired:
        res.seconds = time.monotonic() - started
        res.failures.append(f"timed out after {step.get('timeout_seconds', 120)}s")
        return res
    res.seconds = time.monotonic() - started

    check_assertions(step, res, sess)
    return res


def check_assertions(step: dict, res: StepResult, sess: Session) -> None:
    combined = res.stdout + res.stderr

    if "exit" in step:
        want = step["exit"]
        want = want if isinstance(want, list) else [want]
        if res.exit_code not in want:
            res.failures.append(f"exit code {res.exit_code}, expected {want}")

    ceiling = float(step.get("max_seconds", DEFAULT_MAX_SECONDS))
    if res.seconds > ceiling:
        res.failures.append(f"took {res.seconds:.1f}s, ceiling is {ceiling:.0f}s")

    for needle in step.get("contains", []):
        if sess.expand(needle) not in combined:
            res.failures.append(f"output does not contain {needle!r}")
    for needle in step.get("not_contains", []):
        if sess.expand(needle) in combined:
            res.failures.append(f"output must not contain {needle!r} but does")
    for pattern in step.get("matches", []):
        if not re.search(pattern, combined, re.MULTILINE):
            res.failures.append(f"output does not match /{pattern}/")
    for pattern in step.get("not_matches", []):
        found = re.search(pattern, combined, re.MULTILINE)
        if found:
            res.failures.append(f"output must not match /{pattern}/ but does: {found.group(0)!r}")

    for pattern in step.get("stdout_matches", []):
        if not re.search(pattern, res.stdout, re.MULTILINE):
            res.failures.append(f"stdout does not match /{pattern}/")

    # Assertions about files the step wrote, rather than about its output.
    for relative, patterns in (step.get("file_matches") or {}).items():
        target = sess.work / sess.expand(relative)
        if not target.exists():
            res.failures.append(f"{relative} was not written")
            continue
        body = target.read_text(errors="replace")
        for pattern in patterns:
            if not re.search(pattern, body, re.MULTILINE):
                res.failures.append(f"{relative} does not match /{pattern}/")

    if "json_file" in step or step.get("json_stdout"):
        import json as _json

        try:
            if step.get("json_stdout"):
                doc = _json.loads(res.stdout)
            else:
                doc = _json.loads((sess.work / sess.expand(step["json_file"])).read_text())
        except Exception as exc:  # noqa: BLE001 - report whatever went wrong
            res.failures.append(f"JSON did not parse: {exc}")
            return
        for key in step.get("json_required_keys", []):
            if key not in doc:
                res.failures.append(f"JSON is missing top-level key {key!r}")
        for dotted, expected in (step.get("json_equals") or {}).items():
            try:
                actual = json_path_get(doc, dotted)
            except (KeyError, IndexError, ValueError):
                res.failures.append(f"JSON has no path {dotted!r}")
                continue
            if actual != expected:
                res.failures.append(f"JSON {dotted} is {actual!r}, expected {expected!r}")
        if "json_schema" in step:
            schema_path = REPO / sess.expand(step["json_schema"])
            if not schema_path.exists():
                res.failures.append(f"schema not found at {schema_path}")
            else:
                errs = validate_subset(doc, _json.loads(schema_path.read_text()))
                res.failures.extend(f"schema: {e}" for e in errs[:10])

    if "xml_file" in step:
        path = sess.work / sess.expand(step["xml_file"])
        raw = path.read_text(errors="replace") if path.exists() else ""
        # The stdlib parser resolves external entities, and a JUnit file is
        # written by the tool under test, so it is not trusted input here. A
        # well-formed JUnit report has no DOCTYPE and no entity declarations,
        # so refusing them costs nothing and keeps the runner dependency-free.
        if "<!DOCTYPE" in raw or "<!ENTITY" in raw:
            res.failures.append("XML declares a DOCTYPE or ENTITY; a JUnit report must not")
            return
        try:
            root = ET.fromstring(raw)
        except Exception as exc:  # noqa: BLE001
            res.failures.append(f"XML is not well formed: {exc}")
            return
        for tag in step.get("xml_required_tags", []):
            if root.tag != tag and root.find(f".//{tag}") is None:
                res.failures.append(f"XML has no <{tag}> element")
        for attr, expected in (step.get("xml_root_attrs") or {}).items():
            if root.get(attr) != expected:
                res.failures.append(f"XML root @{attr} is {root.get(attr)!r}, expected {expected!r}")


# --------------------------------------------------------------------------
# scenario driving


def run_scenario(directory: Path, bin_dir: Path, work_root: Path) -> ScenarioResult:
    spec = tomllib.loads((directory / "scenario.toml").read_text())
    result = ScenarioResult(
        ident=directory.name,
        title=spec.get("title", directory.name),
        persona=spec.get("persona", ""),
        goal=spec.get("goal", ""),
    )

    for needed in spec.get("requires_paths", []):
        if not (REPO / needed).exists():
            result.skipped = f"needs {needed}, which this checkout does not have"
            return result

    work = work_root / directory.name
    work.mkdir(parents=True, exist_ok=True)
    sess = Session(bin_dir, work, directory)

    for i, step in enumerate(spec.get("step", []), start=1):
        step_result = run_step(step, sess, i)
        step_result.stdout = sess.redact(step_result.stdout)
        step_result.stderr = sess.redact(step_result.stderr)
        step_result.argv = [sess.redact(a) for a in step_result.argv]
        result.steps.append(step_result)
        if not step_result.ok and step.get("fatal", True) and step_result.kind != "run":
            result.error = "a setup step failed, so the rest of the session could not run"
            break
    return result


def write_report(results: list[ScenarioResult], out_dir: Path, bin_dir: Path) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / "report.md"
    lines = [
        "# Scenario QC report",
        "",
        f"Binaries: `{bin_dir}`",
        "",
        "| Scenario | Title | Result | Steps | Wall |",
        "| --- | --- | --- | --- | --- |",
    ]
    for r in results:
        lines.append(
            f"| {r.ident} | {r.title} | {r.status} | {sum(1 for s in r.steps if s.ok)}/{len(r.steps)} "
            f"| {r.seconds:.1f}s |"
        )
    lines += ["", "## Per-scenario transcripts", ""]
    for r in results:
        lines += [f"### {r.ident}: {r.title}", "", f"- Result: **{r.status}**"]
        # The metadata is authored as wrapped TOML strings; a markdown bullet has
        # to be one line or the list breaks.
        if r.persona:
            lines.append("- Persona: " + " ".join(r.persona.split()))
        if r.goal:
            lines.append("- Goal: " + " ".join(r.goal.split()))
        if r.skipped:
            lines += ["", f"Skipped: {r.skipped}", ""]
            continue
        if r.failures:
            lines += ["", "Failures:", ""] + [f"- {f}" for f in r.failures]
        lines.append("")
        for s in r.steps:
            mark = "ok" if s.ok else "FAILED"
            lines.append(f"#### {s.name} [{mark}]")
            if s.kind == "run":
                lines += [
                    "",
                    "```",
                    "$ " + " ".join(s.argv),
                    f"exit {s.exit_code}   ({s.seconds:.2f}s)",
                    "```",
                    "",
                ]
                for label, text in (("stdout", s.stdout), ("stderr", s.stderr)):
                    if text.strip():
                        lines += [f"{label}:", "", "```", text.rstrip(), "```", ""]
            else:
                lines += ["", f"({s.kind} step)", ""]
            for f in s.failures:
                lines.append(f"- FAILED: {f}")
            lines.append("")
    path.write_text("\n".join(lines) + "\n")
    return path


def print_table(results: list[ScenarioResult]) -> None:
    id_w = max([len(r.ident) for r in results] + [8])
    title_w = min(max([len(r.title) for r in results] + [5]), 46)
    print(f"┌─{'─' * id_w}─┬─{'─' * title_w}─┬─────────┬────────┬──────────┐")
    print(f"│ {'Scenario'.ljust(id_w)} │ {'Title'.ljust(title_w)} │ Result  │ Steps  │ Wall     │")
    print(f"├─{'─' * id_w}─┼─{'─' * title_w}─┼─────────┼────────┼──────────┤")
    for r in results:
        title = r.title if len(r.title) <= title_w else r.title[: title_w - 1] + "…"
        steps = f"{sum(1 for s in r.steps if s.ok)}/{len(r.steps)}"
        print(
            f"│ {r.ident.ljust(id_w)} │ {title.ljust(title_w)} │ {r.status.ljust(7)} │ "
            f"{steps.ljust(6)} │ {f'{r.seconds:.1f}s'.rjust(8)} │"
        )
    print(f"└─{'─' * id_w}─┴─{'─' * title_w}─┴─────────┴────────┴──────────┘")


def main() -> int:
    parser = argparse.ArgumentParser(description="Run the hauksbee scenario QC suite.")
    parser.add_argument(
        "--bin-dir",
        default="target/release",
        help="directory holding the hauksbee binaries (default: target/release)",
    )
    parser.add_argument("--scenario", help="only scenarios whose directory name contains this")
    parser.add_argument(
        "--results-dir",
        help="where to write the timestamped report (default: qc/results)",
    )
    parser.add_argument("--list", action="store_true", help="list the scenarios and exit")
    args = parser.parse_args()

    bin_dir = Path(args.bin_dir)
    if not bin_dir.is_absolute():
        bin_dir = (REPO / bin_dir).resolve()

    directories = sorted(d for d in (QC_DIR / "scenarios").iterdir() if (d / "scenario.toml").exists())
    if args.scenario:
        directories = [d for d in directories if args.scenario in d.name]
    if not directories:
        print("no scenarios matched", file=sys.stderr)
        return 2

    if args.list:
        for d in directories:
            spec = tomllib.loads((d / "scenario.toml").read_text())
            print(f"{d.name}  {spec.get('title', '')}")
        return 0

    if not bin_dir.is_dir():
        print(f"error: no binary directory at {bin_dir}", file=sys.stderr)
        print("build first:  cargo build --release", file=sys.stderr)
        return 2
    for needed in ("hauksbee", "hauksbee-ci"):
        if not (bin_dir / needed).exists():
            print(f"error: {bin_dir / needed} is missing; run cargo build --release", file=sys.stderr)
            return 2

    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    results_root = Path(args.results_dir) if args.results_dir else QC_DIR / "results"
    out_dir = results_root / stamp

    results = []
    for d in directories:
        print(f"... {d.name}", flush=True)
        results.append(run_scenario(d, bin_dir, out_dir / "work"))

    report = write_report(results, out_dir, bin_dir)
    print()
    print_table(results)
    print()
    print(f"report: {report}")

    failed = [r for r in results if r.status in ("FAIL", "ERROR")]
    if failed:
        print()
        for r in failed:
            print(f"{r.ident}:")
            for f in r.failures:
                print(f"  - {f}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
