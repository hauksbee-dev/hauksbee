#!/usr/bin/env python3
"""Run the small co-simulation tier that release evidence is required to earn."""

from __future__ import annotations

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


def _stop_process_group(process: subprocess.Popen[str]) -> tuple[str, bool]:
    """Stop cargo and every emulator child; return output and whether KILL was needed."""

    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        output, _ = process.communicate(timeout=5)
        return _text(output), False
    except subprocess.TimeoutExpired as error:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
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


def main() -> int:
    repo = Path(__file__).resolve().parent.parent
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
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
