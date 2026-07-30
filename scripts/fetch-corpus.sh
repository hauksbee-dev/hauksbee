#!/usr/bin/env bash
# fetch-corpus.sh - materialise the board corpus hauksbee's checks are measured
# against.
#
# Every board is public open hardware fetched from its author at the exact
# revision recorded in corpus.toml. They are not vendored into this repository:
# they carry CC BY-SA, GPL-3.0 and CERN-OHL terms, so you obtain each one
# directly from its author under that author's licence.
#
# Usage:
#   scripts/fetch-corpus.sh [--dir DIR] [--only ID[,ID...]] [--include-unconfirmed]
#                           [--list] [--force] [--help]
#
# Options:
#   --dir DIR              Where to put the corpus (default: $HAUKSBEE_CORPUS_DIR,
#                          else the manifest's default_dir).
#   --only ID[,ID...]      Fetch just these board ids.
#   --include-unconfirmed  Also fetch boards whose licence could not be
#                          established. Skipped by default; read corpus.toml
#                          and decide for yourself.
#   --list                 Print what would be fetched and exit.
#   --force                Re-fetch boards that are already present.
#   --help                 Show this help.
#
# Then point the tests at it:
#   export HAUKSBEE_CORPUS_DIR=$PWD/board-corpus
#   HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace --features avr
set -euo pipefail
# shellcheck source=scripts/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; }

MANIFEST="${HAUKSBEE_ROOT}/corpus.toml"
DIR=""
ONLY=""
INCLUDE_UNCONFIRMED=0
LIST_ONLY=0
FORCE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --dir) DIR="${2:?--dir needs a directory}"; shift 2 ;;
    --dir=*) DIR="${1#*=}"; shift ;;
    --only) ONLY="${2:?--only needs at least one id}"; shift 2 ;;
    --only=*) ONLY="${1#*=}"; shift ;;
    --include-unconfirmed) INCLUDE_UNCONFIRMED=1; shift ;;
    --list) LIST_ONLY=1; shift ;;
    --force) FORCE=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown option: $1 (try --help)" ;;
  esac
done

command -v git >/dev/null 2>&1 || die "git is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required to read corpus.toml"
[ -f "$MANIFEST" ] || die "manifest not found: $MANIFEST"

# Parse the manifest once into records delimited by the ASCII unit separator.
# Tab would be wrong here: bash collapses runs of whitespace IFS, so a board
# with no `rev` would silently shift every later field one place left.
# tomllib is stdlib from Python 3.11; older interpreters get a clear error
# rather than a wrong answer.
read_manifest() {
  python3 - "$MANIFEST" <<'PY'
import sys
try:
    import tomllib
except ModuleNotFoundError:
    sys.exit("python3 3.11+ is required to read corpus.toml (tomllib)")
SEP = "\x1f"
with open(sys.argv[1], "rb") as f:
    doc = tomllib.load(f)
print("DEFAULT_DIR" + SEP + doc.get("meta", {}).get("default_dir", "board-corpus"))
for b in doc.get("board", []):
    print(SEP.join([
        "BOARD",
        b["id"],
        b.get("kind", "git"),
        b["url"],
        b.get("rev", ""),
        b.get("subdir", ""),
        b.get("license", "unknown"),
        "1" if b.get("license_confirmed", True) else "0",
        b.get("name", b["id"]),
    ]))
PY
}

MANIFEST_LINES="$(read_manifest)"
DEFAULT_DIR="$(printf '%s\n' "$MANIFEST_LINES" | awk -F'\x1f' '$1=="DEFAULT_DIR"{print $2}')"

if [ -z "$DIR" ]; then
  DIR="${HAUKSBEE_CORPUS_DIR:-${HAUKSBEE_ROOT}/${DEFAULT_DIR}}"
fi

wanted() {
  local id="$1"
  [ -z "$ONLY" ] && return 0
  case ",$ONLY," in *",$id,"*) return 0 ;; esac
  return 1
}

