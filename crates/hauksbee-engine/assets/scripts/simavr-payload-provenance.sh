#!/usr/bin/env bash
# Bind an installed simavr marker to the exact public headers and static archive.
set -euo pipefail

mode="${1:-}"
prefix="${2:-}"
record="$prefix/.hauksbee-simavr-payload.sha256"

if [ -z "$mode" ] || [ -z "$prefix" ]; then
  echo "usage: $0 record|verify PREFIX" >&2
  exit 2
fi

digest() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

archive="$prefix/lib/libsimavr.a"
if [ ! -d "$prefix/include/simavr" ] || [ ! -f "$archive" ]; then
  echo "simavr payload is incomplete under $prefix" >&2
  exit 1
fi
expected="$(
  find "$prefix/include/simavr" -type f -name '*.h' -print \
    | LC_ALL=C sort \
    | while IFS= read -r header; do
        printf '%s  %s\n' "$(digest "$header")" "${header#"$prefix/"}"
      done
  printf '%s  %s\n' "$(digest "$archive")" "lib/libsimavr.a"
)"

case "$mode" in
  record)
    umask 022
    printf '%s\n' "$expected" > "$record"
    ;;
  verify)
    if [ ! -f "$record" ] || [ "$(cat "$record")" != "$expected" ]; then
      echo "simavr payload digest mismatch under $prefix" >&2
      exit 1
    fi
    ;;
  *)
    echo "unknown mode: $mode" >&2
    exit 2
    ;;
esac
