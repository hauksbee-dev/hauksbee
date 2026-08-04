#!/usr/bin/env python3
"""hauksbee-ci pre-commit hook: run hardware checks before a commit lands.

This is arguably the *most natural* schematic-stage integration. eeschema has
no in-editor plugin API yet (KiCad 9/10 expose the IPC API for the PCB editor
only; headless support arrives with kicad-cli in KiCad 11), so the place to
catch a schematic-level fault before it ships is the commit, not the editor.

What it does, on the files staged for commit:
  * find every hauksbee-ci spec under the configured spec directories,
  * run only the specs whose board is a staged `.kicad_sch` / `.kicad_pcb`
    (so editing one page does not re-run unrelated checks),
  * fail the commit (non-zero exit) if any assertion is RED.

It reuses the file-type-agnostic core in ../kicad-plugin/hauksbee_ci_core.py, so
schematic-stage and layout-stage specs run through exactly the same path.

Wire it up with the `pre-commit` framework (.pre-commit-config.yaml in this
directory) or as a plain git hook:

    ln -s ../../integrations/pre-commit/hauksbee_ci_precommit.py \
          .git/hooks/pre-commit

Environment:
  HAUKSBEE_CI_BIN   path to the hauksbee-ci binary (else taken from PATH)
  HAUKSBEE_CI_SPECS colon-separated spec directories to search
                   (default: "ci:.")
  HAUKSBEE_CI_HOOK_OPTIONAL
                   set to 1 to SKIP the check when the binary is absent instead
                   of blocking the commit. Without it a missing binary is a
                   blocked commit, because a gate that passes for want of the
                   tool is green forever on a fresh clone.
"""

from __future__ import annotations

import os
import subprocess
import sys

# Make the shared, pcbnew-free core importable whether this runs from the repo
# root or from .git/hooks. Use realpath, not abspath: the README-documented
# install symlinks this file into `.git/hooks/pre-commit`, and abspath does not
# follow the symlink, so `_HERE` would resolve to `.git/hooks` and the sibling
# `../kicad-plugin/hauksbee_ci_core.py` would be missing (ModuleNotFoundError).
# realpath resolves the link back to the real integrations/pre-commit directory.
_HERE = os.path.dirname(os.path.realpath(__file__))
sys.path.insert(0, os.path.join(_HERE, "..", "kicad-plugin"))

import hauksbee_ci_core as core  # noqa: E402


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


def invoked_by_hand() -> bool:
    """True when a person ran this directly rather than git running it as a hook.

    Git exports these into a hook's environment; a shell does not. The
    distinction decides whether "nothing to do" is worth saying out loud: on
    every commit it would be noise, and to someone testing their setup silence
    is the whole problem.
    """
    return not any(
        os.environ.get(v) for v in ("GIT_INDEX_FILE", "GIT_DIR", "GIT_AUTHOR_DATE")
    )


def main() -> int:
    # Anyone setting a hook up runs it by hand once to see whether it works, and
    # typing --help is how they ask. It used to be swallowed: the hook found no
    # staged board, printed nothing, and exited 0, which is indistinguishable
    # from a hook that does not work.
    if any(a in ("-h", "--help") for a in sys.argv[1:]):
        print(__doc__.strip())
        return 0
    if len(sys.argv) > 1:
        print(
            f"hauksbee-ci: unexpected argument {sys.argv[1]!r}. This hook takes "
            "no arguments; it reads the staged files and the environment. "
            "Run it with --help for the details.",
            file=sys.stderr,
        )
        return 2

    spec_dirs = os.environ.get("HAUKSBEE_CI_SPECS", "ci:.").split(":")
    root = git_root()
    searched = [os.path.join(root, d) for d in spec_dirs]
    specs = core.find_specs(*searched)
    if not specs:
        # No specs configured: nothing to do, do not block the commit. Silence is
        # right during an actual commit and wrong when someone ran the hook by
        # hand to check their setup, so only the second case gets an answer.
        if invoked_by_hand():
            print(
                "hauksbee-ci: no spec found, so nothing would run on a commit.\n"
                "  looked in: " + ", ".join(searched) + "\n"
                "  scaffold one with `hauksbee-ci init <board>`, put it in `ci/`,\n"
                "  or point HAUKSBEE_CI_SPECS at where yours lives."
            )
        return 0

    staged = set(staged_files())
    binary = core.find_binary(os.environ.get("HAUKSBEE_CI_BIN"))
    if not binary:
        # A gate that passes because the tool is absent is green forever on a
        # fresh clone, which is worse than no gate: the repo looks checked. Block
        # by default and name the opt-out, the same contract (and the same
        # variable) the plain `.git/hooks/pre-commit` this tool installs uses.
        if os.environ.get("HAUKSBEE_CI_HOOK_OPTIONAL") == "1":
            print(
                "hauksbee-ci: binary not found; HAUKSBEE_CI_HOOK_OPTIONAL=1, "
                "skipping the hardware check.",
                file=sys.stderr,
            )
            return 0
        print(
            "hauksbee-ci: binary not found, so the hardware check did NOT run; "
            "commit blocked.\n"
            "  install it and set HAUKSBEE_CI_BIN or put it on PATH,\n"
            "  or set HAUKSBEE_CI_HOOK_OPTIONAL=1 to skip the check when it is "
            "absent,\n"
            "  or commit with --no-verify to override once.",
            file=sys.stderr,
        )
        return 1

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
        print(f"hauksbee-ci [{stage}]: {os.path.relpath(spec, root)}")
        run = core.run_ci(spec, binary=binary, cwd=os.path.dirname(spec))
        print(core.format_report(run))
        print()
        if not run.passed:
            failed = True

    if failed:
        # The reports went to stdout and this line goes to stderr, which is
        # block-buffered and line-buffered respectively. Without this flush the
        # user reads "commit blocked" before the report that explains why, and
        # in a terminal the two interleave unpredictably.
        sys.stdout.flush()
        print("hauksbee-ci: hardware check RED - commit blocked.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
