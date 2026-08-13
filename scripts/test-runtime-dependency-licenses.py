#!/usr/bin/env python3
"""Fail closed when a runtime dependency violates the release licence boundary."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked"], text=True
        )
    )
    forbidden: list[str] = []
    evalexpr: tuple[str, str] | None = None
    for package in metadata["packages"]:
        if package.get("source") is None:
            continue
        license_expr = package.get("license") or ""
        if "AGPL" in license_expr.upper():
            forbidden.append(
                f"{package['name']} {package['version']} ({license_expr or 'unknown licence'})"
            )
        if package["name"] == "evalexpr":
            evalexpr = (package["version"], license_expr)

    if forbidden:
        print("forbidden AGPL runtime dependency: " + ", ".join(forbidden), file=sys.stderr)
        return 1
    if evalexpr != ("11.3.1", "MIT"):
        print(
            "evalexpr must remain on the final MIT line 11.3.1; "
            f"resolved {evalexpr!r}",
            file=sys.stderr,
        )
        return 1
    notice = root / "licenses/evalexpr-MIT.txt"
    if not notice.is_file() or "Copyright (c) 2019 Sebastian Schmidt" not in notice.read_text():
        print("evalexpr's exact MIT notice is missing", file=sys.stderr)
        return 1
    required_consumers = {
        "scripts/bundle.sh": "LICENSE-EVALEXPR-MIT.txt",
        "scripts/bundle-windows.ps1": "LICENSE-EVALEXPR-MIT.txt",
        "app/macos/build-app.sh": "LICENSE-EVALEXPR-MIT.txt",
        "docker/Dockerfile.slim": "LICENSE-EVALEXPR-MIT.txt",
    }
    for relative, marker in required_consumers.items():
        if marker not in (root / relative).read_text():
            print(f"{relative} does not retain evalexpr's MIT notice", file=sys.stderr)
            return 1
    print("runtime dependency licences: no AGPL packages; evalexpr 11.3.1 is MIT")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
