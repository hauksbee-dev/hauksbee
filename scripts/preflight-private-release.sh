#!/usr/bin/env bash
# shellcheck disable=SC1003,SC2016
# Fail-closed tag-release preflight for the private hauksbee release repository.
set -euo pipefail

root="${1:-}"

fail() {
  echo "::error title=Private release preflight failed::$*" >&2
  exit 1
}

[ -n "$root" ] || fail "usage: $0 CHECKOUT_ROOT"
[ -d "$root" ] || fail "checkout root does not exist: $root"
[ -n "${GH_TOKEN:-}" ] \
  || fail "GH_TOKEN is required for tag publication; refusing to skip repository visibility verification"

checker="$root/scripts/check-private-release-surfaces.py"
[ -f "$checker" ] || fail "scripts/check-private-release-surfaces.py is missing"
if ! repo="$(python3 "$checker" "$root" --print-repository)"; then
  fail "private release surface classification failed"
fi

if ! visibility="$(gh api "repos/$repo" --jq .visibility)"; then
  fail "gh api repos/$repo failed; create the private release repository before publishing"
fi
[ "$visibility" = "private" ] \
  || fail "$repo reports visibility '$visibility', not private; refusing to publish release assets"

if ! immutable_releases="$(gh api "repos/$repo/immutable-releases" --jq .enabled)"; then
  fail "$repo does not enforce immutable releases; enable release immutability before publishing"
fi
[ "$immutable_releases" = true ] \
  || fail "$repo does not enforce immutable releases; refusing replaceable release assets"

echo "private release preflight: $repo is private, enforces immutable releases, and all baked repository surfaces match"
