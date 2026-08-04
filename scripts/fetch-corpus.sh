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
        # Where the board lands under the corpus root. Defaults to the id; a
        # revision pair overrides it so both halves group under one directory.
        b.get("dest", b["id"]),
        b.get("sha256", ""),
        b.get("hoist", ""),
        b.get("unpack", ""),
        # Newline-free and separator-free by construction: a list of relative
        # paths, joined with the record separator's sibling so one field can
        # carry several.
        "\x1e".join(b.get("drop", [])),
        b.get("name", b["id"]),
    ]))
PY
}

MANIFEST_LINES="$(read_manifest)"

# Read DEFAULT_DIR with the same `IFS=$'\x1f' read` the board loop below uses,
# and for the same reason: portability. This was `awk -F'\x1f'`, and the one-true
# awk that ships with macOS takes that pattern literally rather than as the
# separator byte, so the field split never happened, DEFAULT_DIR came back
# EMPTY, and DIR collapsed to "${HAUKSBEE_ROOT}/" - the repository root. Every
# board landed loose in the checkout, `board-corpus/` was never created, and so
# every corpus-gated test skipped while the fetch reported success. Bash's own
# $'\x1f' is unambiguous on every platform, and it drops the awk dependency.
DEFAULT_DIR=""
while IFS=$'\x1f' read -r tag value _rest; do
  [ "$tag" = "DEFAULT_DIR" ] || continue
  DEFAULT_DIR="$value"
done <<< "$MANIFEST_LINES"
[ -n "$DEFAULT_DIR" ] || die "manifest has no meta.default_dir and none was parsed"

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
  local direct=0
  if run_bounded git "${GIT_STALL_ARGS[@]}" -C "$dest.partial" fetch -q --depth 1 origin "$rev" 2>/dev/null; then
    git -C "$dest.partial" checkout -q FETCH_HEAD || return 1
    direct=1
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
  # --verify --quiet matters: a bare `rev-parse <name>` ECHOES an unresolvable
  # name back to stdout (alongside the non-zero exit), so `want` came out as
  # the literal string "9.0.0^{commit}" instead of empty and the comparison
  # below failed every tag pin the direct fetch path served.
  want="$(git -C "$dest.partial" rev-parse --verify --quiet "${rev}^{commit}" || echo "")"
  # The manifest may pin an abbreviated sha, a tag, or a branch. Accept a match
  # on the resolved commit, or an abbreviated sha that prefixes what we got.
  #
  # A tag pin taken through the DIRECT path is a special case: `git fetch
  # origin <tag>` leaves no local tag ref behind, so `rev-parse <tag>^{commit}`
  # resolves nothing here even though the transport served exactly the ref we
  # named (an annotated tag arrives peeled to its commit in FETCH_HEAD). That
  # used to fail the pin check with "asked for 9.0.0, landed on <commit>" where
  # <commit> WAS 9.0.0's commit. The wrong-commit hazard this check exists for
  # (a history fetch landing on the default branch head) is confined to the
  # fallback path, where the strict comparison still applies.
  if [ -n "$want" ] && [ "$got" != "$want" ]; then
    err "$id: asked for $rev, landed on ${got:0:12}"
    return 1
  fi
  if [ -z "$want" ] && [ "$direct" -ne 1 ] && [ "${got#"$rev"}" = "$got" ]; then
    err "$id: asked for $rev, landed on ${got:0:12}"
    return 1
  fi

  # Record what we actually got, so a board in the corpus can always be traced
  # back to its origin without re-reading the manifest.
  printf '%s\n' "$got" > "$dest.partial/.hauksbee-rev"
  rm -rf "$dest.partial/.git"
  mkdir -p "$(dirname "$dest")"
  mv "$dest.partial" "$dest"
}

# sha256 of a file, on either a GNU or a macOS box. `sha256sum` is coreutils and
# absent from a stock macOS; `shasum -a 256` ships with it. Prints the bare hex.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    return 1
  fi
}

