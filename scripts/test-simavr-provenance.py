#!/usr/bin/env python3
"""Focused black-box contracts for immutable simavr setup and release provenance."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
COMMIT = "f44723e8c42431136d5b4de81f789ded56d7e8fa"


class SimavrProvenanceContract(unittest.TestCase):
    def check_avr(self, include: Path, library: Path) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.update(
            {
                "CARGO_TERM_COLOR": "never",
                "SIMAVR_COMMIT": COMMIT,
                "SIMAVR_INCLUDE_DIR": str(include),
                "SIMAVR_LIB_DIR": str(library),
            }
        )
        return subprocess.run(
            [
                "cargo",
                "check",
                "-p",
                "hauksbee-mcu",
                "--no-default-features",
                "--features",
                "avr",
                "--lib",
            ],
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_payload_change_cannot_reuse_the_commit_marker(self) -> None:
        source = (ROOT / "crates/hauksbee-mcu/build.rs").read_text()
        self.assertIn(".hauksbee-simavr-payload.sha256", source)
        self.assertIn("simavr payload digest mismatch", source)
        installer = (ROOT / "scripts/install-sims.sh").read_text()
        self.assertIn('simavr-payload-provenance.sh" record', installer)
        bundle = (ROOT / "scripts/bundle.sh").read_text()
        self.assertIn('simavr-payload-provenance.sh" verify', bundle)

        with tempfile.TemporaryDirectory() as raw_tmp:
            prefix = Path(raw_tmp) / "simavr"
            (prefix / "include/simavr").mkdir(parents=True)
            (prefix / "lib").mkdir()
            header = prefix / "include/simavr/sim_avr.h"
            archive = prefix / "lib/libsimavr.a"
            header.write_text("recorded header\n")
            archive.write_text("recorded archive\n")
            (prefix / ".hauksbee-simavr-commit").write_text(f"{COMMIT}\n")
            subprocess.run(
                [str(ROOT / "scripts/simavr-payload-provenance.sh"), "record", str(prefix)],
                check=True,
            )
            archive.write_text("substituted archive\n")
            result = self.check_avr(prefix / "include", prefix / "lib")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("simavr payload digest mismatch", result.stderr)

    def test_every_installed_header_is_part_of_the_payload_identity(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            prefix = Path(raw_tmp) / "simavr"
            include = prefix / "include/simavr"
            include.mkdir(parents=True)
            (prefix / "lib").mkdir()
            (include / "sim_avr.h").write_text("primary\n")
            ioport = include / "avr_ioport.h"
            ioport.write_text("enum-before\n")
            (prefix / "lib/libsimavr.a").write_text("archive\n")
            subprocess.run(
                [str(ROOT / "scripts/simavr-payload-provenance.sh"), "record", str(prefix)],
                check=True,
            )
            ioport.write_text("enum-after\n")
            result = subprocess.run(
                [str(ROOT / "scripts/simavr-payload-provenance.sh"), "verify", str(prefix)],
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("payload digest mismatch", result.stderr)

    def test_split_include_and_library_prefixes_cannot_claim_one_commit(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            include_prefix = tmp / "headers"
            library_prefix = tmp / "archive"
            (include_prefix / "include/simavr").mkdir(parents=True)
            (library_prefix / "lib").mkdir(parents=True)
            (include_prefix / "include/simavr/sim_avr.h").write_text("fixture\n")
            (library_prefix / "lib/libsimavr.a").write_text("fixture\n")
            (include_prefix / ".hauksbee-simavr-commit").write_text(f"{COMMIT}\n")
            (library_prefix / ".hauksbee-simavr-commit").write_text(f"{COMMIT}\n")
            result = self.check_avr(include_prefix / "include", library_prefix / "lib")

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(
            "SIMAVR_INCLUDE_DIR and SIMAVR_LIB_DIR must share one prefix",
            result.stderr,
        )

    def test_default_bundle_exports_verified_commit_before_cargo(self) -> None:
        bundle = (ROOT / "scripts/bundle.sh").read_text()
        self.assertIn("refusing to bundle a dirty source tree", bundle)
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            prefix = tmp / "simavr"
            (prefix / "include/simavr").mkdir(parents=True)
            (prefix / "lib").mkdir()
            (prefix / "include/simavr/sim_avr.h").write_text("fixture\n")
            (prefix / "lib/libsimavr.a").write_text("fixture\n")
            (prefix / ".hauksbee-simavr-commit").write_text(f"{COMMIT}\n")
            subprocess.run(
                [str(ROOT / "scripts/simavr-payload-provenance.sh"), "record", str(prefix)],
                check=True,
            )

            record = tmp / "cargo-environment"
            fake_cargo = tmp / "cargo"
            fake_cargo.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n%s\\n%s\\n" "$SIMAVR_COMMIT" '
                '"$SIMAVR_INCLUDE_DIR" "$SIMAVR_LIB_DIR" > "$BUNDLE_ENV_RECORD"\n'
                "exit 73\n"
            )
            fake_cargo.chmod(0o755)
            fake_bin = tmp / "bin"
            fake_bin.mkdir()
            fake_bun = fake_bin / "bun"
            fake_bun.write_text("#!/bin/sh\nexit 0\n")
            fake_bun.chmod(0o755)
            fake_git = fake_bin / "git"
            fake_git.write_text(
                "#!/bin/sh\n"
                'case " $* " in\n'
                f'  *" rev-parse HEAD "*) printf "%s\\n" "{COMMIT}" ;;\n'
                '  *" status --porcelain "*) : ;;\n'
                "  *) exit 2 ;;\n"
                "esac\n"
            )
            fake_git.chmod(0o755)
            env = os.environ.copy()
            env.update(
                {
                    "BUNDLE_ENV_RECORD": str(record),
                    "CARGO": str(fake_cargo),
                    "PATH": f"{fake_bin}:{env['PATH']}",
                    "SIMAVR_INCLUDE_DIR": f"{prefix / 'include'}/",
                    "SIMAVR_LIB_DIR": f"{prefix / 'lib'}/",
                }
            )
            result = subprocess.run(
                [
                    str(ROOT / "scripts/bundle.sh"),
                    "--shape",
                    "default",
                    "--out",
                    str(tmp / "dist"),
                ],
                cwd=ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            recorded = record.read_text().splitlines() if record.exists() else []

        self.assertEqual(result.returncode, 73, result.stdout + result.stderr)
        self.assertEqual(
            recorded,
            [COMMIT, f"{prefix / 'include'}/", f"{prefix / 'lib'}/"],
        )

    def cached_installer(self, tagged: bool, installed: bool):
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            prefix, fake_bin = tmp / "simavr", tmp / "bin"
            for directory in (prefix / "include/simavr", prefix / "lib/pkgconfig", fake_bin):
                directory.mkdir(parents=True)
            (prefix / "include/simavr/sim_avr.h").write_text("fixture\n")
            (prefix / "lib/libsimavr.a").write_text("fixture\n")
            (prefix / ".hauksbee-simavr-commit").write_text(f"{COMMIT}\n")
            if tagged:
                (prefix / "lib/pkgconfig/simavr.pc").write_text("Version: v1.8\n")
                subprocess.run(
                    [str(ROOT / "scripts/simavr-payload-provenance.sh"), "record", str(prefix)],
                    check=True,
                )
            marker, calls = tmp / "libelf", tmp / "calls"
            if installed:
                marker.touch()
            commands = {
                "uname": 'case "$1" in -s) printf "Darwin\\n";; -m) printf "arm64\\n";; *) exit 73;; esac',
                "brew": 'printf "%s\\n" "$*" >> "$CALLS"\n'
                'case "$*" in\n'
                ' "list libelf") test -f "$LIBELF";;\n'
                ' "install libelf") touch "$LIBELF";;\n'
                " *) exit 73;; esac",
                "pkg-config": 'test "$*" = "--exists libelf" && test -f "$LIBELF"',
                "git": "echo FETCH_ATTEMPTED >&2; exit 73",
            }
            for command in ("make", "curl", "sudo"):
                commands[command] = 'echo "unexpected external command" >> "$CALLS"; exit 73'
            for name, body in commands.items():
                executable = fake_bin / name
                executable.write_text(f"#!/bin/sh\n{body}\n")
                executable.chmod(0o755)
            env = dict(
                os.environ,
                PATH=f"{fake_bin}:{os.environ['PATH']}",
                HOME=str(tmp),
                CALLS=str(calls),
                LIBELF=str(marker),
            )
            probe = [str(fake_bin / "pkg-config"), "--exists", "libelf"]
            self.assertEqual(subprocess.run(probe, env=env).returncode, 0 if installed else 1)
            result = subprocess.run(
                [str(ROOT / "scripts/install-sims.sh"), "--avr", "--prefix", str(prefix)],
                cwd=ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(subprocess.run(probe, env=env).returncode, 0)
            return result, calls.read_text().splitlines() if calls.exists() else []

    def test_tagless_matching_marker_is_not_accepted_as_a_complete_install(self) -> None:
        result, _ = self.cached_installer(tagged=False, installed=True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("FETCH_ATTEMPTED", result.stderr)

    def test_verified_cache_ensures_host_libelf_without_rebuilding(self) -> None:
        for installed in (False, True):
            with self.subTest(installed=installed):
                result, calls = self.cached_installer(tagged=True, installed=installed)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("already installed", result.stdout)
                self.assertEqual(
                    calls, ["list libelf"] if installed else ["list libelf", "install libelf"]
                )


if __name__ == "__main__":
    unittest.main()
