#!/usr/bin/env bash
# Gzipped size budget for the web app's JavaScript, run after `vite build`.
#   main chunk  (index-*.js, the first paint)            <= MAIN_BUDGET_KB   (default 228)
#   editor chunk (TomlEditor-*.js + chunks only it pulls) <= EDITOR_BUDGET_KB (default 140)
# usage: scripts/check-bundle-size.sh [frontend/dist]
set -euo pipefail

dist=${1:-"$(cd "$(dirname "$0")/.." && pwd)/frontend/dist"}
assets="$dist/assets"
main_budget=${MAIN_BUDGET_KB:-228}
editor_budget=${EDITOR_BUDGET_KB:-140}

gz() { gzip -9 -c "$1" | wc -c | tr -d ' '; }
kb() { awk -v b="$1" 'BEGIN { printf "%.1f", b / 1024 }'; }

shopt -s nullglob
mains=("$assets"/index-*.js)
editors=("$assets"/TomlEditor-*.js)
if [ "${#mains[@]}" -ne 1 ]; then
  echo "bundle-size: expected one main chunk in $assets, found ${#mains[@]}" >&2
  exit 1
fi
main_gz=$(gz "${mains[0]}")

editor_gz=0
if [ "${#editors[@]}" -eq 1 ]; then
  editor_gz=$(gz "${editors[0]}")
  # Chunks the editor imports that the main chunk does not (a vendor split).
  for dep in $(grep -oE '"\./[A-Za-z0-9_.-]+\.js"' "${editors[0]}" | tr -d '"' | sed 's#^\./##' | sort -u); do
    case "$dep" in index-*) continue ;; esac
    [ -f "$assets/$dep" ] && editor_gz=$((editor_gz + $(gz "$assets/$dep")))
  done
else
  echo "bundle-size: expected one TomlEditor chunk in $assets, found ${#editors[@]}" >&2
  exit 1
fi

status=0
echo "bundle-size: main $(kb "$main_gz") KB gz (budget $main_budget KB), editor $(kb "$editor_gz") KB gz (budget $editor_budget KB)"
if [ "$main_gz" -gt $((main_budget * 1024)) ]; then
  echo "bundle-size: main chunk is over budget" >&2
  status=1
fi
if [ "$editor_gz" -gt $((editor_budget * 1024)) ]; then
  echo "bundle-size: editor chunk is over budget" >&2
  status=1
fi
exit $status
