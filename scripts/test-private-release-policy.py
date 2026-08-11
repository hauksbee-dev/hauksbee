#!/usr/bin/env python3
"""Black-box tests for the release launcher's repository-privacy contract."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]
LAUNCHER = ROOT / "scripts" / "make-public.sh"
PREFLIGHT = ROOT / "scripts" / "preflight-private-release.sh"
RELEASE_URL_SURFACES = (
    Path("scripts/get-hauksbee.sh"),
    Path("scripts/get-hauksbee.ps1"),
    Path("scripts/bundle.sh"),
    Path("app/macos/build-app.sh"),
    Path("docker/Dockerfile.slim"),
    Path("docker/Dockerfile.full"),
    Path(".github/workflows/docker.yml"),
    Path("integrations/github-action/action.yml"),
    Path("integrations/kicad-plugin/build-pcm.sh"),
    Path("integrations/kicad-plugin/metadata.json"),
    Path("frontend/src/lib/version.ts"),
    Path("crates/hauksbee-ci/src/integrate.rs"),
)


class PrivateReleasePolicyTests(unittest.TestCase):
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
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            checkout = tmp / "hauksbee"
            for relative in RELEASE_URL_SURFACES:
                destination = checkout / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(ROOT / relative, destination)
            if PREFLIGHT.exists():
                destination = checkout / PREFLIGHT.relative_to(ROOT)
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(PREFLIGHT, destination)
            if drift is not None:
                path = checkout / drift
                path.write_text(
                    path.read_text().replace(
                        "hauksbee-dev/hauksbee", "wrong-owner/wrong-repo"
                    )
                )

            gh = tmp / "gh"
            gh.write_text(
                textwrap.dedent(
                    """\
                    #!/usr/bin/env bash
                    set -eu
                    printf '%s\\n' "$*" >> "$FAKE_GH_LOG"
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
        for relative in RELEASE_URL_SURFACES:
            with self.subTest(path=relative):
                result = self.run_release_preflight(drift=relative)
                self.assertNotEqual(
                    result.returncode,
                    0,
                    f"slug drift in {relative} was ignored:\n{result.stdout}{result.stderr}",
                )
                self.assertIn(str(relative), result.stdout + result.stderr)

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
