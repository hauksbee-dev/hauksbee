#!/usr/bin/env bash
# shellcheck disable=SC1003,SC2016
# Fail-closed tag-release preflight for the private hauksbee release repository.
set -euo pipefail

root="${1:-}"
repo="hauksbee-dev/hauksbee"

fail() {
  echo "::error title=Private release preflight failed::$*" >&2
  exit 1
}

[ -n "$root" ] || fail "usage: $0 CHECKOUT_ROOT"
[ -d "$root" ] || fail "checkout root does not exist: $root"
[ -n "${GH_TOKEN:-}" ] \
  || fail "GH_TOKEN is required for tag publication; refusing to skip repository visibility verification"

# These are the product-repository references baked into release artifacts or
# generated integration configuration. The count pins every occurrence in each
# file, while the exact line pins the executable/metadata field rather than
# accepting the slug in an unrelated comment.
check_surface() {
  local relative="$1"
  local expected_count="$2"
  local exact_line="$3"
  local path="$root/$relative"
  local actual_count

  [ -f "$path" ] || fail "$relative is missing"
  actual_count="$(awk -v needle="$repo" '
    { count += gsub(needle, "&") }
    END { print count + 0 }
  ' "$path")"
  [ "$actual_count" = "$expected_count" ] \
    || fail "$relative contains $actual_count release-repository reference(s); expected $expected_count exact reference(s) to $repo"
  grep -Fqx -- "$exact_line" "$path" \
    || fail "$relative does not carry the exact private release-repository field"
}

# The expectations below intentionally use literal single-quoted $, backticks
# and trailing backslashes from the target files; none is shell expansion.
check_surface "scripts/get-hauksbee.sh" 3 'REPO="hauksbee-dev/hauksbee"'
check_surface "scripts/get-hauksbee.ps1" 3 '$Repo = "hauksbee-dev/hauksbee"'
check_surface "scripts/bundle.sh" 2 'Source: https://github.com/hauksbee-dev/hauksbee  commit ${GIT_SHA}'
check_surface "app/macos/build-app.sh" 1 '  https://github.com/hauksbee-dev/hauksbee   commit ${GIT_SHA}'
check_surface "docker/Dockerfile.slim" 2 '      org.opencontainers.image.source="https://github.com/hauksbee-dev/hauksbee" \'
check_surface "docker/Dockerfile.full" 1 '      org.opencontainers.image.source="https://github.com/hauksbee-dev/hauksbee" \'
check_surface ".github/workflows/docker.yml" 3 '  IMAGE: ghcr.io/hauksbee-dev/hauksbee'
check_surface "integrations/github-action/action.yml" 3 '    default: "hauksbee-dev/hauksbee"'
check_surface "integrations/kicad-plugin/build-pcm.sh" 1 '    download_url=f"https://github.com/hauksbee-dev/hauksbee/releases/download/v{version}/{zip_name}",'
check_surface "integrations/kicad-plugin/metadata.json" 3 '        "repository": "https://github.com/hauksbee-dev/hauksbee",'
check_surface "frontend/src/lib/version.ts" 1 'export const ACTION_REF = `hauksbee-dev/hauksbee/integrations/github-action@${RELEASE_TAG}`'
check_surface "crates/hauksbee-ci/src/integrate.rs" 3 '        "  - repo: https://github.com/hauksbee-dev/hauksbee\n\'

if ! visibility="$(gh api "repos/$repo" --jq .visibility)"; then
  fail "gh api repos/$repo failed; create the private release repository before publishing"
fi
[ "$visibility" = "private" ] \
  || fail "$repo reports visibility '$visibility', not private; refusing to publish release assets"

echo "private release preflight: $repo exists, is private, and all baked repository surfaces match"
