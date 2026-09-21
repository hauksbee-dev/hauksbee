#!/usr/bin/env bash
# Local Cargo entrypoint: retain useful caches, clear abandoned linker objects.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# Explicit alternate output locations or retained temporaries are not ours to prune.
for arg in "$@"; do
  case "$arg" in
    --target-dir*|--build-dir*|*save-temps*) exec cargo "$@" ;;
  esac
done
python3 scripts/clean_abandoned_rust_objects.py --apply
set +e
cargo "$@"
status=$?
set -e
# A failed/interrupted compiler can leave temporary objects behind. The cleaner
# takes Cargo's locks and skips while any compiler/linker still owns the work.
python3 scripts/clean_abandoned_rust_objects.py --apply || printf '%s\n' 'Rust object cleanup failed; Cargo status preserved.' >&2
exit "$status"
