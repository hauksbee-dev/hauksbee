#!/usr/bin/env python3
"""Black-box tests for the release launcher's repository-privacy contract."""

from __future__ import annotations

import ast
import os
import pathlib
from pathlib import Path
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import threading
import unittest


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from qc import runner as qc_runner

LAUNCHER = ROOT / "scripts" / "make-public.sh"
PREFLIGHT = ROOT / "scripts" / "preflight-private-release.sh"
SURFACE_MANIFEST = ROOT / "scripts" / "private-release-surfaces.json"
SURFACE_CHECKER = ROOT / "scripts" / "check-private-release-surfaces.py"
CONTAINER_PREFLIGHT = ROOT / "scripts" / "check-private-container-publication.sh"
MIRROR_DEPENDENCY_CHECKER = ROOT / "scripts" / "check-mirror-dependencies.py"
REGISTRY_USER = ROOT / "integrations" / "github-action" / "resolve-registry-user.sh"


def provenance_checkout(root: Path) -> None:
    """Use a real existing commit, without creating commits or configuring identity."""
    source = (ROOT / "crates/hauksbee-ci/build.rs").read_text()
    watched = re.findall(r'"../../([^"\n]+)"', source)
    files = [p for p in watched if Path(p).suffix or p in ("LICENSE", "NOTICE")]
    files += ["docs/START_HERE.md", *(f"crates/{c}/build.rs" for c in
              ("hauksbee-engine", "hauksbee-ci", "hauksbee-mcp"))]
    subprocess.run(["git", "clone", "--quiet", "--shared", "--no-checkout", str(ROOT), str(root)], check=True)
    subprocess.run(["git", "-C", str(root), "sparse-checkout", "set", "--no-cone", "--stdin"],
                   input="".join(f"/{p}\n" for p in files), text=True, check=True)
    subprocess.run(["git", "-C", str(root), "checkout", "--quiet", "-b", "provenance-fixture", "HEAD"], check=True)
    subprocess.run(["git", "-C", str(root), "pack-refs", "--all"], check=True)
    for path in watched:
        if path not in files:
            (root / path).mkdir(parents=True, exist_ok=True)


