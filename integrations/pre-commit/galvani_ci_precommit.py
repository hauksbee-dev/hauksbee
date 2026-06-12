#!/usr/bin/env python3
"""galvani-ci pre-commit hook: run hardware checks before a commit lands.

This is arguably the *most natural* schematic-stage integration. eeschema has
no in-editor plugin API yet (KiCad 9/10 expose the IPC API for the PCB editor
only; headless support arrives with kicad-cli in KiCad 11), so the place to
catch a schematic-level fault before it ships is the commit, not the editor.

What it does, on the files staged for commit:
  * find every galvani-ci spec under the configured spec directories,
  * run only the specs whose board is a staged `.kicad_sch` / `.kicad_pcb`
    (so editing one page does not re-run unrelated checks),
  * fail the commit (non-zero exit) if any assertion is RED.

It reuses the file-type-agnostic core in ../kicad-plugin/galvani_ci_core.py, so
schematic-stage and layout-stage specs run through exactly the same path.

Wire it up with the `pre-commit` framework (.pre-commit-config.yaml in this
directory) or as a plain git hook:

    ln -s ../../integrations/pre-commit/galvani_ci_precommit.py \
          .git/hooks/pre-commit

Environment:
  GALVANI_CI_BIN   path to the galvani-ci binary (else taken from PATH)
  GALVANI_CI_SPECS colon-separated spec directories to search
                   (default: "ci:.")
"""

from __future__ import annotations

import os
import subprocess
import sys

# Make the shared, pcbnew-free core importable whether this runs from the repo
# root or from .git/hooks.
_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(_HERE, "..", "kicad-plugin"))

import galvani_ci_core as core  # noqa: E402


def staged_files() -> list[str]:
    """Absolute paths of files staged for this commit (added/copied/modified)."""
    try:
        out = subprocess.check_output(
            ["git", "diff", "--cached", "--name-only", "--diff-filter=ACM"],
            text=True,
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        return []
    root = git_root()
    files = []
    for line in out.splitlines():
        line = line.strip()
        if line:
            files.append(os.path.normpath(os.path.join(root, line)))
    return files


def git_root() -> str:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return os.getcwd()


def resolve_board(spec_path: str) -> str | None:
    """Absolute path of a spec's board, resolved relative to the spec file."""
    board = core.spec_board(spec_path)
    if not board:
        return None
    if os.path.isabs(board):
        return os.path.normpath(board)
    return os.path.normpath(os.path.join(os.path.dirname(spec_path), board))


def main() -> int:
    spec_dirs = os.environ.get("GALVANI_CI_SPECS", "ci:.").split(":")
    root = git_root()
    spec_dirs = [os.path.join(root, d) for d in spec_dirs]
    specs = core.find_specs(*spec_dirs)
    if not specs:
        # No specs configured: nothing to do, do not block the commit.
        return 0

    staged = set(staged_files())
    binary = core.find_binary(os.environ.get("GALVANI_CI_BIN"))
    if not binary:
        print(
            "galvani-ci: binary not found (build it and set GALVANI_CI_BIN or "
            "put it on PATH). Skipping hardware checks.",
            file=sys.stderr,
        )
        # A missing binary should not silently pass a broken board, but blocking
        # every commit on a tooling gap is worse; warn and let it through.
        return 0

    # Run only specs whose board was actually touched by this commit.
    to_run = []
    for spec in specs:
        board = resolve_board(spec)
        if board and board in staged:
            to_run.append((spec, board))

    if not to_run:
        return 0

    failed = False
    for spec, board in to_run:
        stage = "schematic" if board.lower().endswith(".kicad_sch") else "layout"
        print(f"galvani-ci [{stage}]: {os.path.relpath(spec, root)}")
        run = core.run_ci(spec, binary=binary, cwd=os.path.dirname(spec))
        print(core.format_report(run))
        print()
        if not run.passed:
            failed = True

    if failed:
        print("galvani-ci: hardware check RED - commit blocked.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
