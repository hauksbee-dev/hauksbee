"""Tests for the pre-commit hook. Run with:

    python3 -m pytest integrations/pre-commit/test_hauksbee_ci_precommit.py
    # or, with no pytest:
    python3 integrations/pre-commit/test_hauksbee_ci_precommit.py

These cover the argument handling and the by-hand detection, which is all the
logic that does not need a git repo and a built binary. The end-to-end path
(stage a board, watch a RED spec block the commit) is exercised by hand against
a real board; what is tested here is everything that used to be silent.
"""

import os
import subprocess
import sys
import tempfile

_HERE = os.path.dirname(os.path.realpath(__file__))
_HOOK = os.path.join(_HERE, "hauksbee_ci_precommit.py")

sys.path.insert(0, _HERE)

import hauksbee_ci_precommit as hook  # noqa: E402


def run_hook(args=(), env=None, cwd=None):
    e = dict(os.environ)
    # Start from a shell-like environment: drop anything git would have set, so
    # a test inherits nothing from the harness that ran it.
    for v in ("GIT_INDEX_FILE", "GIT_DIR", "GIT_AUTHOR_DATE"):
        e.pop(v, None)
    e.update(env or {})
    return subprocess.run(
        [sys.executable, _HOOK, *args],
        capture_output=True,
        text=True,
        env=e,
        cwd=cwd,
    )


def test_help_prints_something():
    # It used to print nothing and exit 0, which is what a broken hook looks
    # like to the person who just installed it.
    r = run_hook(["--help"])
    assert r.returncode == 0, r.stderr
    assert "pre-commit hook" in r.stdout
    assert "HAUKSBEE_CI_SPECS" in r.stdout, "the env vars are the whole configuration"
    assert len(r.stdout.splitlines()) > 10, "a usage message, not a one-liner"


def test_short_help_works_too():
    assert "pre-commit hook" in run_hook(["-h"]).stdout


def test_an_unknown_argument_is_refused_not_ignored():
    r = run_hook(["--jsno"])
    assert r.returncode == 2, "a typo must not read as a clean run"
    assert "--jsno" in r.stderr, "and must name what it did not understand"


def test_running_it_by_hand_with_no_spec_explains_itself():
    with tempfile.TemporaryDirectory() as d:
        subprocess.run(["git", "init", "-q", d], check=True)
        r = run_hook(cwd=d)
        assert r.returncode == 0, "no spec must never block a commit"
        assert "no spec found" in r.stdout
        assert "looked in" in r.stdout, "say where, or the user cannot act on it"


def test_a_real_commit_with_no_spec_stays_silent():
    # The same case during an actual commit. Git exports these; a shell does not.
    with tempfile.TemporaryDirectory() as d:
        subprocess.run(["git", "init", "-q", d], check=True)
        r = run_hook(cwd=d, env={"GIT_DIR": os.path.join(d, ".git")})
        assert r.returncode == 0
        assert r.stdout.strip() == "", "a hook that talks on every commit gets uninstalled"


def test_by_hand_detection():
    saved = {v: os.environ.pop(v, None) for v in ("GIT_INDEX_FILE", "GIT_DIR", "GIT_AUTHOR_DATE")}
    try:
        assert hook.invoked_by_hand()
        os.environ["GIT_DIR"] = "/somewhere/.git"
        assert not hook.invoked_by_hand()
    finally:
        os.environ.pop("GIT_DIR", None)
        for k, v in saved.items():
            if v is not None:
                os.environ[k] = v


def test_no_em_dashes_in_anything_a_user_reads():
    # A house rule, and the kind that only holds if something checks it.
    for name in ("hauksbee_ci_precommit.py",):
        text = open(os.path.join(_HERE, name), encoding="utf-8").read()
        assert "—" not in text, f"{name} contains an em dash"
    core = os.path.join(_HERE, "..", "kicad-plugin", "hauksbee_ci_core.py")
    assert "—" not in open(core, encoding="utf-8").read()


if __name__ == "__main__":
    failed = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"ok   {name}")
            except AssertionError as e:
                failed += 1
                print(f"FAIL {name}: {e}")
    print()
    print("all pre-commit hook tests passed" if not failed else f"{failed} failed")
    sys.exit(1 if failed else 0)
