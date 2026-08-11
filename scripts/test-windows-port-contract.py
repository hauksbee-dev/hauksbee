#!/usr/bin/env python3
"""Offline release-contract tests for the mandatory Windows port.

These tests intentionally inspect the executable CI/release surfaces rather
than accepting a cross-compile as proof that a Windows user can install and use
the product.  Native behavior still runs on ``windows-latest``; this file keeps
the job, bundle, installer, process-containment, and browser evidence from
quietly drifting apart.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


class WindowsPortContract(unittest.TestCase):
    def test_ci_runs_native_engine_ci_and_mcu_tests_with_warnings_denied(self) -> None:
        ci = read(".github/workflows/ci.yml")
        self.assertRegex(ci, r"runs-on:\s*windows-latest")
        self.assertIn("cargo test -p hauksbee-engine -p hauksbee-ci -p hauksbee-mcu", ci)
        self.assertIn("--no-default-features --features renode,qemu", ci)
        self.assertIn("RUSTFLAGS: -D warnings", ci)

    def test_ci_exercises_the_real_release_binary_through_drag_and_drop(self) -> None:
        ci = read(".github/workflows/ci.yml")
        self.assertIn("frontend/tests/e2e/drag-drop-release.ts", ci)
        self.assertRegex(ci, r"target[\\/]release[\\/]hauksbee\.exe")
        self.assertIn("windows-frontdoor-evidence", ci)
        self.assertRegex(ci, r"HB_BOARD_FILES.*starter\.board")
        self.assertIn('$env:HB_RELEASE_COHORT = "smoke"', ci)

    def test_release_builds_and_verifies_a_permissive_windows_zip(self) -> None:
        release = read(".github/workflows/release.yml")
        self.assertRegex(release, r"build-windows:")
        self.assertRegex(release, r"runs-on:\s*windows-latest")
        self.assertRegex(release, r"scripts[\\/]bundle-windows\.ps1")
        self.assertIn("hauksbee-$V-windows-x86_64-permissive.zip", release)
        self.assertIn("LICENSE-BINARY.txt", release)

    def test_windows_bundle_contains_every_release_binary_and_checksum(self) -> None:
        bundle = read("scripts/bundle-windows.ps1")
        for binary in ("hauksbee.exe", "hauksbee-ci.exe", "hauksbee-mcp.exe"):
            self.assertIn(binary, bundle)
        self.assertIn("LICENSE-BINARY.txt", bundle)
        self.assertIn("Get-FileHash", bundle)
        self.assertIn("Compress-Archive", bundle)
        self.assertNotIn(
            "$LASTEXITCODE -ne 0",
            bundle,
            "doctor may exit nonzero solely because an external backend is absent",
        )

    def test_installer_installs_mcp_and_refuses_non_permissive_windows_shape(self) -> None:
        installer = read("scripts/get-hauksbee.ps1")
        self.assertIn("hauksbee-mcp.exe", installer)
        self.assertNotIn("there are no published Windows release assets yet", installer)
        self.assertRegex(installer, r"if\s*\(\s*-not\s+\$Permissive\s*\)")
        self.assertIn("Windows releases are permissive-only", installer)

    def test_windows_children_are_owned_by_kill_on_close_jobs(self) -> None:
        children = read("crates/hauksbee-mcu/src/children.rs")
        for symbol in (
            "CreateJobObjectW",
            "SetInformationJobObject",
            "AssignProcessToJobObject",
            "JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE",
        ):
            self.assertIn(symbol, children)
        self.assertRegex(children, r"windows_job_kills_child_when_guard_closes")

    def test_windows_emulators_are_pinned_and_named_firmware_flows_are_required(self) -> None:
        installer = read("scripts/install-sims-windows.ps1")
        for digest in (
            "d09b7934cfd560cd06bde8f131ef78f521f10d423d5aac6096f2a583224aeb3e",
            "3c483d77f5350a568df1faf4d8dbc82c95d6bc2b826d0d4be910485e0a68ca2a",
            "697aa4800a1f52be0b1693b30e22a684f7ea93c46c489e619384cae7b0e9b87b",
        ):
            self.assertIn(digest, installer)
        required = read("scripts/run-required-integrations-windows.ps1")
        for test_name in (
            "rp2040_adc_injection_reaches_firmware",
            "esp32_i2c_firmware_drives_gpio_from_temperature",
            "esp32c3_full_cosim_through_solved_circuit",
        ):
            self.assertIn(test_name, required)
        self.assertIn('if ($output -match "SKIP:")', required)
        for workflow in (".github/workflows/ci.yml", ".github/workflows/release.yml"):
            text = read(workflow)
            self.assertIn("install-sims-windows.ps1", text)
            self.assertIn("run-required-integrations-windows.ps1", text)

    def test_windows_job_assignment_is_before_execution_and_covers_hard_parent_death(self) -> None:
        children = read("crates/hauksbee-mcu/src/children.rs")
        for symbol in (
            "CREATE_SUSPENDED",
            "CreateToolhelp32Snapshot",
            "ResumeThread",
            "spawn_owned",
            "windows_hard_parent_death_kills_immediate_grandchild",
        ):
            self.assertIn(symbol, children)
        for process in (
            "crates/hauksbee-mcu/src/renode/process.rs",
            "crates/hauksbee-mcu/src/qemu/process.rs",
        ):
            self.assertIn("spawn_owned", read(process))

    def test_installer_is_exercised_and_replaces_all_binaries_transactionally(self) -> None:
        workflow = read(".github/workflows/release.yml")
        self.assertIn("test-windows-installer.ps1", workflow)
        test_script = read("scripts/test-windows-installer.ps1")
        for contract in (
            "HAUKSBEE_API_BASE",
            "corrupt checksum",
            "hauksbee-mcp.exe",
            "rollback",
        ):
            self.assertIn(contract, test_script)
        installer = read("scripts/get-hauksbee.ps1")
        self.assertIn("install-staging", installer)
        self.assertIn("install-backup", installer)
        self.assertIn("Move-Item", installer)

    def test_build_jobs_do_not_retain_write_credentials(self) -> None:
        release = read(".github/workflows/release.yml")
        self.assertRegex(
            release,
            r"(?m)^permissions:\s*\n(?:\s*#.*\n)*\s+contents:\s*read\b",
        )
        self.assertRegex(
            release,
            r"(?ms)^  release:.*?^    permissions:\s*\n\s+contents:\s*write\b",
        )
        self.assertRegex(
            release,
            r"(?ms)^  build-windows:.*?persist-credentials:\s*false",
        )

    def test_windows_limitations_are_counted_and_name_unlocking_paths(self) -> None:
        limits = read("docs/about/LIMITATIONS.md")
        self.assertIn("Windows x86_64", limits)
        self.assertIn("pseudo-terminal", limits)
        self.assertIn("--serial-transport tcp", limits)
        self.assertIn("AVR", limits)
        self.assertIn("MSYS2", limits)

    def test_no_mutable_actions_are_added_to_touched_workflows(self) -> None:
        for workflow in (".github/workflows/ci.yml", ".github/workflows/release.yml"):
            for line in read(workflow).splitlines():
                match = re.search(r"(?:^|-)\s*uses:\s*[^#\s]+@([^\s#]+)", line)
                if match:
                    self.assertRegex(
                        match.group(1),
                        r"^[0-9a-f]{40}$",
                        f"mutable action in {workflow}: {line.strip()}",
                    )


if __name__ == "__main__":
    unittest.main(verbosity=2)
