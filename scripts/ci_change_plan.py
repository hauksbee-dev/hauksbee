#!/usr/bin/env python3
"""Choose the smallest safe CI tier for a set of changed repository paths."""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import PurePosixPath


OUTPUTS = ("rust", "frontend", "vscode", "speed", "full")


def _all_standard() -> dict[str, bool]:
    return {"rust": True, "frontend": True, "vscode": True, "speed": True, "full": False}


def classify(paths: list[str], *, full: bool = False) -> dict[str, bool]:
    if full:
        return {name: True for name in OUTPUTS}
    if not paths:
        return _all_standard()

    result = {name: False for name in OUTPUTS}
    known = True
    for raw in paths:
        path = PurePosixPath(raw.strip().replace("\\", "/"))
        parts = path.parts
        if not parts or path.is_absolute() or ".." in parts:
            known = False
            continue
        top = parts[0]
        value = path.as_posix()

        if top == "frontend":
            result["frontend"] = True
        elif top == "editors":
            result["vscode"] = True
        elif top == "crates":
            result["rust"] = True
            crate = parts[1] if len(parts) > 1 else ""
            if crate in {"hauksbee-engine", "hauksbee-frontdoor-api", "hauksbee-server"}:
                result["frontend"] = True
            if crate == "hauksbee-ci":
                result["vscode"] = True
            if crate == "hauksbee-solve":
                result["speed"] = True
        elif top in {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo"}:
            result.update(rust=True, frontend=True, vscode=True, speed=True)
        elif top in {"scripts", ".github", "app", "docker", "packaging"}:
            # Build, packaging, and workflow changes can alter any compiled shape.
            result.update(rust=True, frontend=True, vscode=True, speed=True)
        elif top in {"integrations", "docs", "examples", "testdata", "qc"}:
            # Their cheap dedicated checks always run. Only source-like fixtures
            # need the compiled suites as well.
            if top in {"examples", "testdata"}:
                result["rust"] = True
        elif top in {
            "README.md", "BETA.md", "COMPLIANCE.md", "CONTRIBUTING.md",
            "LICENSE", "NOTICE", "SECURITY.md", "CLA.md", ".gitignore",
            "deny.toml", "corpus.toml", "bun.lock",
        }:
            pass
        else:
            known = False

        if value.endswith((".rs", ".toml")) and top not in {"docs", "qc"}:
            result["rust"] = True

    if not known:
        conservative = _all_standard()
        for name, enabled in conservative.items():
            result[name] = result[name] or enabled
    return result


def changed_paths(base: str, head: str) -> list[str]:
    proc = subprocess.run(
        ["git", "diff", "--name-only", "--diff-filter=ACMRTUXB", base, head, "--"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return [line for line in proc.stdout.splitlines() if line]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--full", action="store_true")
    parser.add_argument("--github-output")
    parser.add_argument("paths", nargs="*")
    args = parser.parse_args()

    try:
        paths = args.paths or (changed_paths(args.base, args.head) if args.base else [])
    except (OSError, subprocess.CalledProcessError) as exc:
        print(f"CI planner could not resolve the diff; using the conservative tier: {exc}", file=sys.stderr)
        paths = []
    plan = classify(paths, full=args.full)
    rendered = "".join(f"{name}={'true' if plan[name] else 'false'}\n" for name in OUTPUTS)
    if args.github_output:
        with open(args.github_output, "a", encoding="utf-8") as output:
            output.write(rendered)
    else:
        sys.stdout.write(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
