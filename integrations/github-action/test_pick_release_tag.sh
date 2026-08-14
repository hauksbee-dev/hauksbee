#!/usr/bin/env bash
# Tests for pick-release-tag.sh. Run with:
#
#     bash integrations/github-action/test_pick_release_tag.sh
#
# The case that matters is the last one: a caller who pins hauksbee-ref to a
# branch or a SHA must not silently get the latest release instead. A green run
# from a hauksbee they did not name tells them nothing about the one they did.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PICK="$HERE/pick-release-tag.sh"
fails=0

check() {
  local want="$1" desc="$2"; shift 2
  local got
  got="$(bash "$PICK" "$@" 2>&1)"
  if [ "$got" = "$want" ]; then
    printf 'ok   %s\n' "$desc"
  else
    printf 'FAIL %s\n       args: %s\n       want: %s\n       got:  %s\n' \
      "$desc" "$*" "$want" "$got"
    fails=$((fails + 1))
  fi
}

# args are: <version> <ref> <default-ref>
check "v1.2.3" "an explicit version is taken as given"            "v1.2.3" "main" "main"
check "v1.2.3" "and gains its v when written without one"         "1.2.3"  "main" "main"
check "v0.9.0" "an explicit version beats the ref"                "0.9.0"  "v1.0.0" "main"
check "v1.0.0" "a ref that is a release tag names that release"   ""       "v1.0.0" "main"
check "LATEST" "nothing pinned: the newest release, no compile"   ""       "main" "main"
check "LATEST" "an empty ref is not a pin"                        ""       ""     "main"
check "SOURCE" "a pinned branch is built, not swapped for latest" ""       "my-fix" "main"
check "SOURCE" "a pinned SHA likewise"                            ""       "9f1c2ab0" "main"
check "SOURCE" "and a non-default default-ref is respected"       ""       "main" "develop"

if bash "$PICK" 'v1$(id)' main main >/dev/null 2>&1; then
  printf 'FAIL unsafe explicit version was accepted\n'
  fails=$((fails + 1))
else
  printf 'ok   unsafe explicit version is refused\n'
fi
if bash "$PICK" '' 'v1;echo-PWN' main >/dev/null 2>&1; then
  printf 'FAIL unsafe release ref was accepted\n'
  fails=$((fails + 1))
else
  printf 'ok   unsafe release ref is refused\n'
fi
if bash "$PICK" $'v1.2.3\nforged-output=1' main main >/dev/null 2>&1; then
  printf 'FAIL newline-bearing explicit version was accepted\n'
  fails=$((fails + 1))
else
  printf 'ok   newline-bearing explicit version is refused\n'
fi

printf '\n'
if [ "$fails" -eq 0 ]; then
  echo "all pick-release-tag tests passed"
else
  echo "$fails failed"
  exit 1
fi
