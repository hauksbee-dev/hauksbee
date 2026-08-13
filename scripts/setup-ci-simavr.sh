#!/usr/bin/env bash
# Prepare the immutable libsimavr prerequisite for default-feature Rust CI jobs.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
simavr_commit="f44723e8c42431136d5b4de81f789ded56d7e8fa"
simavr_tag="v1.8"
prefix="${HAUKSBEE_CI_SIMAVR_PREFIX:-$HOME/.hauksbee-simavr/$simavr_commit}"

case "$(uname -s)" in
  Linux)
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends \
      build-essential git pkg-config libelf-dev zlib1g-dev clang libclang-dev
    ;;
  Darwin)
    brew list pkg-config >/dev/null 2>&1 || brew install pkg-config
    ;;
  *)
    echo "unsupported CI host for simavr setup: $(uname -s)" >&2
    exit 1
    ;;
esac

"$root/scripts/install-sims.sh" --avr --prefix "$prefix"
test -f "$prefix/include/simavr/sim_avr.h"
test -f "$prefix/lib/libsimavr.a"
test "$(cat "$prefix/.hauksbee-simavr-commit")" = "$simavr_commit"
"$root/scripts/simavr-payload-provenance.sh" verify "$prefix"
grep -Fx "Version: $simavr_tag" "$prefix/lib/pkgconfig/simavr.pc" >/dev/null

if [ -n "${GITHUB_ENV:-}" ]; then
  printf 'SIMAVR_INCLUDE_DIR=%s\nSIMAVR_LIB_DIR=%s\nSIMAVR_COMMIT=%s\n' \
    "$prefix/include" "$prefix/lib" "$simavr_commit" >> "$GITHUB_ENV"
else
  printf 'export SIMAVR_INCLUDE_DIR=%q\nexport SIMAVR_LIB_DIR=%q\nexport SIMAVR_COMMIT=%q\n' \
    "$prefix/include" "$prefix/lib" "$simavr_commit"
fi
