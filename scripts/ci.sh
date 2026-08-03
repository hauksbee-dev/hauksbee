#!/usr/bin/env bash
# ci.sh - run hauksbee-ci specs in a pipeline, the pleasant way.
#
# A thin wrapper around the `hauksbee-ci` binary that:
#   * finds the binary (PATH, HAUKSBEE_CI_BIN, or a local target/release build),
#   * builds it on the fly if it is missing and cargo is available,
#   * runs one or more specs, writing a JUnit file per spec,
#   * prints a compact summary and exits non-zero if any spec failed.
#
# Designed to be dropped into any CI (GitHub, GitLab, Buildkite). When
# GITHUB_ACTIONS is set, hauksbee-ci itself emits inline ::error annotations.
#
# Usage:
#   scripts/ci.sh [--junit-dir DIR] [--quiet] [--no-build] SPEC [SPEC ...]
#   scripts/ci.sh --help
#
# Options:
#   --junit-dir DIR  Write <spec-name>.xml per spec into DIR (default: ./hauksbee-ci-junit).
#   --quiet          Pass --quiet to hauksbee-ci (suppress per-assertion lines).
#   --no-build       Never build; fail if the binary is not found.
#   --help           Show this help.
#
# Environment:
#   HAUKSBEE_CI_BIN   Explicit path to the hauksbee-ci binary.
#   CARGO            cargo binary (default: cargo), used only if a build is needed.
set -euo pipefail
# shellcheck source=scripts/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; }

JUNIT_DIR="hauksbee-ci-junit"
QUIET=0
NO_BUILD=0
SPECS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --junit-dir) JUNIT_DIR="${2:?--junit-dir needs a directory}"; shift 2 ;;
    --junit-dir=*) JUNIT_DIR="${1#*=}"; shift ;;
    --quiet) QUIET=1; shift ;;
    --no-build) NO_BUILD=1; shift ;;
    -h|--help) usage; exit 0 ;;
    --) shift; while [ $# -gt 0 ]; do SPECS+=("$1"); shift; done ;;
    -*) die "unknown option '$1' (try --help)" ;;
    *) SPECS+=("$1"); shift ;;
  esac
done

[ "${#SPECS[@]}" -gt 0 ] || die "no spec given. Usage: scripts/ci.sh SPEC [SPEC ...] (try --help)"

# Locate hauksbee-ci: explicit env, then the checkout's release build, then
# PATH. The checkout build wins because it is the one this working tree just
# produced; an installed copy on PATH can lag behind it by weeks. When both
# exist and differ, say which one runs and where the other lives.
find_bin() {
  if [ -n "${HAUKSBEE_CI_BIN:-}" ] && [ -x "${HAUKSBEE_CI_BIN}" ]; then
    printf '%s\n' "$HAUKSBEE_CI_BIN"; return 0
  fi
  local local_bin path_bin
  local_bin="$(hauksbee_target_bin)/hauksbee-ci"
  path_bin="$(command -v hauksbee-ci 2>/dev/null || true)"
  if [ -x "$local_bin" ]; then
    if [ -n "$path_bin" ] && ! cmp -s "$local_bin" "$path_bin"; then
      warn "using the checkout build $local_bin; the installed $path_bin differs (re-run scripts/install.sh to refresh it)."
    fi
    printf '%s\n' "$local_bin"; return 0
  fi
  if [ -n "$path_bin" ]; then printf '%s\n' "$path_bin"; return 0; fi
  return 1
}

if ! BIN="$(find_bin)"; then
  if [ "$NO_BUILD" -eq 1 ]; then
    die "hauksbee-ci not found and --no-build set. Set HAUKSBEE_CI_BIN or put it on PATH."
  fi
  CARGO="${CARGO:-cargo}"
  have "$CARGO" || die "hauksbee-ci not found and cargo unavailable to build it."
  log "hauksbee-ci not found; building it (release)"
  ( cd "$HAUKSBEE_ROOT" && "$CARGO" build --release -p hauksbee-ci )
  BIN="$(hauksbee_target_bin)/hauksbee-ci"
fi
log "Using hauksbee-ci: $BIN"

mkdir -p "$JUNIT_DIR"
QUIET_ARG=()
[ "$QUIET" -eq 1 ] && QUIET_ARG=(--quiet)

failed=0
passed=0
for spec in "${SPECS[@]}"; do
  [ -f "$spec" ] || die "spec not found: $spec"
  base="$(basename "${spec%.toml}")"
  junit="$JUNIT_DIR/$base.xml"
  log "Running spec: $spec"
  set +e
  # `${arr[@]+...}` guards empty-array expansion under `set -u` on bash 3.2
  # (the macOS default), where a bare `"${arr[@]}"` on an empty array errors.
  "$BIN" run "$spec" --junit "$junit" ${QUIET_ARG[@]+"${QUIET_ARG[@]}"}
  code=$?
  set -e
  if [ "$code" -eq 0 ]; then
    passed=$((passed + 1)); ok "$base GREEN"
  else
    failed=$((failed + 1)); err "$base RED (exit $code)"
  fi
done

printf '\n'
total=$((passed + failed))
if [ "$failed" -eq 0 ]; then
  log "${C_GREEN}All ${total} spec(s) GREEN.${C_RESET} JUnit in $JUNIT_DIR/"
  exit 0
else
  log "${C_RED}${failed} of ${total} spec(s) RED.${C_RESET} JUnit in $JUNIT_DIR/"
  exit 1
fi
