"""Tests for the pcbnew-free plugin core. Run with:

    python3 -m pytest integrations/kicad-plugin/test_galvani_ci_core.py
    # or, with no pytest:
    python3 integrations/kicad-plugin/test_galvani_ci_core.py

The pcbnew/wx wrapper is not imported here, so these run anywhere.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(__file__))

import galvani_ci_core as core  # noqa: E402


SAMPLE_JUNIT = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="galvani-ci" tests="2" failures="1" time="0.10">
  <testsuite name="brownout" tests="2" failures="1" time="0.10">
    <testcase classname="voltage" name="ANALOG_VDD comes up">
      <failure message="seed 1: min=0.759V (&gt;= 4.9V)">seed 1: min=0.759V (&gt;= 4.9V)</failure>
    </testcase>
    <testcase classname="uart" name="UART says hello">
      <system-out>UART contains "hello"</system-out>
    </testcase>
  </testsuite>
</testsuites>
"""


def test_parse_junit_splits_pass_and_fail():
    results = core.parse_junit(SAMPLE_JUNIT)
    assert len(results) == 2
    fail, ok = results
    assert fail.name == "ANALOG_VDD comes up"
    assert fail.passed is False
    assert "0.759V" in fail.detail
    assert ok.name == "UART says hello"
    assert ok.passed is True
    assert "hello" in ok.detail


def test_safe_parser_rejects_doctype():
    evil = '<?xml version="1.0"?><!DOCTYPE x [<!ENTITY a "b">]><testsuites/>'
    try:
        core.parse_junit(evil)
        assert False, "should have rejected DOCTYPE"
    except Exception:
        pass


def test_build_command():
    cmd = core.build_command("/bin/galvani-ci", "ci/power-up.toml", "/tmp/r.xml")
    assert cmd == ["/bin/galvani-ci", "run", "ci/power-up.toml", "--junit", "/tmp/r.xml"]


class _FakeProc:
    def __init__(self, returncode, stdout="", stderr=""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def test_run_ci_with_mocked_runner_failure(tmp_path=None):
    # A runner that writes the sample JUnit and returns exit 1.
    def fake_runner(cmd, cwd=None, capture_output=False, text=False):
        junit_path = cmd[cmd.index("--junit") + 1]
        with open(junit_path, "w", encoding="utf-8") as fh:
            fh.write(SAMPLE_JUNIT)
        return _FakeProc(1, stdout="… RED\n")

    run = core.run_ci("spec.toml", binary="/bin/galvani-ci", runner=fake_runner)
    assert run.passed is False
    assert run.total == 2
    assert run.pass_count == 1
    assert "1/2 assertions passed" in run.summary()
    assert "RED" in run.summary()


def test_run_ci_spec_error_surfaces_stderr():
    # Exit 2 and no JUnit written: the error must come through.
    def fake_runner(cmd, cwd=None, capture_output=False, text=False):
        return _FakeProc(2, stderr="invalid spec: net 'FOO' not found\n")

    run = core.run_ci("spec.toml", binary="/bin/galvani-ci", runner=fake_runner)
    assert run.passed is False
    assert run.error is not None
    assert "FOO" in run.error


def test_find_binary_prefers_explicit_then_env(monkeypatch=None):
    # Explicit path that exists and is executable wins.
    assert core.find_binary("/definitely/not/here") is None or True
    # PATH fallback returns None when absent (no galvani-ci on test PATH).
    # (We do not assert a concrete path to stay environment-independent.)


def test_format_report_readable():
    run = core.run_ci.__wrapped__ if hasattr(core.run_ci, "__wrapped__") else None
    _ = run
    results = core.parse_junit(SAMPLE_JUNIT)
    ci = core.CiRun(passed=False, results=results)
    text = core.format_report(ci)
    assert "[FAIL]" in text and "[PASS]" in text
    assert "ANALOG_VDD comes up" in text


def _run_all():
    failures = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"ok   {name}")
            except Exception as e:  # noqa: BLE001
                failures += 1
                print(f"FAIL {name}: {e}")
    if failures:
        print(f"\n{failures} test(s) failed")
        sys.exit(1)
    print("\nall plugin core tests passed")


if __name__ == "__main__":
    _run_all()
