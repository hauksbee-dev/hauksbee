#!/usr/bin/env bash
# Fail closed around GHCR publication. This guard is deliberately read-only:
# the repository and pre-provisioned package must already be private.
set -euo pipefail

phase="${1:-}"
case "$phase" in
    before|after) ;;
    *)
        echo "usage: $0 before|after" >&2
        exit 2
        ;;
esac

repo="hauksbee-dev/hauksbee"
package="orgs/hauksbee-dev/packages/container/hauksbee"

if [ -z "${GH_TOKEN:-}" ]; then
    echo "error: GH_TOKEN is required to verify private container publication" >&2
    exit 1
fi

if ! repo_visibility="$(gh api "repos/$repo" --jq .visibility)"; then
    echo "error: cannot verify repository visibility for $repo" >&2
    exit 1
fi
if [ "$repo_visibility" != private ]; then
    echo "error: repository $repo is not private (reported: $repo_visibility)" >&2
    exit 1
fi

if ! package_visibility="$(gh api "$package" --jq .visibility)"; then
    echo "error: cannot verify GHCR package visibility; pre-provision the package as private before publication" >&2
    exit 1
fi
if [ "$package_visibility" != private ]; then
    echo "error: GHCR package is not private (reported: $package_visibility); pre-provision it as private before publication" >&2
    exit 1
fi

echo "private container publication check ($phase): repository and package are private"