class PrivateReleasePolicyTests(unittest.TestCase):
    def test_retained_qc_report_never_leaks_a_local_binary_path(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            versions = {"hauksbee": "hauksbee test", "hauksbee-ci": "hauksbee-ci test"}
            in_repo = qc_runner.write_report([], tmp / "in-repo", ROOT / "target/release", versions)
            in_repo_text = in_repo.read_text()
            self.assertIn("Binaries: `<REPO>/target/release`", in_repo_text)
            self.assertNotIn(str(ROOT), in_repo_text)

            external = qc_runner.write_report([], tmp / "external", tmp / "private/bin", versions)
            external_text = external.read_text()
            self.assertIn("Binaries: `<EXTERNAL-BIN-DIR>`", external_text)
            self.assertNotIn(str(tmp), external_text)

    def test_source_installer_never_succeeds_after_a_failed_web_build(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            fake_bin = tmp / "bin"
            fake_bin.mkdir()
            cargo_ran = tmp / "cargo-ran"

            def executable(name: str, body: str) -> Path:
                path = fake_bin / name
                path.write_text("#!/bin/sh\n" + body)
                path.chmod(0o755)
                return path

            executable("clang", "exit 0\n")
            executable("bun", "exit 23\n")
            cargo = executable("cargo", f": > {cargo_ran!s}\nexit 0\n")
            simavr = tmp / "simavr" / "lib"
            simavr.mkdir(parents=True)
            (simavr / "libsimavr.a").write_bytes(b"contract fixture")

            result = subprocess.run(
                [str(ROOT / "scripts" / "install.sh"), "--prefix", str(tmp / "prefix")],
                env={
                    **os.environ,
                    "PATH": f"{fake_bin}:{os.environ.get('PATH', '')}",
                    "CARGO": str(cargo),
                    "SIMAVR_LIB_DIR": str(simavr),
                    "NO_COLOR": "1",
                },
                text=True,
                capture_output=True,
            )
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("Frontend build via bun failed", result.stderr)
            self.assertFalse(cargo_ran.exists(), "Cargo/install must not run after web failure")

    def test_build_provenance_never_borrows_an_enclosing_consumer_commit(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            consumer = tmp / "consumer"
            provenance_checkout(consumer)
            env = {k: v for k, v in os.environ.items() if not k.startswith("HAUKSBEE_")}
            for crate in ("hauksbee-engine", "hauksbee-ci", "hauksbee-mcp"):
                source = ROOT / "crates" / crate / "build.rs"
                binary = tmp / f"{crate}-build-script"
                subprocess.run(["rustc", str(source), "-o", str(binary)], check=True)
                watches = [
                    "cargo:rerun-if-changed=build.rs",
                    "cargo:rerun-if-env-changed=HAUKSBEE_SOURCE_COMMIT",
                ]
                if crate == "hauksbee-ci":
                    watches.append("cargo:rerun-if-env-changed=HAUKSBEE_RELEASE_TAG")
                # Both a workspace-shaped copy and Cargo's flat vendor layout
                # must avoid borrowing identity or watching the consumer tree.
                for layout in ("vendor/hauksbee/crates", "vendor"):
                    manifest = consumer / layout / crate
                    manifest.mkdir(parents=True)
                    result = subprocess.check_output(
                        [str(binary)], env={**env, "CARGO_MANIFEST_DIR": str(manifest)}, text=True
                    )
                    self.assertEqual(result.splitlines(), watches, (crate, layout))
                # An explicit identity must not even need a manifest path or Git.
                for sha in ("a" * 40, "A" * 40, "a" * 39, "g" * 40, ""):
                    result = subprocess.run(
                        [str(binary)], env={"PATH": "", "HAUKSBEE_SOURCE_COMMIT": sha},
                        text=True, capture_output=True,
                    )
                    if sha == "a" * 40:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(
                            result.stdout.splitlines(), watches + [f"cargo:rustc-env=GIT_HASH={sha}"]
                        )
                    else:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("HAUKSBEE_SOURCE_COMMIT must be", result.stderr)
                        self.assertEqual(result.stdout.splitlines(), watches)
                self.assertNotIn('println!("cargo:rerun-if-changed=../../.git");', source.read_text())
                for name in ('"HEAD"', '"index"', '"packed-refs"', '"refs"'):
                    self.assertIn(name, source.read_text())

    def test_build_provenance_refuses_a_dirty_owned_source_tree(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            root = tmp / "hauksbee"
            provenance_checkout(root)
            env = {k: v for k, v in os.environ.items() if not k.startswith("HAUKSBEE_")}
            for crate in ("hauksbee-engine", "hauksbee-ci", "hauksbee-mcp"):
                manifest = root / "crates" / crate
                binary = tmp / crate
                subprocess.run(["rustc", str(ROOT / "crates" / crate / "build.rs"), "-o", str(binary)], check=True)
                for relative in ("docs/START_HERE.md", f"crates/{crate}/build.rs",
                                 "docs/untracked.txt", "crates/untracked.txt"):
                    path = root / relative
                    original = path.read_bytes() if path.exists() else None
                    clean = subprocess.check_output(
                        [str(binary)], env={**env, "CARGO_MANIFEST_DIR": str(manifest)}, text=True
                    )
                    self.assertIn("cargo:rustc-env=GIT_HASH=", clean, crate)
                    for watched in ("crates", "vendor", "frontend/src", "integrations", "docs"):
                        self.assertIn(f"cargo:rerun-if-changed=../../{watched}\n", clean)
                    self.assertNotIn("cargo:rerun-if-changed=../../site\n", clean)
                    path.write_text("dirty\n")
                    dirty = subprocess.check_output(
                        [str(binary)], env={**env, "CARGO_MANIFEST_DIR": str(manifest)}, text=True
                    )
                    self.assertNotIn("cargo:rustc-env=GIT_HASH=", dirty, (crate, relative))
                    if original is None:
                        path.unlink()
                    else:
                        path.write_bytes(original)

    def test_cargo_rechecks_provenance_when_a_workspace_sibling_becomes_dirty(self) -> None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            root = Path(raw_tmp) / "hauksbee"
            provenance_checkout(root)
            manifest = root / "crates/hauksbee-ci"
            binary = Path(raw_tmp) / "build-script"
            subprocess.run(["rustc", str(ROOT / "crates/hauksbee-ci/build.rs"), "-o", str(binary)], check=True)
            crate = Path(raw_tmp) / "probe"
            (crate / "src").mkdir(parents=True)
            # Cargo runs a tiny external probe; the unmodified production build
            # script inspects the real checkout. Only relative watches are rebased.
            (crate / "build.rs").write_text('''fn main() {
                let manifest = std::path::Path::new(''' + json.dumps(str(manifest), ensure_ascii=False) + ''');
                let output = std::process::Command::new(''' + json.dumps(str(binary), ensure_ascii=False) + ''')
                    .current_dir(manifest).env("CARGO_MANIFEST_DIR", manifest).output().unwrap();
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                for line in std::str::from_utf8(&output.stdout).unwrap().lines() {
                    if let Some(path) = line.strip_prefix("cargo:rerun-if-changed=") {
                        println!("cargo:rerun-if-changed={}", manifest.join(path).display());
                    } else { println!("{line}"); }
                }
            }''')
            (crate / "Cargo.toml").write_text('[package]\nname="probe"\nversion="0.0.0"\nedition="2021"\n')
            (crate / "src/main.rs").write_text(
                'fn main() { println!("{}", option_env!("GIT_HASH").unwrap_or("none")); }\n'
            )
            env = {k: v for k, v in os.environ.items() if not k.startswith("HAUKSBEE_")}
            env.update(
                CARGO_TARGET_DIR=str(Path(raw_tmp) / "target"),
                CARGO_BUILD_BUILD_DIR=str(Path(raw_tmp) / "build"),
                CARGO_TERM_COLOR="never",  # CI forces colour; assertions parse plain Cargo output.
            )
            command = ["cargo", "run", "--verbose", "--manifest-path", str(crate / "Cargo.toml")]
            clean = subprocess.run(command, env=env, text=True, capture_output=True, check=True)
            self.assertRegex(clean.stdout.strip(), r"^[0-9a-f]{40}$")
            fresh = subprocess.run(command, env=env, text=True, capture_output=True, check=True)
            self.assertEqual(fresh.stdout, clean.stdout)
            self.assertIn("Fresh probe", fresh.stderr)
            (root / "docs/START_HERE.md").write_text("dirty\n")
            dirty = subprocess.check_output(command, env=env, text=True).strip()
            self.assertEqual(dirty, "none")

    def test_canonical_surface_manifest_exists(self) -> None:
        self.assertTrue(
            SURFACE_MANIFEST.is_file(),
            "private release surfaces need one canonical, machine-readable manifest",
        )

    def surface_manifest(self) -> dict[str, object]:
        return json.loads(SURFACE_MANIFEST.read_text())

    def release_url_surfaces(self) -> tuple[Path, ...]:
        manifest = self.surface_manifest()
        return tuple(Path(entry["path"]) for entry in manifest["surfaces"])

    def test_surface_manifest_classifies_every_repository_slug_occurrence(self) -> None:
        result = subprocess.run(
            ["python3", str(SURFACE_CHECKER), str(ROOT)],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

        manifest = self.surface_manifest()
        entries = manifest["surfaces"]
        paths = [entry["path"] for entry in entries]
        self.assertEqual(len(paths), len(set(paths)), "manifest paths must be unique")
        self.assertTrue(all(entry["classification"] for entry in entries))
        exclusions = manifest["excluded_prefixes"]
        excluded_paths = [entry["path"] for entry in exclusions]
        self.assertEqual(
            len(excluded_paths),
            len(set(excluded_paths)),
            "manifest exclusions must be unique",
        )
        self.assertTrue(all(entry["classification"] for entry in exclusions))

    def test_mirror_scope_matches_the_curated_release_tree(self) -> None:
        manifest = self.surface_manifest()
        development_only = {
            "BETA.md",
            "scripts/build-public-mirror.sh",
            "docs/dev-plans/launch-checklist.md",
            "docs/dev-plans/launch-gtm-strategy.md",
            "docs/dev-plans/linux-clean-room-report-2026-08-14.md",
            "docs/dev-plans/public-release-cleanup-plan.md",
            "docs/dev-plans/tasks.md",
            "frontend/capture/cards.ts",
            "scripts/build-private-beta-snapshot.sh",
            "scripts/make-public.sh",
        }
        for entry in manifest["surfaces"]:
            path = entry["path"]
            scopes = entry.get("scopes", ["development", "mirror"])
            with self.subTest(path=path):
                self.assertIn("development", scopes)
                self.assertEqual("mirror" in scopes, path not in development_only)

        with tempfile.TemporaryDirectory() as raw_tmp:
            mirror = Path(raw_tmp) / "mirror"
            for entry in manifest["surfaces"]:
                if "mirror" not in entry.get("scopes", ["development", "mirror"]):
                    continue
                relative = Path(entry["path"])
                destination = mirror / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(ROOT / relative, destination)
            for policy_file in (SURFACE_MANIFEST, SURFACE_CHECKER):
                destination = mirror / policy_file.relative_to(ROOT)
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(policy_file, destination)
            result = subprocess.run(
                [
                    "python3",
                    str(mirror / SURFACE_CHECKER.relative_to(ROOT)),
                    str(mirror),
                    "--scope",
                    "mirror",
                ],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

        builder = (ROOT / "scripts/build-public-mirror.sh").read_text()
        self.assertIn(
            "check-private-release-surfaces.py . --scope mirror",
            builder,
            "the real curated output, not only a synthetic fixture, must run the mirror policy",
        )
        self.assertIn("manifest['excluded_prefixes'] = []", builder)
        self.assertIn("surface.get('scopes', ['development', 'mirror'])", builder)

        # The developer probes leave by one glob; hauksbee-ci/examples/ is the
        # user-facing spec-and-board set and stays, which holds only while that
        # directory carries no Rust for the probe glob to catch.
        self.assertIn("\n  'crates/*/examples/*.rs'\n", builder)
        ci_examples = subprocess.check_output(
            ["git", "ls-files", "crates/hauksbee-ci/examples"], cwd=ROOT, text=True).split()
        self.assertTrue(ci_examples and not any(p.endswith(".rs") for p in ci_examples))

        # Run only the curation helper on a copy; never launch the mirror builder.
        helper = builder.split("def drop_test(path, name):\n", 1)[1].split("\nPY\n", 1)[0]
        with tempfile.TemporaryDirectory() as raw_tmp:
            target = Path(raw_tmp) / "test-policy.py"
            target.write_text(Path(__file__).read_text())
            helper = "def drop_test(path, name):\n" + helper
            helper = helper.replace("'scripts/test-private-release-policy.py'", repr(str(target)))
            exec(compile(helper, "mirror-policy-curation", "exec"), {"pathlib": pathlib})
            retained = ast.parse(target.read_text())
            methods = {node.name for node in ast.walk(retained) if isinstance(node, ast.FunctionDef)}
            launcher_methods = {
                "run_visibility_phase", "test_visibility_policy_and_dry_run",
                "test_mirror_visibility_is_checked_before_the_sentinel_builder",
            }
            self.assertTrue(launcher_methods.isdisjoint(methods))
            self.assertIn("test_public_installer_downloads_without_credential", methods)

    def test_private_suite_disclosure_counts_each_absent_test_file(self) -> None:
        builder = (ROOT / "scripts/build-public-mirror.sh").read_text()
        board_block = builder.split("BOARD_EXCLUDE=(", 1)[1].split("\n)", 1)[0]
        absent_paths = [
            ROOT / line.strip()
            for line in board_block.splitlines()
            if line.strip().endswith(".rs") and "/tests/" in line
        ]
        # The historical engine paths remain in BOARD_EXCLUDE so filter-repo
        # removes them from older commits. At the current tip, resolve the six
        # extracted suites from the private crate that now owns them.
        absent_paths = [
            path
            if path.exists()
            else ROOT / "crates" / "hauksbee-tarski" / "tests" / path.name
            for path in absent_paths
        ]

        disclosure = (ROOT / "docs/about/PRIVATE_SUITE.md").read_text()
        rows = {
            name: int(count)
            for name, count in re.findall(
                r"^\| `([^`]+)` \| ([0-9]+) \|", disclosure, re.MULTILINE
            )
        }
        actual: dict[str, int] = {}
        for path in absent_paths:
            count = len(re.findall(r"^#\[(?:tokio::)?test\]", path.read_text(), re.MULTILINE))
            actual[path.stem] = count
            with self.subTest(suite=path.stem):
                self.assertEqual(rows.get(path.stem), count)

        headline = re.search(
            r"\*\*([0-9]+) tests: ([0-9]+) in the ([0-9]+) absent files below, "
            r"plus ([0-9]+) removed",
            disclosure,
        )
        self.assertIsNotNone(headline)
        total, absent, files, removed = map(int, headline.groups())
        self.assertEqual(files, len(actual))
        self.assertEqual(absent, sum(actual.values()))
        self.assertEqual(total, absent + removed)
        namespace = {"re": re}
        exec((ROOT / "scripts/mirror_content.py").read_text(), namespace)
        declarations = builder.split("FIXTURE = re.compile(", 1)[1].split("import os, sys", 1)[0]
        exec("FIXTURE = re.compile(" + declarations, namespace)
        partial = 0
        for path in namespace["PARTIAL"]:
            source = (ROOT / path).read_text()
            partial += sum(bool(namespace["FIXTURE"].search(
                source[m.end():namespace["end_of_fn"](source, m.end())]))
                for m in re.finditer(r"#\[(?:tokio::)?test\]\n", source))
        self.assertEqual(removed, partial)

        nep = (ROOT / "crates/hauksbee-engine/tests/nep_private_acceptance.rs").read_text()
        self.assertIn("fn real_nep_host_exposes_standard_grade_7ms_gap()", nep)
        self.assertIn("fn real_nep_host_succeeds_with_compliant_firmware()", nep)

    def test_mirror_content_preserves_public_bytes(self) -> None:
        namespace = {}
        exec((ROOT / "scripts/mirror_content.py").read_text(), namespace)
        curate, end = namespace["curate"], namespace["end_of_fn"]
        for body in ('let s = "}\\\"{";', 'let s = r###"}"###;', '// }\n{}',
                     '/* } { */ {}', "let x: &'static str = \"}\";"):
            source = 'fn example() { ' + body + ' }'
            self.assertEqual(end(source + '\nfn keep() {}', 0), len(source))
        public = b'## Public\nUnchanged \\ bytes\r\n'
        private = b'## Worked example: the Tarski brownout\nprivate\n### Detail\nprivate\n'
        for path in (b'docs/CI.md', b'docs/ci/CI.md'):
            self.assertEqual(curate(path, public + private + public), public * 2)
        self.assertEqual(curate(b'other.md', public + private), public + private)
        with self.assertRaisesRegex(ValueError, 'curation drifted'):
            curate(b'docs/ci/CI.md', b'## Renamed private section\ntarski_brownout.toml\n')

        # No shipped document may tell a reader to run a filtered probe; its
        # recorded output stays, being evidence rather than an instruction.
        invocation = re.compile(rb"--example +" + namespace["PROBE"].encode())
        for name in sorted(namespace["PROBE_DOCS"]):
            source = (ROOT / name).read_bytes()
            curated = curate(name.encode(), source)
            self.assertRegex(source, invocation, name)
            self.assertNotRegex(curated, invocation, name)
            if "oracle" in name:
                self.assertIn(b"clearance_mm=0.2 primitives=225347 shorts=0", curated)
        binary = b'\x00/Users/hauksbee-user\xff'
        self.assertEqual(curate(b'firmware.elf', binary), b'\x00/Users/hauksbee-user\xff')
        elf = curate(b'x.elf', (ROOT / 'testdata/firmware/esp32_native_gpio/esp32_native_gpio.elf').read_bytes())
        self.assertEqual((len(elf), elf.count(b'Tarski'), elf.count(b'eoghancollins')), (2877308, 0, 0))
        source = (b'#[test]\nfn the_flagship_board_is_the_case_this_came_from() {\n'
                  b'let s = r#"}"#; /* } */\n}\n' + public)
        self.assertEqual(curate(b'crates/hauksbee-ci/tests/unpowered_rail_is_declared.rs', source), b'\n' + public)
        scoped = b'    /// Private-only docs\n    #[cfg(feature = "avr")]\n    ' + source.replace(b'the_flagship_board_is_the_case_this_came_from', b'private_nep_topology_installs_edge_exact_parallel_memory')
        following = b'\n    #[test]\n    fn public_without_avr() {}\n\n    #[cfg(feature = "avr")]\n    #[test]\n    fn public_with_avr() {}\n'
        self.assertEqual(curate(b'crates/hauksbee-cosim/src/scheduler.rs', scoped + following), b'\n' + public + following)
        # Historical fixtures stay private in the build cache, never in Git.
        fixture_index = os.environ.get("MIRROR_HISTORY_FIXTURES")
        cases = [(str(p.relative_to(ROOT)), p.read_bytes()) for p in
                 (ROOT / "examples/sessions").glob('*.txt') if not p.name.startswith(('06_', '07_', '12_'))]
        if fixture_index:
            index = Path(fixture_index)
            cases += [(path, (index.parent / 'fixtures' / sha).read_bytes())
                      for path, shas in json.loads(index.read_text()).items() for sha in shas]
        for path, data in cases:
            with self.subTest(path=path, sha=hashlib.sha256(data).hexdigest()):
                output = curate(path.encode(), data)
                self.assertEqual(curate(path.encode(), output), output, "curation must be idempotent")
                if path.endswith('.txt'):
                    self.assertEqual(output, data.replace(b'/Users/hauksbee-user', b'/Users/hauksbee-user'))
                else:
                    self.assertFalse(re.search(rb'tarski_(?:brownout|miswire)|Tarski bring-up|06_ci_spec_red|07_ci_spec_repaired_green|12_miswire_repair_demo|fn private_nep_topology_installs_edge_exact_parallel_memory', output))
                    if path.endswith('.rs'):
                        if path.endswith('/scheduler.rs'):
                            boundary = b'    #[cfg(feature = "avr")]\n    #[test]\n    #[ignore = "requires the private NEP board path"]'
                            start = data.index(boundary)
                            finish = data.index(b'\n    }', data.index(b'fn private_nep_topology_installs_edge_exact_parallel_memory', start)) + len(b'\n    }')
                            self.assertEqual(output, data[:start] + data[finish:], "remove exactly the private item including its AVR gate")
                        for match in re.finditer(r'(?:^|\n)[ \t]*#\[test\]\n', data.decode()):
                            start = match.end()
                            finish = end(data.decode(), start)
                            block = data.decode()[start:finish].encode()
                            if b'the_flagship_board_is_the_case_this_came_from' not in block and b'private_nep_topology_installs_edge_exact_parallel_memory' not in block:
                                self.assertIn(block, output, "public Rust tests must be byte-identical")
                    else:
                        for block in re.split(rb'(?m)(?=^#{2,3} )', data):
                            heading = block.split(b'\n', 1)[0]
                            if re.search(rb'DSL|Logical re-layout|Routing|Power supplies|Net drives|Rail suppression|Component overrides|Initial-state fuzzing|Component tolerances|Assertions:|Boot coverage|provenance|Step [234]:|trap:|test that proves|Honest notes', heading):
                                protected = block.replace(b' (see the brownout below)', b'')
                                # Step 4 also contains a separate private recalled-values aside.
                                protected = re.sub(rb' \(The recalled Tarski\b.*?\)', b'', protected, flags=re.S)
                                self.assertIn(protected, output, "public document section must be byte-identical")
                # The same bytes at an unclassified path receive only the existing
                # home redaction; private rules must never become global rewrites.
                self.assertEqual(curate(b'negative-control', data), data.replace(b'/Users/hauksbee-user', b'/Users/hauksbee-user'))

    def private_installer_fixture(
        self, *, include_mcp: bool = True, bad_binary_version: str | None = None
    ) -> tuple[str, bytes, bytes]:
        system = subprocess.check_output(["uname", "-s"], text=True).strip()
        machine = subprocess.check_output(["uname", "-m"], text=True).strip()
        os_slug = {"Linux": "linux", "Darwin": "darwin"}[system]
        arch_slug = {
            "x86_64": "x86_64",
            "aarch64": "aarch64",
            "arm64": "arm64",
        }[machine]
        asset = f"hauksbee-0.1.0-{os_slug}-{arch_slug}.tar.gz"
        root = asset.removesuffix(".tar.gz")
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
            binaries = ["hauksbee", "hauksbee-ci"]
            if include_mcp:
                binaries.append("hauksbee-mcp")
            for binary in binaries:
                reported = "9.9.9" if binary == bad_binary_version else "0.1.0"
                content = (
                    b"#!/usr/bin/env bash\n"
                    b'test -z "${HAUKSBEE_GITHUB_TOKEN:-}${GITHUB_TOKEN:-}${GH_TOKEN:-}"\n'
                    + f'printf "%s {reported} (git 0123456789abcdef0123456789abcdef01234567)\\n" "$(basename "$0")"\n'.encode()
                )
                info = tarfile.TarInfo(f"{root}/bin/{binary}")
                info.mode = 0o755
                info.size = len(content)
                archive.addfile(info, io.BytesIO(content))
        tarball = buffer.getvalue()
        checksum = f"{hashlib.sha256(tarball).hexdigest()}  {asset}\n".encode()
        return asset, tarball, checksum

    def run_private_installer(
        self,
        *,
        token: str | None,
        corrupt_asset: bool = False,
        existing_install: bool = False,
        fail_commit_binary: str | None = None,
        immutable_release: bool = True,
        active_install_lock: bool = False,
        seeded_journal: str | None = None,
        include_mcp: bool = True,
        fail_atomic_link: bool = False,
        cross_user_install_lock: bool = False,
        stale_install_lock: bool = False,
        bad_binary_version: str | None = None,
    ) -> tuple[
        subprocess.CompletedProcess[str],
        list[tuple[str, str, str]],
        dict[str, bytes],
    ]:
        asset, tarball, checksum = self.private_installer_fixture(
            include_mcp=include_mcp, bad_binary_version=bad_binary_version
        )
        # GitHub's immutable-release digests describe the published bytes; a
        # corrupted download is served with the GENUINE digests, the way real
        # tampering or truncation would present.
        genuine_tarball = tarball
        if corrupt_asset:
            tarball += b"corrupt-after-checksum"
        # Public policy: without a token the installer must send NO
        # Authorization header at all; with one it must send it on every
        # request. The mock enforces whichever contract applies.
        expected_auth = None if token is None else f"Bearer {token}"
        release_sha = "0123456789abcdef0123456789abcdef01234567"
        requests: list[tuple[str, str, str]] = []

        class Handler(BaseHTTPRequestHandler):
            def do_GET(handler) -> None:  # noqa: N802 - stdlib callback name
                requests.append(
                    (
                        handler.path,
                        handler.headers.get("Authorization", ""),
                        handler.headers.get("Accept", ""),
                    )
                )
                auth = handler.headers.get("Authorization", "")
                if (expected_auth is None and auth) or (
                    expected_auth is not None and auth != expected_auth
                ):
                    handler.send_response(401)
                    handler.end_headers()
                    return
                port = handler.server.server_port
                release = json.dumps(
                    {
                        "tag_name": "v0.1.0",
                        "immutable": immutable_release,
                        "assets": [
                            {
                                "name": asset,
                                "digest": "sha256:"
                                + hashlib.sha256(genuine_tarball).hexdigest(),
                                "url": f"http://127.0.0.1:{port}/repos/hauksbee-dev/hauksbee/releases/assets/101",
                            },
                            {
                                "name": f"{asset}.sha256",
                                "digest": "sha256:"
                                + hashlib.sha256(checksum).hexdigest(),
                                "url": f"http://127.0.0.1:{port}/repos/hauksbee-dev/hauksbee/releases/assets/102",
                            },
                        ],
                    }
                ).encode()
                body = {
                    "/repos/hauksbee-dev/hauksbee/releases/tags/v0.1.0": release,
                    "/repos/hauksbee-dev/hauksbee/commits/v0.1.0": json.dumps(
                        {"sha": release_sha}
                    ).encode(),
                    "/repos/hauksbee-dev/hauksbee/releases/assets/101": tarball,
                    "/repos/hauksbee-dev/hauksbee/releases/assets/102": checksum,
                }.get(handler.path)
                if body is None:
                    handler.send_response(404)
                    handler.end_headers()
                    return
                if "/assets/" in handler.path and handler.headers.get("Accept") != "application/octet-stream":
                    handler.send_response(406)
                    handler.end_headers()
                    return
                handler.send_response(200)
                handler.send_header("Content-Type", "application/json")
                handler.send_header("Content-Length", str(len(body)))
                handler.end_headers()
                handler.wfile.write(body)

            def log_message(self, *_args: object) -> None:
                return

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as raw_tmp:
                env = os.environ.copy()
                env.pop("GITHUB_TOKEN", None)
                env.pop("HAUKSBEE_GITHUB_TOKEN", None)
                env["HAUKSBEE_API_BASE"] = (
                    f"http://127.0.0.1:{server.server_port}/repos/hauksbee-dev/hauksbee"
                )
                env.pop("HAUKSBEE_RELEASES_BASE", None)
                if token is not None:
                    env["HAUKSBEE_GITHUB_TOKEN"] = token
                fake_bin = Path(raw_tmp) / "fake-bin"
                fake_bin.mkdir()
                if fail_atomic_link:
                    fake_ln = fake_bin / "ln"
                    fake_ln.write_text("#!/usr/bin/env bash\nexit 70\n")
                    fake_ln.chmod(0o755)
                if cross_user_install_lock:
                    fake_ps = fake_bin / "ps"
                    fake_ps.write_text("#!/usr/bin/env bash\nprintf '999999\\n'\n")
                    fake_ps.chmod(0o755)
                if fail_commit_binary is not None:
                    fake_mv = fake_bin / "mv"
                    fake_mv.write_text(
                        "#!/usr/bin/env bash\n"
                        "set -euo pipefail\n"
                        'for arg in "$@"; do\n'
                        f'  if [[ "$arg" == */new-{fail_commit_binary} ]] '
                        '&& [ ! -e "$HAUKSBEE_FAKE_MV_MARKER" ]; then\n'
                        '    : > "$HAUKSBEE_FAKE_MV_MARKER"\n'
                        "    exit 70\n"
                        "  fi\n"
                        "done\n"
                        'exec /bin/mv "$@"\n'
                    )
                    fake_mv.chmod(0o755)
                    env["HAUKSBEE_FAKE_MV_MARKER"] = str(Path(raw_tmp) / "mv-failed")
                env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
                prefix = Path(raw_tmp) / "prefix"
                if existing_install:
                    install_dir = prefix / "bin"
                    install_dir.mkdir(parents=True)
                    for binary in ("hauksbee", "hauksbee-ci", "hauksbee-mcp"):
                        path = install_dir / binary
                        path.write_bytes(f"old-{binary}\n".encode())
                        path.chmod(0o755)
                if active_install_lock or stale_install_lock:
                    install_dir = prefix / "bin"
                    install_dir.mkdir(parents=True, exist_ok=True)
                    (install_dir / ".hauksbee-install.lock").write_text(
                        f"{999999 if (cross_user_install_lock or stale_install_lock) else os.getpid()}\nACTIVE-OWNER-TOKEN\n"
                    )
                if seeded_journal is not None:
                    install_dir = prefix / "bin"
                    install_dir.mkdir(parents=True, exist_ok=True)
                    journal = install_dir / ".hauksbee-install.seeded"
                    journal.mkdir()
                    for binary in ("hauksbee", "hauksbee-ci", "hauksbee-mcp"):
                        live = install_dir / binary
                        live.write_bytes(f"new-{binary}\n".encode())
                        live.chmod(0o755)
                        (journal / f"old-{binary}").write_bytes(f"old-{binary}\n".encode())
                        (journal / f"installing-{binary}").touch()
                    if seeded_journal == "committed":
                        (journal / "committed").touch()
                    env["HAUKSBEE_TEST_EXIT_AFTER_RECOVERY"] = "1"
                result = subprocess.run(
                    [
                        "bash",
                        str(ROOT / "scripts/get-hauksbee.sh"),
                        "--version",
                        "v0.1.0",
                        "--prefix",
                        str(prefix),
                    ],
                    cwd=ROOT,
                    env=env,
                    text=True,
                    capture_output=True,
                    check=False,
                )
                installed = {
                    path.name: path.read_bytes()
                    for path in (prefix / "bin").glob("hauksbee*")
                    if path.is_file()
                }
                installed["transaction_dirs"] = "\n".join(
                    sorted(path.name for path in (prefix / "bin").glob(".hauksbee-install.*"))
                ).encode()
                lock = prefix / "bin/.hauksbee-install.lock"
                installed["install_lock"] = lock.read_bytes() if lock.is_file() else b""
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
        return result, requests, installed

    def test_public_installer_downloads_without_credential(self) -> None:
        result, requests, _installed = self.run_private_installer(token=None)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            [(path, auth) for path, auth, _accept in requests],
            [
                ("/repos/hauksbee-dev/hauksbee/releases/tags/v0.1.0", ""),
                ("/repos/hauksbee-dev/hauksbee/releases/assets/101", ""),
                ("/repos/hauksbee-dev/hauksbee/releases/assets/102", ""),
                ("/repos/hauksbee-dev/hauksbee/commits/v0.1.0", ""),
            ],
            "tokenless install must send no Authorization header anywhere",
        )

    def test_optional_token_authenticates_every_request(self) -> None:
        result, requests, _installed = self.run_private_installer(token="installer-token")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            requests,
            [
                (
                    "/repos/hauksbee-dev/hauksbee/releases/tags/v0.1.0",
                    "Bearer installer-token",
                    "application/vnd.github+json",
                ),
                (
                    "/repos/hauksbee-dev/hauksbee/releases/assets/101",
                    "Bearer installer-token",
                    "application/octet-stream",
                ),
                (
                    "/repos/hauksbee-dev/hauksbee/releases/assets/102",
                    "Bearer installer-token",
                    "application/octet-stream",
                ),
                (
                    "/repos/hauksbee-dev/hauksbee/commits/v0.1.0",
                    "Bearer installer-token",
                    "application/vnd.github+json",
                ),
            ],
        )

        end_to_end = (ROOT / "scripts/test-install-mock.sh").read_text()
        self.assertNotIn("HAUKSBEE_RELEASES_BASE", end_to_end)
        self.assertIn("/releases/assets/101", end_to_end)
        self.assertIn('"assets"', end_to_end)
        self.assertNotIn("installer itself needs only `curl`", (ROOT / "README.md").read_text())

    def test_private_installer_refuses_corrupt_api_asset_bytes(self) -> None:
        result, requests, _installed = self.run_private_installer(
            token="installer-token", corrupt_asset=True
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("do not match", (result.stdout + result.stderr).lower())
        self.assertEqual(len(requests), 3)

    def test_private_installer_refuses_a_mutable_release(self) -> None:
        result, requests, _installed = self.run_private_installer(
            token="installer-token", immutable_release=False
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("not immutable", (result.stdout + result.stderr).lower())
        self.assertEqual(len(requests), 1, "mutable release must refuse before assets")

    def test_private_installer_rolls_back_a_mid_commit_failure(self) -> None:
        result, _requests, installed = self.run_private_installer(
            token="installer-token",
            existing_install=True,
            fail_commit_binary="hauksbee-ci",
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        for binary in ("hauksbee", "hauksbee-ci", "hauksbee-mcp"):
            self.assertEqual(installed[binary], f"old-{binary}\n".encode())
        self.assertEqual(installed["transaction_dirs"], b"")

    def test_private_installer_removes_partial_fresh_install_on_commit_failure(self) -> None:
        result, _requests, installed = self.run_private_installer(
            token="installer-token",
            existing_install=False,
            fail_commit_binary="hauksbee-ci",
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            installed,
            {"transaction_dirs": b"", "install_lock": b""},
            "a failed fresh transaction must leave no installed subset",
        )

    def test_private_installer_probes_staged_binaries_before_replacing_live_files(self) -> None:
        result, _requests, installed = self.run_private_installer(
            token="installer-token",
            existing_install=True,
            bad_binary_version="hauksbee-ci",
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("existing install left untouched", result.stdout + result.stderr)
        for binary in ("hauksbee", "hauksbee-ci", "hauksbee-mcp"):
            self.assertEqual(installed[binary], f"old-{binary}\n".encode())
        self.assertEqual(installed["transaction_dirs"], b"")

    def test_unix_release_and_installer_bind_every_binary_to_one_commit(self) -> None:
        installer = (ROOT / "scripts/get-hauksbee.sh").read_text()
        release = (ROOT / ".github/workflows/release.yml").read_text()
        for binary in ("hauksbee", "hauksbee-ci", "hauksbee-mcp"):
            self.assertIn(f'for b in ${{BINARIES}}', installer)
            self.assertIn(binary, release)
        self.assertIn('expected_version="$b ${VERSION#v} (git $RELEASE_SHA)"', installer)
        self.assertIn('expected="$b $expected_version (git $expected_sha)"', release)

    def test_unix_installer_serializes_and_recovers_crash_journals(self) -> None:
        installer = (ROOT / "scripts/get-hauksbee.sh").read_text()
        self.assertIn('INSTALL_LOCK="${INSTALL_DIR}/.hauksbee-install.lock"', installer)
        self.assertIn("LOCK_OWNED=0", installer)
        self.assertIn('[ "$LOCK_OWNED" -eq 1 ] || return 0', installer)
        self.assertIn('ln "$candidate_lock" "$INSTALL_LOCK"', installer)
        self.assertIn('> "$TXN_DIR/committed"', installer)
        self.assertIn("kill -0", installer)
        self.assertIn("refusing concurrent replacement", installer)
        self.assertIn("recover_transaction", installer)
        self.assertIn("Multiple interrupted install journals", installer)
        self.assertLess(installer.index("acquire_install_lock"), installer.index('TXN_DIR="$(mktemp'))

    def test_rejected_unix_installer_cannot_delete_the_active_owners_lock(self) -> None:
        result, _requests, installed = self.run_private_installer(
            token="installer-token", active_install_lock=True
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("refusing concurrent replacement", result.stdout + result.stderr)
        self.assertEqual(
            installed["install_lock"],
            f"{os.getpid()}\nACTIVE-OWNER-TOKEN\n".encode(),
        )

    def test_unix_installer_recovers_every_binary_named_by_an_old_journal(self) -> None:
        result, _requests, installed = self.run_private_installer(
            token="installer-token", seeded_journal="uncommitted", include_mcp=False
        )
        self.assertEqual(result.returncode, 75, result.stdout + result.stderr)
        for binary in ("hauksbee", "hauksbee-ci", "hauksbee-mcp"):
            self.assertEqual(installed[binary], f"old-{binary}\n".encode())
        self.assertEqual(installed["transaction_dirs"], b"")

    def test_unix_installer_fails_instead_of_spinning_when_atomic_lock_is_unsupported(self) -> None:
        result, _requests, installed = self.run_private_installer(
            token="installer-token", fail_atomic_link=True
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("cannot atomically acquire", result.stdout + result.stderr)
        self.assertEqual(installed["install_lock"], b"")

    def test_cross_user_live_owner_is_not_mistaken_for_a_stale_lock(self) -> None:
        result, _requests, installed = self.run_private_installer(
            token="installer-token", active_install_lock=True, cross_user_install_lock=True
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("refusing concurrent replacement", result.stdout + result.stderr)
        self.assertEqual(installed["install_lock"], b"999999\nACTIVE-OWNER-TOKEN\n")

    def test_stale_lock_is_left_for_explicit_inspection_not_racy_reclamation(self) -> None:
        result, _requests, installed = self.run_private_installer(
            token="installer-token", stale_install_lock=True
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("automatic stale-lock reclamation is unsafe", result.stdout + result.stderr)
        self.assertEqual(installed["install_lock"], b"999999\nACTIVE-OWNER-TOKEN\n")

    def test_unix_installer_keeps_live_files_for_a_committed_crash_journal(self) -> None:
        result, _requests, installed = self.run_private_installer(
            token="installer-token", seeded_journal="committed", include_mcp=False
        )
        self.assertEqual(result.returncode, 75, result.stdout + result.stderr)
        for binary in ("hauksbee", "hauksbee-ci", "hauksbee-mcp"):
            self.assertEqual(installed[binary], f"new-{binary}\n".encode())
        self.assertEqual(installed["transaction_dirs"], b"")

    def test_powershell_installer_authenticates_asset_downloads(self) -> None:
        text = (ROOT / "scripts/get-hauksbee.ps1").read_text()
        self.assertIn("HAUKSBEE_GITHUB_TOKEN", text)
        self.assertIn('"$ApiBase/releases/tags/$Version"', text)
        self.assertIn("$matches[0].url", text)
        self.assertIn('"Accept" = "application/octet-stream"', text)
        self.assertIn("Get-FileHash -Algorithm SHA256", text)
        self.assertIn(".digest", text)
        self.assertIn("GitHub asset digest", text)
        self.assertNotIn("ReleasesBase", text)

    def test_unix_installer_disables_xtrace_before_reading_credentials(self) -> None:
        env = os.environ.copy()
        env["HAUKSBEE_GITHUB_TOKEN"] = "XTRACE-CONTRACT-SECRET"
        result = subprocess.run(
            ["bash", "-x", str(ROOT / "scripts/get-hauksbee.sh"), "--help"],
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("XTRACE-CONTRACT-SECRET", result.stdout + result.stderr)

    def test_action_source_fallback_uses_the_stock_runner_feature_set(self) -> None:
        action = (ROOT / "integrations/github-action/action.yml").read_text()
        build = action[action.index("- name: Build hauksbee (fallback build)") :]
        self.assertEqual(build.count("--no-default-features --features renode,qemu"), 2)
        self.assertIn("working-directory: .hauksbee", build)
        toolchain = action[action.index("- name: Install Rust toolchain (fallback build)") : action.index("- name: Cache cargo (fallback build)")]
        self.assertIn("toolchain: 1.92.0", toolchain)
        self.assertNotIn("toolchain: stable", toolchain)

    def test_action_release_version_fallback_checks_out_that_exact_release_sha(self) -> None:
        action = (ROOT / "integrations/github-action/action.yml").read_text()
        pick = action[
            action.index("- name: Pick prebuilt release tag") :
            action.index("- name: Fetch prebuilt hauksbee-ci")
        ]
        self.assertIn('echo "source-ref=$source_ref"', pick)
        self.assertIn('if [ -n "$release_sha" ]; then source_ref="$release_sha"; fi', pick)
        checkout = action[
            action.index("- name: Checkout hauksbee (fallback build)") :
            action.index("- name: Install Rust toolchain (fallback build)")
        ]
        self.assertIn("steps.reltag.outputs.source-ref || inputs.hauksbee-ref", checkout)
        image = action[action.index("- name: Run the hardware check (image)") :]
        self.assertIn("RELEASE_SHA: ${{ steps.reltag.outputs.expected-sha }}", image)

    def test_prebuilt_release_bytes_are_uncached_and_publisher_attested(self) -> None:
        action = (ROOT / "integrations/github-action/action.yml").read_text()
        self.assertIn('repos/$REPO/commits/$tag', action)
        self.assertIn(
            "release tag $tag resolves to $release_sha, not pinned ref $REF",
            action,
        )
        self.assertIn('[[ "$ci_version" == *"(git $release_sha)"* ]]', action)
        self.assertIn('[[ "$engine_version" == *"(git $release_sha)"* ]]', action)
        self.assertNotIn("key: hauksbee-prebuilt-", action)
        self.assertNotIn("~/.cache/hauksbee/prebuilt-archives", action)
        fallback_cache = action[
            action.index("- name: Cache cargo (fallback build)") :
            action.index("- name: Build hauksbee (fallback build)")
        ]
        self.assertNotIn(".hauksbee/target", fallback_cache)
        self.assertIn("$RUNNER_TEMP/hauksbee-release-archives", action)
        self.assertIn('gh release verify-asset "$TAG" "$tarball"', action)
        self.assertIn('--jq .immutable', action)
        release = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("Verify GitHub's immutable release attestations", release)
        self.assertNotIn("actions/attest@", release)
        self.assertIn("env -u GH_TOKEN -u GITHUB_TOKEN", action)
        self.assertNotIn("path: .hauksbee-prebuilt", action)
        self.assertIn('cached_sha256', action)
        self.assertIn('actual_asset_sha256', action)
        self.assertIn(
            'dl="$(mktemp -d "$RUNNER_TEMP/hauksbee-prebuilt-$platform.XXXXXX")"',
            action,
        )
        self.assertLess(action.index('actual_asset_sha256='), action.index('tar -xzf "$tarball"'))
        self.assertLess(
            action.index('simulator-provenance.py" archive "$tarball"'),
            action.index('tar -xzf "$tarball"'),
        )
        self.assertNotIn(".hauksbee-provenance", action)

    def test_prebuilt_platform_labels_match_release_assets(self) -> None:
        action = (ROOT / "integrations/github-action/action.yml").read_text()
        prebuilt = action[
            action.index("- name: Fetch prebuilt hauksbee-ci") :
            action.index("- name: Checkout hauksbee (fallback build)")
        ]
        self.assertIn("Linux/ARM64)             arch=aarch64", prebuilt)
        self.assertIn("macOS/ARM64)             arch=arm64", prebuilt)
        release = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("label: linux-aarch64", release)
        self.assertIn("label: darwin-arm64", release)

    def test_release_uploads_qc_report_and_can_reconcile_exact_prior_state(self) -> None:
        release = (ROOT / ".github/workflows/release.yml").read_text()
        stage = release[
            release.index("- name: Reconcile exact release state") :
            release.index("- name: Verify GitHub's immutable release attestations")
        ]
        self.assertIn("dist/*.md", stage)
        self.assertIn("target_commitish: ${{ github.sha }}", stage)
        self.assertIn("already-published=true", stage)
        self.assertIn("gh release delete", stage)
        self.assertIn("steps.release-state.outputs.already-published != 'true'", stage)

    def test_bundle_resolves_default_version_before_exporting_release_tag(self) -> None:
        bundle = (ROOT / "scripts/bundle.sh").read_text()
        version = bundle.index("# Version: from the workspace Cargo.toml")
        tag = bundle.index('export HAUKSBEE_RELEASE_TAG="v$VERSION"')
        self.assertLess(version, tag)

    def test_binary_bundle_drops_every_private_flagship_example(self) -> None:
        bundle = (ROOT / "scripts/bundle.sh").read_text()
        for name in (
            "tarski_brownout.toml",
            "tarski_brownout_repaired.toml",
            "tarski_stage0_powerup.toml",
        ):
            self.assertIn(f'examples/ci-specs/{name}', bundle)
        self.assertIn("BOARD_LEAK", bundle)

    def test_trusted_workflow_run_report_can_read_its_artifact(self) -> None:
        readme = (ROOT / "integrations/github-action/README.md").read_text()
        report = readme[readme.index("# hauksbee-ci-report.yml") :]
        self.assertIn("permissions:\n  actions: read\n  checks: write", report)

    def test_registry_username_supports_pat_and_github_app_tokens(self) -> None:
        def resolve(token: str, explicit: str = "") -> subprocess.CompletedProcess[str]:
            env = os.environ.copy()
            env["GH_TOKEN"] = token
            return subprocess.run(
                ["bash", str(REGISTRY_USER), explicit, "workflow-actor"],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertEqual(resolve("github_pat_example").stdout.strip(), "workflow-actor")
        self.assertEqual(resolve("ghs_example").stdout.strip(), "x-access-token")
        self.assertEqual(resolve("ghs_example", "explicit-user").stdout.strip(), "explicit-user")

    def test_public_action_needs_no_credential_and_pins_one_commit(self) -> None:
        action = (ROOT / "integrations/github-action/action.yml").read_text()
        # The token input survives as an optional override, defaulting to the
        # calling workflow's own token for public downloads.
        self.assertIn("hauksbee-token:", action)
        self.assertNotIn("required: true", action)
        self.assertIn("GH_TOKEN: ${{ inputs.hauksbee-token || github.token }}", action)
        self.assertIn("token: ${{ inputs.hauksbee-token || github.token }}", action)

        readme = (ROOT / "integrations/github-action/README.md").read_text()
        self.assertIn("No secret and\nno token are needed", readme)
        self.assertIn(
            "uses: hauksbee-dev/hauksbee/integrations/github-action@", readme
        )
        self.assertNotIn("secrets.HAUKSBEE_READ_TOKEN", readme)
        self.assertNotIn(".hauksbee-action", readme)

        generated = (ROOT / "crates/hauksbee-ci/src/integrate.rs").read_text()
        self.assertNotIn("secrets.HAUKSBEE_READ_TOKEN", generated)
        self.assertNotIn("path: .hauksbee-action", generated)
        self.assertIn(
            "uses: hauksbee-dev/hauksbee/integrations/github-action@{release_commit}", generated
        )

        frontend_workflow = (ROOT / "frontend/src/lib/ci-workflow.ts").read_text()
        self.assertIn(
            "uses: ${ACTION_REPOSITORY}/integrations/github-action@${RELEASE_COMMIT}",
            frontend_workflow,
        )
        self.assertNotIn("PRIVATE_TOKEN_SECRET", frontend_workflow)
        version = (ROOT / "frontend/src/lib/version.ts").read_text()
        self.assertNotIn("secrets.HAUKSBEE_READ_TOKEN", version)

        checkout_sources = {
            "integrations/github-action/action.yml": 1,
            "integrations/github-action/README.md": 1,
            "integrations/github-action/example-workflow.yml": 2,
            "crates/hauksbee-ci/src/integrate.rs": 2,
            "frontend/src/lib/ci-workflow.ts": 1,
        }
        for relative, expected in checkout_sources.items():
            text = (ROOT / relative).read_text()
            with self.subTest(path=relative):
                self.assertEqual(
                    text.count("persist-credentials: false"),
                    expected,
                    "every checkout must erase its credential after checkout",
                )

        # When a dedicated token IS provided for a private mirror, the
        # registry login is confined, cleaned up, and never argv-visible.
        registry_auth = (ROOT / "integrations/github-action/action.yml").read_text()
        self.assertIn('mktemp -d "$RUNNER_TEMP/hauksbee-docker-auth.XXXXXX"', registry_auth)
        self.assertIn("docker logout ghcr.io", registry_auth)
        self.assertIn("Cleanup registry credential", registry_auth)
        self.assertIn("if: ${{ always()", registry_auth)
        self.assertIn(
            "inputs.use-image == 'true' && inputs.hauksbee-token != ''", registry_auth
        )
        login = registry_auth[
            registry_auth.index("Authenticate to the image registry") :
            registry_auth.index("\n    - name: Run the hardware check (image)")
        ]
        self.assertLess(login.index("set +x"), login.index('printf \'%s\' "$GH_TOKEN"'))

    def test_release_build_jobs_do_not_retain_publication_credentials(self) -> None:
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        publication = workflow.index("\n  release:\n")
        build_half = workflow[:publication]
        release_half = workflow[publication:]
        self.assertRegex(
            workflow,
            r"(?m)^permissions:\s*\n(?:\s*#.*\n)*\s+contents:\s*read\b",
        )
        self.assertRegex(
            release_half,
            r"(?m)^    permissions:\s*\n\s+contents:\s*write\b",
        )
        checkout_count = build_half.count("uses: actions/checkout@")
        self.assertGreater(checkout_count, 0)
        self.assertEqual(
            build_half.count("persist-credentials: false"),
            checkout_count,
            "every release build checkout must erase even its read credential before dependencies run",
        )

    def test_macos_release_imports_ephemeral_signing_credentials_and_cleans_up(self) -> None:
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        import_start = workflow.index("- name: Import macOS Developer ID credentials")
        import_end = workflow.index("- name: Install Rust toolchain", import_start)
        import_step = workflow[import_start:import_end]
        cleanup_start = workflow.index("- name: Clean up macOS signing keychain")
        cleanup_end = workflow.index("\n  build-windows:", cleanup_start)
        cleanup_step = workflow[cleanup_start:cleanup_end]
        for secret in (
            "HAUKSBEE_SIGN_IDENTITY",
            "HAUKSBEE_SIGNING_CERTIFICATE_BASE64",
            "HAUKSBEE_SIGNING_CERTIFICATE_PASSWORD",
            "HAUKSBEE_NOTARY_APPLE_ID",
            "HAUKSBEE_NOTARY_TEAM_ID",
            "HAUKSBEE_NOTARY_PASSWORD",
        ):
            with self.subTest(secret=secret):
                self.assertIn(f"secrets.{secret}", import_step)
        for contract in (
            "security create-keychain",
            "security unlock-keychain",
            "base64 -D",
            "security import",
            "security set-key-partition-list",
            "security find-identity -v -p codesigning",
            "HAUKSBEE_SIGNING_KEYCHAIN=",
            "HAUKSBEE_SIGNING_STATE_DIR=",
        ):
            with self.subTest(contract=contract):
                self.assertIn(contract, import_step)
        self.assertIn("if: ${{ always() && runner.os == 'macOS' }}", cleanup_step)
        for contract in (
            "security list-keychains -d user -s",
            "security delete-keychain",
            "unlink",
        ):
            with self.subTest(cleanup=contract):
                self.assertIn(contract, cleanup_step)

        signing_doc = (ROOT / "app/macos/SIGNING.md").read_text()
        for secret in (
            "HAUKSBEE_SIGN_IDENTITY",
            "HAUKSBEE_SIGNING_CERTIFICATE_BASE64",
            "HAUKSBEE_SIGNING_CERTIFICATE_PASSWORD",
            "HAUKSBEE_NOTARY_APPLE_ID",
            "HAUKSBEE_NOTARY_TEAM_ID",
            "HAUKSBEE_NOTARY_PASSWORD",
        ):
            self.assertIn(secret, signing_doc)

    def test_shipped_installer_examples_use_the_public_one_liner(self) -> None:
        one_liner = (
            "curl -fsSL https://raw.githubusercontent.com/hauksbee-dev/hauksbee/"
            "main/scripts/get-hauksbee.sh | bash"
        )
        for relative in (
            Path("README.md"),
            Path("docs/START_HERE.md"),
            Path("crates/hauksbee-mcp/README.md"),
            Path("frontend/src/demo/DemoApp.tsx"),
        ):
            text = (ROOT / relative).read_text()
            with self.subTest(path=relative):
                self.assertIn(one_liner, text)
                # The private-era authenticated contents-API bootstrap must not
                # resurface anywhere an installer example ships.
                self.assertNotIn("export HAUKSBEE_GITHUB_TOKEN", text)
                self.assertNotIn(
                    "api.github.com/repos/hauksbee-dev/hauksbee/contents/", text
                )

    def test_mirror_rejects_retained_scripts_with_missing_operational_dependencies(self) -> None:
        manifest = self.surface_manifest()
        launcher = next(
            item for item in manifest["surfaces"] if item["path"] == "scripts/make-public.sh"
        )
        self.assertEqual(launcher.get("scopes"), ["development"])
        builder = (ROOT / "scripts/build-public-mirror.sh").read_text()
        self.assertIn("scripts/make-public.sh", builder)
        self.assertIn("check-mirror-dependencies.py .", builder)

        with tempfile.TemporaryDirectory() as raw_tmp:
            mirror = Path(raw_tmp)
            scripts = mirror / "scripts"
            scripts.mkdir()
            launcher_fixture = scripts / "launcher.sh"
            launcher_fixture.write_text(
                '#!/usr/bin/env bash\nbash "$HAUKSBEE_ROOT/scripts/missing-builder.sh"\n'
            )
            launcher_fixture.chmod(0o755)
            bad = subprocess.run(
                ["python3", str(MIRROR_DEPENDENCY_CHECKER), str(mirror)],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(bad.returncode, 0, bad.stdout + bad.stderr)
            self.assertIn("scripts/missing-builder.sh", bad.stderr)
            launcher_fixture.unlink()
            good = subprocess.run(
                ["python3", str(MIRROR_DEPENDENCY_CHECKER), str(mirror)],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(good.returncode, 0, good.stdout + good.stderr)

    def test_b3_names_the_real_qc_path_and_manual_failure_contract(self) -> None:
        tasks = (ROOT / "docs/dev-plans/tasks.md").read_text()
        start_match = re.search(r"(?m)^- \[[ x~]\] B3 ", tasks)
        end_match = re.search(r"(?m)^- \[[ x~]\] B4 ", tasks)
        self.assertIsNotNone(start_match)
        self.assertIsNotNone(end_match)
        assert start_match is not None and end_match is not None
        start = start_match.start()
        end = end_match.start()
        b3 = tasks[start:end]
        self.assertIn("qc/scenarios/", b3)
        self.assertIn("qc/results/<timestamp>/report.md", b3)
        self.assertIn("exits non-zero", b3)
        self.assertIn("manually", b3)
        self.assertNotIn("files a defect", b3)

    def test_docker_publication_checks_private_repo_and_package_before_and_after(self) -> None:
        workflow = (ROOT / ".github/workflows/docker.yml").read_text()
        probe = workflow.index("Probe private container publication")
        bootstrap = workflow.index("Bootstrap private GHCR package")
        before = workflow.index("Check private container publication before push")
        slim = workflow.index("Build and push slim")
        full = workflow.index("Build and push full")
        after = workflow.index("Check private container publication after push")
        self.assertLess(probe, bootstrap)
        self.assertLess(bootstrap, before)
        self.assertLess(before, slim)
        self.assertLess(full, after)
        self.assertIn("if: always()", workflow[after : after + 300])
        self.assertIn("FROM scratch", workflow[bootstrap:before])
        self.assertNotIn("COPY", workflow[bootstrap:before])
        self.assertEqual(workflow.count("check-private-container-publication.sh"), 3)

    def test_docker_publication_binds_source_actions_and_slim_base_immutably(self) -> None:
        workflow = (ROOT / ".github/workflows/docker.yml").read_text()
        for line in workflow.splitlines():
            match = re.search(r"(?:^|-)\s*uses:\s*[^#\s]+@([^\s#]+)", line)
            if match:
                self.assertRegex(
                    match.group(1),
                    r"^[0-9a-f]{40}$",
                    f"mutable action in docker workflow: {line.strip()}",
                )
        self.assertIn("github.event.inputs.tag || github.ref", workflow)
        self.assertIn(
            "tag-push checkout $source_sha does not match immutable event SHA",
            workflow,
        )
        self.assertIn('refs/tags/$raw^{commit}', workflow)
        self.assertIn("source_sha=$source_sha", workflow)
        self.assertIn("id: slim", workflow)
        self.assertIn("SLIM_IMAGE=${{ env.IMAGE }}@${{ steps.slim.outputs.digest }}", workflow)
        self.assertNotIn("SLIM_IMAGE=${{ env.IMAGE }}:slim-", workflow)
        # Derived from the verified version, not the git ref, so a manual
        # reconcile run of an existing tag records under the original name.
        self.assertIn('digest_tag="container-digests-v${VERSION}"', workflow)
        self.assertNotIn('digest_tag="container-digests-${GITHUB_REF_NAME}"', workflow)
        self.assertNotIn('digest_tag="${GITHUB_REF_NAME}-docker-digests"', workflow)
        self.assertIn('gh release create "$digest_tag" "$manifest" --repo "$GH_REPO"', workflow)
        # The workflow token may not mint refs at explicit commits (403 from
        # both a bare-SHA target_commitish and a git/refs create), so the
        # record's release is created with no target and the source binding
        # is carried by the verified manifest instead.
        self.assertNotIn('--target "$SOURCE_SHA"', workflow)
        self.assertNotIn('-f ref="refs/tags/$digest_tag"', workflow)
        self.assertIn('recorded_source="$(awk', workflow)
        self.assertIn("--prerelease --latest=false", workflow)
        self.assertIn('gh release edit "$digest_tag" --draft=false --repo "$GH_REPO"', workflow)
        self.assertIn('gh release verify-asset "$digest_tag" "$manifest" --repo "$GH_REPO"', workflow)
        self.assertIn('gh release delete "$digest_tag" --repo "$GH_REPO" --yes', workflow)
        self.assertIn("could not prove digest release absence", workflow)
        slim = (ROOT / "docker/Dockerfile.slim").read_text()
        self.assertIn("grep -Eq '^[0-9a-f]{40}$'", slim)
        from_lines = [line for line in slim.splitlines() if line.startswith("FROM ")]
        self.assertGreaterEqual(len(from_lines), 3)
        pinned_bases = dict(
            re.findall(r"(?m)^ARG ([A-Z_]+)=([^\n]+@sha256:[0-9a-f]{64})$", slim)
        )
        for line in from_lines:
            match = re.match(r"FROM \$\{([A-Z_]+)\}", line)
            self.assertIsNotNone(match, f"mutable base image in {line}")
            self.assertIn(match.group(1), pinned_bases, f"unpinned base in {line}")

    def test_manual_release_dispatch_builds_the_requested_tag_source(self) -> None:
        release = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("resolve-source:", release)
        self.assertEqual(release.count("github.event.inputs.tag || github.ref"), 1)
        self.assertIn('tag_sha="$(git rev-parse "refs/tags/$tag^{commit}")"', release)
        self.assertIn('[ "$source_sha" = "$EVENT_SHA" ]', release)
        self.assertGreaterEqual(
            release.count("ref: ${{ needs.resolve-source.outputs.source_sha }}"), 4
        )
        self.assertGreaterEqual(
            release.count("EXPECTED_SHA: ${{ needs.resolve-source.outputs.source_sha }}"), 4
        )
        self.assertIn("checked-out source $source_sha does not match captured release source", release)
        self.assertIn("checked-out source $sourceSha does not match captured release source", release)
        self.assertGreaterEqual(
            release.count("does not match workspace package"), 2
        )
        self.assertIn('Source commit: `${{ github.sha }}`', release)
        self.assertIn("draft: true", release)
        self.assertIn(
            'gh release edit "$GITHUB_REF_NAME" --draft=false --repo "$GITHUB_REPOSITORY"',
            release,
        )
        self.assertIn("REPLACE_WITH_RELEASE_COMMIT_SHA", release)

        docker = (ROOT / ".github/workflows/docker.yml").read_text()
        self.assertIn(
            "github.event.inputs.tag || github.ref", docker
        )
        self.assertNotIn(
            "github.event.inputs.tag || github.sha", release + docker
        )

    def test_release_runners_and_setup_preserve_a_clean_tag_checkout(self) -> None:
        release = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertNotIn("macos-13", release)
        self.assertEqual(release.count("macos-15-intel"), 3)
        self.assertNotIn("chmod +x hauksbee/scripts/*.sh", release)
        self.assertIn(
            'bash scripts/install-sims.sh --avr --prefix "$simavr_prefix"',
            release,
        )
        self.assertLess(
            release.index("Build and install libsimavr"),
            release.index("Resolve version"),
        )

    def test_full_container_verifies_every_download_and_carries_licenses(self) -> None:
        dockerfile = (ROOT / "docker/Dockerfile.full").read_text()
        args = dict(
            re.findall(r"(?m)^ARG ([A-Z0-9_]+)=([0-9a-f]{64})$", dockerfile)
        )
        for knob in (
            "RENODE_AMD64_SHA256",
            "RENODE_ARM64_SHA256",
            "QEMU_XTENSA_AMD64_SHA256",
            "QEMU_XTENSA_ARM64_SHA256",
            "QEMU_RISCV32_AMD64_SHA256",
            "QEMU_RISCV32_ARM64_SHA256",
            "FREEROUTING_SHA256",
        ):
            self.assertRegex(dockerfile, rf"ARG {knob}=[0-9a-f]{{64}}")
        def checksums(relative: str) -> dict[str, str]:
            return {
                name: digest
                for line in (ROOT / relative).read_text().splitlines()
                if line and not line.startswith("#")
                for digest, name in [line.split()]
            }

        renode = checksums("scripts/renode-checksums.txt")
        qemu = checksums("scripts/espressif-qemu-checksums.txt")
        freerouting = checksums("scripts/freerouting-checksums.txt")
        self.assertEqual(args["RENODE_AMD64_SHA256"], renode["renode-1.16.1.linux-portable-dotnet.tar.gz"])
        self.assertEqual(args["RENODE_ARM64_SHA256"], renode["renode-1.16.1.linux-arm64-portable-dotnet.tar.gz"])
        self.assertEqual(args["QEMU_XTENSA_AMD64_SHA256"], qemu["qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz"])
        self.assertEqual(args["QEMU_XTENSA_ARM64_SHA256"], qemu["qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-aarch64-linux-gnu.tar.xz"])
        self.assertEqual(args["QEMU_RISCV32_AMD64_SHA256"], qemu["qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz"])
        self.assertEqual(args["QEMU_RISCV32_ARM64_SHA256"], qemu["qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-aarch64-linux-gnu.tar.xz"])
        self.assertEqual(args["FREEROUTING_SHA256"], freerouting["freerouting-1.9.0.jar"])
        for verification in (
            "printf '%s  %s\\n' \"$rsha\" /tmp/renode.tar.gz | sha256sum -c -;",
            "printf '%s  %s\\n' \"$qsha\" /tmp/qemu.tar.xz | sha256sum -c -;",
            "printf '%s  %s\\n' \"$FREEROUTING_SHA256\" \"/opt/freerouting-${FREEROUTING_VERSION}.jar\" | sha256sum -c -",
            "printf '%s  %s\\n' \"$RENODE_LICENSE_SHA256\" /usr/share/doc/hauksbee/third-party/RENODE-LICENSE | sha256sum -c -;",
            "printf '%s  %s\\n' \"$QEMU_LICENSE_SHA256\" /usr/share/doc/hauksbee/third-party/QEMU-COPYING | sha256sum -c -;",
            "printf '%s  %s\\n' \"$FREEROUTING_LICENSE_SHA256\" /usr/share/doc/hauksbee/third-party/FREEROUTING-LICENSE | sha256sum -c -",
        ):
            self.assertIn(verification, dockerfile)
        workflow = (ROOT / ".github/workflows/docker.yml").read_text()
        self.assertIn("Verify slim and full image contents and labels", workflow)
        for platform in ("linux/amd64", "linux/arm64"):
            self.assertIn(platform, workflow)
        for command in (
            "hauksbee --version",
            "hauksbee-ci --version",
            "renode --version",
            "qemu-system-xtensa --version",
            "qemu-system-riscv32 --version",
            'grep -F "Freerouting v1.9.0"',
        ):
            self.assertIn(command, workflow)
        verify = workflow.index("Verify slim and full image contents and labels")
        record = workflow.index("Publish or reconcile immutable digest manifest")
        promote = workflow.index("Promote only the recorded immutable image digests")
        post = workflow.index("Check private container publication after push")
        self.assertIn("group: docker-${{ github.ref }}", workflow)
        self.assertIn("cancel-in-progress: false", workflow)
        self.assertLess(verify, record)
        self.assertLess(record, promote)
        self.assertIn("gh release verify-asset", workflow[record:promote])
        self.assertIn(
            'gh release verify "$digest_tag" --repo "$GITHUB_REPOSITORY"',
            workflow[record:promote],
        )
        self.assertIn("GH_REPO: ${{ github.repository }}", workflow[record:promote])
        self.assertIn('--jq .draft', workflow[record:promote])
        self.assertIn(
            'gh release edit "$digest_tag" --draft=false --repo "$GH_REPO"',
            workflow[record:promote],
        )
        self.assertIn("recorded_slim", workflow[record:promote])
        self.assertIn("recorded_full", workflow[record:promote])
        slim_build = workflow[workflow.index("Build and push slim") : workflow.index("Build and push full")]
        full_build = workflow[workflow.index("Build and push full") : verify]
        self.assertIn("slim-candidate-", slim_build)
        self.assertNotIn("${{ env.IMAGE }}:slim\n", slim_build)
        self.assertIn("full-candidate-", full_build)
        self.assertNotIn("${{ env.IMAGE }}:full\n", full_build)
        promotion = workflow[promote:]
        self.assertIn('"$SLIM_REF"', promotion)
        self.assertIn('"$FULL_REF"', promotion)
        self.assertIn('if [[ "$VERSION" == *-* ]]; then', promotion)
        self.assertIn('--tag "${IMAGE}:slim-${VERSION}"', promotion)
        self.assertIn('--tag "${IMAGE}:full-${VERSION}"', promotion)
        self.assertNotIn("SLIM_DIGEST", promotion)
        self.assertNotIn("FULL_DIGEST", promotion)
        for notice in ("RENODE-LICENSE", "QEMU-COPYING", "FREEROUTING-LICENSE"):
            self.assertIn(
                f"test -s /usr/share/doc/hauksbee/third-party/{notice}", workflow
            )
        self.assertIn("CORRESPONDING SOURCE OFFER", dockerfile)
        self.assertRegex(dockerfile, r"ARG ESP_QEMU_SOURCE_COMMIT=[0-9a-f]{40}")
        self.assertRegex(dockerfile, r"ARG FREEROUTING_SOURCE_COMMIT=[0-9a-f]{40}")
        self.assertIn("at least three years", dockerfile)
        self.assertIn("security@hauksbee.dev", dockerfile)
        self.assertIn(
            "test -s /usr/share/doc/hauksbee/third-party/SOURCE-OFFER.txt", workflow
        )
        self.assertIn("test -s /usr/share/doc/hauksbee/SOURCE-OFFER.txt", workflow)
        self.assertIn("SOURCE_COMMIT=${{ steps.ver.outputs.source_sha }}", workflow)
        self.assertIn(
            'org.opencontainers.image.licenses="GPL-3.0-only AND GPL-2.0-only AND MIT"',
            dockerfile,
        )

    def run_container_preflight(
        self,
        *,
        phase: str = "before",
        token: str | None = "container-token",
        repo_visibility: str = "public",
        package_visibility: str = "public",
    ) -> tuple[subprocess.CompletedProcess[str], str]:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            log = tmp / "gh.log"
            gh = tmp / "gh"
            gh.write_text(
                textwrap.dedent(
                    """\
                    #!/usr/bin/env bash
                    set -eu
                    printf '%s\\n' "$*" >> "$FAKE_GH_LOG"
                    [ "${GH_TOKEN:-}" = container-token ] || exit 65
                    if [ "$#" -eq 4 ] && [ "$1" = api ] && [ "$3" = --jq ] && [ "$4" = .visibility ]; then
                      case "$2" in
                        repos/hauksbee-dev/hauksbee)
                          [ "$FAKE_REPO_VISIBILITY" != inaccessible ] || exit 66
                          printf '%s\\n' "$FAKE_REPO_VISIBILITY"
                          ;;
                        *) exit 64 ;;
                      esac
                    elif [ "$#" -eq 3 ] && [ "$1" = api ] && [ "$2" = --include ] && [ "$3" = orgs/hauksbee-dev/packages/container/hauksbee ]; then
                      case "$FAKE_PACKAGE_VISIBILITY" in
                        inaccessible)
                          printf 'HTTP/2 403 Forbidden\\n\\n{"message":"forbidden"}\\n'
                          exit 66
                          ;;
                        missing)
                          printf 'HTTP/2 404 Not Found\\n\\n{"message":"not found"}\\n'
                          exit 1
                          ;;
                        *)
                          printf 'HTTP/2 200 OK\\n\\n{"visibility":"%s"}\\n' "$FAKE_PACKAGE_VISIBILITY"
                          ;;
                      esac
                    else
                      exit 64
                    fi
                    """
                )
            )
            gh.chmod(0o755)
            env = os.environ.copy()
            env.update(
                {
                    "PATH": f"{tmp}:{env['PATH']}",
                    "FAKE_GH_LOG": str(log),
                    "FAKE_REPO_VISIBILITY": repo_visibility,
                    "FAKE_PACKAGE_VISIBILITY": package_visibility,
                }
            )
            if token is None:
                env.pop("GH_TOKEN", None)
            else:
                env["GH_TOKEN"] = token
            result = subprocess.run(
                ["bash", str(CONTAINER_PREFLIGHT), phase],
                cwd=ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            return result, log.read_text() if log.exists() else ""

    def test_container_probe_bootstraps_only_an_inert_missing_package(self) -> None:
        existing, _calls = self.run_container_preflight(phase="probe")
        self.assertEqual(existing.returncode, 0, existing.stdout + existing.stderr)
        self.assertIn("bootstrap_required=false", existing.stdout)

        missing, _calls = self.run_container_preflight(
            phase="probe", package_visibility="missing"
        )
        self.assertEqual(missing.returncode, 0, missing.stdout + missing.stderr)
        self.assertIn("bootstrap_required=true", missing.stdout)

        inaccessible, _calls = self.run_container_preflight(
            phase="probe", package_visibility="inaccessible"
        )
        self.assertNotEqual(
            inaccessible.returncode, 0, inaccessible.stdout + inaccessible.stderr
        )
        self.assertNotIn("bootstrap_required=true", inaccessible.stdout)

        docs = (ROOT / "docs/ci/DOCKER.md").read_text()
        self.assertIn("privacy-bootstrap", docs)
        self.assertIn("no Hauksbee binaries or source", docs)

    def test_container_publication_preflight_is_public_and_read_only(self) -> None:
        for phase in ("before", "after"):
            with self.subTest(phase=phase):
                result, calls = self.run_container_preflight(phase=phase)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(
                    calls.splitlines(),
                    [
                        "api repos/hauksbee-dev/hauksbee --jq .visibility",
                        "api --include orgs/hauksbee-dev/packages/container/hauksbee",
                    ],
                )

        # A just-bootstrapped package is private until a maintainer flips it;
        # the pre-push check must tolerate that while the post-push check
        # refuses to call the publication done.
        bootstrap_window, _calls = self.run_container_preflight(
            phase="before", package_visibility="private"
        )
        self.assertEqual(
            bootstrap_window.returncode,
            0,
            bootstrap_window.stdout + bootstrap_window.stderr,
        )

        for kwargs, phrase in (
            ({"token": None}, "GH_TOKEN"),
            ({"repo_visibility": "private"}, "repository"),
            ({"repo_visibility": "inaccessible"}, "repository"),
            ({"phase": "after", "package_visibility": "private"}, "package"),
            ({"package_visibility": "missing"}, "package"),
            ({"package_visibility": "inaccessible"}, "package"),
        ):
            with self.subTest(kwargs=kwargs):
                result, _calls = self.run_container_preflight(**kwargs)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(phrase.lower(), (result.stdout + result.stderr).lower())

    def test_pcm_is_manual_install_from_file_until_officially_listed(self) -> None:
        readme = (ROOT / "integrations/kicad-plugin/README.md").read_text()
        self.assertNotIn("official listing", readme.lower())
        self.assertNotIn("pcm handles updates", readme.lower())
        # Public state: the download needs no credential, and no token env
        # var is prescribed in the instructions.
        self.assertNotIn("GH_TOKEN", readme)
        self.assertIn("no credential needed", readme)
        self.assertIn("gh release download", readme)
        self.assertIn("Install from File", readme)

        builder = (ROOT / "integrations/kicad-plugin/build-pcm.sh").read_text()
        self.assertNotIn("registry listing", builder)
        self.assertNotIn("download_url=", builder)

        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        build = workflow.index("Build KiCad PCM archive")
        upload = workflow.index("Upload build artifact")
        publish = workflow.index("Stage immutable GitHub Release draft")
        self.assertLess(build, upload)
        self.assertIn("integrations/kicad-plugin/build-pcm.sh", workflow[build:upload])
        self.assertIn("hauksbee-ci-pcm-v", workflow[build:upload])
        self.assertIn("hauksbee-ci-pcm-v*.zip", workflow[upload:publish])
        self.assertIn("hauksbee-ci-pcm-v*.zip.sha256", workflow[upload:publish])

    def test_semver_beta_tags_publish_only_as_prereleases(self) -> None:
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        publish = workflow.index("Stage immutable GitHub Release draft")
        verify = workflow.index("Verify GitHub's immutable release attestations")
        release_step = workflow[publish:verify]
        self.assertIn("prerelease: ${{ contains(github.ref_name, '-') }}", release_step)
        self.assertIn(
            "make_latest: ${{ contains(github.ref_name, '-') && 'false' || 'true' }}",
            release_step,
        )

    def test_consumers_keep_optional_credentials_out_of_argv(self) -> None:
        # The optional-token path in the installer must still feed the header
        # through curl's config stdin, never argv.
        installer = (ROOT / "scripts/get-hauksbee.sh").read_text()
        self.assertNotIn("curl --config -", installer)
        self.assertIn("curl -q --config -", installer)

        action = (ROOT / "integrations/github-action/action.yml").read_text()
        self.assertIn('asset manifest did not contain one sha256', action)
        self.assertIn('actual_asset_sha256=', action)
        self.assertIn('expected_asset_sha256=', action)
        self.assertIn('[[ "$IMAGE" =~ ^ghcr\\.io/${REPO}@sha256:[0-9a-f]{64}$ ]]', action)
        self.assertIn('digest_tag="container-digests-${release_tag}"', action)
        self.assertIn('gh release verify "$digest_tag" --repo "$REPO"', action)
        self.assertIn('image digest is not present in the immutable', action)
        self.assertIn('org.opencontainers.image.revision', action)
        self.assertIn('default: "ghcr.io/hauksbee-dev/hauksbee:slim"', action)
        image_step = action[
            action.index("- name: Run the hardware check (image)") :
            action.index("- name: Cleanup registry credential")
        ]
        self.assertIn(
            "GH_TOKEN: ${{ inputs.hauksbee-token || github.token }}", image_step
        )
        self.assertIn('image_flavor=slim', image_step)
        self.assertIn('image_flavor=full', image_step)

        slim = (ROOT / "docker/Dockerfile.slim").read_text()
        full = (ROOT / "docker/Dockerfile.full").read_text()
        self.assertIn("at least three years", slim)
        self.assertIn("Hauksbee and libsimavr", slim)
        self.assertIn("Hauksbee and libsimavr", full)

        # No beta release publishes images, so the doc must say a pull needs
        # the reader's own authenticated candidate build, and its login
        # guidance must keep the credential off argv (the bare `docker login`
        # prompt, never --password).
        docker_doc = (ROOT / "docs/ci/DOCKER.md").read_text()
        self.assertIn("Authenticate against the registry", docker_doc)
        self.assertIn("docker login ghcr.io", docker_doc)
        self.assertNotIn("docker login --password", docker_doc)

        recipes = (ROOT / "docs/ci/RECIPES.md").read_text()
        self.assertIn("nothing to pull\nanonymously", recipes)
        self.assertIn("protected secret store", recipes)
        for credential_contract in (
            "DOCKER_AUTH_CONFIG",
            "registryCredentialsId",
            "docker login --password-stdin",
        ):
            self.assertIn(credential_contract, recipes)

    def test_container_carries_exact_corresponding_source_without_repository_access(self) -> None:
        slim = (ROOT / "docker/Dockerfile.slim").read_text()
        self.assertIn("/usr/share/doc/hauksbee/source/hauksbee-source.tar.gz", slim)
        self.assertIn("/usr/share/doc/hauksbee/source/simavr-source.tar.gz", slim)
        self.assertIn("cargo vendor --locked --versioned-dirs", slim)
        self.assertIn("third-party/cargo-vendor", slim)
        self.assertIn("cargo build --release --locked --offline", slim)
        self.assertIn("cargo metadata --locked --offline", slim)
        self.assertIn(".cargo-checksum.json", slim)
        workflow = (ROOT / ".github/workflows/docker.yml").read_text()
        for archive in ("hauksbee-source.tar.gz", "simavr-source.tar.gz"):
            self.assertIn(f"test -s /usr/share/doc/hauksbee/source/{archive}", workflow)
        self.assertIn("third-party/cargo-vendor", workflow)
        self.assertIn("cargo-checksum", workflow)
        self.assertIn("tar -xOf", workflow)

    def test_release_serializes_and_retains_quality_source_and_evidence(self) -> None:
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("group: release-${{ github.ref }}", workflow)
        self.assertIn("cancel-in-progress: false", workflow)
        self.assertIn("release-quality-rust:", workflow)
        self.assertIn(
            "needs: [build, build-windows, required-integrations, release-quality-rust,\n"
            "            release-quality-shapes, release-quality-release-binaries,\n"
            "            release-quality-offline, release-quality-residue]",
            workflow,
        )
        for gate in (
            "cargo fmt --all --check",
            "cargo clippy --locked --workspace --all-targets -- -D warnings",
            "cargo test --locked --workspace",
            "python3 scripts/test-private-release-policy.py",
            "bun run test:unit",
            "bun run test:e2e",
            "bun run visual-lint",
            "qc/run.sh",
        ):
            self.assertIn(gate, workflow)
        self.assertIn("scripts/build-release-source.sh", workflow)
        self.assertIn("scripts/test-release-artifact-runtime.sh", workflow)
        self.assertIn("dist/*.json", workflow)
        self.assertIn('sha256sum "$evidence" > "$evidence.sha256"', workflow)
        self.assertIn("for asset in dist/*", workflow)

        source_builder = (ROOT / "scripts/build-release-source.sh").read_text()
        self.assertIn("cargo vendor --locked --versioned-dirs", source_builder)
        self.assertIn("third-party/cargo-vendor", source_builder)
        self.assertIn("third-party/simavr", source_builder)
        self.assertIn("actual_registry_packages", source_builder)
        bundle = (ROOT / "scripts/bundle.sh").read_text()
        app = (ROOT / "app/macos/build-app.sh").read_text()
        self.assertIn("hauksbee-${VERSION}-source.tar.gz", bundle)
        self.assertIn("hauksbee-${VERSION}-source.tar.gz", app)
        self.assertIn('expected_version="$bin $VERSION (git $GIT_SHA)"', app)
        self.assertIn("rebuild from this exact checkout before assembling the app", app)

        docker = (ROOT / ".github/workflows/docker.yml").read_text()
        dockerfile = (ROOT / "docker/Dockerfile.slim").read_text()
        self.assertIn("/opt/hauksbee/crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb", docker)
        self.assertIn("/opt/hauksbee/testdata/firmware/boot_gate_a/boot_gate.hex", docker)
        self.assertIn("--firmware /opt/hauksbee/testdata/firmware/boot_gate_a/boot_gate.hex", docker)
        self.assertIn("hauksbee-ci run boot_gate_pass.toml", docker)
        self.assertIn("/opt/hauksbee/crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb", dockerfile)
        full_dockerfile = (ROOT / "docker/Dockerfile.full").read_text()
        for fixture in ("stm32.kicad_pcb", "stm32.elf", "esp32.kicad_pcb", "esp32-flash.bin", "blinky.board"):
            self.assertIn(f"/opt/hauksbee/external-smoke/{fixture}", full_dockerfile)
        self.assertIn("/opt/hauksbee/external-smoke/stm32.elf", docker)
        self.assertIn("/opt/hauksbee/external-smoke/esp32-flash.bin", docker)
        self.assertIn("freerouting handoff", docker)
        self.assertIn("--route --route-passes 2", docker)

    def test_release_plans_do_not_advertise_a_public_installer_endpoint(self) -> None:
        for relative in (
            Path("docs/dev-plans/launch-video.md"),
            Path("docs/dev-plans/go-to-market.md"),
        ):
            with self.subTest(path=relative):
                self.assertNotIn("hauksbee.dev/install", (ROOT / relative).read_text())

    def test_release_contract_never_requires_public_repository_or_issues(self) -> None:
        forbidden_by_file = {
            ROOT / ".github/workflows/release.yml": (
                "public slug",
                "public repo",
            ),
            ROOT / "docs/dev-plans/launch-video.md": (
                "public repo",
                "repo is public",
            ),
            ROOT / "docs/dev-plans/prelaunch-c-plan.md": ("public issue",),
            ROOT / "docs/dev-plans/tasks.md": ("public issue",),
        }

        for path, forbidden_phrases in forbidden_by_file.items():
            text = path.read_text().lower()
            for phrase in forbidden_phrases:
                with self.subTest(path=path.relative_to(ROOT), phrase=phrase):
                    self.assertNotIn(
                        phrase,
                        text,
                        f"{path.relative_to(ROOT)} contradicts the private-only release policy",
                    )

    def release_preflight_body(self) -> str:
        lines = (ROOT / ".github/workflows/release.yml").read_text().splitlines(True)
        step = next(
            i
            for i, line in enumerate(lines)
            if line.startswith("      - name: Preflight the private release slug")
        )
        run = next(
            i
            for i in range(step, len(lines))
            if lines[i].startswith("        run:")
        )
        declaration = lines[run].strip()
        if declaration != "run: |":
            return declaration.removeprefix("run:").strip()
        end = next(
            (
                i
                for i in range(run + 1, len(lines))
                if lines[i].startswith("      - name:")
            ),
            len(lines),
        )
        return textwrap.dedent("".join(lines[run + 1 : end]))

    def run_release_preflight(
        self,
        *,
        token: str | None = "release-token",
        visibility: str = "private",
        drift: Path | None = None,
        extra_unclassified: Path | None = None,
        extra_content: str | None = None,
        requested_repo: str | None = None,
        immutable_releases: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            checkout = tmp / "hauksbee"
            for relative in self.release_url_surfaces():
                destination = checkout / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(ROOT / relative, destination)
            for policy_file in (PREFLIGHT, SURFACE_MANIFEST, SURFACE_CHECKER):
                destination = checkout / policy_file.relative_to(ROOT)
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(policy_file, destination)
            if drift is not None:
                path = checkout / drift
                path.write_text(
                    path.read_text().replace(
                        "hauksbee-dev/hauksbee", "wrong-owner/wrong-repo"
                    )
                )
            if extra_unclassified is not None:
                path = checkout / extra_unclassified
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(
                    extra_content
                    if extra_content is not None
                    else 'repository = "hauksbee-dev/hauksbee"\n'
                )
            if requested_repo is not None:
                manifest_path = checkout / SURFACE_MANIFEST.relative_to(ROOT)
                manifest_path.write_text(
                    manifest_path.read_text().replace(
                        "hauksbee-dev/hauksbee", requested_repo
                    )
                )
                for relative in self.release_url_surfaces():
                    path = checkout / relative
                    path.write_text(
                        path.read_text().replace(
                            "hauksbee-dev/hauksbee", requested_repo
                        )
                    )

            gh = tmp / "gh"
            gh.write_text(
                textwrap.dedent(
                    """\
                    #!/usr/bin/env bash
                    set -eu
                    printf '%s\\n' "$*" >> "$FAKE_GH_LOG"
                    [ "$#" -eq 4 ] || exit 64
                    [ "$1" = api ] || exit 64
                    [ "$3" = --jq ] || exit 64
                    [ "${GH_TOKEN:-}" = release-token ] || exit 65
                    case "$2 $4" in
                      "repos/hauksbee-dev/hauksbee .visibility")
                        [ "$FAKE_GH_VISIBILITY" != missing ] || exit 1
                        printf '%s\\n' "$FAKE_GH_VISIBILITY"
                        ;;
                      "repos/hauksbee-dev/hauksbee/immutable-releases .enabled")
                        printf '%s\\n' "$FAKE_IMMUTABLE_RELEASES"
                        ;;
                      *) exit 64 ;;
                    esac
                    """
                )
            )
            gh.chmod(0o755)
            env = os.environ.copy()
            env.update(
                {
                    "PATH": f"{tmp}:{env['PATH']}",
                    "FAKE_GH_LOG": str(tmp / "gh.log"),
                    "FAKE_GH_VISIBILITY": visibility,
                    "FAKE_IMMUTABLE_RELEASES": str(immutable_releases).lower(),
                }
            )
            if token is None:
                env.pop("GH_TOKEN", None)
            else:
                env["GH_TOKEN"] = token
            return subprocess.run(
                ["bash", "-c", self.release_preflight_body()],
                cwd=tmp,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )

    def test_tag_release_preflight_fails_without_token(self) -> None:
        result = self.run_release_preflight(token=None)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("GH_TOKEN", result.stdout + result.stderr)

    def test_release_preflight_refuses_non_private_visibility(self) -> None:
        result = self.run_release_preflight(visibility="internal")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("not private", result.stdout + result.stderr)

    def test_release_preflight_accepts_private_target_with_exact_surfaces(self) -> None:
        result = self.run_release_preflight()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_release_preflight_requires_immutable_releases(self) -> None:
        result = self.run_release_preflight(immutable_releases=False)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("immutable releases", (result.stdout + result.stderr).lower())

    def test_release_preflight_fails_closed_on_every_baked_url_surface(self) -> None:
        for relative in self.release_url_surfaces():
            with self.subTest(path=relative):
                result = self.run_release_preflight(drift=relative)
                self.assertNotEqual(
                    result.returncode,
                    0,
                    f"slug drift in {relative} was ignored:\n{result.stdout}{result.stderr}",
                )
                self.assertIn(str(relative), result.stdout + result.stderr)

    def test_release_preflight_rejects_new_unclassified_slug_occurrence(self) -> None:
        result = self.run_release_preflight(
            extra_unclassified=Path("new-package-metadata.json")
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("new-package-metadata.json", result.stdout + result.stderr)
        self.assertIn("unclassified", result.stdout + result.stderr)

    def test_release_preflight_discovers_wrong_repository_surface_by_pattern(self) -> None:
        surfaces = {
            "image": "image: ghcr.io/wrong-owner/wrong-repo:slim\n",
            "installer": (
                "curl https://raw.githubusercontent.com/wrong-owner/wrong-repo/"
                "main/scripts/get-hauksbee.sh | bash\n"
            ),
            "action": "uses: wrong-owner/wrong-repo/integrations/github-action@v1\n",
            "metadata": (
                'repository = "https://github.com/wrong-owner/wrong-repo"\n'
            ),
        }
        for kind, content in surfaces.items():
            relative = Path(f"new-{kind}-surface.yml")
            with self.subTest(kind=kind):
                result = self.run_release_preflight(
                    extra_unclassified=relative,
                    extra_content=content,
                )
                self.assertNotEqual(
                    result.returncode, 0, result.stdout + result.stderr
                )
                output = result.stdout + result.stderr
                self.assertIn(str(relative), output)
                self.assertIn("wrong-owner/wrong-repo", output)
                self.assertIn("repository-bearing", output)

    def test_release_preflight_fake_gh_rejects_wrong_repository(self) -> None:
        result = self.run_release_preflight(requested_repo="wrong-owner/wrong-repo")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("gh api repos/wrong-owner/wrong-repo failed", result.stdout + result.stderr)

    def test_release_preflight_rejects_inaccessible_authorization(self) -> None:
        result = self.run_release_preflight(token="inaccessible-token")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("gh api repos/hauksbee-dev/hauksbee failed", result.stdout + result.stderr)

    def run_visibility_phase(
        self,
        developer: str = "private",
        mirror: str = "public",
        *,
        armed: bool = False,
        phase: str = "visibility",
    ) -> tuple[subprocess.CompletedProcess[str], list[str]]:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            scripts = tmp / "project/scripts"
            scripts.mkdir(parents=True)
            for source in (LAUNCHER, ROOT / "scripts/common.sh"):
                shutil.copy(source, scripts / source.name)
            (scripts.parent / "Cargo.toml").write_text('version = "0.0.0"\n')
            log = tmp / "commands.log"
            log.touch()
            for name, value in (("developer", developer), ("mirror", mirror), ("immutable", "false")):
                (tmp / name).write_text(value + "\n")
            # No inherited credentials, shell startup hooks, or publishing tools.
            bin_dir = tmp / "bin"
            bin_dir.mkdir()
            for name in ("bash", "dirname", "grep", "sed"):
                (bin_dir / name).symlink_to(shutil.which(name, path=os.defpath))
            # A fixture-local function avoids spawning an interpreter per gh call.
            common = scripts / "common.sh"
            common.write_text(common.read_text() + "\ngh() {\n" + textwrap.dedent('''\
                local repo="${2#repos/}" field=.visibility state value
                printf 'gh %s\\n' "$*" >> "$FAKE_COMMAND_LOG"
                if [ "$*" = 'api --method PUT repos/hauksbee-dev/hauksbee/immutable-releases' ]; then
                  printf 'true\\n' > "$HOME/immutable"
                  return 0
                fi
                [ "$1" != repo ] || repo="$3"
                case "$repo" in
                  ETM-Code/hauksbee-developer) state=developer ;;
                  hauksbee-dev/hauksbee) state=mirror ;;
                  hauksbee-dev/hauksbee/immutable-releases) state=immutable; field=.enabled ;;
                  *) return 64 ;;
                esac
                if [ "$1" = api ] && [ "$#" = 4 ] && [ "$3" = --jq ] && [ "$4" = "$field" ]; then
                  read -r value < "$HOME/$state"
                  [ "$value" != missing ] || return 1
                  printf '%s\\n' "$value"
                elif [ "$1 $2" = 'repo create' ] && [ "$#" = 4 ] && [ "$state" = mirror ]; then
                  case "$4" in
                    --private|--public) printf '%s\\n' "${4#--}" > "$HOME/$state" ;;
                    *) return 64 ;;
                  esac
                elif [ "$1 $2" = 'repo edit' ] && [ "$#" = 6 ] && [ "$field" = .visibility ]; then
                  [ "$4" = --visibility ] && [ "$6" = --accept-visibility-change-consequences ] || return 64
                  case "$5" in
                    private|public) printf '%s\\n' "$5" > "$HOME/$state" ;;
                    *) return 64 ;;
                  esac
                else
                  return 64
                fi
            ''') + "}\n")
            # Stop before the launcher's fixed /tmp logs, Cargo, or Git commands.
            (scripts / "build-public-mirror.sh").write_text(
                "printf 'builder %s\\n' \"$*\" >> \"$FAKE_COMMAND_LOG\"\nexit 73\n"
            )
            env = {
                "PATH": str(bin_dir),
                "HOME": str(tmp),
                "NO_COLOR": "1",
                "FAKE_COMMAND_LOG": str(log),
            }
            args = [shutil.which("bash", path=os.defpath), str(scripts / "make-public.sh"), "--only", phase]
            result = subprocess.run(
                args + (["--arm"] if armed else []),
                cwd=scripts.parent,
                env=env,
                text=True,
                capture_output=True,
                check=False,
                timeout=10,
            )
            calls = log.read_text().splitlines()
            self.assertNotIn(str(ROOT), "\n".join(calls))
            return result, calls

    def test_visibility_policy_and_dry_run(self) -> None:
        edit = "gh repo edit {} --visibility {} --accept-visibility-change-consequences"
        enable = "gh api --method PUT repos/hauksbee-dev/hauksbee/immutable-releases"
        cases = (
            ("private", "public", []),
            ("public", "public", [edit.format("ETM-Code/hauksbee-developer", "private")]),
            ("private", "private", [edit.format("hauksbee-dev/hauksbee", "public")]),
            ("private", "missing", ["gh repo create hauksbee-dev/hauksbee --public"]),
            ("missing", "public", []),
        )
        for developer, mirror, changes in cases:
            for armed in (False, True):
                with self.subTest(developer=developer, mirror=mirror, armed=armed):
                    result, calls = self.run_visibility_phase(developer, mirror, armed=armed)
                    missing = developer == "missing"
                    self.assertEqual(result.returncode, 1 if missing else 0, result.stderr)
                    mutations = [c for c in calls if c.startswith("gh repo ") or c == enable]
                    self.assertEqual(
                        mutations, changes + [enable] if armed and not missing else []
                    )
                    self.assertFalse(any(c.startswith("builder ") for c in calls))
                    if missing:
                        self.assertIn("developer repository is missing", result.stderr)
                    elif armed or not changes:
                        self.assertIn("developer repository is private", result.stdout)
                        self.assertIn("release mirror is public", result.stdout)
                    if not armed and not missing:
                        for command in changes + [enable]:
                            self.assertIn("WOULD: " + command, result.stdout)

    def test_mirror_visibility_is_checked_before_the_sentinel_builder(self) -> None:
        dry_run, calls = self.run_visibility_phase(phase="mirror")
        self.assertEqual(dry_run.returncode, 0, dry_run.stderr)
        self.assertEqual(calls, [])
        self.assertIn("force-push to public hauksbee-dev/hauksbee", dry_run.stdout)
        for mirror, code in (("private", 1), ("missing", 1), ("public", 73)):
            with self.subTest(mirror=mirror):
                result, calls = self.run_visibility_phase(mirror=mirror, armed=True, phase="mirror")
                self.assertEqual(result.returncode, code, result.stdout + result.stderr)
                self.assertEqual(calls[0], "gh api repos/hauksbee-dev/hauksbee --jq .visibility")
                self.assertEqual(len(calls), 2 if mirror == "public" else 1)
                if mirror == "public":
                    self.assertTrue(calls[1].startswith("builder --out "), calls)
                    self.assertTrue(calls[1].endswith("/hauksbee-public-next"), calls)
                else:
                    self.assertIn(
                        "refusing to push" if mirror == "private" else "missing or inaccessible",
                        result.stderr,
                    )


if __name__ == "__main__":
    unittest.main()
