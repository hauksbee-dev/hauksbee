#!/usr/bin/env python3
"""Fail when a copied release fact disagrees with the file that owns it.

Each fact is stated once and every other site is read, generated or listed
here, so a bump that misses a site fails loudly instead of shipping skew:

  version     [workspace.package] version in Cargo.toml -> the internal path-dep
              pins in every crate manifest, frontend/package.json, the KiCad
              plugin metadata and the example workflow
  simavr      SIMAVR_COMMIT / SIMAVR_TAG in install-sims.sh -> the CI setup
              script, both Dockerfiles' build args and the provenance test
  simulators  scripts/required-simulator-versions.env -> the Windows installer,
              Dockerfile.full and the QEMU installer's fallback tag

    check-release-facts.py            check every fact
    check-release-facts.py --write    rewrite the version sites from Cargo.toml

Only reads files (the crate lists come from scripts/workspace_crates.py).
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import workspace_crates  # noqa: E402

REPO = workspace_crates.REPO
ENGINE_SCRIPTS = "crates/hauksbee-engine/assets/scripts"


def read(rel: str) -> str:
    return (REPO / rel).read_text(encoding="utf-8")


def find(rel: str, pattern: str) -> list[str]:
    return re.findall(pattern, read(rel), re.M)


# --- version ---------------------------------------------------------------

def workspace_version() -> str:
    return re.search(r'(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"', read("Cargo.toml")).group(1)


def version_sites(members: list[dict]) -> list[tuple[str, str, bool]]:
    """(file, regex, must match) triples; group 1 of every match is a copy of the workspace version."""
    pin = r'^hauksbee-[a-z-]+ = \{ version = "([^"]+)", path'
    return [(f'{m["dir"]}/Cargo.toml', pin, False) for m in members] + [
        ("frontend/package.json", r'^  "version": "([^"]+)"', True),
        ("integrations/kicad-plugin/metadata.json", r'"version": "(\d[^"]*)"', True),
        ("integrations/github-action/example-workflow.yml", r"hauksbee-version: v(\S+)", True),
    ]


# --- pinned tool versions --------------------------------------------------

def pin_groups() -> dict[str, tuple[tuple[str, str], list[tuple[str, str]]]]:
    """fact -> (owner (file, regex), copies [(file, regex)]); group 1 is the value."""
    env = "scripts/required-simulator-versions.env"
    sims = "scripts/install-sims-windows.ps1"
    return {
        "simavr commit": (
            (f"{ENGINE_SCRIPTS}/install-sims.sh", r'^SIMAVR_COMMIT="([0-9a-f]{40})"'),
            [("scripts/setup-ci-simavr.sh", r'^simavr_commit="([0-9a-f]{40})"'),
             ("docker/Dockerfile.slim", r"^ARG SIMAVR_COMMIT=([0-9a-f]{40})"),
             ("scripts/test-simavr-provenance.py", r'^COMMIT = "([0-9a-f]{40})"'),
             ("frontend/tests/ci-wiring.test.ts", r'SIMAVR_COMMIT="([0-9a-f]{40})"')],
        ),
        "simavr tag": (
            (f"{ENGINE_SCRIPTS}/install-sims.sh", r'^SIMAVR_TAG="([^"]+)"'),
            [("scripts/setup-ci-simavr.sh", r'^simavr_tag="([^"]+)"'),
             ("docker/Dockerfile.slim", r"^ARG SIMAVR_TAG=(\S+)")],
        ),
        "Renode version": (
            (env, r'^RENODE_VERSION="([^"]+)"'),
            [(sims, r'^\$RenodeVersion = "([^"]+)"'), ("docker/Dockerfile.full", r"^ARG RENODE_VERSION=(\S+)")],
        ),
        "ESP-QEMU tag": (
            (env, r'^QEMU_VERSION="([^"]+)"'),
            [(sims, r'^\$QemuTag = "([^"]+)"'), ("docker/Dockerfile.full", r"^ARG ESP_QEMU_TAG=(\S+)"),
             ("crates/hauksbee-mcu/src/qemu/install.rs", r'^pub const FALLBACK_TAG: &str = "([^"]+)"')],
        ),
        "ESP-QEMU commit": (
            (env, r'^QEMU_COMMIT="([0-9a-f]{40})"'),
            [("docker/Dockerfile.full", r"^ARG ESP_QEMU_SOURCE_COMMIT=([0-9a-f]{40})")],
        ),
    }


def problems(members: list[dict]) -> list[str]:
    out: list[str] = []
    want = workspace_version()
    for rel, pattern, required in version_sites(members):
        found = find(rel, pattern)
        if not found and required:
            out.append(f"{rel}: no version found (pattern {pattern!r})")
        out += [f"{rel}: version {v} != workspace {want}" for v in found if v != want]
    for fact, ((owner_file, owner_re), copies) in pin_groups().items():
        owner = find(owner_file, owner_re)
        if len(owner) != 1:
            out.append(f"{owner_file}: expected one {fact}, found {owner}")
            continue
        for rel, pattern in copies:
            found = find(rel, pattern)
            if not found:
                out.append(f"{rel}: no {fact} found (pattern {pattern!r})")
            out += [f"{rel}: {fact} {v} != {owner[0]} (owner {owner_file})" for v in found if v != owner[0]]
    # The ESP-QEMU asset name is the tag with `esp-develop-` -> `esp_develop_` and dashes -> underscores.
    tag = find("scripts/required-simulator-versions.env", r'^QEMU_VERSION="([^"]+)"')[0]
    for v in find("docker/Dockerfile.full", r"^ARG ESP_QEMU_FILE=(\S+)"):
        if v != tag.replace("-", "_"):
            out.append(f"docker/Dockerfile.full: ESP_QEMU_FILE {v} != tag {tag} with dashes as underscores")
    return out


def write_versions(members: list[dict]) -> None:
    want = workspace_version()
    for rel, pattern, _ in version_sites(members):
        text = read(rel)
        new = re.sub(pattern, lambda m: m.group(0).replace(m.group(1), want), text, flags=re.M)
        if new != text:
            (REPO / rel).write_text(new, encoding="utf-8")
            print(f"updated {rel}")


def main(argv: list[str]) -> int:
    members, _ = workspace_crates.load()
    if argv[1:] == ["--write"]:
        write_versions(members)
    elif argv[1:]:
        print(__doc__, file=sys.stderr)
        return 2
    bad = problems(members)
    if bad:
        print("\n".join(bad), file=sys.stderr)
        print(f"\n{len(bad)} release fact(s) out of step. Version sites: run this script with --write.", file=sys.stderr)
        return 1
    print(f"release facts consistent (version {workspace_version()}, {len(pin_groups())} pinned-tool groups)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
