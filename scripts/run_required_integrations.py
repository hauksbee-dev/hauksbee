#!/usr/bin/env python3
"""Run the small co-simulation tier that release evidence is required to earn."""

from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Gate:
    """One named, externally-backed integration proof."""

    name: str
    command: tuple[str, ...]
    expected_test: str
    timeout_seconds: int = 600


GATES = (
    Gate(
        name="renode-rp2040-adc",
        command=(
            "cargo",
            "test",
            "-p",
            "hauksbee-mcu",
            "--no-default-features",
            "--features",
            "renode",
            "--test",
            "renode_rp2040_adc",
            "rp2040_adc_injection_reaches_firmware",
            "--",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ),
        expected_test="rp2040_adc_injection_reaches_firmware",
    ),
    Gate(
        name="qemu-xtensa-i2c",
        command=(
            "cargo",
            "test",
            "-p",
            "hauksbee-engine",
            "--no-default-features",
            "--features",
            "qemu",
            "--test",
            "i2c_sensor_cosim_qemu",
            "esp32_i2c_firmware_drives_gpio_from_temperature",
            "--",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ),
        expected_test="esp32_i2c_firmware_drives_gpio_from_temperature",
    ),
    Gate(
        name="qemu-riscv32-circuit",
        command=(
            "cargo",
            "test",
            "-p",
            "hauksbee-engine",
            "--no-default-features",
            "--features",
            "qemu",
            "--test",
            "esp32_qemu_cosim",
            "esp32c3_full_cosim_through_solved_circuit",
            "--",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ),
        expected_test="esp32c3_full_cosim_through_solved_circuit",
    ),
)


def evaluate_result(gate: Gate, returncode: int, output: str) -> list[str]:
    """Return every reason this command failed to prove its named integration."""

    problems: list[str] = []
    if returncode != 0:
        problems.append(f"{gate.name}: cargo exited with status {returncode}")
    if "SKIP:" in output:
        skipped = [line.strip() for line in output.splitlines() if "SKIP:" in line]
        problems.append(f"{gate.name}: reported SKIP: {'; '.join(skipped)}")

    # `--nocapture` may place the test's own evidence between Cargo's
    # `test name ...` prefix and libtest's eventual `ok`. Every command above
    # filters to exactly one named test, so require both that exact start and a
    # one-test green summary rather than assuming `... ok` stays on one line.
    named_start = re.compile(
        rf"^test {re.escape(gate.expected_test)} \.\.\.", re.MULTILINE
    )
    one_pass_summary = re.compile(
        r"^test result: ok\. 1 passed; 0 failed(?:;|$)", re.MULTILINE
    )
    if named_start.search(output) is None or one_pass_summary.search(output) is None:
        problems.append(
            f"{gate.name}: did not observe named test {gate.expected_test!r} passing"
        )
    return problems


def _text(output: str | bytes | None) -> str:
    """Normalize subprocess timeout capture, which is bytes even in text mode."""

    if output is None:
        return ""
    if isinstance(output, bytes):
        return output.decode("utf-8", errors="replace")
    return output


def _merge_capture(earlier: str | bytes | None, final: str | bytes | None) -> str:
    """Keep timeout output once when communicate returns overlapping captures."""

    earlier_text = _text(earlier)
    final_text = _text(final)
    if final_text.startswith(earlier_text):
        return final_text
    if earlier_text.startswith(final_text):
        return earlier_text
    return earlier_text + final_text


