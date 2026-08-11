#!/usr/bin/env python3
"""Black-box tests for the release launcher's repository-privacy contract."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]
LAUNCHER = ROOT / "scripts" / "make-public.sh"


class PrivateReleasePolicyTests(unittest.TestCase):
    def test_release_contract_never_requires_public_repository_or_issues(self) -> None:
        forbidden_by_file = {
            ROOT / ".github/workflows/release.yml": (
                "public slug",
                "public repo",
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

    def test_release_preflight_requires_private_visibility(self) -> None:
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn(
            'visibility="$(gh api repos/hauksbee-dev/hauksbee --jq .visibility)"',
            workflow,
        )
        self.assertIn('if [ "$visibility" != "private" ]; then', workflow)

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
