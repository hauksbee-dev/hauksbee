#!/usr/bin/env bash
# Local Cargo entrypoint: seed new worktrees, retain useful caches, clear abandoned linker objects.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# Explicit alternate output locations or retained temporaries are not ours to prune.
for arg in "$@"; do
  case "$arg" in
    --target-dir*|--build-dir*|*save-temps*) exec cargo "$@" ;;
  esac
done
# A new linked worktree starts from an APFS clone of the main checkout's build dir, minus
# workspace fingerprints: those crates rebuild here, so CARGO_MANIFEST_DIR is never the main's.
seed="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null || echo /nonexistent)/../target/.build/debug"
new=target/.build/debug.seeding
if [[ "$(uname)" == Darwin && -z "${CARGO_TARGET_DIR:-}${CARGO_BUILD_BUILD_DIR:-}" && ! -e target/.build/debug && -f "$seed/.cargo-lock" ]] &&
  rm -rf "$new" && mkdir -p "$new" target/debug && lockf -ks -t 120 "$seed/.cargo-lock" cp -c -R -p "$seed/deps" "$seed/build" "$seed/.fingerprint" "$new/"; then
  find "$new/.fingerprint" -maxdepth 1 \( -name 'hauksbee*' -o -name 'forge-*' \) -exec rm -rf {} +
  touch "$new/.cargo-lock" target/debug/.cargo-lock && mv "$new" target/.build/debug
fi
python3 scripts/clean_abandoned_rust_objects.py --apply
set +e
cargo "$@"
status=$?
set -e
# A failed/interrupted compiler can leave temporary objects behind. The cleaner
# takes Cargo's locks and skips while any compiler/linker still owns the work.
python3 scripts/clean_abandoned_rust_objects.py --apply || printf '%s\n' 'Rust object cleanup failed; Cargo status preserved.' >&2
exit "$status"