def _descendant_process_groups(root_pid: int) -> set[int]:
    """Snapshot every POSIX process group rooted below ``root_pid``."""

    groups: set[int] = {root_pid}
    if os.name != "posix":
        return groups
    try:
        table = subprocess.run(
            ("ps", "-axo", "pid=,ppid=,pgid="),
            text=True,
            capture_output=True,
            check=True,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return groups

    children: dict[int, list[tuple[int, int]]] = {}
    for line in table.splitlines():
        fields = line.split()
        if len(fields) != 3:
            continue
        try:
            pid, parent, group = map(int, fields)
        except ValueError:
            continue
        children.setdefault(parent, []).append((pid, group))

    pending = [root_pid]
    seen = {root_pid}
    while pending:
        parent = pending.pop()
        for pid, group in children.get(parent, []):
            if pid in seen:
                continue
            seen.add(pid)
            pending.append(pid)
            groups.add(group)
    # Never signal the required-integration runner's own process group even if
    # a corrupt process table somehow attributes it below the child.
    groups.discard(os.getpgrp())
    return groups


def _signal_process_groups(
    groups: set[int], root_group: int, sig: signal.Signals
) -> None:
    """Signal descendant groups before the cargo group that parents them."""

    ordered = sorted(groups - {root_group})
    if root_group in groups:
        ordered.append(root_group)
    for group in ordered:
        try:
            os.killpg(group, sig)
        except ProcessLookupError:
            pass


def _live_process_groups(groups: set[int]) -> set[int]:
    live = set()
    for group in groups:
        try:
            os.killpg(group, 0)
        except ProcessLookupError:
            continue
        except PermissionError:
            # It still exists; inability to signal it is a cleanup failure, not
            # evidence that the emulator exited.
            pass
        live.add(group)
    return live


def _stop_process_group(process: subprocess.Popen[str]) -> tuple[str, bool]:
    """Stop cargo and emulator groups; return output and whether KILL was needed."""

    groups = _descendant_process_groups(process.pid)
    _signal_process_groups(groups, process.pid, signal.SIGTERM)
    try:
        output, _ = process.communicate(timeout=5)
        live_descendants = _live_process_groups(groups - {process.pid})
        if live_descendants:
            _signal_process_groups(live_descendants, process.pid, signal.SIGKILL)
            return _text(output), True
        return _text(output), False
    except subprocess.TimeoutExpired as error:
        # Retain the first snapshot: cargo commonly exits on TERM while Renode
        # or QEMU keeps a separately-created process group alive and becomes
        # reparented before this second lookup can see it.
        groups.update(_descendant_process_groups(process.pid))
        _signal_process_groups(groups, process.pid, signal.SIGKILL)
        output, _ = process.communicate()
        return _merge_capture(error.output, output), True


def run_command(
    command: tuple[str, ...], repo: Path, *, timeout_seconds: int = 600
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    # Stable evidence: GitHub's global CARGO_TERM_COLOR=always otherwise places
    # ANSI escapes inside the exact pass line this contract validates.
    env["CARGO_TERM_COLOR"] = "never"
    process = subprocess.Popen(
        command,
        cwd=repo,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )
    try:
        output, _ = process.communicate(timeout=timeout_seconds)
        return subprocess.CompletedProcess(command, process.returncode, output)
    except subprocess.TimeoutExpired:
        output, killed = _stop_process_group(process)
        termination = "SIGTERM then SIGKILL" if killed else "SIGTERM"
        output += (
            f"\nREQUIRED INTEGRATION TIMEOUT: exceeded {timeout_seconds}s; "
            f"cargo and its emulator process group were terminated with {termination}\n"
        )
        return subprocess.CompletedProcess(command, 124, output)
    except KeyboardInterrupt:
        _stop_process_group(process)
        raise


def _git_sha(repo: Path) -> str:
    result = subprocess.run(
        ("git", "rev-parse", "HEAD"),
        cwd=repo,
        text=True,
        capture_output=True,
        check=True,
    )
    return result.stdout.strip()


def _verify_evidence(path: Path, expected_sha: str) -> list[str]:
    problems: list[str] = []
    try:
        document = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        return [f"cannot read required integration evidence {path}: {error}"]
    if document.get("schema_version") != 1:
        problems.append("required integration evidence has unsupported schema_version")
    if document.get("commit_sha") != expected_sha:
        problems.append(
            "required integration evidence commit SHA mismatch: "
            f"expected {expected_sha}, found {document.get('commit_sha')!r}"
        )
    expected_gates = [gate.name for gate in GATES]
    if document.get("gates") != expected_gates:
        problems.append(
            "required integration evidence does not contain the complete ordered gate set"
        )
    return problems


def _write_evidence(path: Path, commit_sha: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    temporary.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "commit_sha": commit_sha,
                "gates": [gate.name for gate in GATES],
            },
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )
    os.replace(temporary, path)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected-sha")
    parser.add_argument("--evidence-out", type=Path)
    parser.add_argument("--verify-evidence", type=Path)
    args = parser.parse_args(argv)

    if args.verify_evidence is not None:
        if not args.expected_sha:
            parser.error("--verify-evidence requires --expected-sha")
        problems = _verify_evidence(args.verify_evidence, args.expected_sha)
        if problems:
            for problem in problems:
                print(problem, file=sys.stderr)
            return 1
        print(
            f"required integration evidence matches release commit {args.expected_sha}"
        )
        return 0

    repo = Path(__file__).resolve().parent.parent
    commit_sha = _git_sha(repo)
    if args.expected_sha and commit_sha != args.expected_sha:
        print(
            "required integrations: checkout commit SHA mismatch: "
            f"expected {args.expected_sha}, found {commit_sha}",
            file=sys.stderr,
        )
        return 1
    preflight = run_command(
        (str(repo / "scripts/install-sims.sh"), "--check", "--require-pinned"),
        repo,
    )
    sys.stdout.write(preflight.stdout)
    if preflight.returncode != 0:
        print(
            "required integrations: simulator preflight failed; no release evidence was earned",
            file=sys.stderr,
        )
        return 1

    failures: list[str] = []
    for gate in GATES:
        print(f"\n==> required integration: {gate.name}", flush=True)
        result = run_command(gate.command, repo, timeout_seconds=gate.timeout_seconds)
        sys.stdout.write(result.stdout)
        failures.extend(evaluate_result(gate, result.returncode, result.stdout))

    if failures:
        print("\nrequired integration evidence FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(f"\nrequired integration evidence: {len(GATES)}/{len(GATES)} gates passed")
    if args.evidence_out is not None:
        _write_evidence(args.evidence_out, commit_sha)
        print(f"required integration evidence retained at {args.evidence_out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
