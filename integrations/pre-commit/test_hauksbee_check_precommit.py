"""Tests for the hauksbee-check hook. Run with:

    python3 -m pytest integrations/pre-commit/test_hauksbee_check_precommit.py
    # or, with no pytest:
    python3 integrations/pre-commit/test_hauksbee_check_precommit.py

These cover everything the hook itself decides: argument handling, the
missing-binary error, and exit-code passthrough (0 clean, 2 findings, 3
invalid-for-analysis, all of which must reach pre-commit unchanged). The
file filtering is not tested here because the hook does none: pre-commit's
`files` pattern in ../../.pre-commit-hooks.yaml selects the staged boards
and passes them as arguments.
"""

import contextlib
import io
import os
import re
import stat
import subprocess
import sys
import tempfile

_HERE = os.path.dirname(os.path.realpath(__file__))
_HOOK = os.path.join(_HERE, "hauksbee_check_precommit.py")

sys.path.insert(0, _HERE)

import hauksbee_check_precommit as hook  # noqa: E402


class _Proc:
    def __init__(self, returncode):
        self.returncode = returncode


def _quiet_main(argv, runner=None):
    """Run hook.main with stdout/stderr captured, returning (code, out, err)."""
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        if runner is None:
            code = hook.main(argv)
        else:
            saved = hook.find_hauksbee
            hook.find_hauksbee = lambda explicit=None: "hauksbee"
            try:
                code = hook.main(argv, runner=runner)
            finally:
                hook.find_hauksbee = saved
    return code, out.getvalue(), err.getvalue()


def _stub_binary(directory, exit_code):
    """Drop a fake `hauksbee` into `directory` that logs its argv and exits."""
    path = os.path.join(directory, "hauksbee")
    log = os.path.join(directory, "argv.log")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write('#!/bin/sh\necho "$@" >> "%s"\nexit %d\n' % (log, exit_code))
    os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path, log


def run_hook(args=(), env=None):
    e = dict(os.environ)
    e.pop("HAUKSBEE_BIN", None)
    e.update(env or {})
    return subprocess.run(
        [sys.executable, _HOOK, *args],
        capture_output=True,
        text=True,
        env=e,
    )


def test_help_prints_something():
    r = run_hook(["--help"])
    assert r.returncode == 0, r.stderr
    assert "pre-commit hook" in r.stdout
    assert "HAUKSBEE_BIN" in r.stdout, "the env var is the whole configuration"
    assert "--check --strict" in r.stdout, "say exactly what command runs"


def test_an_unknown_option_is_refused_not_ignored():
    r = run_hook(["--jsno", "board.kicad_pcb"])
    assert r.returncode == 2, "a typo must not read as a clean run"
    assert "--jsno" in r.stderr, "and must name what it did not understand"


def test_no_files_is_a_clean_noop_that_says_so():
    r = run_hook()
    assert r.returncode == 0
    assert "nothing to do" in r.stdout, "silence looks like a broken hook"


def test_a_missing_binary_fails_with_a_clear_error():
    # In-process, because on a dev machine the PATH/target-release fallbacks
    # would find a real binary and the case could never be exercised.
    saved = hook.find_hauksbee
    hook.find_hauksbee = lambda explicit=None: None
    try:
        code, _, err = _quiet_main(["board.kicad_pcb"])
    finally:
        hook.find_hauksbee = saved
    assert code == 1, "a gate with no tool must fail, not silently pass"
    assert "not found" in err
    assert "HAUKSBEE_BIN" in err, "tell the user how to fix it"


def test_exit_codes_pass_through_via_a_real_subprocess():
    with tempfile.TemporaryDirectory() as d:
        for expected in (0, 2, 3):
            _stub_binary(d, expected)
            r = run_hook(
                ["board.kicad_pcb"],
                env={"PATH": d},
            )
            assert r.returncode == expected, (expected, r.stderr)


def test_the_exact_command_line_is_run_per_file():
    with tempfile.TemporaryDirectory() as d:
        _, log = _stub_binary(d, 0)
        r = run_hook(["a.kicad_sch", "b.kicad_pcb"], env={"PATH": d})
        assert r.returncode == 0, r.stderr
        lines = open(log, encoding="utf-8").read().splitlines()
        assert lines == [
            "run a.kicad_sch --check --strict",
            "run b.kicad_pcb --check --strict",
        ]


def test_exit_3_names_the_board_and_explains_invalid_for_analysis():
    with tempfile.TemporaryDirectory() as d:
        _stub_binary(d, 3)
        r = run_hook(["flaky.kicad_pcb"], env={"PATH": d})
        assert r.returncode == 3, "invalid-for-analysis must fail the hook"
        assert "flaky.kicad_pcb" in r.stderr
        assert "invalid for analysis" in r.stderr
        assert "commit blocked" in r.stderr


def test_the_worst_exit_code_wins_across_files():
    codes = iter([0, 3, 2])
    code, _, err = _quiet_main(
        ["a.kicad_pcb", "b.kicad_pcb", "c.kicad_pcb"],
        runner=lambda cmd: _Proc(next(codes)),
    )
    assert code == 3, "one untrustworthy board taints the whole commit"
    assert "b.kicad_pcb" in err