# A zip has no commit to pin, so its sha256 is the pin. This used to be a bare
# curl-and-unzip with nothing checked, which meant two of the corpus boards were
# whatever the URL served today while every git-hosted board was pinned to a
# revision and verified against it. An archive re-published upstream could change
# our measured results with no trace. Refuse an unpinned zip outright rather than
# fetch it and hope: a corpus entry that cannot be reproduced is not a pin.
fetch_zip() {
  local id="$1" url="$2" want_sha="$3" dest="$4"
  command -v curl >/dev/null 2>&1 || die "curl is required for $id"
  command -v unzip >/dev/null 2>&1 || die "unzip is required for $id"
  if [ -z "$want_sha" ]; then
    err "$id: zip-hosted boards need a sha256 in corpus.toml; refusing an unverified download"
    return 1
  fi
  rm -rf "$dest.partial"
  mkdir -p "$dest.partial"
  local zip="$dest.partial/download.zip"
  curl -fsSL --retry 3 -o "$zip" "$url" || return 1
  local got_sha
  got_sha="$(sha256_of "$zip")" || { err "$id: no sha256sum or shasum available to verify the download"; return 1; }
  if [ "$got_sha" != "$want_sha" ]; then
    err "$id: sha256 mismatch"
    err "  expected $want_sha"
    err "  got      $got_sha"
    err "  the upstream archive changed, or the download was corrupted. Do not"
    err "  update the hash without establishing which."
    return 1
  fi
  unzip -q -o "$zip" -d "$dest.partial" || return 1
  rm -f "$zip"
  # Record the pin, not just the URL: a URL alone cannot be checked later.
  printf '%s\nsha256:%s\n' "$url" "$want_sha" > "$dest.partial/.hauksbee-rev"
  mkdir -p "$(dirname "$dest")"
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

while IFS=$'\x1f' read -r tag id kind url rev subdir license confirmed dest_rel sha256 hoist unpack drop name; do
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

  dest="$DIR/${dest_rel:-$id}"
  if [ -d "$dest" ] && [ "$FORCE" = 0 ]; then
    ok "$id already present"
    skipped=$((skipped + 1))
    continue
  fi
  if [ "$FORCE" = 1 ]; then rm -rf "$dest"; fi
  mkdir -p "$(dirname "$dest")"

  # Name the destination when the manifest overrode it, and say when the pin is a
  # hash rather than a revision. Both are visible in the log rather than
  # something you have to go and look up in corpus.toml.
  where=""
  [ "$dest_rel" = "$id" ] || where=" -> $dest_rel"
  info "$id$where  <- $url${rev:+ @ $rev}${sha256:+ @ sha256:${sha256:0:12}}"
  case "$kind" in
    git) fetch_git "$id" "$url" "$rev" "$dest"          || { failed=$((failed+1)); FAILED_IDS+=("$id"); rm -rf "$dest.partial"; warn "$id FAILED"; continue; } ;;
    zip) fetch_zip "$id" "$url" "$sha256" "$dest"       || { failed=$((failed+1)); FAILED_IDS+=("$id"); rm -rf "$dest.partial"; warn "$id FAILED"; continue; } ;;
    *) warn "$id: unknown kind '$kind'"; failed=$((failed+1)); FAILED_IDS+=("$id"); continue ;;
  esac

  # `subdir`: only this part of a large upstream is wanted. KiCad's own
  # repository is the case that forced this. The manifest has said
  # `subdir = "demos"` since the entry was added and nothing acted on it, so the
  # fetch pulled in KiCad's `qa/` tree too, and the zero-shorts corpus gate then
  # ran over boards whose entire purpose is to reproduce a KiCad bug
  # (issue14559, issue5750, vme-wren). Those are not known-good hardware; they
  # are regression fixtures, and grading a false-positive gate on them is a
  # category error. Everything outside the subtree goes. The subtree LEVEL stays
  # (`kicad_demos/demos/<project>`), because that is the path the corpus tests
  # resolve.
  #
  # `subdir` may be nested (`hardware/hackrf-one`, where the sibling boards under
  # `hardware/` carry different terms). A per-level `find ! -name "$subdir"`
  # cannot express that: `hardware` does not equal `hardware/hackrf-one`, so the
  # wanted subtree's own parent was the first thing deleted. The subtree is moved
  # aside, the tree is emptied, and the subtree is put back at its declared path.
  if [ -n "$subdir" ]; then
    if [ -d "$dest/$subdir" ]; then
      keep="$dest.keep"
      rm -rf "$keep"
      mkdir -p "$(dirname "$keep/$subdir")"
      mv "$dest/$subdir" "$keep/$subdir"
      # The paperwork travels with it: a share-alike board without its licence
      # file is not a board anyone may pass on.
      find "$dest" -maxdepth 1 -type f \
        \( -iname 'LICENSE*' -o -iname 'COPYING*' -o -iname 'LICENCE*' \
           -o -iname 'README*' -o -name '.hauksbee-rev' \) \
        -exec mv -f {} "$keep/" \; 2>/dev/null || true
      find "$dest" -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null || true
      find "$keep" -mindepth 1 -maxdepth 1 -exec mv -f {} "$dest/" \; 2>/dev/null || true
      rm -rf "$keep"
    else
      err "$id: subdir '$subdir' is declared and $dest/$subdir does not exist"
      failed=$((failed+1)); FAILED_IDS+=("$id"); continue
    fi
  fi

  # `drop`: paths inside the fetched tree that are not boards. An upstream that
  # ships the manufacturing PANEL of a design beside the design, or a
  # deliberately-broken regression fixture beside working hardware, would
  # otherwise have both counted as coverage. The SparkFun `Production/` panels
  # and KiCad's `demos/vme-wren` are the cases; both would be graded as
  # known-good hardware by a false-positive gate, and neither is.
  #
  # A declared path that is not there is an error, not a no-op: it means the
  # upstream moved and the entry's description of it has stopped being true.
  if [ -n "$drop" ]; then
    drop_missing=""
    while IFS= read -r d; do
      [ -n "$d" ] || continue
      if [ -e "$dest/$d" ]; then
        rm -rf "$dest/$d"
      else
        drop_missing="$drop_missing $d"
      fi
    done <<< "$(printf '%s' "$drop" | tr '\x1e' '\n')"
    if [ -n "$drop_missing" ]; then
      err "$id: drop declares$drop_missing and none of those exist in the fetched tree"
      failed=$((failed+1)); FAILED_IDS+=("$id"); continue
    fi
  fi

  # `hoist`: the upstream keeps the design one level down, and the corpus does
  # not. The ZSWatch DevKit revisions ship as `<repo>/devkit/*` (and `dev-kit/*`
  # at 1.1.0) while every test asks for `zswatch_devkit/v1.2.0/<file>`, so a
  # fetched corpus had the boards on disk and no test could find one. Lift the
  # subtree's contents into the board directory.
  if [ -n "$hoist" ] && [ -d "$dest/$hoist" ]; then
    # A dotglob-safe move: `mv "$dest/$hoist"/* ` misses dotfiles and breaks on
    # a name with a space, both of which occur in this corpus.
    find "$dest/$hoist" -mindepth 1 -maxdepth 1 -exec mv -f {} "$dest/" \; 2>/dev/null || true
    rmdir "$dest/$hoist" 2>/dev/null || true
  fi

  # `unpack`: the board is published as an archive inside a repository, and the
  # archive IS the board. The Inkplate 6 is the case: the repo carries the films
  # only as `Schematics, Gerber, BOM/v1.0/... .zip`, so a fetched corpus had a zip
  # where the reverse-extraction test wanted a directory of films.
  #
  # The rest of the repository then goes, and that is the point rather than
  # tidiness. Inkplate's repo also ships a 3D-printed-case project with its own
  # F_Cu/B_Cu films; left in place they sat in the same directory as the main
  # board's, and the gerber reader read FOUR copper layers on a two-layer board.
  # A reverse-extraction directory holds one board's films or it holds nonsense.
  if [ -n "$unpack" ] && [ -f "$dest/$unpack" ]; then
    if command -v unzip >/dev/null 2>&1; then
      unpack_tmp="$dest/.hauksbee-unpack"
      rm -rf "$unpack_tmp"
      mkdir -p "$unpack_tmp"
      if unzip -q -o -j "$dest/$unpack" -d "$unpack_tmp" 2>/dev/null; then
        find "$dest" -mindepth 1 -maxdepth 1 \
          ! -name '.hauksbee-unpack' ! -name '.hauksbee-rev' \
          ! -iname 'LICENSE*' ! -iname 'COPYING*' ! -iname 'README*' \
          -exec rm -rf {} + 2>/dev/null || true
        find "$unpack_tmp" -mindepth 1 -maxdepth 1 -exec mv -f {} "$dest/" \; 2>/dev/null || true
      else
        warn "$id: could not unpack $unpack"
      fi
      rm -rf "$unpack_tmp"
    else
      warn "$id: unzip is absent, leaving $unpack packed"
    fi
  fi

  # AppleDouble junk. Archives zipped on a Mac carry a `__MACOSX/` tree and a
  # `._<name>` resource-fork stub beside every file, and `unzip` writes both. The
  # Inkplate gerber zip is one: `._EPD_board.GTL` sorted alongside the real film,
  # the gerber reader picked it up as a layer, and reverse-extraction died on
  # "stream did not contain valid UTF-8". These are not data on any platform.
  find "$dest" -name '__MACOSX' -type d -exec rm -rf {} + 2>/dev/null || true
  find "$dest" -name '._*' -type f -delete 2>/dev/null || true

  # KiCad's own auto-backup directories. The LumenPnP motherboard ships six
  # historical copies of mobo.kicad_sch under `mobo-backups/`, and a corpus sweep
  # that walks every design file would grade the tool on five stale revisions of
  # one board and report the count as coverage. They are not boards; they are
  # undo history.
  #
  # `BackupFiles` and `Backup` are the same thing under Olimex's and Duet3D's
  # naming. Olimex's ESP32-PoE revision E ships a full second copy of the board
  # under `BackupFiles/`, which would have been counted twice.
  find "$dest" -type d \( -name '*-backups' -o -iname 'BackupFiles' -o -iname 'Backup' \) \
    -exec rm -rf {} + 2>/dev/null || true

  # Keep only the design files and the paperwork. Upstream repos carry 3D
  # models, production archives and firmware that we never read, and that turn
  # a lean corpus into gigabytes.
  #
  # `*.TXT` is case-sensitive on purpose: it is Altium's drill-file extension
  # (the Inkplate's EPD_board-RoundHoles.TXT and its slot files), and the gerber
  # reader cannot stitch layers without it. Matching case-insensitively would
  # drag in every readme.txt and changelog.txt in every upstream.
  # `*.SchDoc` and `*.PrjPcb` are Altium's schematic and project files. Without
  # them the ODrive entries landed a .PcbDoc with no schematic beside it, which is
  # half a board: the Altium schematic reader would have had nothing to read.
  find "$dest" -type f \
    ! -iname '*.kicad_pcb' ! -iname '*.kicad_sch' ! -iname '*.kicad_pro' \
    ! -iname '*.sch' ! -iname '*.brd' ! -iname '*.net' ! -iname '*.PcbDoc' \
    ! -iname '*.SchDoc' ! -iname '*.PrjPcb' \
    ! -iname '*.d356' ! -iname '*.art' ! -iname '*.g?[lb]' ! -iname '*.drl' \
    ! -iname '*.gbr' ! -iname '*.zip' ! -iname '*.7z' ! -name '*.TXT' \
    ! -iname 'LICENSE*' ! -iname 'LICENCE*' ! -iname 'COPYING*' ! -iname 'README*' \
    ! -name '.hauksbee-rev' \
    -delete 2>/dev/null || true
  find "$dest" -type d -empty -delete 2>/dev/null || true

  # The board's own paperwork has to have survived all of the above. A share-alike
  # board whose licence file was pruned is one a reader cannot pass on, and the
  # subdir and unpack transforms both delete whole trees.
  if ! find "$dest" -type f \( -iname 'LICENSE*' -o -iname 'LICENCE*' -o -iname 'COPYING*' -o -iname 'README*' \) \
       | grep -q .; then
    warn "$id: no licence or readme file survived the fetch; the terms are recorded in corpus.toml only"
  fi

  boards=$(find "$dest" \( -iname '*.kicad_pcb' -o -iname '*.brd' -o -iname '*.net' -o -iname '*.PcbDoc' \) 2>/dev/null | wc -l | tr -d ' ')
  ok "$id  ($boards board file(s))  [$license]"
  fetched=$((fetched + 1))
