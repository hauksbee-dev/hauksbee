"""Core logic for the hauksbee-ci KiCad plugin, kept free of any pcbnew or wx
imports so it can be unit-tested with plain python.

The plugin is deliberately thin: it shells out to the `hauksbee-ci` binary on
the currently open board with a chosen spec, then parses the JUnit XML the
binary writes. All the simulation lives in the Rust runner; this module only
builds the command, runs it, and turns the results into something a dialog can
show.
"""

from __future__ import annotations

import filecmp
import os
import shutil
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from typing import List, Optional


@dataclass
class AssertionResult:
    """One assertion from the JUnit report."""

    name: str
    classname: str
    passed: bool
    detail: str


@dataclass
class CiRun:
    """The parsed outcome of a hauksbee-ci invocation."""

    passed: bool
    results: List[AssertionResult] = field(default_factory=list)
    # Raw stdout/stderr, for the dialog's detail pane and debugging.
    stdout: str = ""
    stderr: str = ""
    exit_code: int = 0
    error: Optional[str] = None

    @property
    def pass_count(self) -> int:
        return sum(1 for r in self.results if r.passed)

    @property
    def total(self) -> int:
        return len(self.results)

    def summary(self) -> str:
        if self.error:
            return f"hauksbee-ci could not run: {self.error}"
        verdict = "GREEN" if self.passed else "RED"
        return f"{self.pass_count}/{self.total} assertions passed - {verdict}"


def find_specs(*search_dirs: str) -> List[str]:
    """All ``*.toml`` hauksbee-ci specs found in the given directories.

    File-type-agnostic on purpose: a spec's ``board`` may be a ``.kicad_pcb``
    (layout-stage CI) or a ``.kicad_sch`` (schematic-stage CI). Discovery only
    looks at the spec files, never the board, so the same logic serves the
    pcbnew action plugin, a schematic-stage pre-commit hook, and any CLI driver.
    Directories that do not exist are skipped; results are sorted and de-duped.
    """
    seen = set()
    out: List[str] = []
    for d in search_dirs:
        if not d or not os.path.isdir(d):
            continue
        for name in sorted(os.listdir(d)):
            if name.endswith(".toml"):
                full = os.path.join(d, name)
                if full not in seen:
                    seen.add(full)
                    out.append(full)
    return out


def spec_board(spec_path: str) -> Optional[str]:
    """The raw ``board = "..."`` value declared in a spec, or None.

    A deliberately tiny TOML reader (no dependency): it scans for the first
    top-level ``board`` key. Used to tell schematic-stage specs (``.kicad_sch``)
    from layout-stage specs (``.kicad_pcb``) without binding to either file type.
    """
    try:
        with open(spec_path, "r", encoding="utf-8") as fh:
            for line in fh:
                stripped = line.strip()
                if stripped.startswith("#") or "=" not in stripped:
                    continue
                key, _, rhs = stripped.partition("=")
                # Exact key match: `board`, not `board_rev` / `board_notes`.
                if key.strip() != "board":
                    continue
                rhs = rhs.strip()
                # Strip an inline comment, then surrounding quotes.
                if "#" in rhs:
                    rhs = rhs.split("#", 1)[0].strip()
                return rhs.strip('"').strip("'")
    except OSError:
        return None
    return None


def spec_targets_schematic(spec_path: str) -> bool:
    """True when a spec's board is a `.kicad_sch` (schematic-stage CI)."""
    board = spec_board(spec_path)
    return bool(board) and board.lower().endswith(".kicad_sch")


def _prebuilt_candidates() -> List[str]:
    """Likely locations of a prebuilt / locally-built hauksbee-ci binary.

    Covers the two ways a user gets a binary without compiling on the spot:
    an unpacked release bundle (``bin/hauksbee-ci`` next to where this plugin or
    a sibling ``hauksbee`` checkout lives), and a prior ``cargo build --release``
    in a nearby workspace (``target/release/hauksbee-ci``). Order is most- to
    least-specific; non-existent paths are skipped by the caller.
    """
    here = os.path.dirname(os.path.abspath(__file__))
    # integrations/kicad-plugin -> repo root is two levels up.
    repo_root = os.path.normpath(os.path.join(here, "..", ".."))
    home = os.path.expanduser("~")
    rel = [
        # Unpacked release bundle layouts.
        os.path.join(repo_root, "bin", "hauksbee-ci"),
        os.path.join(here, "bin", "hauksbee-ci"),
        os.path.join(home, ".hauksbee", "bin", "hauksbee-ci"),
        # A local cargo build of the workspace.
        os.path.join(repo_root, "target", "release", "hauksbee-ci"),
    ]
    return rel