# A shallow fetch of one revision: cheaper than a full clone, and the pin is
# what makes the corpus reproducible, so history is of no use to us.
# A hung remote must not eat the whole run. One board that stops responding
# used to stall the fetch indefinitely, leaving a `.partial` directory behind
# and no way to tell a slow clone from a dead one. These bound it: git gives up
# on a transfer that has moved less than a byte a second for a minute, and the
# whole fetch for one board is capped outright.
GIT_STALL_ARGS=(-c http.lowSpeedLimit=1 -c http.lowSpeedTime=60)
FETCH_TIMEOUT_S="${HAUKSBEE_FETCH_TIMEOUT:-600}"

# `timeout` is GNU coreutils and absent on a stock macOS. Fall back to running
# the command unbounded rather than failing, since an unbounded fetch is what
# every previous run did anyway.
run_bounded() {
  if command -v timeout >/dev/null 2>&1; then
    timeout "$FETCH_TIMEOUT_S" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout "$FETCH_TIMEOUT_S" "$@"
  else
    "$@"
  fi
}

fetch_git() {
  local id="$1" url="$2" rev="$3" dest="$4"
  rm -rf "$dest.partial"
  mkdir -p "$dest.partial"
  git -C "$dest.partial" init -q
  git -C "$dest.partial" remote add origin "$url"
  if run_bounded git "${GIT_STALL_ARGS[@]}" -C "$dest.partial" fetch -q --depth 1 origin "$rev" 2>/dev/null; then
    git -C "$dest.partial" checkout -q FETCH_HEAD || return 1
  else
    # Some hosts refuse to serve an arbitrary commit directly. Fetch history
    # instead and check out the pinned revision by name. Checking out
    # FETCH_HEAD here would silently land on the default branch head, which
    # looks like success and quietly discards the pin.
    #
    # The refspec is explicit on purpose. This repo was made by `git init`, so
    # it has no configured remote.fetch, and a bare `fetch --tags origin`
    # brings back tags and NOT ONE BRANCH. The checkout then failed with
    # "pathspec did not match", which reads like a moved upstream rather than
    # our own missing refspec, and it took out 16 of 28 boards.
    run_bounded git "${GIT_STALL_ARGS[@]}" -C "$dest.partial" fetch -q origin \
      '+refs/heads/*:refs/remotes/origin/*' --tags || return 1
    git -C "$dest.partial" checkout -q "$rev" || return 1
  fi

  local got want
  got="$(git -C "$dest.partial" rev-parse HEAD)"
  want="$(git -C "$dest.partial" rev-parse "${rev}^{commit}" 2>/dev/null || echo "")"
  # The manifest may pin an abbreviated sha, a tag, or a branch. Accept a match
  # on the resolved commit, or an abbreviated sha that prefixes what we got.
  if [ -n "$want" ] && [ "$got" != "$want" ]; then
    err "$id: asked for $rev, landed on ${got:0:12}"
    return 1
  fi
  if [ -z "$want" ] && [ "${got#"$rev"}" = "$got" ]; then
    err "$id: asked for $rev, landed on ${got:0:12}"
    return 1
  fi

  # Record what we actually got, so a board in the corpus can always be traced
  # back to its origin without re-reading the manifest.
  printf '%s\n' "$got" > "$dest.partial/.hauksbee-rev"
  rm -rf "$dest.partial/.git"
  mv "$dest.partial" "$dest"
}

fetch_zip() {
  local id="$1" url="$2" dest="$3"
  command -v curl >/dev/null 2>&1 || die "curl is required for $id"
  command -v unzip >/dev/null 2>&1 || die "unzip is required for $id"
  rm -rf "$dest.partial"
  mkdir -p "$dest.partial"
  local zip="$dest.partial/download.zip"
  curl -fsSL --retry 3 -o "$zip" "$url" || return 1
  unzip -q -o "$zip" -d "$dest.partial" || return 1
  rm -f "$zip"
  printf '%s\n' "$url" > "$dest.partial/.hauksbee-rev"
  mv "$dest.partial" "$dest"
}

