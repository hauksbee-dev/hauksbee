#!/usr/bin/env bash
# Resolve GHCR's username without printing or persisting the credential.
set -euo pipefail

explicit="${1:-}"
actor="${2:-}"
[ -n "${GH_TOKEN:-}" ] || {
  echo "GH_TOKEN is required" >&2
  exit 1
}

if [ -n "$explicit" ]; then
  printf '%s\n' "$explicit"
elif [[ "$GH_TOKEN" == ghs_* ]]; then
  printf '%s\n' x-access-token
elif [ -n "$actor" ]; then
  printf '%s\n' "$actor"
else
  echo "registry username is required for this token" >&2
  exit 1
fi
