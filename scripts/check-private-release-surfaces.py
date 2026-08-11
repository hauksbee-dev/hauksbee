#!/usr/bin/env python3
"""Validate every shipped occurrence of the private release repository slug."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys


MANIFEST_PATH = Path("scripts/private-release-surfaces.json")
EXCLUDED_PATHS = {
    MANIFEST_PATH.as_posix(),
    "scripts/check-private-release-surfaces.py",
    "scripts/preflight-private-release.sh",
    "scripts/test-private-release-policy.py",
}


def shipped_files(root: Path) -> list[Path]:
    """Return tracked files, or all files in a manifest fixture without Git."""
    if (root / ".git").exists():
        result = subprocess.run(
            ["git", "-C", str(root), "ls-files", "-z"],
            check=True,
            capture_output=True,
        )
        return [root / raw.decode() for raw in result.stdout.split(b"\0") if raw]
    return [path for path in root.rglob("*") if path.is_file()]


def validate(root: Path) -> tuple[str, list[str]]:
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
    errors: list[str] = []
    for entry in manifest.get("surfaces", []):
        relative = entry.get("path", "")
        classification = entry.get("classification", "")
        occurrences = entry.get("occurrences")
        if (
            not isinstance(relative, str)
            or not relative
            or Path(relative).is_absolute()
            or ".." in Path(relative).parts
        ):
            errors.append(f"{MANIFEST_PATH}: invalid surface path {relative!r}")
            continue
        if relative in expected:
            errors.append(f"{MANIFEST_PATH}: duplicate surface path {relative}")
            continue
        if not isinstance(classification, str) or not classification.strip():
            errors.append(f"{MANIFEST_PATH}: {relative} has no classification")
        if not isinstance(occurrences, int) or occurrences < 1:
            errors.append(f"{MANIFEST_PATH}: {relative} has invalid occurrence count")
        expected[relative] = entry

    observed: dict[str, int] = {}
    for path in shipped_files(root):
        try:
            relative = path.relative_to(root).as_posix()
        except ValueError:
            continue
        if relative in EXCLUDED_PATHS or not path.is_file():
            continue
        try:
            data = path.read_bytes()
        except OSError as error:
            errors.append(f"{relative}: cannot read shipped file: {error}")
            continue
        count = data.count(needle)
        if count:
            observed[relative] = count

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
    args = parser.parse_args()
    root = args.root.resolve()
    repository, errors = validate(root)
    if errors:
        for error in errors:
            print(f"private release surface error: {error}", file=sys.stderr)
        return 1
    if args.print_repository:
        print(repository)
    else:
        count = len(json.loads((root / MANIFEST_PATH).read_text())["surfaces"])
        print(f"private release surfaces: {count} classified files match")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
