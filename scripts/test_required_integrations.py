#!/usr/bin/env python3
"""Contract tests for the release-required co-simulation runner."""

import os
import re
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from run_required_integrations import GATES, Gate, evaluate_result, run_command


class RequiredIntegrationEvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.gate = Gate(
            name="renode-rp2040-adc",
            command=("cargo", "test"),
            expected_test="rp2040_adc_injection_reaches_firmware",
        )

    def test_zero_exit_skip_is_not_a_pass(self) -> None:
        output = """\
running 1 test
SKIP: Renode not installed
test rp2040_adc_injection_reaches_firmware ... ok
test result: ok. 1 passed; 0 failed
"""

        problems = evaluate_result(self.gate, 0, output)

        self.assertIn("reported SKIP", "\n".join(problems))

    def test_named_real_test_must_be_observed_passing(self) -> None:
        output = "test result: ok. 0 passed; 0 failed; 1 filtered out\n"

        problems = evaluate_result(self.gate, 0, output)

        self.assertIn(self.gate.expected_test, "\n".join(problems))

    def test_nonzero_cargo_status_is_a_failure(self) -> None:
        output = "test rp2040_adc_injection_reaches_firmware ... FAILED\n"

        problems = evaluate_result(self.gate, 101, output)

        self.assertIn("status 101", "\n".join(problems))

    def test_exact_named_pass_without_skip_is_accepted(self) -> None:
        output = """\
running 1 test
test rp2040_adc_injection_reaches_firmware ... ok
test result: ok. 1 passed; 0 failed
"""

        self.assertEqual(evaluate_result(self.gate, 0, output), [])

    def test_named_pass_may_print_nocapture_evidence_before_ok(self) -> None:
        output = """\
running 1 test
test rp2040_adc_injection_reaches_firmware ... ADC counts: low=410 high=3276
firmware observed both injected values
ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
"""

        self.assertEqual(evaluate_result(self.gate, 0, output), [])

    def test_required_gates_compile_only_the_backend_they_prove(self) -> None:
        commands = {gate.name: gate.command for gate in GATES}

        for name, command in commands.items():
            with self.subTest(gate=name):
                self.assertIn("--no-default-features", command)
        self.assertIn("renode", commands["renode-rp2040-adc"])
        self.assertNotIn("qemu", commands["renode-rp2040-adc"])
        for name in ("qemu-xtensa-i2c", "qemu-riscv32-circuit"):
            self.assertIn("qemu", commands[name])
            self.assertNotIn("renode", commands[name])
            self.assertNotIn("avr", commands[name])

    def test_required_backend_cache_tracks_installer_and_version_manifest(self) -> None:
        workflow = (
            Path(__file__).resolve().parents[1]
            / ".github"
            / "workflows"
            / "corpus-gate.yml"
        ).read_text()
        bundle = (Path(__file__).resolve().parent / "bundle.sh").read_text()
        cache_key = next(
            line for line in workflow.splitlines() if "key: required-sims-" in line
        )

        self.assertIn("scripts/install-sims.sh", cache_key)
        self.assertIn("scripts/required-simulator-versions.env", cache_key)
        self.assertIn("required-simulator-versions.env", bundle)

    def _fake_backend(self, directory: Path, name: str, version: str) -> Path:
        path = directory / name
        machine_help = "printf 'esp32\\n'" if name.startswith("qemu-") else ":"
        path.write_text(
            "#!/usr/bin/env bash\n"
            "if [ \"${1:-}\" = --version ]; then\n"
            f"  printf '%s\\n' '{version}'\n"
            "elif [ \"${1:-}\" = -machine ]; then\n"
            f"  {machine_help}\n"
            "fi\n"
        )
        path.chmod(0o755)
        return path

    def _run_sim_check(
        self, *args: str, env_updates: dict[str, str]
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.update(env_updates)
        return subprocess.run(
            [
                str(Path(__file__).resolve().parent / "install-sims.sh"),
                "--check",
                "--require-pinned",
                *args,
            ],
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_required_check_rejects_stale_renode(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            renode = self._fake_backend(tmp, "renode", "Renode v1.15.0.1234")

            result = self._run_sim_check(
                "--renode-only", env_updates={"HAUKSBEE_RENODE": str(renode)}
            )

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1.16.1", result.stdout + result.stderr)

    def test_required_check_rejects_stale_espressif_qemu(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            xtensa = self._fake_backend(
                tmp,
                "qemu-system-xtensa",
                "QEMU emulator version 9.2.2 (esp_develop_9.2.2_20240101)",
            )
            riscv = self._fake_backend(
                tmp,
                "qemu-system-riscv32",
                "QEMU emulator version 9.2.2 (esp_develop_9.2.2_20240101)",
            )

            result = self._run_sim_check(
                "--qemu-only",
                env_updates={
                    "HAUKSBEE_QEMU_XTENSA": str(xtensa),
                    "HAUKSBEE_QEMU_RISCV32": str(riscv),
                },
            )

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("esp-develop-9.2.2-20260417", result.stdout + result.stderr)

    def test_required_check_accepts_exact_pinned_backend_versions(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            renode = self._fake_backend(tmp, "renode", "Renode v1.16.1.16858")
            xtensa = self._fake_backend(
                tmp,
                "qemu-system-xtensa",
                "QEMU emulator version 9.2.2 (esp_develop_9.2.2_20260417)",
            )
            riscv = self._fake_backend(
                tmp,
                "qemu-system-riscv32",
                "QEMU emulator version 9.2.2 (esp_develop_9.2.2_20260417)",
            )

            result = self._run_sim_check(
                env_updates={
                    "HAUKSBEE_RENODE": str(renode),
                    "HAUKSBEE_QEMU_XTENSA": str(xtensa),
                    "HAUKSBEE_QEMU_RISCV32": str(riscv),
                }
            )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_external_command_timeout_is_bounded_and_explicit(self) -> None:
        started = time.monotonic()

        result = run_command(
            (sys.executable, "-c", "import time; time.sleep(30)"),
            Path.cwd(),
            timeout_seconds=0.05,
        )

        self.assertEqual(result.returncode, 124)
        self.assertIn("REQUIRED INTEGRATION TIMEOUT", result.stdout)
        self.assertLess(time.monotonic() - started, 2.0)

    @unittest.skipUnless(os.name == "posix", "process-group signals require POSIX")
    def test_timeout_kills_sigterm_resistant_descendant_and_keeps_output(self) -> None:
        child_code = """
import os, signal, sys, time
signal.signal(signal.SIGTERM, lambda *_: print('CHILD_IGNORED_SIGTERM', flush=True))
print('CHILD_READY', flush=True)
os.write(int(sys.argv[1]), b'1')
while True:
    time.sleep(1)
"""
        parent_code = f"""
import os, signal, subprocess, sys, time
signal.signal(signal.SIGTERM, lambda *_: print('PARENT_IGNORED_SIGTERM', flush=True))
ready_read, ready_write = os.pipe()
child = subprocess.Popen(
    [sys.executable, '-u', '-c', {child_code!r}, str(ready_write)],
    pass_fds=(ready_write,),
)
os.close(ready_write)
os.read(ready_read, 1)
os.close(ready_read)
print(f'CHILD_PID={{child.pid}}', flush=True)
while True:
    time.sleep(1)
"""

        result = run_command(
            (sys.executable, "-u", "-c", parent_code),
            Path.cwd(),
            timeout_seconds=0.5,
        )

        self.assertEqual(result.returncode, 124)
        self.assertIn("CHILD_READY", result.stdout)
        self.assertIn("CHILD_IGNORED_SIGTERM", result.stdout)
        self.assertIn("SIGKILL", result.stdout)
        child_match = re.search(r"CHILD_PID=(\d+)", result.stdout)
        self.assertIsNotNone(child_match, result.stdout)
        child_pid = int(child_match.group(1))
        for _ in range(50):
            try:
                os.kill(child_pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.02)
        else:
            os.kill(child_pid, signal.SIGKILL)
            self.fail(f"descendant {child_pid} survived process-group cleanup")


if __name__ == "__main__":
    unittest.main()
