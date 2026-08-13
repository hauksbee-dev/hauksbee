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

set +e
package_response="$(gh api --include "$package" 2>&1)"
package_status=$?
set -e
http_status="$(printf '%s\n' "$package_response" \
    | awk '/^HTTP\/[0-9.]+ [0-9][0-9][0-9]/{status=$2} END{print status}')"
if [ "$package_status" -ne 0 ]; then
    if [ "$phase" = probe ] && [ "$http_status" = 404 ]; then
        # Bootstrap only after GitHub authenticated the request and returned an
        # exact package absence. Transport, auth, rate-limit, and parse failures
        # leave visibility unknown and must never authorize a registry mutation.
        echo "bootstrap_required=true"
        if [ -n "${GITHUB_OUTPUT:-}" ]; then
            echo "bootstrap_required=true" >> "$GITHUB_OUTPUT"
        fi
        echo "private container publication probe: authenticated API reports the package absent; an inert bootstrap is required"
        exit 0
    fi
    echo "error: cannot verify GHCR package visibility (HTTP ${http_status:-unknown}); no image may be published" >&2
    exit 1
fi
package_visibility="$(printf '%s\n' "$package_response" | sed -n '/^{/,$p' \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["visibility"])')" || {
    echo "error: GHCR package response did not contain a visibility value" >&2
    exit 1
}
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
