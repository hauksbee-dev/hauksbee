#!/usr/bin/env bash
# Decide which prebuilt hauksbee-ci release, if any, the action should download.
#
# Split out of action.yml so the decision can be tested. It is the one piece of
# the action with a wrong answer available: silently running a different build
# of hauksbee-ci than the caller pinned makes a green run mean nothing.
#
# Usage:  pick-release-tag.sh <hauksbee-version> <hauksbee-ref> <default-ref>
# Prints one of:
#   v1.2.3    download exactly this release
#   LATEST    download the newest release
#   SOURCE    do not download; build from the pinned ref
set -euo pipefail

VERSION="${1-}"
REF="${2-}"
DEFAULT_REF="${3-main}"

# An explicit version wins over everything. Accept it with or without the v.
if [ -n "$VERSION" ]; then
  case "$VERSION" in
    v*) printf '%s\n' "$VERSION" ;;
    *)  printf 'v%s\n' "$VERSION" ;;
  esac
  exit 0
fi

# A ref that is itself a release tag names the release it wants.
if printf '%s' "$REF" | grep -Eq '^v[0-9]'; then
  printf '%s\n' "$REF"
  exit 0
fi

# A ref the caller deliberately changed away from the default is a pin: a
# branch they are testing, or a SHA they are reproducing a result from.
# Downloading the latest release there would run a DIFFERENT hauksbee than the
# one they named, and report green as though it had been theirs. Build it.
if [ -n "$REF" ] && [ "$REF" != "$DEFAULT_REF" ]; then
  printf 'SOURCE\n'
  exit 0
fi

# Default ref, no version: nobody pinned anything, so the newest release is
# what they want, and it saves everyone a multi-minute compile.
printf 'LATEST\n'
