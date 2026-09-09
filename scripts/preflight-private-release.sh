#!/usr/bin/env bash
# shellcheck disable=SC1003,SC2016
# Fail-closed tag-release preflight. Development defaults to a private target;
# a curated public mirror sets HAUKSBEE_EXPECTED_REPOSITORY_VISIBILITY=public.
set -euo pipefail

root="${1:-}"

fail() {
  echo "::error title=Release preflight failed::$*" >&2
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
  fail "gh api repos/$repo failed; create the release repository before publishing"
fi
expected_visibility="${HAUKSBEE_EXPECTED_REPOSITORY_VISIBILITY:-private}"
case "$expected_visibility" in
  private|public) ;;
  *) fail "HAUKSBEE_EXPECTED_REPOSITORY_VISIBILITY must be private or public" ;;
esac
[ "$visibility" = "$expected_visibility" ] \
  || fail "$repo reports visibility '$visibility', not $expected_visibility; refusing to publish release assets"

# Reading the repository immutable-release setting needs administration
# read, which GitHub never grants a workflow's integration token on a
# private repository. The publish job proves immutability on the release
# itself afterwards (`gh release verify` and `verify-asset` fail on
# anything mutable, and the reconcile step refuses a mutable existing
# release), so that one specific 403 defers to those checks instead of
# failing a gate this credential can never satisfy. Every readable answer
# still enforces here, and any other error still fails closed.
immutable_status=0
immutable_probe="$(gh api "repos/$repo/immutable-releases" --jq .enabled 2>&1)" || immutable_status=$?
if [ "$immutable_status" -eq 0 ]; then
  [ "$immutable_probe" = true ] \
    || fail "$repo does not enforce immutable releases; refusing replaceable release assets"
  immutable_note="enforces immutable releases"
elif printf '%s' "$immutable_probe" | grep -qi "Resource not accessible by integration"; then
  echo "release preflight: the integration token cannot read the immutable-release setting; deferring to post-publish immutable attestation verification"
  immutable_note="defers immutable-release proof to post-publish attestation"
else
  fail "$repo immutable-release setting could not be read: $immutable_probe"
fi

echo "release preflight: $repo is $expected_visibility, $immutable_note, and all baked repository surfaces match"
