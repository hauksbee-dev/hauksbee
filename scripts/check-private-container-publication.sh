#!/usr/bin/env bash
# Fail closed around GHCR publication. The probe may request creation of an
# inert, zero-layer package marker; before/after remain strict and read-only.
set -euo pipefail

phase="${1:-}"
case "$phase" in
    probe|before|after) ;;
    *)
        echo "usage: $0 probe|before|after" >&2
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
    if [ "$phase" = probe ]; then
        # GitHub creates a new container package private by default. The
        # workflow is allowed to push only a FROM-scratch marker here, then
        # immediately runs the strict `before` check. An API outage or token
        # problem can therefore cause an inert push, but never a product push.
        echo "bootstrap_required=true"
        if [ -n "${GITHUB_OUTPUT:-}" ]; then
            echo "bootstrap_required=true" >> "$GITHUB_OUTPUT"
        fi
        echo "private container publication probe: package is absent or unverifiable; an inert bootstrap is required"
        exit 0
    fi
    echo "error: cannot verify GHCR package visibility; no usable image may be published" >&2
    exit 1
fi
if [ "$package_visibility" != private ]; then
    echo "error: GHCR package is not private (reported: $package_visibility); no usable image may be published" >&2
    exit 1
fi

if [ "$phase" = probe ]; then
    echo "bootstrap_required=false"
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
        echo "bootstrap_required=false" >> "$GITHUB_OUTPUT"
    fi
fi

echo "private container publication check ($phase): repository and package are private"
