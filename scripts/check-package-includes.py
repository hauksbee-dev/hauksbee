#!/usr/bin/env python3
"""Fail when a publish-set crate embeds a file from outside its own directory.

`cargo package` ships only files under the crate directory, and it does not
parse Rust source: an `include_str!("../../../scripts/x")` passes packaging,
then fails to COMPILE for the first user of the published crate. `cargo
package --no-verify` cannot catch this (no build), and the full verify build
cannot run until every workspace dependency is on crates.io, so this script
is the gate: it resolves every include_str!/include_bytes! path in each
publish-set crate's src/ and fails, naming file:line, when the target
escapes the crate directory or does not exist.

Scope is src/**.rs (what dependents compile), EXCLUDING `#[cfg(test)]`
module bodies: a test-only include inside src is compiled by `cargo test` in
this repo but never by a dependent of the published crate, and never by the
publish verify build. tests/ also ships in the package but is never built by
a dependent, so it is out of scope here.
"""
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Mirrors scripts/make-public.sh CRATES_ORDER (the authoritative publish set).
CRATES = [
    "vendor/kicad-forge/crates/forge-sexpr",
    "vendor/kicad-forge/crates/forge-model",
    "vendor/kicad-forge/crates/forge-codegen",
    "crates/hauksbee-ir",
    "crates/hauksbee-testkit",
    "crates/hauksbee-solve",
    "crates/hauksbee-models",
    "crates/hauksbee-extract",
    "crates/hauksbee-mcu",
    "crates/hauksbee-server",
    "crates/hauksbee-engine",
    "crates/hauksbee-ci",
    "crates/hauksbee-mcp",
]

INCLUDE = re.compile(r'include_(?:str|bytes)!\s*\(\s*"([^"]+)"\s*\)')


def strip_strings_and_comments(text: str) -> str:
    """Replace string/comment contents with spaces, preserving line structure.

    Brace-counting over raw source is wrong the moment a test fixture embeds a
    board file with unbalanced braces in a string literal; this state machine
    blanks string and comment interiors (keeping newlines) so the brace count
    below sees only code."""
    out = []
    i, n = 0, len(text)
    state = "code"  # code | line_comment | block_comment | str | raw_str | char
    raw_hashes = 0
    block_depth = 0
    while i < n:
        c = text[i]
        nxt = text[i + 1] if i + 1 < n else ""
        if state == "code":
            if c == "/" and nxt == "/":
                state = "line_comment"; out.append("  "); i += 2; continue
            if c == "/" and nxt == "*":
                state = "block_comment"; block_depth = 1; out.append("  "); i += 2; continue
            if c == "r" and (nxt == '"' or nxt == "#"):
                j = i + 1; h = 0
                while j < n and text[j] == "#":
                    h += 1; j += 1
                if j < n and text[j] == '"':
                    state = "raw_str"; raw_hashes = h
                    out.append(" " * (j - i + 1)); i = j + 1; continue
            if c == '"':
                state = "str"; out.append(" "); i += 1; continue
            out.append(c); i += 1; continue
        if state == "line_comment":
            if c == "\n":
                state = "code"; out.append(c)
            else:
                out.append(" ")
            i += 1; continue
        if state == "block_comment":
            if c == "/" and nxt == "*":
                block_depth += 1; out.append("  "); i += 2; continue
            if c == "*" and nxt == "/":
                block_depth -= 1; out.append("  "); i += 2
                if block_depth == 0:
                    state = "code"
                continue
            out.append(c if c == "\n" else " "); i += 1; continue
        if state == "str":
            if c == "\\":
                # An escaped char may be a line-continuation newline; blanking
                # it would shift every subsequent line number.
                esc = text[i + 1] if i + 1 < n else ""
                out.append(" ")
                out.append(esc if esc == "\n" else " ")
                i += 2; continue
            if c == '"':
                state = "code"; out.append(" "); i += 1; continue
            out.append(c if c == "\n" else " "); i += 1; continue
        if state == "raw_str":
            if c == '"' and text[i + 1 : i + 1 + raw_hashes] == "#" * raw_hashes:
                state = "code"; out.append(" " * (1 + raw_hashes)); i += 1 + raw_hashes; continue
            out.append(c if c == "\n" else " "); i += 1; continue
    return "".join(out)


def test_module_lines(text: str) -> set[int]:
    """1-based line numbers inside test-cfg'd mod bodies.

    Matches `#[cfg(test)]` and compound forms like
    `#[cfg(all(test, feature = "renode"))]`: any cfg attribute carrying the
    bare `test` predicate marks the following mod as dependent-invisible."""
    stripped = strip_strings_and_comments(text)
    lines = stripped.splitlines()
    orig_lines = text.splitlines()
    inside: set[int] = set()
    i = 0
    while i < len(lines):
        line_i = orig_lines[i]
        is_test_cfg = "#[cfg(test)]" in line_i or (
            "#[cfg(" in line_i and re.search(r"[(,\s]test[),\s]", line_i)
        )
        if is_test_cfg:
            j = i
            while j < len(lines) and "mod " not in lines[j]:
                j += 1
                if j - i > 3:
                    break
            if j < len(lines) and "mod " in lines[j]:
                depth = 0
                opened = False
                k = j
                while k < len(lines):
                    depth += lines[k].count("{") - lines[k].count("}")
                    if lines[k].count("{"):
                        opened = True
                    inside.add(k + 1)
                    if opened and depth <= 0:
                        break
                    k += 1
                i = k
        i += 1
    return inside


def main() -> int:
    failures = []
    for crate_rel in CRATES:
        crate_root = (REPO / crate_rel).resolve()
        if not crate_root.is_dir():
            failures.append(f"{crate_rel}: crate directory missing (publish set drifted?)")
            continue
        for rs in sorted((crate_root / "src").rglob("*.rs")):
            text = rs.read_text(encoding="utf-8", errors="replace")
            in_tests = test_module_lines(text)
            for lineno, line in enumerate(text.splitlines(), 1):
                if lineno in in_tests:
                    continue
                for m in INCLUDE.finditer(line):
                    target = (rs.parent / m.group(1)).resolve()
                    rel_rs = rs.relative_to(REPO)
                    inside = target.is_relative_to(crate_root)
                    exists = target.is_file()
                    if inside and exists:
                        continue
                    if not inside:
                        failures.append(
                            f"{rel_rs}:{lineno}: include escapes the crate directory: "
                            f'"{m.group(1)}" -> {target} (cargo package will not ship it)'
                        )
                    else:
                        failures.append(
                            f"{rel_rs}:{lineno}: include target does not exist: \"{m.group(1)}\""
                        )
    if failures:
        print(f"\n{len(failures)} package-breaking include(s):", file=sys.stderr)
        for f in failures:
            print(f"  {f}", file=sys.stderr)
        print(
            "\nFix: mirror the asset into the crate (see scripts/sync-crate-assets.sh) "
            "and include it from there.",
            file=sys.stderr,
        )
        return 1
    print(f"package includes clean across {len(CRATES)} crates")
    return 0

if __name__ == "__main__":
    sys.exit(main())