def test_a_crash_fails_instead_of_reading_as_clean():
    # subprocess returncodes are negative when the child dies on a signal;
    # max() alone would let -11 lose to 0 and wave the commit through.
    code, _, err = _quiet_main(
        ["a.kicad_pcb"], runner=lambda cmd: _Proc(-11)
    )
    assert code == 1
    assert "crashed" in err


def test_hauksbee_bin_env_var_is_respected():
    with tempfile.TemporaryDirectory() as d:
        path, log = _stub_binary(d, 0)
        r = run_hook(
            ["board.kicad_pcb"],
            env={"HAUKSBEE_BIN": path, "PATH": "/nonexistent"},
        )
        assert r.returncode == 0, r.stderr
        assert os.path.exists(log), "the HAUKSBEE_BIN binary must be the one run"


def _ci_file_patterns():
    """Read the CI hook's pre-commit ``files`` pattern from both examples."""
    paths = (
        os.path.join(_HERE, ".pre-commit-config.yaml"),
        os.path.join(_HERE, "..", "..", ".pre-commit-hooks.yaml"),
    )
    patterns = []
    for path in paths:
        text = open(path, encoding="utf-8").read()
        matches = re.findall(r"(?m)^\s+files:\s*'([^']*ci/[^']*)'\s*$", text)
        assert len(matches) == 1, f"expected one CI files pattern in {path}"
        patterns.append(matches[0])
    return patterns


def test_precommit_path_filters_cover_all_board_formats_and_ci_specs():
    # These are the single-file inputs accepted by `hauksbee run`. Archives
    # are content-sniffed, so matching their suffix is the only way for a
    # staged fab.zip (or ODB++ tarball) to trigger this path-only hook. XML is
    # intentionally broad: IPC-2581 is also content-sniffed, and pre-commit
    # cannot distinguish it from unrelated XML without reading the file.
    supported_extensions = (
        "kicad_pcb",
        "kicad_sch",
        "net",
        "brd",
        "d356",
        "pcbdoc",
        "board",
    )
    positive = [f"board.{ext}" for ext in supported_extensions]
    positive += [f"nested/board.{ext}" for ext in supported_extensions]
    archive_extensions = ("xml", "zip", "tgz", "tar.gz", "tar")
    positive += [f"fab.{ext}" for ext in archive_extensions]
    positive += [f"nested/fab.{ext}" for ext in archive_extensions]
    positive += [
        "board.PcbDoc",
        "nested/board.PcbDoc",
        "nested/fab.TAR.GZ",
        "ci/power.toml",
        "nested/ci/power.toml",
        "nested/ci/deep/spec.TOML",
    ]
    # In particular, `(?:^|/)ci/` must not turn a filename containing `ci/`
    # into a spec, and extension matching must stop at the real suffix.
    hostile = [
        "power.toml",
        "not-ci/power.toml",
        "nested/not-ci/power.toml",
        "ci-not/power.toml",
        "nested/ci/power.yaml",
        "nested/ci/power.toml.bak",
        "board.kicad_pcb.bak",
        "nested/board.PcbDoc.txt",
        "nested/board.gbr",
        "nested/fab.gz",
        "nested/fab.tar.bz2",
        "nested/fab.tar.gz.bak",
        "nested/fab.tgz.bak",
        "nested/fab.zip.bak",
    ]

    patterns = _ci_file_patterns()
    assert patterns[0] == patterns[1], "the shipped examples must stay in sync"
    for pattern in patterns:
        compiled = re.compile(pattern)
        for path in positive:
            assert compiled.search(path), f"{path!r} did not match {pattern!r}"
        for path in hostile:
            assert not compiled.search(path), f"hostile path matched: {path!r}"


def test_board_hook_path_filter_covers_archives_and_exchange_documents():
    text = open(
        os.path.join(_HERE, "..", "..", ".pre-commit-hooks.yaml"),
        encoding="utf-8",
    ).read()
    matches = re.findall(r"(?m)^\s+files:\s*'([^']*)'\s*$", text)
    assert len(matches) == 2, "expected board and CI files patterns"
    board_pattern = re.compile(matches[0])
    for path in (
        "fab.zip",
        "nested/fab.tgz",
        "nested/fab.tar.gz",
        "nested/fab.tar",
        "nested/ipc2581.xml",
    ):
        assert board_pattern.search(path), f"{path!r} did not match board hook"
    for path in ("ci/power.toml", "nested/ci/power.toml", "notes.xml.bak"):
        assert not board_pattern.search(path), f"non-board path matched: {path!r}"


def test_no_em_dashes_in_anything_a_user_reads():
    for name in ("hauksbee_check_precommit.py",):
        text = open(os.path.join(_HERE, name), encoding="utf-8").read()
        assert "—" not in text, f"{name} contains an em dash"
    hooks_yaml = os.path.join(_HERE, "..", "..", ".pre-commit-hooks.yaml")
    assert "—" not in open(hooks_yaml, encoding="utf-8").read()


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
    print("all hauksbee-check hook tests passed" if not failed else f"{failed} failed")
    sys.exit(1 if failed else 0)
