#!/usr/bin/env bash
# Fail closed around GHCR publication. This is the public open-core repo: its
# images are consumed without credentials (the GitHub Action defaults to the
# slim image and pulls it with the caller's automatic token), so the
# repository must be public and the published package must end up public. The
# probe may request creation of an inert, zero-layer package marker; before
# remains read-only, and after verifies what consumers will actually see.
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
    echo "error: GH_TOKEN is required to verify container publication" >&2
    exit 1
fi

if ! repo_visibility="$(gh api "repos/$repo" --jq .visibility)"; then
    echo "error: cannot verify repository visibility for $repo" >&2
    exit 1
fi
if [ "$repo_visibility" != public ]; then
    echo "error: repository $repo is not public (reported: $repo_visibility); its images are published for anonymous pulls and must come from the public repository" >&2
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
        echo "container publication probe: authenticated API reports the package absent; an inert bootstrap is required"
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
if [ "$phase" = after ] && [ "$package_visibility" != public ]; then
    echo "error: GHCR package is not public (reported: $package_visibility). Consumers pull it anonymously, so it is unusable until a maintainer makes it public once: github.com -> hauksbee-dev organization -> Packages -> hauksbee -> Package settings -> Change visibility -> Public. GitHub exposes no API for this change." >&2
    exit 1
fi

if [ "$phase" = probe ]; then
    echo "bootstrap_required=false"
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
        echo "bootstrap_required=false" >> "$GITHUB_OUTPUT"
    fi
fi

case "$phase" in
    after) echo "container publication check (after): repository and package are public" ;;
    *) echo "container publication check ($phase): repository is public and the package is present" ;;
esac