def _checkout_build() -> Optional[str]:
    """The enclosing checkout's ``target/release/hauksbee-ci``, when this file
    lives inside a hauksbee source tree and that build exists."""
    here = os.path.dirname(os.path.abspath(__file__))
    repo_root = os.path.normpath(os.path.join(here, "..", ".."))
    if not os.path.isfile(os.path.join(repo_root, "Cargo.toml")):
        return None
    build = os.path.join(repo_root, "target", "release", "hauksbee-ci")
    if os.path.isfile(build) and os.access(build, os.X_OK):
        return build
    return None


def _binaries_differ(a: str, b: str) -> bool:
    """True when two candidate binaries are different files by content."""
    try:
        return not filecmp.cmp(a, b, shallow=False)
    except OSError:
        return True


def find_binary(explicit: Optional[str] = None) -> Optional[str]:
    """Locate the hauksbee-ci binary, preferring a ready-to-run one.

    Order: an explicit path, the HAUKSBEE_CI_BIN env var, then, when this
    plugin lives inside a hauksbee checkout, that checkout's
    ``target/release`` build, then PATH, then a prebuilt release bundle.
    The checkout build outranks PATH because it is the binary this working
    tree just produced; an installed copy on PATH can lag it by weeks. When
    both exist and differ, a warning says which one runs and where the other
    lives (the same contract scripts/ci.sh has).
    """
    # An explicit override is authoritative. Falling through to a checkout or
    # PATH binary when it is misspelled can run a different build than the one
    # the user deliberately selected, which is especially unsafe in a commit
    # gate. Treat an invalid override as unavailable and let the caller fail
    # closed with its normal remediation message.
    if explicit is not None:
        return (
            explicit
            if os.path.isfile(explicit) and os.access(explicit, os.X_OK)
            else None
        )
    configured = os.environ.get("HAUKSBEE_CI_BIN")
    if configured is not None:
        return (
            configured
            if os.path.isfile(configured) and os.access(configured, os.X_OK)
            else None
        )
    on_path = shutil.which("hauksbee-ci")
    checkout_build = _checkout_build()
    if checkout_build:
        if on_path and _binaries_differ(checkout_build, on_path):
            print(
                "hauksbee-ci: using the checkout build %s; the installed %s "
                "differs (re-run scripts/install.sh to refresh it)."
                % (checkout_build, on_path),
                file=sys.stderr,
            )
        return checkout_build
    if on_path:
        return on_path
    for c in _prebuilt_candidates():
        if os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    return None


def ensure_binary(
    explicit: Optional[str] = None,
    build: bool = False,
    runner=subprocess.run,
) -> Optional[str]:
    """Return a usable hauksbee-ci binary, optionally building one as a fallback.

    First tries :func:`find_binary` (prebuilt / PATH / local build). If nothing
    is found and ``build`` is True and cargo is available, runs
    ``cargo build --release -p hauksbee-ci`` in the workspace and returns the
    freshly built binary. Returns None if no binary could be obtained.

    The build is the explicit, opt-in fallback so a prebuilt binary is always
    preferred and a user is never forced to compile silently.
    """
    found = find_binary(explicit)
    if found:
        return found
    if not build:
        return None
    cargo = shutil.which("cargo")
    if not cargo:
        return None
    here = os.path.dirname(os.path.abspath(__file__))
    repo_root = os.path.normpath(os.path.join(here, "..", ".."))
    manifest = os.path.join(repo_root, "Cargo.toml")
    if not os.path.isfile(manifest):
        return None
    try:
        proc = runner(
            [cargo, "build", "--release", "-p", "hauksbee-ci", "--manifest-path", manifest],
            capture_output=True,
            text=True,
        )
    except OSError:
        return None
    if getattr(proc, "returncode", 1) != 0:
        return None
    built = os.path.join(repo_root, "target", "release", "hauksbee-ci")
    if os.path.isfile(built) and os.access(built, os.X_OK):
        return built
    return None


def build_command(binary: str, spec: str, junit_path: str) -> List[str]:
    """The argv hauksbee-ci is invoked with."""
    return [binary, "run", spec, "--junit", junit_path]


