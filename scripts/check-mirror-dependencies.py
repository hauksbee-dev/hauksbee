#!/usr/bin/env python3
"""Reject retained executable shell scripts whose literal script dependencies vanished."""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys


SCRIPT_REFERENCE = re.compile(
    r"(?:\$\{?[A-Z_]*ROOT\}?/)?(scripts/[A-Za-z0-9_.@/+-]+\.(?:sh|py))"
)


def missing_dependencies(root: Path) -> list[str]:
    errors: list[str] = []
    for path in root.rglob("*.sh"):
        if not path.is_file() or not path.stat().st_mode & 0o111:
            continue
        relative = path.relative_to(root).as_posix()
        for line_number, line in enumerate(path.read_text(errors="replace").splitlines(), 1):
            if line.lstrip().startswith("#"):
                continue
            for match in SCRIPT_REFERENCE.finditer(line):
                dependency = match.group(1)
                if not (root / dependency).is_file():
                    errors.append(f"{relative}:{line_number}: missing {dependency}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("root", type=Path)
    args = parser.parse_args()
    errors = missing_dependencies(args.root.resolve())
    if errors:
        for error in errors:
            print(f"mirror dependency error: {error}", file=sys.stderr)
        return 1
    print("mirror operational script dependencies: closed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
