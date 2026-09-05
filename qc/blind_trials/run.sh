#!/usr/bin/env bash
# Prepare one repeatable, genuinely blind first-use trial.
#
# This script only materialises the pinned upstream board and prints the
# operator instructions. It never invokes an agent.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd -P)"
REGISTRY="$SCRIPT_DIR/boards.toml"
WORK_ROOT="$SCRIPT_DIR/work"

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

if [[ $# -ne 1 ]]; then
  printf 'Usage: %s <board-id>\n' "$0" >&2
  exit 2
fi

board_id=$1

# Emit six tab-separated fields so the shell does not have to guess how to
# quote TOML strings containing spaces or punctuation.
entry="$(python3 - "$REGISTRY" "$board_id" <<'PY'
from __future__ import annotations

import sys
import tomllib

registry, wanted = sys.argv[1:]
with open(registry, "rb") as handle:
    data = tomllib.load(handle)

entries = data.get("board", data.get("boards", []))
if not isinstance(entries, list):
    print(f"registry has no [[board]] entries: {registry}", file=sys.stderr)
    raise SystemExit(2)

for item in entries:
    if item.get("id") == wanted:
        fields = (
            item["upstream"],
            item["pre_fix"],
            item["fix"],
            item["board"],
            str(item["approx_footprints"]),
            item["defect"],
        )
        print("\t".join(fields))
        raise SystemExit(0)

known = ", ".join(str(item.get("id", "<missing id>")) for item in entries)
print(f"unknown board id {wanted!r}; known ids: {known}", file=sys.stderr)
raise SystemExit(2)
PY
)" || exit $?

IFS=$'\t' read -r upstream pre_fix fix_ref board_path approx_footprints defect <<< "$entry"

case "$board_path" in
  /*|../*|*/../*|*/..)
    die "registry board path must stay inside the clone: $board_path"
    ;;
esac

trial_dir="$WORK_ROOT/$board_id"
mkdir -p "$WORK_ROOT"

if [[ -d "$trial_dir/.git" ]]; then
  printf 'Upstream clone already present; reusing: %s\n' "$trial_dir"
elif [[ -e "$trial_dir" ]]; then
  die "work path exists but is not a git clone: $trial_dir (move it aside and retry)"
else
  printf 'Cloning %s into %s\n' "$upstream" "$trial_dir"
  git clone --depth=1 --filter=blob:none --no-checkout "$upstream" "$trial_dir"
fi

if ! git -C "$trial_dir" cat-file -e "$pre_fix^{commit}" 2>/dev/null; then
  printf 'Fetching PRE-FIX ref %s\n' "$pre_fix"
  git -C "$trial_dir" fetch --depth=1 origin "$pre_fix"
fi

printf 'Checking out PRE-FIX ref %s\n' "$pre_fix"
git -C "$trial_dir" checkout --detach --quiet "$pre_fix"
actual_ref="$(git -C "$trial_dir" rev-parse HEAD)"
[[ "$actual_ref" == "$pre_fix" ]] || die "checkout resolved to $actual_ref, not PRE-FIX ref $pre_fix"

source_board="$trial_dir/$board_path"
if [[ ! -f "$source_board" ]]; then
  die "board file missing at PRE-FIX ref $pre_fix: expected $board_path (resolved path: $source_board)"
fi

board_copy="$trial_dir/board"
if [[ "$(cd -- "$(dirname -- "$source_board")" && pwd -P)" == "$(cd -- "$board_copy" 2>/dev/null && pwd -P || true)" ]]; then
  die "board copy directory would overwrite the source board directory: $board_copy"
fi

printf 'Copying blind board inputs from %s\n' "$(dirname -- "$source_board")"
python3 - "$source_board" "$board_copy" <<'PY'
from __future__ import annotations

import shutil
import sys
from pathlib import Path

source_board = Path(sys.argv[1])
destination = Path(sys.argv[2])
destination.mkdir(parents=True, exist_ok=True)

# Refresh only this generated board-copy directory. The clone itself is never
# modified except by checkout; a rerun therefore produces the same bytes.
for child in destination.iterdir():
    if child.is_dir() and not child.is_symlink():
        shutil.rmtree(child)
    else:
        child.unlink()

note_suffixes = {".md", ".txt", ".csv", ".xlsx"}
copied = []
for candidate in sorted(source_board.parent.iterdir(), key=lambda path: path.name.casefold()):
    if not candidate.is_file():
        continue
    suffix = candidate.suffix.casefold()
    name = candidate.name.casefold()
    is_input = (
        candidate.name == source_board.name
        or suffix in {".kicad_sch", ".kicad_pro", ".kicad_prl"}
        or name in {"fp-lib-table", "sym-lib-table"}
        or suffix in note_suffixes
    )
    if is_input:
        shutil.copy2(candidate, destination / candidate.name)
        copied.append(candidate.name)

print("Copied files:")
for name in copied:
    print(f"  {name}")

removed = []
for candidate in sorted(destination.rglob("*"), key=lambda path: str(path).casefold()):
    if candidate.is_file() and candidate.suffix.casefold() in note_suffixes:
        removed.append(candidate)
        candidate.unlink()

print("Removed engineering-note/BOM files (exact list):")
if removed:
    for candidate in removed:
        print(f"  {candidate}")
else:
    print("  (none)")
PY

source_pro_count="$(find "$(dirname -- "$source_board")" -maxdepth 1 -type f -iname '*.kicad_pro' -print | wc -l | tr -d '[:space:]')"
copy_pro_count="$(find "$board_copy" -maxdepth 1 -type f -iname '*.kicad_pro' -print | wc -l | tr -d '[:space:]')"
if [[ "$source_pro_count" -gt 0 ]]; then
  [[ "$copy_pro_count" -eq "$source_pro_count" ]] || die "upstream has $source_pro_count .kicad_pro sibling(s), but only $copy_pro_count reached $board_copy"
  printf '.kicad_pro copied: yes (%s file(s))\n' "$copy_pro_count"
else
  printf '.kicad_pro copied: no upstream sibling present\n'
fi

binary="${HAUKSBEE_BIN:-$ROOT/target/release/hauksbee}"
printf '\nBoard copy (absolute path): %s\n' "$(cd -- "$board_copy" && pwd -P)"
printf 'Approximate footprints: %s\n' "$approx_footprints"
printf '\nReady-to-run blind-agent command (operator runs it manually; this script does not invoke an agent):\n'
printf '  python3 -c '\''import pathlib,sys; p=pathlib.Path(sys.argv[1]).read_text(); print(p.replace("{{BOARD}}",sys.argv[2]).replace("{{BINARY}}",sys.argv[3]))'\'' %q %q %q\n' \
  "$SCRIPT_DIR/prompt.md" "$(cd -- "$board_copy" && pwd -P)" "$binary"

printf '\n============================================================\n'
printf 'DO NOT SHOW THIS SECTION TO THE BLIND AGENT\n'
printf 'PLANTED DEFECT: %s\n' "$defect"
printf 'FIX REF: %s\n' "$fix_ref"
