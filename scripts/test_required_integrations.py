#!/usr/bin/env python3
"""Contract tests for the release-required co-simulation runner."""

import hashlib
import json
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

    def test_tag_release_requires_same_sha_real_integration_evidence(self) -> None:
        workflow = (
            Path(__file__).resolve().parents[1]
            / ".github"
            / "workflows"
            / "release.yml"
        ).read_text()

        self.assertIn("required-integrations:", workflow)
        integration_job = workflow.split("  required-integrations:", 1)[1].split(
            "\n  release:", 1
        )[0]
        self.assertIn("ref: ${{ github.sha }}", integration_job)
        self.assertIn("scripts/install-sims.sh --require-pinned", integration_job)
        self.assertIn("scripts/run_required_integrations.py", integration_job)
        self.assertIn("--expected-sha \"$GITHUB_SHA\"", integration_job)
        self.assertIn("--evidence-out", integration_job)

        release_job = workflow.split("\n  release:", 1)[1]
        self.assertRegex(
            release_job,
            r"needs:\s*\[[^\]]*required-integrations[^\]]*\]",
        )
        verify_at = release_job.index("--verify-evidence")
        publish_at = release_job.index("softprops/action-gh-release")
        self.assertLess(verify_at, publish_at)

    def test_evidence_verifier_requires_the_release_sha_and_all_real_gates(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            evidence = Path(raw_tmp) / "required-integrations.json"
            evidence.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "commit_sha": "a" * 40,
                        "gates": [gate.name for gate in GATES],
                    }
                )
            )
            command = [
                sys.executable,
                str(Path(__file__).resolve().parent / "run_required_integrations.py"),
                "--verify-evidence",
                str(evidence),
                "--expected-sha",
                "a" * 40,
            ]
            verifier_env = os.environ.copy()
            verifier_env.update(
                {
                    "HAUKSBEE_RENODE": str(Path(raw_tmp) / "missing-renode"),
                    "HAUKSBEE_QEMU_XTENSA": str(Path(raw_tmp) / "missing-xtensa"),
                    "HAUKSBEE_QEMU_RISCV32": str(Path(raw_tmp) / "missing-riscv32"),
                }
            )
            matching = subprocess.run(
                command,
                env=verifier_env,
                text=True,
                capture_output=True,
                check=False,
            )
            command[-1] = "b" * 40
            mismatched = subprocess.run(
                command,
                env=verifier_env,
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertEqual(matching.returncode, 0, matching.stdout + matching.stderr)
        self.assertNotEqual(mismatched.returncode, 0)
        self.assertIn("commit SHA mismatch", mismatched.stdout + mismatched.stderr)

    def _fake_backend(self, directory: Path, name: str, version: str) -> Path:
        directory.mkdir(parents=True, exist_ok=True)
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

    def _verified_receipt(
        self,
        binary: Path,
        backend: str,
        version: str,
        checksums: Path,
        asset_name: str,
    ) -> None:
        asset_sha = hashlib.sha256(asset_name.encode()).hexdigest()
        artifact_sha = hashlib.sha256(binary.read_bytes()).hexdigest()
        with checksums.open("a") as stream:
            stream.write(f"{asset_sha}  {asset_name}\n")
        Path(f"{binary}.hkb-pinned").write_text(
            "format=1\n"
            f"backend={backend}\n"
            f"version={version}\n"
            f"asset_name={asset_name}\n"
            f"asset_sha256={asset_sha}\n"
            f"artifact_sha256={artifact_sha}\n"
        )

    def _run_sim_check(
        self,
        *args: str,
        env_updates: dict[str, str],
        require_pinned: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        for key in (
            "HAUKSBEE_RENODE",
            "HAUKSBEE_QEMU_DIR",
            "HAUKSBEE_QEMU_XTENSA",
            "HAUKSBEE_QEMU_RISCV32",
            "RENODE_CHECKSUMS",
            "QEMU_CHECKSUMS",
        ):
            env.pop(key, None)
        env.update(env_updates)
        command = [
            str(Path(__file__).resolve().parent / "install-sims.sh"),
            "--check",
        ]
        if require_pinned:
            command.append("--require-pinned")
        command.extend(args)
        return subprocess.run(
            command,
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

    def test_required_check_rejects_self_reported_exact_versions_without_receipts(self) -> None:
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

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("verified install receipt", result.stdout + result.stderr)

    def test_ordinary_check_remains_permissive_without_install_receipts(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            renode = self._fake_backend(tmp, "renode", "Renode v1.15.0")
            result = self._run_sim_check(
                "--renode-only",
                env_updates={"HAUKSBEE_RENODE": str(renode)},
                require_pinned=False,
            )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_required_check_accepts_digest_verified_installed_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            renode_sums = tmp / "renode-checksums.txt"
            qemu_sums = tmp / "qemu-checksums.txt"
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
            self._verified_receipt(
                renode, "renode", "1.16.1", renode_sums, "renode-test.asset"
            )
            self._verified_receipt(
                xtensa,
                "qemu-system-xtensa",
                "esp-develop-9.2.2-20260417",
                qemu_sums,
                "qemu-xtensa-test.asset",
            )
            self._verified_receipt(
                riscv,
                "qemu-system-riscv32",
                "esp-develop-9.2.2-20260417",
                qemu_sums,
                "qemu-riscv32-test.asset",
            )

            result = self._run_sim_check(
                env_updates={
                    "HAUKSBEE_RENODE": str(renode),
                    "HAUKSBEE_QEMU_XTENSA": str(xtensa),
                    "HAUKSBEE_QEMU_RISCV32": str(riscv),
                    "RENODE_CHECKSUMS": str(renode_sums),
                    "QEMU_CHECKSUMS": str(qemu_sums),
                }
            )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_required_check_rejects_backend_changed_after_verified_install(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            sums = tmp / "renode-checksums.txt"
            renode = self._fake_backend(tmp, "renode", "Renode v1.16.1.16858")
            self._verified_receipt(
                renode, "renode", "1.16.1", sums, "renode-test.asset"
            )
            with renode.open("a") as stream:
                stream.write("# changed after verification\n")

            result = self._run_sim_check(
                "--renode-only",
                env_updates={
                    "HAUKSBEE_RENODE": str(renode),
                    "RENODE_CHECKSUMS": str(sums),
                },
            )

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("artifact digest mismatch", result.stdout + result.stderr)

    def test_required_discovery_skips_stale_path_for_verified_pinned_renode(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            home = tmp / "home"
            stale_dir = tmp / "stale-bin"
            pinned_dir = home / "renode-portable"
            stale_dir.mkdir(parents=True)
            pinned_dir.mkdir(parents=True)
            stale = self._fake_backend(stale_dir, "renode", "Renode v1.15.0")
            pinned = self._fake_backend(
                pinned_dir, "renode", "Renode v1.16.1.16858"
            )
            sums = tmp / "renode-checksums.txt"
            self._verified_receipt(
                pinned, "renode", "1.16.1", sums, "renode-test.asset"
            )

            result = self._run_sim_check(
                "--renode-only",
                env_updates={
                    "HOME": str(home),
                    "PATH": f"{stale_dir}:{os.environ['PATH']}",
                    "RENODE_CHECKSUMS": str(sums),
                },
            )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(str(pinned), result.stdout)
        self.assertNotIn(f"FOUND  {stale}", result.stdout)

    def test_required_discovery_skips_older_qemu_directory_for_pinned_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            home = tmp / "home"
            sums = tmp / "qemu-checksums.txt"
            pinned_paths = []
            for arch in ("xtensa", "riscv32"):
                root = home / ".espressif" / "tools" / f"qemu-{arch}"
                stale = self._fake_backend(
                    root / "000-old" / "qemu" / "bin",
                    f"qemu-system-{arch}",
                    "QEMU emulator version 9.2.2 (esp_develop_9.2.2_20240101)",
                )
                self.assertTrue(stale.exists())
                pinned = self._fake_backend(
                    root
                    / "esp_develop_9.2.2_20260417"
                    / "qemu"
                    / "bin",
                    f"qemu-system-{arch}",
                    "QEMU emulator version 9.2.2 (esp_develop_9.2.2_20260417)",
                )
                asset = f"qemu-{arch}-test.asset"
                self._verified_receipt(
                    pinned,
                    f"qemu-system-{arch}",
                    "esp-develop-9.2.2-20260417",
                    sums,
                    asset,
                )
                pinned_paths.append(pinned)

            result = self._run_sim_check(
                "--qemu-only",
                env_updates={"HOME": str(home), "QEMU_CHECKSUMS": str(sums)},
            )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for pinned in pinned_paths:
            self.assertIn(str(pinned), result.stdout)

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
import os, pathlib, signal, sys, time
os.setpgrp()
signal.signal(
    signal.SIGTERM,
    lambda *_: pathlib.Path(sys.argv[2]).write_text('CHILD_IGNORED_SIGTERM'),
)
print('CHILD_READY', flush=True)
print(f'CHILD_PGID={os.getpgrp()}', flush=True)
os.write(int(sys.argv[1]), b'1')
sys.stdout.flush()
os.close(sys.stdout.fileno())
os.close(sys.stderr.fileno())
while True:
    time.sleep(1)
"""
        with tempfile.TemporaryDirectory() as raw_tmp:
            term_marker = Path(raw_tmp) / "child-saw-term"
            parent_code = f"""
import os, signal, subprocess, sys, time
signal.signal(signal.SIGTERM, lambda *_: print('PARENT_IGNORED_SIGTERM', flush=True))
ready_read, ready_write = os.pipe()
child = subprocess.Popen(
    [sys.executable, '-u', '-c', {child_code!r}, str(ready_write), {str(term_marker)!r}],
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

            child_match = re.search(r"CHILD_PID=(\d+)", result.stdout)
            self.assertIsNotNone(child_match, result.stdout)
            child_pid = int(child_match.group(1))
            try:
                self.assertEqual(result.returncode, 124)
                self.assertIn("CHILD_READY", result.stdout)
                self.assertIn("CHILD_PGID", result.stdout)
                self.assertIn("SIGKILL", result.stdout)
                self.assertTrue(
                    term_marker.is_file(),
                    "separately grouped descendant never received SIGTERM",
                )
                for _ in range(50):
                    try:
                        os.kill(child_pid, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.02)
                else:
                    self.fail(
                        f"separately grouped descendant {child_pid} survived cleanup"
                    )
            finally:
                try:
                    os.killpg(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    @unittest.skipUnless(os.name == "posix", "process-group signals require POSIX")
    def test_timeout_reaps_detached_group_after_cargo_exits_on_term(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            term_marker = Path(raw_tmp) / "detached-saw-term"
            child_code = f"""
import os, pathlib, signal, sys, time
os.setpgrp()
signal.signal(signal.SIGTERM, lambda *_: pathlib.Path({str(term_marker)!r}).write_text('term'))
print(f'DETACHED_PID={{os.getpid()}}', flush=True)
os.close(sys.stdout.fileno())
os.close(sys.stderr.fileno())
while True:
    time.sleep(1)
"""
            parent_code = f"""
import subprocess, sys, time
subprocess.Popen([sys.executable, '-u', '-c', {child_code!r}])
print('CARGO_READY', flush=True)
while True:
    time.sleep(1)
"""
            result = run_command(
                (sys.executable, "-u", "-c", parent_code),
                Path.cwd(),
                timeout_seconds=0.5,
            )
            child_match = re.search(r"DETACHED_PID=(\d+)", result.stdout)
            self.assertIsNotNone(child_match, result.stdout)
            child_pid = int(child_match.group(1))
            try:
                self.assertIn("CARGO_READY", result.stdout)
                self.assertTrue(term_marker.is_file(), result.stdout)
                self.assertIn("SIGKILL", result.stdout)
                for _ in range(50):
                    try:
                        os.kill(child_pid, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.02)
                else:
                    self.fail(f"detached emulator group {child_pid} survived cleanup")
            finally:
                try:
                    os.killpg(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass


if __name__ == "__main__":
    unittest.main()