done <<< "$MANIFEST_LINES"

if [ "$LIST_ONLY" = 1 ]; then
  info ""
  info "$total board(s) in the manifest"
  exit 0
fi

# What actually landed, counted rather than asserted. A human reading this can
# tell a fetch that worked from one that reported success and wrote nothing,
# which is the failure the awk bug produced for as long as it went unnoticed.
count_ext() { find "$DIR" -type f -iname "$1" 2>/dev/null | wc -l | tr -d ' '; }
pcbs=$(( $(count_ext '*.kicad_pcb') + $(count_ext '*.brd') + $(count_ext '*.PcbDoc') ))
schs=$(( $(count_ext '*.kicad_sch') + $(count_ext '*.sch') + $(count_ext '*.SchDoc') ))
nets=$(count_ext '*.net')
films=$(( $(count_ext '*.gbr') + $(count_ext '*.g?[lb]') + $(count_ext '*.art') ))
revs=$(find "$DIR" -name .hauksbee-rev 2>/dev/null | wc -l | tr -d ' ')
size=$(du -sh "$DIR" 2>/dev/null | cut -f1)

log "done: $fetched fetched, $skipped skipped, $failed failed (of $total)"
info "landed: $revs board director(ies), $pcbs layout, $schs schematic, $nets netlist, $films film file(s), $size"
if [ "$skipped" -gt 0 ] && [ "$INCLUDE_UNCONFIRMED" = 0 ]; then
  info "skipped boards are either already present or licence-unconfirmed; see the lines above"