total=0; fetched=0; skipped=0; failed=0
declare -a FAILED_IDS=()

if [ "$LIST_ONLY" = 1 ]; then
  log "boards in $MANIFEST"
else
  log "fetching the board corpus into $DIR"
  info "boards are fetched from their authors under their own licences"
  mkdir -p "$DIR"
fi

while IFS=$'\x1f' read -r tag id kind url rev subdir license confirmed name; do
  [ "$tag" = "BOARD" ] || continue
  wanted "$id" || continue
  total=$((total + 1))

  if [ "$confirmed" = "0" ] && [ "$INCLUDE_UNCONFIRMED" = 0 ]; then
    if [ "$LIST_ONLY" = 1 ]; then
      info "skip  $id  [$license]  $name  (licence unconfirmed)"
    else
      warn "$id skipped: licence unconfirmed (--include-unconfirmed to override)"
    fi
    skipped=$((skipped + 1))
    continue
  fi

  if [ "$LIST_ONLY" = 1 ]; then
    info "$id  [$license]  $name"
    continue
  fi

  dest="$DIR/$id"
  if [ -d "$dest" ] && [ "$FORCE" = 0 ]; then
    ok "$id already present"
    skipped=$((skipped + 1))
    continue
  fi
  if [ "$FORCE" = 1 ]; then rm -rf "$dest"; fi

  info "$id  <- $url${rev:+ @ $rev}"
  case "$kind" in
    git) fetch_git "$id" "$url" "$rev" "$dest" || { failed=$((failed+1)); FAILED_IDS+=("$id"); rm -rf "$dest.partial"; warn "$id FAILED"; continue; } ;;
    zip) fetch_zip "$id" "$url" "$dest"       || { failed=$((failed+1)); FAILED_IDS+=("$id"); rm -rf "$dest.partial"; warn "$id FAILED"; continue; } ;;
    *) warn "$id: unknown kind '$kind'"; failed=$((failed+1)); FAILED_IDS+=("$id"); continue ;;
  esac

  # Keep only the design files and the paperwork. Upstream repos carry 3D
  # models, production archives and firmware that we never read, and that turn
  # a lean corpus into gigabytes.
  find "$dest" -type f \
    ! -iname '*.kicad_pcb' ! -iname '*.kicad_sch' ! -iname '*.kicad_pro' \
    ! -iname '*.sch' ! -iname '*.brd' ! -iname '*.net' ! -iname '*.PcbDoc' \
    ! -iname '*.d356' ! -iname '*.art' ! -iname '*.g?[lb]' ! -iname '*.drl' \
    ! -iname '*.gbr' ! -iname '*.zip' ! -iname '*.7z' \
    ! -iname 'LICENSE*' ! -iname 'COPYING*' ! -iname 'README*' \
    ! -name '.hauksbee-rev' \
    -delete 2>/dev/null || true
  find "$dest" -type d -empty -delete 2>/dev/null || true

  boards=$(find "$dest" \( -iname '*.kicad_pcb' -o -iname '*.brd' -o -iname '*.net' -o -iname '*.PcbDoc' \) 2>/dev/null | wc -l | tr -d ' ')
  ok "$id  ($boards board file(s))"
  fetched=$((fetched + 1))
done <<< "$MANIFEST_LINES"

if [ "$LIST_ONLY" = 1 ]; then
  info ""
  info "$total board(s) in the manifest"
  exit 0
fi

log "done: $fetched fetched, $skipped skipped, $failed failed (of $total)"
if [ "$failed" -gt 0 ]; then
  err "failed: ${FAILED_IDS[*]}"
  err "an upstream repository may have moved or rewritten the pinned revision."
  err "re-run with --only <id> to retry one, and please open an issue."
  exit 1
fi
info ""
info "point the tests at it:"
info "  export HAUKSBEE_CORPUS_DIR=$DIR"
info "  HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace --features avr"
