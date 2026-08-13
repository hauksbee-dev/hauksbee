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
        self.assertIn(
            "cargo test -p hauksbee-engine -p hauksbee-ci -p hauksbee-mcu", ci
        )
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

    def test_installer_installs_mcp_and_refuses_non_permissive_windows_shape(
        self,
    ) -> None:
        installer = read("scripts/get-hauksbee.ps1")
        self.assertIn("hauksbee-mcp.exe", installer)
        self.assertNotIn("there are no published Windows release assets yet", installer)
        self.assertRegex(installer, r"if\s*\(\s*-not\s+\$Permissive\s*\)")
        self.assertIn("Windows releases are permissive-only", installer)
        self.assertIn("release.immutable", installer)
        self.assertIn("refusing replaceable private assets", installer)

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

    def test_windows_emulators_are_pinned_and_named_firmware_flows_are_required(
        self,
    ) -> None:
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
        for workflow in (
            ".github/workflows/ci.yml",
            ".github/workflows/release.yml",
        ):
            text = read(workflow)
            self.assertIn("install-sims-windows.ps1", text)
            self.assertIn("run-required-integrations-windows.ps1", text)

    def test_required_windows_gates_bind_and_attest_exact_pinned_backends(self) -> None:
        required = read("scripts/run-required-integrations-windows.ps1")
        for name in (
            "HAUKSBEE_RENODE",
            "HAUKSBEE_QEMU_XTENSA",
            "HAUKSBEE_QEMU_RISCV32",
        ):
            self.assertRegex(required, rf"\$env:{name}\s*=", name)
        self.assertIn("artifact_sha256", required)
        self.assertIn("archive_sha256", required)
        self.assertIn("backends", required)

        release = read(".github/workflows/release.yml")
        self.assertIn('--expected-platform "windows-x86_64"', release)

    def test_required_windows_gate_owns_the_complete_timeout_process_tree(self) -> None:
        required = read("scripts/run-required-integrations-windows.ps1")
        self.assertTrue(
            (ROOT / "scripts/windows-owned-process.ps1").is_file(),
            "the release-gate process runner must own a Windows Job Object",
        )
        helper = read("scripts/windows-owned-process.ps1")
        native_test = read("scripts/test-windows-process-tree.ps1")

        self.assertIn("Invoke-HauksbeeJobProcess", required)
        for symbol in (
            "CREATE_SUSPENDED",
            "AssignProcessToJobObject",
            "JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE",
            "TerminateJobObject",
        ):
            self.assertIn(symbol, helper)
        self.assertIn("grandchild", native_test.lower())
        for workflow in (".github/workflows/ci.yml", ".github/workflows/release.yml"):
            self.assertIn("test-windows-process-tree.ps1", read(workflow))

    def test_windows_backend_evidence_is_pinned_to_complete_install_payloads(
        self,
    ) -> None:
        installer = read("scripts/install-sims-windows.ps1")
        required = read("scripts/run-required-integrations-windows.ps1")
        verifier = read("scripts/run_required_integrations.py")

        self.assertIn("Assert-InstallTree", installer)
        self.assertIn("install_tree_sha256", required)
        self.assertGreaterEqual(
            required.count("-Check -EvidenceOut"),
            2,
            "the full installed payload must be reverified after firmware gates run",
        )
        self.assertIn("artifact_sha256", verifier)
        self.assertIn("install_tree_sha256", verifier)
        for fingerprint in (
            "895fddb36f65237af5a47928e49984cf1e1992e27e0d37546b3b8ea29ad57385",
            "3b12f1dd7b613cd9b73994a985fcd77107f471c352c52b4f3f2ff1528d4e7e8d",
            "7716f734130a20193ab45a4c14581918822e5ae684eb5cf3073b9429bee29825",
            "ec900387a3f7b54800d4690db575b86162769add55aa3b09056a943b29ec6644",
            "4f02f4495f50ddf3baed71de29192932bd09053f0a1df498b854e0f5be0d8171",
        ):
            self.assertIn(fingerprint, installer)
            self.assertIn(fingerprint, verifier)
        self.assertNotIn("expected_artifact_sha256", required)
        self.assertIn(
            "test-windows-simulator-integrity.ps1",
            read(".github/workflows/release.yml"),
        )

    def test_windows_job_assignment_is_before_execution_and_covers_hard_parent_death(
        self,
    ) -> None:
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
        release_docs = read("docs/about/release-and-licensing.md")
        self.assertIn("spawn-to-assignment window", release_docs)
        self.assertNotIn("descendant cannot escape between spawn and assignment", release_docs)

    def test_production_dependency_installs_use_owned_jobs_and_native_qemu(self) -> None:
        deps = read("crates/hauksbee-engine/src/deps.rs")
        self.assertIn("hauksbee_mcu::children::spawn_owned", deps)
        self.assertIn("tree_guard.terminate", deps)
        self.assertNotIn('Command::new("taskkill")', deps)
        self.assertIn('arg("-QemuOnly")', deps)
        self.assertIn("timeout_kills_a_real_installer_grandchild", deps)
        docs = read("docs/cosim/SIMULATORS.md")
        self.assertIn("invokes this same pinned PowerShell route", docs)

    def test_every_public_windows_qemu_install_front_door_uses_the_native_route(
        self,
    ) -> None:
        commands = read("crates/hauksbee-engine/src/commands/install.rs")
        self.assertGreaterEqual(
            commands.count("crate::deps::install_esp_qemu"),
            2,
            "the explicit install command and interactive run offer must both use "
            "the checksum-pinned PowerShell route on Windows",
        )
        self.assertGreaterEqual(commands.count("#[cfg(windows)]"), 2)

    def test_simulator_installer_initializes_native_exit_status_under_strict_mode(
        self,
    ) -> None:
        installer = read("scripts/install-sims-windows.ps1")
        strict = installer.index("Set-StrictMode -Version Latest")
        first_read = installer.index("$LASTEXITCODE -ne 0")
        initialization = installer.index("$global:LASTEXITCODE = 0")
        self.assertLess(strict, initialization)
        self.assertLess(initialization, first_read)

    def test_windows_teardown_never_targets_a_recycled_numeric_pid(self) -> None:
        children = read("crates/hauksbee-mcu/src/children.rs")
        self.assertNotIn('Command::new("taskkill")', children)
        self.assertIn("TerminateJobObject", children)
        self.assertIn("windows_reaped_child_teardown_uses_owned_job", children)

    def test_installer_is_exercised_and_replaces_all_binaries_transactionally(
        self,
    ) -> None:
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

    def test_windows_packaging_and_install_bind_tag_to_every_binary(self) -> None:
        bundle = read("scripts/bundle-windows.ps1")
        installer = read("scripts/get-hauksbee.ps1")
        for binary in ("hauksbee.exe", "hauksbee-ci.exe", "hauksbee-mcp.exe"):
            self.assertIn(binary, bundle)
            self.assertIn(binary, installer)
        self.assertIn("Assert-BinaryVersion", bundle)
        self.assertIn("Assert-BinaryVersion", installer)
        self.assertRegex(bundle, r"Assert-BinaryVersion[^\n]+\$Version")
        self.assertRegex(installer, r"Assert-BinaryVersion[^\n]+\$VersionBare")
        self.assertIn("ExpectedCommit", bundle)
        self.assertIn("ExpectedCommit", installer)
        self.assertIn('"$ApiBase/commits/$Version"', installer)
        self.assertIn("$tagCommit.sha -cne $ExpectedCommit", installer)
        self.assertIn(r"\(git $escapedCommit\)", bundle)
        self.assertIn(r"\(git $escapedCommit\)", installer)
        self.assertIn("Out-String).Trim()", bundle)
        self.assertIn("Out-String).Trim()", installer)
        bootstrap = read("README.md")
        self.assertIn("-Version $releaseTag -ExpectedCommit $releaseCommit", bootstrap)

    def test_windows_tree_swaps_use_unique_recoverable_backups(self) -> None:
        for path in ("scripts/get-hauksbee.ps1", "scripts/install-sims-windows.ps1"):
            script = read(path)
            self.assertIn("[guid]::NewGuid()", script)
            self.assertIn("Recover-StaleBackup", script)
            self.assertRegex(
                script, r"try\s*\{[\s\S]*Move-Item[^\n]+\$[Tt]arget[^\n]+\$backup"
            )
        fixture = read("scripts/test-windows-installer.ps1")
        self.assertIn("stale-interruption", fixture)

    def test_failed_windows_transactions_remove_unconsumed_staging_trees(self) -> None:
        installer = read("scripts/get-hauksbee.ps1")
        simulators = read("scripts/install-sims-windows.ps1")
        fixture = read("scripts/test-windows-installer.ps1")

        self.assertIn("Remove-AbandonedStaging", installer)
        self.assertIn("Remove-AbandonedStaging", simulators)
        self.assertIn("install-staging-*", fixture)

    def test_downloaded_executable_probes_receive_no_private_tokens(self) -> None:
        installer = read("scripts/get-hauksbee.ps1")
        self.assertIn("Invoke-TokenFreeVersionProbe", installer)
        for token in ("HAUKSBEE_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"):
            self.assertRegex(installer, rf"Remove-Item Env:{token}")
        fixture = read("scripts/test-windows-installer.ps1")
        self.assertIn('env::var_os("HAUKSBEE_GITHUB_TOKEN")', fixture)
        self.assertIn('env::var_os("GITHUB_TOKEN")', fixture)
        self.assertIn('env::var_os("GH_TOKEN")', fixture)

    def test_installer_contract_runs_under_both_windows_powershells(self) -> None:
        fixture = read("scripts/test-windows-installer.ps1")
        self.assertIn('"powershell.exe"', fixture)
        self.assertIn('"pwsh.exe"', fixture)

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
        for workflow in (
            ".github/workflows/ci.yml",
            ".github/workflows/release.yml",
            ".github/workflows/docker.yml",
        ):
            for line in read(workflow).splitlines():
                match = re.search(r"(?:^|-)\s*uses:\s*[^#\s]+@([^\s#]+)", line)
                if match:
                    self.assertRegex(
                        match.group(1),
                        r"^[0-9a-f]{40}$",
                        f"mutable action in {workflow}: {line.strip()}",
                    )

    def test_windows_simulator_archives_reject_links_and_special_members(self) -> None:
        installer = read("scripts/install-sims-windows.ps1")
        safe = installer.split("function Assert-SafeTar", 1)[1].split(
            "function Recover-StaleBackup", 1
        )[0]
        self.assertIn("tar -tvf", safe)
        self.assertIn("@('-', 'd')", safe)
        self.assertIn("unsafe archive member type", safe)


if __name__ == "__main__":
    unittest.main(verbosity=2)