fi
if [ "$failed" -gt 0 ]; then
  err "failed: ${FAILED_IDS[*]}"
  err "an upstream repository may have moved or rewritten the pinned revision,"
  err "or an entry's declaration no longer matches the tree it fetches."
  err "re-run with --only <id> to retry one, and please open an issue."
  exit 1
fi

# The manifest has to still describe what landed. This is the check that would
# have caught `subdir = "demos"` being honoured by nothing, and it runs as part of
# the fetch rather than as a separate step someone has to remember, because the
# whole failure was that nobody was looking.
#
# `--only` fetches a subset, so the landed half of the check would report every
# board that was deliberately not fetched. The manifest half still runs.
CHECK="${HAUKSBEE_ROOT}/scripts/check-corpus.py"
if [ -f "$CHECK" ]; then
  info ""
  check_args=(--manifest "$MANIFEST")
  if [ -n "$ONLY" ]; then
    check_args+=(--manifest-only)
  else
    check_args+=(--dir "$DIR")
    [ "$INCLUDE_UNCONFIRMED" = 1 ] && check_args+=(--include-unconfirmed)
  fi
  if ! python3 "$CHECK" "${check_args[@]}"; then
    err "the manifest no longer describes the corpus it fetched. Do not gate on this tree."
    exit 1
  fi
else
  warn "scripts/check-corpus.py is missing; the fetched tree is unchecked against the manifest"
fi

info ""
info "point the tests at it:"
info "  export HAUKSBEE_CORPUS_DIR=$DIR"
info "  HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace --features avr"
