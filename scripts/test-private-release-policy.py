#!/usr/bin/env python3
"""Black-box tests for the release launcher's repository-privacy contract."""

from __future__ import annotations

import os
from pathlib import Path
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import re
import shutil
import subprocess
import tarfile
import tempfile
import textwrap
import threading
import unittest


ROOT = Path(__file__).resolve().parents[1]
LAUNCHER = ROOT / "scripts" / "make-public.sh"
PREFLIGHT = ROOT / "scripts" / "preflight-private-release.sh"
SURFACE_MANIFEST = ROOT / "scripts" / "private-release-surfaces.json"
SURFACE_CHECKER = ROOT / "scripts" / "check-private-release-surfaces.py"
CONTAINER_PREFLIGHT = ROOT / "scripts" / "check-private-container-publication.sh"
MIRROR_DEPENDENCY_CHECKER = ROOT / "scripts" / "check-mirror-dependencies.py"
PREBUILT_PROVENANCE = ROOT / "integrations" / "github-action" / "prebuilt-provenance.sh"
REGISTRY_USER = ROOT / "integrations" / "github-action" / "resolve-registry-user.sh"


class PrivateReleasePolicyTests(unittest.TestCase):
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
            "docs/dev-plans/launch-checklist.md",
            "docs/dev-plans/launch-gtm-strategy.md",
            "docs/dev-plans/public-release-cleanup-plan.md",
            "docs/dev-plans/tasks.md",
            "frontend/capture/cards.ts",
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

    def test_private_suite_disclosure_counts_each_absent_test_file(self) -> None:
        builder = (ROOT / "scripts/build-public-mirror.sh").read_text()
        board_block = builder.split("BOARD_EXCLUDE=(", 1)[1].split("\n)", 1)[0]
        absent_paths = [
            ROOT / line.strip()
            for line in board_block.splitlines()
            if line.strip().endswith(".rs") and "/tests/" in line
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

        nep = (ROOT / "crates/hauksbee-engine/tests/nep_private_acceptance.rs").read_text()
        self.assertIn("fn real_nep_host_exposes_standard_grade_7ms_gap()", nep)
        self.assertIn("fn real_nep_host_succeeds_with_compliant_firmware()", nep)

    def private_installer_fixture(self) -> tuple[str, bytes, bytes]:
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
            for binary in ("hauksbee", "hauksbee-ci", "hauksbee-mcp"):
                content = b"#!/usr/bin/env bash\nexit 0\n"
                info = tarfile.TarInfo(f"{root}/bin/{binary}")
                info.mode = 0o755
                info.size = len(content)
                archive.addfile(info, io.BytesIO(content))
        tarball = buffer.getvalue()
        checksum = f"{hashlib.sha256(tarball).hexdigest()}  {asset}\n".encode()
        return asset, tarball, checksum

    def run_private_installer(
        self, *, token: str | None, corrupt_asset: bool = False
    ) -> tuple[subprocess.CompletedProcess[str], list[tuple[str, str, str]]]:
        asset, tarball, checksum = self.private_installer_fixture()
        if corrupt_asset:
            tarball += b"corrupt-after-checksum"
        expected_auth = "Bearer installer-token"
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
                if handler.headers.get("Authorization") != expected_auth:
                    handler.send_response(401)
                    handler.end_headers()
                    return
                port = handler.server.server_port
                release = json.dumps(
                    {
                        "tag_name": "v0.1.0",
                        "assets": [
                            {
                                "name": asset,
                                "url": f"http://127.0.0.1:{port}/repos/hauksbee-dev/hauksbee/releases/assets/101",
                            },
                            {
                                "name": f"{asset}.sha256",
                                "url": f"http://127.0.0.1:{port}/repos/hauksbee-dev/hauksbee/releases/assets/102",
                            },
                        ],
                    }
                ).encode()
                body = {
                    "/repos/hauksbee-dev/hauksbee/releases/tags/v0.1.0": release,
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
                result = subprocess.run(
                    [
                        "bash",
                        str(ROOT / "scripts/get-hauksbee.sh"),
                        "--version",
                        "v0.1.0",
                        "--prefix",
                        str(Path(raw_tmp) / "prefix"),
                    ],
                    cwd=ROOT,
                    env=env,
                    text=True,
                    capture_output=True,
                    check=False,
                )
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
        return result, requests

    def test_private_installer_refuses_to_download_without_credential(self) -> None:
        result, requests = self.run_private_installer(token=None)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("HAUKSBEE_GITHUB_TOKEN", result.stdout + result.stderr)
        self.assertEqual(requests, [], "missing credentials must fail before HTTP")

    def test_private_installer_authenticates_every_asset_download(self) -> None:
        result, requests = self.run_private_installer(token="installer-token")
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
            ],
        )

        end_to_end = (ROOT / "scripts/test-install-mock.sh").read_text()
        self.assertNotIn("HAUKSBEE_RELEASES_BASE", end_to_end)
        self.assertIn("/releases/assets/101", end_to_end)
        self.assertIn('"assets"', end_to_end)
        self.assertNotIn("installer itself needs only `curl`", (ROOT / "README.md").read_text())

    def test_private_installer_refuses_corrupt_api_asset_bytes(self) -> None:
        result, requests = self.run_private_installer(
            token="installer-token", corrupt_asset=True
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("did not match", (result.stdout + result.stderr).lower())
        self.assertEqual(len(requests), 3)

    def test_powershell_installer_authenticates_asset_downloads(self) -> None:
        text = (ROOT / "scripts/get-hauksbee.ps1").read_text()
        self.assertIn("HAUKSBEE_GITHUB_TOKEN", text)
        self.assertIn('"$ApiBase/releases/tags/$Version"', text)
        self.assertIn("$matches[0].url", text)
        self.assertIn('"Accept" = "application/octet-stream"', text)
        self.assertIn("Get-FileHash -Algorithm SHA256", text)
        self.assertNotIn("ReleasesBase", text)

    def test_action_source_fallback_uses_the_stock_runner_feature_set(self) -> None:
        action = (ROOT / "integrations/github-action/action.yml").read_text()
        build = action[action.index("- name: Build hauksbee (fallback build)") :]
        self.assertEqual(build.count("--no-default-features --features renode,qemu"), 2)
        self.assertIn("stock runner", build)

    def test_prebuilt_cache_is_repository_bound_and_rejects_wrong_provenance(self) -> None:
        action = (ROOT / "integrations/github-action/action.yml").read_text()
        key = next(line for line in action.splitlines() if "key: hauksbee-prebuilt-" in line)
        self.assertIn("inputs.hauksbee-repo", key)
        self.assertIn("prebuilt-provenance.sh", action)
        self.assertIn('verify "$dl" "$REPO" "$TAG" "$platform"', action)
        self.assertIn('TAG="$(gh release view', action)

        with tempfile.TemporaryDirectory() as raw_tmp:
            cache = Path(raw_tmp)
            (cache / "hauksbee-0.1.0/bin").mkdir(parents=True)
            for binary in ("hauksbee", "hauksbee-ci"):
                path = cache / "hauksbee-0.1.0/bin" / binary
                path.write_text("#!/bin/sh\n")
                path.chmod(0o755)
            subprocess.run(
                [
                    "bash",
                    str(PREBUILT_PROVENANCE),
                    "record",
                    str(cache),
                    "owner/one",
                    "v0.1.0",
                    "linux-x86_64",
                ],
                check=True,
                text=True,
                capture_output=True,
            )
            good = subprocess.run(
                [
                    "bash",
                    str(PREBUILT_PROVENANCE),
                    "verify",
                    str(cache),
                    "owner/one",
                    "v0.1.0",
                    "linux-x86_64",
                ],
                text=True,
                capture_output=True,
                check=False,
            )
            wrong = subprocess.run(
                [
                    "bash",
                    str(PREBUILT_PROVENANCE),
                    "verify",
                    str(cache),
                    "owner/two",
                    "v0.1.0",
                    "linux-x86_64",
                ],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(good.returncode, 0, good.stdout + good.stderr)
            self.assertNotEqual(wrong.returncode, 0, wrong.stdout + wrong.stderr)

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

    def test_private_action_requires_and_forwards_authorized_credential(self) -> None:
        action = (ROOT / "integrations/github-action/action.yml").read_text()
        self.assertIn("hauksbee-token:", action)
        self.assertIn("GH_TOKEN: ${{ inputs.hauksbee-token }}", action)
        self.assertIn("token: ${{ inputs.hauksbee-token }}", action)

        readme = (ROOT / "integrations/github-action/README.md").read_text()
        self.assertIn("fine-grained personal access token", readme)
        self.assertIn("GitHub App installation token", readme)
        self.assertIn("Contents: read", readme)
        self.assertIn("uses: ./.hauksbee-action/integrations/github-action", readme)
        self.assertIn("hauksbee-token: ${{ secrets.HAUKSBEE_READ_TOKEN }}", readme)

        generated = (ROOT / "crates/hauksbee-ci/src/integrate.rs").read_text()
        self.assertIn('${{ secrets.HAUKSBEE_READ_TOKEN }}', generated)
        self.assertIn("path: .hauksbee-action", generated)
        self.assertIn("token: {}", generated)
        self.assertIn("uses: ./.hauksbee-action/integrations/github-action", generated)
        self.assertIn("hauksbee-token: {}", generated)

        frontend_workflow = (ROOT / "frontend/src/lib/ci-workflow.ts").read_text()
        self.assertIn("repository: ${ACTION_REPOSITORY}", frontend_workflow)
        self.assertIn("token: ${PRIVATE_TOKEN_SECRET}", frontend_workflow)
        self.assertIn("uses: ./.hauksbee-action/integrations/github-action", frontend_workflow)
        self.assertIn("hauksbee-token: ${PRIVATE_TOKEN_SECRET}", frontend_workflow)
        version = (ROOT / "frontend/src/lib/version.ts").read_text()
        self.assertIn("${{ secrets.HAUKSBEE_READ_TOKEN }}", version)

        private_checkout_sources = {
            "integrations/github-action/action.yml": 1,
            "integrations/github-action/README.md": 1,
            "integrations/github-action/example-workflow.yml": 2,
            "docs/ci/DOCKER.md": 1,
            # One generated line plus the unit assertion that pins it.
            "crates/hauksbee-ci/src/integrate.rs": 2,
            "frontend/src/lib/ci-workflow.ts": 1,
        }
        for relative, expected in private_checkout_sources.items():
            text = (ROOT / relative).read_text()
            with self.subTest(path=relative):
                self.assertEqual(
                    text.count("persist-credentials: false"),
                    expected,
                    "every private-token checkout must erase its credential after checkout",
                )

        registry_auth = (ROOT / "integrations/github-action/action.yml").read_text()
        self.assertIn('mktemp -d "$RUNNER_TEMP/hauksbee-docker-auth.XXXXXX"', registry_auth)
        self.assertIn("docker logout ghcr.io", registry_auth)
        self.assertIn("Cleanup private registry credential", registry_auth)
        self.assertIn("if: ${{ always()", registry_auth)

    def test_shipped_installer_examples_authenticate_the_private_script_fetch(self) -> None:
        for relative in (Path("README.md"), Path("docs/START_HERE.md")):
            text = (ROOT / relative).read_text()
            with self.subTest(path=relative):
                self.assertIn("export HAUKSBEE_GITHUB_TOKEN", text)
                self.assertIn("Authorization: Bearer %s", text)
                self.assertIn("curl --config -", text)
                example = text[text.index("export HAUKSBEE_GITHUB_TOKEN") :]
                self.assertIn("(\n", text[: text.index("export HAUKSBEE_GITHUB_TOKEN") + 1])
                self.assertIn("\n)", example)

        readme = (ROOT / "README.md").read_text()
        docker_example = readme[readme.index("export HAUKSBEE_GHCR_USER") : readme.index("The credential needs")]
        self.assertIn('DOCKER_CONFIG="$(mktemp -d)"', docker_example)
        self.assertIn("trap cleanup EXIT", docker_example)
        self.assertIn("docker logout ghcr.io", docker_example)

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
        start = tasks.index("- [~] B3 ")
        end = tasks.index("\n- [~] B4 ", start)
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

    def run_container_preflight(
        self,
        *,
        phase: str = "before",
        token: str | None = "container-token",
        repo_visibility: str = "private",
        package_visibility: str = "private",
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
                        orgs/hauksbee-dev/packages/container/hauksbee)
                          [ "$FAKE_PACKAGE_VISIBILITY" != inaccessible ] || exit 66
                          [ "$FAKE_PACKAGE_VISIBILITY" != missing ] || exit 66
                          printf '%s\\n' "$FAKE_PACKAGE_VISIBILITY"
                          ;;
                        *) exit 64 ;;
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

        docs = (ROOT / "docs/ci/DOCKER.md").read_text()
        self.assertIn("privacy-bootstrap", docs)
        self.assertIn("no Hauksbee binaries or source", docs)

    def test_container_publication_preflight_is_private_and_read_only(self) -> None:
        for phase in ("before", "after"):
            with self.subTest(phase=phase):
                result, calls = self.run_container_preflight(phase=phase)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(
                    calls.splitlines(),
                    [
                        "api repos/hauksbee-dev/hauksbee --jq .visibility",
                        "api orgs/hauksbee-dev/packages/container/hauksbee --jq .visibility",
                    ],
                )

        for kwargs, phrase in (
            ({"token": None}, "GH_TOKEN"),
            ({"repo_visibility": "public"}, "repository"),
            ({"repo_visibility": "inaccessible"}, "repository"),
            ({"package_visibility": "public"}, "package"),
            ({"package_visibility": "missing"}, "package"),
            ({"package_visibility": "inaccessible"}, "package"),
        ):
            with self.subTest(kwargs=kwargs):
                result, _calls = self.run_container_preflight(**kwargs)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(phrase.lower(), (result.stdout + result.stderr).lower())

    def test_private_pcm_is_manual_authenticated_install_from_file_only(self) -> None:
        readme = (ROOT / "integrations/kicad-plugin/README.md").read_text()
        self.assertNotIn("official listing", readme.lower())
        self.assertNotIn("pcm handles updates", readme.lower())
        self.assertIn("GH_TOKEN", readme)
        self.assertIn("gh release download", readme)
        self.assertIn("Install from File", readme)
        self.assertIn("manually repeat", readme)

        builder = (ROOT / "integrations/kicad-plugin/build-pcm.sh").read_text()
        self.assertNotIn("registry listing", builder)
        self.assertNotIn("download_url=", builder)

        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        build = workflow.index("Build KiCad PCM archive")
        upload = workflow.index("Upload build artifact")
        publish = workflow.index("Create / update GitHub Release")
        self.assertLess(build, upload)
        self.assertIn("integrations/kicad-plugin/build-pcm.sh", workflow[build:upload])
        self.assertIn("hauksbee-ci-pcm-v", workflow[build:upload])
        self.assertIn("hauksbee-ci-pcm-v*.zip", workflow[upload:publish])
        self.assertIn("hauksbee-ci-pcm-v*.zip.sha256", workflow[upload:publish])

    def test_all_named_private_consumers_authenticate_before_fetching(self) -> None:
        mcp = (ROOT / "crates/hauksbee-mcp/README.md").read_text()
        self.assertIn("HAUKSBEE_GITHUB_TOKEN", mcp)
        self.assertIn("curl --config -", mcp)

        demo = (ROOT / "frontend/src/demo/DemoApp.tsx").read_text()
        self.assertIn("HAUKSBEE_GITHUB_TOKEN", demo)
        self.assertIn("curl --config -", demo)
        self.assertNotIn("curl -fsSL https://raw.githubusercontent.com", demo)

        for relative in (Path("README.md"), Path("docs/ci/DOCKER.md")):
            text = (ROOT / relative).read_text()
            with self.subTest(path=relative):
                login = text.index("docker login ghcr.io")
                self.assertLess(
                    login,
                    text.index("\n  docker run", login),
                )

        recipes = (ROOT / "docs/ci/RECIPES.md").read_text()
        for credential_contract in (
            "DOCKER_AUTH_CONFIG",
            "registryCredentialsId",
            "endpoint: hauksbee-ghcr",
            "docker login ghcr.io",
        ):
            self.assertIn(credential_contract, recipes)

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
                    [ "$2" = repos/hauksbee-dev/hauksbee ] || exit 64
                    [ "$3" = --jq ] || exit 64
                    [ "$4" = .visibility ] || exit 64
                    [ "${GH_TOKEN:-}" = release-token ] || exit 65
                    [ "$FAKE_GH_VISIBILITY" != missing ] || exit 1
                    printf '%s\\n' "$FAKE_GH_VISIBILITY"
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

    def run_privacy_phase(
        self,
        *,
        developer_visibility: str = "private",
        mirror_visibility: str = "private",
        armed: bool = False,
        phase: str = "privacy",
    ) -> tuple[subprocess.CompletedProcess[str], str]:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            log = tmp / "gh.log"
            mirror_state = tmp / "mirror.state"
            developer_state = tmp / "developer.state"
            mirror_state.write_text(mirror_visibility)
            developer_state.write_text(developer_visibility)
            gh = tmp / "gh"
            gh.write_text(
                textwrap.dedent(
                    """\
                    #!/usr/bin/env bash
                    set -eu
                    printf '%s\\n' "$*" >> "$FAKE_GH_LOG"
                    if [ "$1" = api ]; then
                      case "$2" in
                        repos/ETM-Code/hauksbee-developer)
                          visibility=$(cat "$FAKE_DEVELOPER_STATE")
                          [ "$visibility" != missing ] || exit 1
                          printf '%s\\n' "$visibility"
                          ;;
                        repos/hauksbee-dev/hauksbee)
                          visibility=$(cat "$FAKE_MIRROR_STATE")
                          [ "$visibility" != missing ] || exit 1
                          printf '%s\\n' "$visibility"
                          ;;
                        *) exit 64 ;;
                      esac
                    elif [ "$1 $2" = "repo create" ]; then
                      printf '%s\\n' private > "$FAKE_MIRROR_STATE"
                    elif [ "$1 $2" = "repo edit" ]; then
                      case "$3" in
                        ETM-Code/hauksbee-developer)
                          printf '%s\\n' private > "$FAKE_DEVELOPER_STATE"
                          ;;
                        hauksbee-dev/hauksbee)
                          printf '%s\\n' private > "$FAKE_MIRROR_STATE"
                          ;;
                        *) exit 64 ;;
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
                    "FAKE_MIRROR_STATE": str(mirror_state),
                    "FAKE_DEVELOPER_STATE": str(developer_state),
                    "NO_COLOR": "1",
                }
            )
            args = ["bash", str(LAUNCHER)]
            if armed:
                args.append("--arm")
            args.extend(["--only", phase])
            result = subprocess.run(
                args,
                cwd=ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            return result, log.read_text() if log.exists() else ""

    def test_private_repositories_are_left_private(self) -> None:
        result, calls = self.run_privacy_phase()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("developer repository is private", result.stdout)
        self.assertIn("release mirror is private", result.stdout)
        self.assertNotIn("repo edit", calls)
        self.assertNotIn("repo create", calls)

    def test_absent_mirror_is_created_private_when_armed(self) -> None:
        result, calls = self.run_privacy_phase(mirror_visibility="missing", armed=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("repo create hauksbee-dev/hauksbee --private", calls)
        self.assertNotIn("--visibility public", calls)

    def test_public_repository_is_changed_to_private_never_the_reverse(self) -> None:
        result, calls = self.run_privacy_phase(mirror_visibility="public", armed=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "repo edit hauksbee-dev/hauksbee --visibility private", calls
        )
        self.assertNotIn("--visibility public", calls)

    def test_public_developer_repository_is_changed_to_private(self) -> None:
        result, calls = self.run_privacy_phase(
            developer_visibility="public", armed=True
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "repo edit ETM-Code/hauksbee-developer --visibility private", calls
        )
        self.assertNotIn("--visibility public", calls)

    def test_missing_developer_repository_is_a_hard_failure(self) -> None:
        result, calls = self.run_privacy_phase(developer_visibility="missing")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("developer repository", result.stderr)
        self.assertNotIn("repo create ETM-Code/hauksbee-developer", calls)

    def test_mirror_phase_refuses_a_public_push_target_before_building(self) -> None:
        result, calls = self.run_privacy_phase(
            mirror_visibility="public", armed=True, phase="mirror"
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing to push", result.stderr)
        self.assertEqual(
            calls.strip(),
            "api repos/hauksbee-dev/hauksbee --jq .visibility",
            "the launcher must reject visibility before it rebuilds or pushes",
        )


if __name__ == "__main__":
    unittest.main()
