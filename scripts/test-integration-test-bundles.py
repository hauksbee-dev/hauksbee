#!/usr/bin/env python3
"""Fail if Cargo test bundling drops or duplicates an integration source."""

from __future__ import annotations

import re
import tomllib
from collections import Counter
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PATH_MODULE = re.compile(r'#\[path\s*=\s*"([^"]+\.rs)"\]\s*\n\s*mod\s+', re.MULTILINE)


def main() -> None:
    checked = 0
    for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        data = tomllib.loads(manifest.read_text())
        if data.get("package", {}).get("autotests", True) is not False:
            continue

        crate = manifest.parent
        tests_dir = crate / "tests"
        declarations = data.get("test", [])
        if not declarations:
            raise AssertionError(f"{manifest}: autotests=false without explicit [[test]] targets")

        declared_paths = [crate / target["path"] for target in declarations]
        missing = [path for path in declared_paths if not path.is_file()]
        if missing:
            raise AssertionError(f"{manifest}: missing declared targets: {missing}")

        bundle_drivers: set[Path] = set()
        bundled_sources: list[Path] = []
        for driver in declared_paths:
            refs = PATH_MODULE.findall(driver.read_text())
            if not refs:
                continue
            bundle_drivers.add(driver.resolve())
            for ref in refs:
                source = (driver.parent / ref).resolve()
                if source.parent != tests_dir.resolve():
                    raise AssertionError(f"{driver}: bundled source escapes tests/: {ref}")
                if not source.is_file():
                    raise AssertionError(f"{driver}: bundled source does not exist: {ref}")
                bundled_sources.append(source)

        standalone_sources = [
            path.resolve() for path in declared_paths if path.resolve() not in bundle_drivers
        ]
        actual_sources = {
            path.resolve()
            for path in tests_dir.glob("*.rs")
            if path.resolve() not in bundle_drivers
        }
        accounted = standalone_sources + bundled_sources
        counts = Counter(accounted)
        duplicates = sorted(path for path, count in counts.items() if count != 1)
        omitted = sorted(actual_sources - counts.keys())
        unexpected = sorted(counts.keys() - actual_sources)
        if duplicates or omitted or unexpected:
            raise AssertionError(
                f"{manifest}: integration bundle mismatch\n"
                f"duplicates={duplicates}\nomitted={omitted}\nunexpected={unexpected}"
            )

        checked += 1
        print(
            f"ok {crate.name}: {len(actual_sources)} sources -> "
            f"{len(declarations)} test executables"
        )

    if checked == 0:
        raise AssertionError("no autotests=false crate was checked")


if __name__ == "__main__":
    main()
