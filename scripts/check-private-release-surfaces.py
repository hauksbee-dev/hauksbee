#!/usr/bin/env python3
"""Validate every shipped occurrence of the private release repository slug."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys


MANIFEST_PATH = Path("scripts/private-release-surfaces.json")
EXCLUDED_PATHS = {
    MANIFEST_PATH.as_posix(),
    "scripts/check-private-release-surfaces.py",
    "scripts/preflight-private-release.sh",
    "scripts/test-private-release-policy.py",
}
# These patterns identify release/distribution surfaces without using the
# canonical slug as the search needle. That matters: a typo or owner drift must
# still be discovered even though it no longer contains the expected value.
REPOSITORY_PATTERNS = (
    ("container image", re.compile(r"\bghcr\.io/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)")),
    (
        "private installer",
        re.compile(
            r"\braw\.githubusercontent\.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)/"
            r"[^\s'\"]+/scripts/get-hauksbee(?:\.sh|\.ps1)?"
        ),
    ),
    (
        "GitHub Action",
        re.compile(
            r"\buses:\s*['\"]?([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)/"
            r"integrations/github-action@"
        ),
    ),
    (
        "repository package metadata",
        re.compile(
            r"(?:repository|homepage|org\.opencontainers\.image\.source)"
            r"[^\n]{0,80}?github\.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)"
        ),
    ),
)


def repository_candidates(text: str) -> list[tuple[str, str]]:
    """Return (surface kind, owner/name) candidates found independently."""
    candidates: list[tuple[str, str]] = []
    for kind, pattern in REPOSITORY_PATTERNS:
        for match in pattern.finditer(text):
            owner, name = match.groups()
            candidates.append((kind, f"{owner}/{name.removesuffix('.git')}"))

    # Cross-repository checkout YAML uses a bare owner/name rather than a URL.
    # Restrict this to the generated Action checkout shape so ordinary corpus
    # repository fields are not mistaken for distribution surfaces.
    if ".hauksbee-action" in text:
        bare_checkout = re.compile(
            r"\brepository:\s*['\"]?([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)"
        )
        for match in bare_checkout.finditer(text):
            candidates.append(("private Action checkout", "/".join(match.groups())))
    return candidates


def shipped_files(root: Path) -> list[Path]:
    """Return tracked files, or all files in a manifest fixture without Git."""
    if (root / ".git").exists():
        result = subprocess.run(
            [
                "git",
                "-C",
                str(root),
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ],
            check=True,
            capture_output=True,
        )
        return [root / raw.decode() for raw in result.stdout.split(b"\0") if raw]
    return [path for path in root.rglob("*") if path.is_file()]


def validate(root: Path, scope: str = "development") -> tuple[str, list[str]]:
    manifest_file = root / MANIFEST_PATH
    if not manifest_file.is_file():
        return "", [f"{MANIFEST_PATH}: canonical surface manifest is missing"]
    try:
        manifest = json.loads(manifest_file.read_text())
    except (OSError, json.JSONDecodeError) as error:
        return "", [f"{MANIFEST_PATH}: cannot read manifest: {error}"]

    repository = manifest.get("repository", "")
    if not isinstance(repository, str) or repository.count("/") != 1:
        return "", [f"{MANIFEST_PATH}: repository must be one owner/name slug"]
    needle = repository.encode()

    expected: dict[str, dict[str, object]] = {}
    seen_paths: set[str] = set()
    errors: list[str] = []
    excluded_prefixes: list[str] = []
    for exclusion in manifest.get("excluded_prefixes", []):
        prefix = exclusion.get("path", "")
        classification = exclusion.get("classification", "")
        if (
            not isinstance(prefix, str)
            or not prefix
            or prefix.startswith("/")
            or ".." in Path(prefix).parts
            or not prefix.endswith("/")
        ):
            errors.append(f"{MANIFEST_PATH}: invalid excluded prefix {prefix!r}")
            continue
        if not isinstance(classification, str) or not classification.strip():
            errors.append(
                f"{MANIFEST_PATH}: excluded prefix {prefix} has no classification"
            )
        excluded_prefixes.append(prefix)
    for entry in manifest.get("surfaces", []):
        relative = entry.get("path", "")
        classification = entry.get("classification", "")
        occurrences = entry.get("occurrences")
        scopes = entry.get("scopes", ["development", "mirror"])
        if (
            not isinstance(relative, str)
            or not relative
            or Path(relative).is_absolute()
            or ".." in Path(relative).parts
        ):
            errors.append(f"{MANIFEST_PATH}: invalid surface path {relative!r}")
            continue
        if relative in seen_paths:
            errors.append(f"{MANIFEST_PATH}: duplicate surface path {relative}")
            continue
        seen_paths.add(relative)
        if not isinstance(classification, str) or not classification.strip():
            errors.append(f"{MANIFEST_PATH}: {relative} has no classification")
        if not isinstance(occurrences, int) or occurrences < 1:
            errors.append(f"{MANIFEST_PATH}: {relative} has invalid occurrence count")
        if (
            not isinstance(scopes, list)
            or not scopes
            or any(item not in {"development", "mirror"} for item in scopes)
            or len(scopes) != len(set(scopes))
        ):
            errors.append(f"{MANIFEST_PATH}: {relative} has invalid scopes")
            continue
        if "development" not in scopes:
            errors.append(
                f"{MANIFEST_PATH}: {relative} must remain classified in development"
            )
        if scope in scopes:
            expected[relative] = entry

    observed: dict[str, int] = {}
    for path in shipped_files(root):
        try:
            relative = path.relative_to(root).as_posix()
        except ValueError:
            continue
        if (
            relative in EXCLUDED_PATHS
            or relative.startswith(tuple(excluded_prefixes))
            or not path.is_file()
        ):
            continue
        try:
            data = path.read_bytes()
        except OSError as error:
            errors.append(f"{relative}: cannot read shipped file: {error}")
            continue
        count = data.count(needle)
        if count:
            observed[relative] = count
        text = data.decode(errors="replace")
        for kind, candidate in repository_candidates(text):
            if candidate != repository:
                errors.append(
                    f"{relative}: repository-bearing {kind} references {candidate}; "
                    f"expected {repository}"
                )
            elif relative not in expected:
                errors.append(
                    f"{relative}: unclassified repository-bearing {kind} references {candidate}"
                )

    for relative in sorted(observed.keys() - expected.keys()):
        errors.append(
            f"{relative}: {observed[relative]} unclassified occurrence(s) of {repository}"
        )
    for relative in sorted(expected.keys() - observed.keys()):
        errors.append(
            f"{relative}: classified surface is missing or no longer contains {repository}"
        )
    for relative in sorted(expected.keys() & observed.keys()):
        entry = expected[relative]
        wanted = entry["occurrences"]
        if observed[relative] != wanted:
            errors.append(
                f"{relative}: contains {observed[relative]} occurrence(s) of {repository}; expected {wanted}"
            )
        data = (root / relative).read_text(errors="replace")
        for fragment in entry.get("required_fragments", []):
            if fragment not in data:
                errors.append(
                    f"{relative}: required private-release field is missing: {fragment}"
                )

    return repository, errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("root", type=Path)
    parser.add_argument("--print-repository", action="store_true")
    parser.add_argument(
        "--scope",
        choices=("development", "mirror"),
        default="development",
        help="validate the development tree or the curated release mirror",
    )
    args = parser.parse_args()
    root = args.root.resolve()
    repository, errors = validate(root, args.scope)
    if errors:
        for error in errors:
            print(f"private release surface error: {error}", file=sys.stderr)
        return 1
    if args.print_repository:
        print(repository)
    else:
        manifest = json.loads((root / MANIFEST_PATH).read_text())
        count = sum(
            args.scope in entry.get("scopes", ["development", "mirror"])
            for entry in manifest["surfaces"]
        )
        print(
            f"private release surfaces ({args.scope}): "
            f"{count} classified files match"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
