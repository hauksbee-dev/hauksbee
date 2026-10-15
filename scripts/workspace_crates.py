#!/usr/bin/env python3
"""The crate lists every script and workflow reads, derived from `cargo metadata`.

Nothing here is a hand-kept list. The workspace members, their `publish`
settings and their path dependencies come from `cargo metadata --no-deps`; the
only thing a crate states itself is which CI test shard runs it:

    [package.metadata.ci]
    test-shard = "foundations"

so adding a crate cannot leave it out of the test matrix, the publish order or
the package-include gate. A crate with no shard fails `check`.

    workspace_crates.py publish-dirs   crate directories in publish order
    workspace_crates.py test-matrix    GitHub Actions matrix JSON (one entry per shard)
    workspace_crates.py check          fail when the metadata is incomplete

Path dependencies that live outside the workspace (the vendored forge crates)
are followed one workspace at a time, so they come first in the publish order.
"""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def _cargo_metadata(manifest: Path) -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--manifest-path", str(manifest)],
        check=True, capture_output=True, text=True, cwd=REPO,
    ).stdout
    return json.loads(out)


def _package(raw: dict) -> dict:
    manifest = Path(raw["manifest_path"]).resolve()
    ci = (raw.get("metadata") or {}).get("ci") or {}
    return {
        "name": raw["name"],
        "version": raw["version"],
        "dir": manifest.parent.relative_to(REPO).as_posix(),
        # `publish = false` is `[]`; unset is None (publishable to any registry).
        "publish": raw.get("publish") != [],
        # Dev-dependencies never constrain publish order (cargo strips them).
        "deps": {d["name"] for d in raw["dependencies"] if d.get("path") and d.get("kind") != "dev"},
        "path_deps": {Path(d["path"]).resolve() for d in raw["dependencies"] if d.get("path")},
        "shard": ci.get("test-shard"),
        "manifest": manifest,
    }


def load() -> tuple[list[dict], list[dict]]:
    """(workspace members, packages of every external workspace they depend on by path)."""
    members = [_package(p) for p in _cargo_metadata(REPO / "Cargo.toml")["packages"]]
    known = {m["manifest"].parent for m in members}
    external: list[dict] = []
    seen_workspaces: set[Path] = set()
    for dep_dir in sorted({d for m in members for d in m["path_deps"]} - known):
        # The external workspace root is the nearest ancestor holding a [workspace].
        root = next((p for p in [dep_dir, *dep_dir.parents]
                     if (p / "Cargo.toml").is_file() and "[workspace]" in (p / "Cargo.toml").read_text()), None)
        if root is None or root in seen_workspaces:
            continue
        seen_workspaces.add(root)
        external += [_package(p) for p in _cargo_metadata(root / "Cargo.toml")["packages"]]
    return members, external


def publish_order(members: list[dict], external: list[dict]) -> list[dict]:
    """Publishable packages, dependencies first (Kahn, name-ordered so the result is stable)."""
    pending = {p["name"]: p for p in [*external, *members] if p["publish"]}
    done: list[dict] = []
    while pending:
        ready = sorted(n for n, p in pending.items() if not (p["deps"] & pending.keys()))
        if not ready:
            raise SystemExit(f"publish order: dependency cycle among {sorted(pending)}")
        for name in ready:
            done.append(pending.pop(name))
    return done


def test_shards(members: list[dict]) -> dict[str, list[str]]:
    shards: dict[str, list[str]] = {}
    for m in members:
        if m["shard"]:
            shards.setdefault(m["shard"], []).append(m["name"])
    return {s: sorted(names) for s, names in sorted(shards.items())}


def problems(members: list[dict]) -> list[str]:
    return [
        f'{m["dir"]}/Cargo.toml: no [package.metadata.ci] test-shard (the CI test matrix would never run {m["name"]})'
        for m in members if not m["shard"]
    ]


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] not in {"publish-dirs", "test-matrix", "check"}:
        print(__doc__, file=sys.stderr)
        return 2
    members, external = load()
    if argv[1] == "publish-dirs":
        print("\n".join(p["dir"] for p in publish_order(members, external)))
    elif argv[1] == "test-matrix":
        bad = problems(members)
        if bad:
            print("\n".join(bad), file=sys.stderr)
            return 1
        include = [
            {"os": "ubuntu-latest", "suite": suite, "packages": " ".join(f"-p {n}" for n in names)}
            for suite, names in test_shards(members).items()
        ]
        print(json.dumps({"include": include}))
    else:
        bad = problems(members)
        if bad:
            print("\n".join(bad), file=sys.stderr)
            return 1
        order = publish_order(members, external)
        print(f"{len(members)} workspace crates all sharded; {len(order)} publishable crates in order")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
