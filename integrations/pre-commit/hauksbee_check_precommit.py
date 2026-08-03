#!/usr/bin/env python3
"""hauksbee-check pre-commit hook: the zero-config board gate.

Where `hauksbee-ci` (the sibling hook in this directory) discovers checked-in
spec files and runs full co-simulation specs, this hook needs no spec at all:
pre-commit hands it the staged board files and it runs

    hauksbee run <board> --check --strict

on each one. `--check` is the single-command report (DRC, ERC-grade netlist
findings, and the rest of the check union); `--strict` turns gate-grade
findings into a non-zero exit, so a serious finding blocks the commit.

Exit codes are passed straight through from the tool:
  0  every staged board is clean
  2  gate-grade findings on at least one board (commit blocked)
  3  a board was invalid for analysis (the analog solve aborted, so the
     result is not trustworthy; the commit is blocked rather than waved past)

With several staged boards the hook exits with the worst code seen.

Environment:
  HAUKSBEE_BIN  path to the hauksbee binary (else taken from PATH, then from
                a nearby release bundle or `target/release` build)
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys


def find_hauksbee(explicit: str | None = None) -> str | None:
    """Locate the hauksbee binary, preferring a ready-to-run one.

    Order: an explicit path, the HAUKSBEE_BIN env var, then PATH, then an
    unpacked release bundle or a local ``cargo build --release`` in the
    workspace this script lives in. Mirrors ``find_binary`` in
    ``../kicad-plugin/hauksbee_ci_core.py``, for the ``hauksbee`` binary
    instead of ``hauksbee-ci``.
    """
    for c in (explicit, os.environ.get("HAUKSBEE_BIN")):
        if c and os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    on_path = shutil.which("hauksbee")
    if on_path:
        return on_path
    here = os.path.dirname(os.path.realpath(__file__))
    # integrations/pre-commit -> repo root is two levels up.
    repo_root = os.path.normpath(os.path.join(here, "..", ".."))
    for c in (
        os.path.join(repo_root, "bin", "hauksbee"),
        os.path.join(os.path.expanduser("~"), ".hauksbee", "bin", "hauksbee"),
        os.path.join(repo_root, "target", "release", "hauksbee"),
    ):
        if os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    return None


def main(argv: list[str] | None = None, runner=subprocess.run) -> int:
    """Run the gate on the files pre-commit passed as arguments.

    `runner` is injectable so tests can exercise the exit-code handling
    without a real binary or board (same convention as core.run_ci).
    """
    args = sys.argv[1:] if argv is None else list(argv)
    if any(a in ("-h", "--help") for a in args):
        print(__doc__.strip())
        return 0
    flags = [a for a in args if a.startswith("-")]
    if flags:
        print(
            f"hauksbee-check: unexpected option {flags[0]!r}. This hook takes "
            "only the board files pre-commit passes it; configuration is via "
            "the environment. Run it with --help for the details.",
            file=sys.stderr,
        )
        return 2
    if not args:
        # pre-commit only invokes a hook when its `files` filter matched, so an
        # empty argv means someone ran it by hand; say so instead of silence.
        print("hauksbee-check: no board files given, nothing to do.")
        return 0

    binary = find_hauksbee()
    if not binary:
        print(
            "hauksbee-check: `hauksbee` binary not found. Install a release "
            "(or `cargo build --release -p hauksbee-engine`) and put it on "
            "PATH or set HAUKSBEE_BIN.",
            file=sys.stderr,
        )
        return 1

    worst = 0
    for board in args:
        # Let the tool's own report stream to the terminal; the hook only
        # frames it and carries the exit code.
        proc = runner([binary, "run", board, "--check", "--strict"])
        code = proc.returncode
        if code < 0:
            # Killed by a signal. max() would let a crash read as clean, so
            # turn it into a plain failure and say what happened.
            print(
                f"hauksbee-check: {board}: hauksbee crashed (signal {-code}).",
                file=sys.stderr,
            )
            code = 1
        if code == 3:
            print(
                f"hauksbee-check: {board}: invalid for analysis (exit 3). The "
                "analog solve aborted, so this result cannot be trusted; "
                "fix the board or spec rather than waving it through.",
                file=sys.stderr,
            )
        worst = max(worst, code)

    if worst:
        # Reports went to stdout (line-buffered through the child); flush ours
        # so the verdict lands after the findings that explain it.
        sys.stdout.flush()
        print("hauksbee-check: board check RED - commit blocked.", file=sys.stderr)
    return worst


if __name__ == "__main__":
    sys.exit(main())
