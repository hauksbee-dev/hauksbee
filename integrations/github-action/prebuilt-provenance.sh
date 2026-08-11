#!/usr/bin/env bash
# Record and verify the repository/tag/platform identity of a cached bundle.
set -euo pipefail

mode="${1:-}"
cache_dir="${2:-}"
repository="${3:-}"
tag="${4:-}"
platform="${5:-}"
provenance="$cache_dir/.hauksbee-provenance"

[ -n "$mode" ] && [ -d "$cache_dir" ] && [ -n "$repository" ] \
  && [ -n "$tag" ] && [ -n "$platform" ] || {
  echo "usage: $0 record|verify CACHE_DIR OWNER/REPO TAG PLATFORM" >&2
  exit 2
}

find_one() {
  local name="$1" matches first
  matches="$(find "$cache_dir" -type f -path "*/bin/$name" -perm -111 -print)"
  [ -n "$matches" ] || return 1
  [ "$(printf '%s\n' "$matches" | wc -l | tr -d ' ')" -eq 1 ] || return 1
  first="$(printf '%s\n' "$matches" | head -n 1)"
  printf '%s\n' "$first"
}

digest() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

ci_bin="$(find_one hauksbee-ci)" || {
  echo "cached bundle does not contain exactly one executable hauksbee-ci" >&2
  exit 1
}
run_bin="$(find_one hauksbee)" || {
  echo "cached bundle does not contain exactly one executable hauksbee" >&2
  exit 1
}
ci_rel="${ci_bin#"$cache_dir"/}"
run_rel="${run_bin#"$cache_dir"/}"
expected="$(printf '%s\n' \
  "repository=$repository" \
  "tag=$tag" \
  "platform=$platform" \
  "hauksbee-ci=$ci_rel" \
  "hauksbee-ci-sha256=$(digest "$ci_bin")" \
  "hauksbee=$run_rel" \
  "hauksbee-sha256=$(digest "$run_bin")")"

case "$mode" in
  record)
    umask 077
    printf '%s\n' "$expected" > "$provenance"
    ;;
  verify)
    [ -f "$provenance" ] && [ "$(cat "$provenance")" = "$expected" ] || {
      echo "cached bundle provenance does not match $repository $tag $platform" >&2
      exit 1
    }
    ;;
  *)
    echo "unknown mode: $mode" >&2
    exit 2
    ;;
esac

printf 'ci_bin=%s\nengine_bin=%s\n' "$ci_bin" "$run_bin"
