#!/usr/bin/env bash
# Build the exact Corresponding Source archive shipped beside GPL binaries.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:-}"
OUT="${2:-$ROOT/dist}"
EXPECTED_SHA="${3:-}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]] || {
  echo "usage: $0 VERSION [OUT_DIR]" >&2
  exit 2
}
[ -z "$(git -C "$ROOT" status --porcelain)" ] || {
  echo "refusing to build Corresponding Source from a dirty tree" >&2
  exit 1
}
SOURCE_SHA="$(git -C "$ROOT" rev-parse HEAD)"
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] || exit 1
if [ -n "$EXPECTED_SHA" ] && [ "$SOURCE_SHA" != "$EXPECTED_SHA" ]; then
  echo "source checkout $SOURCE_SHA does not match expected release $EXPECTED_SHA" >&2
  exit 1
fi
SIMAVR_COMMIT="$(sed -nE 's/^SIMAVR_COMMIT="([0-9a-f]{40})"$/\1/p' "$ROOT/scripts/install-sims.sh" | head -1)"
[[ "$SIMAVR_COMMIT" =~ ^[0-9a-f]{40}$ ]] || {
  echo "install-sims.sh does not pin one simavr commit" >&2
  exit 1
}

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/hauksbee-source.XXXXXX")"
cleanup() {
  find "$WORK" -depth -mindepth 1 -delete 2>/dev/null || true
  rmdir "$WORK" 2>/dev/null || true
}
trap cleanup EXIT INT TERM
STAGE="$WORK/hauksbee-${VERSION}-source"
mkdir -p "$STAGE"
git -C "$ROOT" archive "$SOURCE_SHA" | tar -xf - -C "$STAGE"

# GPL Corresponding Source includes the exact locked registry crates compiled
# into the statically linked Rust binaries, not merely Cargo.lock pointers.
mkdir -p "$STAGE/third-party/cargo-vendor" "$STAGE/.cargo"
(cd "$STAGE" && cargo vendor --locked --versioned-dirs \
  third-party/cargo-vendor > .cargo/config.toml)
expected_registry_packages="$(python3 - "$ROOT/Cargo.lock" <<'PY'
import pathlib, sys, tomllib
lock = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())
print(sum(1 for p in lock["package"] if str(p.get("source", "")).startswith("registry+")))
PY
)"
actual_registry_packages="$(find "$STAGE/third-party/cargo-vendor" -mindepth 2 -maxdepth 2 -name .cargo-checksum.json | wc -l | tr -d ' ')"
[ "$actual_registry_packages" = "$expected_registry_packages" ] || {
  echo "cargo vendor retained $actual_registry_packages registry packages; Cargo.lock requires $expected_registry_packages" >&2
  exit 1
}

# Retain the exact libsimavr source too. Fetching the immutable object avoids a
# mutable tag while still carrying the complete upstream tree in the archive.
simavr="$STAGE/third-party/simavr"
git init -q "$simavr"
git -C "$simavr" remote add origin https://github.com/buserror/simavr.git
git -C "$simavr" fetch -q --depth 1 origin "$SIMAVR_COMMIT"
git -C "$simavr" checkout -q --detach FETCH_HEAD
[ "$(git -C "$simavr" rev-parse HEAD)" = "$SIMAVR_COMMIT" ]
find "$simavr/.git" -depth -mindepth 1 -delete
rmdir "$simavr/.git"

cat > "$STAGE/CORRESPONDING-SOURCE.txt" <<EOF
Hauksbee ${VERSION} complete Corresponding Source
Hauksbee commit: ${SOURCE_SHA}
simavr commit: ${SIMAVR_COMMIT}
Registry sources: ${actual_registry_packages} locked Cargo packages under third-party/cargo-vendor

Build the default binaries with scripts/install-sims.sh --avr followed by
scripts/bundle.sh --shape default. The archive is intended to accompany the
same-version GPL binary archives and macOS application on the private release.
EOF

archive="$OUT/hauksbee-${VERSION}-source.tar.gz"
tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
  -czf "$archive" -C "$WORK" "$(basename "$STAGE")"
(cd "$OUT" && sha256sum "$(basename "$archive")" > "$(basename "$archive").sha256")
echo "$archive"