def _safe_fromstring(xml_text: str) -> "ET.Element":
    """Parse XML defensively. hauksbee-ci writes this file itself (trusted, no
    DOCTYPE), but we still refuse any DTD / external entities so a tampered
    results file cannot trigger XXE or billion-laughs. Prefer defusedxml when
    it is available in the KiCad Python environment, else harden the stdlib
    parser by rejecting DOCTYPE declarations outright.
    """
    try:
        import defusedxml.ElementTree as DET  # type: ignore

        return DET.fromstring(xml_text)
    except ImportError:
        # No entity expansion happens without a DOCTYPE; reject any to be safe.
        lowered = xml_text.lstrip()
        if "<!DOCTYPE" in xml_text or "<!ENTITY" in xml_text:
            raise ET.ParseError("refusing XML with a DOCTYPE/ENTITY declaration")
        _ = lowered
        return ET.fromstring(xml_text)


def parse_junit(xml_text: str) -> List[AssertionResult]:
    """Parse hauksbee-ci's JUnit XML into assertion results.

    Each `<testcase>` is one assertion; a child `<failure>` means it failed and
    carries the detail, otherwise `<system-out>` carries the passing detail.
    """
    results: List[AssertionResult] = []
    root = _safe_fromstring(xml_text)
    # <testsuites> wraps one <testsuite> wrapping <testcase>s.
    for testcase in root.iter("testcase"):
        name = testcase.get("name", "")
        classname = testcase.get("classname", "")
        failure = testcase.find("failure")
        if failure is not None:
            detail = (failure.get("message") or failure.text or "").strip()
            results.append(
                AssertionResult(name=name, classname=classname, passed=False, detail=detail)
            )
        else:
            sysout = testcase.find("system-out")
            detail = (sysout.text or "").strip() if sysout is not None else ""
            results.append(
                AssertionResult(name=name, classname=classname, passed=True, detail=detail)
            )
    return results


def run_ci(
    spec: str,
    binary: Optional[str] = None,
    cwd: Optional[str] = None,
    runner=subprocess.run,
) -> CiRun:
    """Run hauksbee-ci on `spec` and parse its results.

    `runner` is injectable so tests can run without a real binary or board.
    """
    # An explicitly supplied binary is trusted as-is (callers and tests may
    # point at a path that is resolved differently, e.g. via a mocked runner);
    # otherwise discover it from the env / PATH and require it to exist.
    bin_path = binary if binary else find_binary(None)
    if not bin_path:
        return CiRun(
            passed=False,
            error=(
                "hauksbee-ci binary not found. Build it with "
                "`cargo build --release -p hauksbee-ci` and put it on PATH or set "
                "HAUKSBEE_CI_BIN."
            ),
        )

    junit_fd, junit_path = tempfile.mkstemp(prefix="hauksbee-ci-", suffix=".xml")
    os.close(junit_fd)
    try:
        cmd = build_command(bin_path, spec, junit_path)
        proc = runner(
            cmd,
            cwd=cwd,
            capture_output=True,
            text=True,
        )
        stdout = proc.stdout or ""
        stderr = proc.stderr or ""
        code = proc.returncode

        # A spec/board error (exit 2) writes no JUnit; surface stderr.
        results: List[AssertionResult] = []
        error: Optional[str] = None
        try:
            with open(junit_path, "r", encoding="utf-8") as fh:
                xml_text = fh.read()
            if xml_text.strip():
                results = parse_junit(xml_text)
        except (OSError, ET.ParseError):
            results = []

        if not results and code != 0:
            error = stderr.strip() or stdout.strip() or f"hauksbee-ci exited {code}"

        return CiRun(
            passed=(code == 0),
            results=results,
            stdout=stdout,
            stderr=stderr,
            exit_code=code,
            error=error,
        )
    finally:
        try:
            os.remove(junit_path)
        except OSError:
            pass


def format_report(run: CiRun) -> str:
    """A plain-text report for a dialog's text area."""
    lines = [run.summary(), ""]
    if run.error and not run.results:
        # `summary()` already reads "hauksbee-ci could not run: <error>", so do
        # not append the raw error a second time (the wrapper-plus-raw double
        # print the CI-owner persona flagged). The summary line carries it once.
        return run.summary()
    for r in run.results:
        mark = "PASS" if r.passed else "FAIL"
        lines.append(f"[{mark}] {r.name}")
        if r.detail:
            lines.append(f"       {r.detail}")
    return "\n".join(lines)
