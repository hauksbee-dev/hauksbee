#!/usr/bin/env bash
# build-app.sh - assemble Hauksbee.app, the double-clickable macOS front door.
#
# Produces <out>/hauksbee-<version>-<target>-app.zip (+ .sha256) containing
# Hauksbee.app:
#   Contents/Info.plist            name / identifier / version (from Cargo.toml)
#   Contents/MacOS/Hauksbee        compiled Swift launcher (a real Mach-O, so
#                                  Finder opens NO Terminal window; a shell
#                                  script here would open one and defeat the app)
#   Contents/Resources/bin/hauksbee     the engine binary; launcher runs `serve`
#   Contents/Resources/bin/hauksbee-ci  the CI runner (the web checks panel
#                                  execs it as a sibling of hauksbee). The
#                                  binaries live under Resources/bin, NOT
#                                  MacOS/: APFS is case-insensitive by default,
#                                  so `hauksbee` next to the `Hauksbee`
#                                  launcher would be the same path and clobber
#                                  it.
#   Contents/Resources/hauksbee.icns  best-effort icon from frontend/public/
#                                  favicon.svg; skipped silently if conversion
#                                  is unavailable
#
# Launch flow: double-click -> launcher spawns `hauksbee serve` with a non-TTY
# stdout -> serve binds its port (with busy-port fallback) and auto-opens the
# system browser at the real URL. Quit (Cmd-Q / Dock > Quit) SIGTERMs the
# server; nothing is orphaned.
#
# Signing is optional and driven by env vars (unsigned builds keep working):
#   HAUKSBEE_SIGN_IDENTITY      a "Developer ID Application: ..." identity in
#                               the keychain. When set, every Mach-O is signed
#                               inside-out with hardened runtime + timestamp,
#                               then verified (codesign --verify --deep
#                               --strict, plus an spctl assessment whose
#                               verdict is printed honestly; spctl rejects
#                               signed-but-unnotarised apps by design).
#   HAUKSBEE_NOTARY_PROFILE     a notarytool keychain profile name
#                               (`xcrun notarytool store-credentials`), OR
#   HAUKSBEE_NOTARY_APPLE_ID + HAUKSBEE_NOTARY_TEAM_ID +
#   HAUKSBEE_NOTARY_PASSWORD    (app-specific password) for direct credentials.
#                               Either form triggers notarytool submit --wait
#                               and stapling; requires HAUKSBEE_SIGN_IDENTITY.
# Without HAUKSBEE_SIGN_IDENTITY the app ships UNSIGNED: Gatekeeper warns on
# first open; see SIGNING.md in this directory for the notarisation path and
# the right-click > Open workaround users have until then.
#
# Usage:
#   app/macos/build-app.sh [--version V] [--target TRIPLE] [--out DIR]
#                          [--no-build] [--no-default-features]
#                          [--features LIST] [--help]
#
# Options (the same conventions as scripts/bundle.sh):
#   --version V     Override the version (default: workspace Cargo.toml version).
#   --target TRIPLE Override the target label in the zip name (default:
#                   darwin-<arch>).
#   --out DIR       Output directory (default: dist).
#   --no-build      Use existing target/release binaries; do not build. This is
#                   what the release workflow passes, right after bundle.sh has
#                   already built them.
#   --no-default-features
#                   Forwarded to scripts/bundle.sh for the GPL-free shape.
#   --features LIST Forwarded to scripts/bundle.sh (e.g. "renode,qemu").
#   --help          Show this help.
#
# When building (no --no-build) this delegates the binary + embedded-web build
# to scripts/bundle.sh so the feature / GPL / embed-web logic lives in exactly
# one place; the tarball bundle.sh also produces lands in a temp dir and is
# discarded.
set -euo pipefail
# shellcheck source=scripts/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts" && pwd)/common.sh"

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; }

VERSION=""
TARGET=""
OUT="dist"
DO_BUILD=1
BUNDLE_FLAGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --target) TARGET="${2:?--target needs a value}"; shift 2 ;;
    --target=*) TARGET="${1#*=}"; shift ;;
    --out) OUT="${2:?--out needs a directory}"; shift 2 ;;
    --out=*) OUT="${1#*=}"; shift ;;
    --no-build) DO_BUILD=0; shift ;;
    --no-default-features) BUNDLE_FLAGS+=(--no-default-features); shift ;;
    --features) BUNDLE_FLAGS+=(--features "${2:?--features needs a value}"); shift 2 ;;
    --features=*) BUNDLE_FLAGS+=("$1"); shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument '$1' (try --help)" ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || die "Hauksbee.app can only be assembled on macOS."
have swiftc || die "swiftc not found. Install the Xcode Command Line Tools (xcode-select --install)."

if [ -z "$VERSION" ]; then
  VERSION="$(grep -m1 '^version' "$HAUKSBEE_ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
  [ -n "$VERSION" ] || die "could not read version from Cargo.toml; pass --version"
fi
if [ -z "$TARGET" ]; then
  TARGET="darwin-$(uname -m)"
fi

SRC="$(hauksbee_target_bin)"
if [ "$DO_BUILD" -eq 1 ]; then
  # One source of truth for the build: bundle.sh handles the frontend build,
  # embed-web enabling and the feature flags. Its tarball goes to a temp dir
  # we throw away; we only want the target/release binaries it leaves behind.
  BUNDLE_OUT="$(mktemp -d "${TMPDIR:-/tmp}/hauksbee-app-build.XXXXXX")"
  trap 'rm -rf "$BUNDLE_OUT"' EXIT
  log "Building binaries via scripts/bundle.sh"
  "$HAUKSBEE_ROOT/scripts/bundle.sh" --version "$VERSION" --out "$BUNDLE_OUT" \
    ${BUNDLE_FLAGS[@]+"${BUNDLE_FLAGS[@]}"}
fi
for bin in hauksbee hauksbee-ci hauksbee-mcp; do
  [ -x "$SRC/$bin" ] || die "$SRC/$bin missing (build first, or drop --no-build)."
done

NAME="hauksbee-${VERSION}-${TARGET}-app"
OUT_ABS="$(mkdir -p "$OUT" && cd "$OUT" && pwd)"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/hauksbee-app.XXXXXX")"
trap 'rm -rf "$STAGE" ${BUNDLE_OUT:+"$BUNDLE_OUT"}' EXIT
APP="$STAGE/Hauksbee.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/bin"

log "Compiling the launcher (swiftc)"
swiftc -O -o "$APP/Contents/MacOS/Hauksbee" \
  "$HAUKSBEE_ROOT/app/macos/launcher/main.swift"

log "Staging binaries"
install -m 0755 "$SRC/hauksbee"     "$APP/Contents/Resources/bin/hauksbee"
install -m 0755 "$SRC/hauksbee-ci"  "$APP/Contents/Resources/bin/hauksbee-ci"
install -m 0755 "$SRC/hauksbee-mcp" "$APP/Contents/Resources/bin/hauksbee-mcp"

# Licence terms travel inside the app, same as inside the tarball: the app
# wraps the DEFAULT-shape binaries, which statically link GPL-3.0 libsimavr,
# so the binaries are GPL-3.0 while hauksbee's source stays Apache-2.0.
log "Staging licence files"
cp "$HAUKSBEE_ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
cp "$HAUKSBEE_ROOT/NOTICE"  "$APP/Contents/Resources/NOTICE"
[ -s "$HAUKSBEE_ROOT/licenses/gpl-3.0.txt" ] \
  || die "licenses/gpl-3.0.txt missing; the app carries GPL-3.0 binaries and must enclose the licence text."
cp "$HAUKSBEE_ROOT/licenses/gpl-3.0.txt" "$APP/Contents/Resources/LICENSE-GPL-3.0.txt"
GIT_SHA="$(cd "$HAUKSBEE_ROOT" && git rev-parse HEAD 2>/dev/null || echo unknown)"
cat > "$APP/Contents/Resources/LICENSE-BINARY.txt" <<EOF
Hauksbee.app ${VERSION} (${TARGET}) - licence of the enclosed binaries
======================================================================

THE BINARIES IN Contents/Resources/bin/ ARE LICENSED TO YOU UNDER GPL-3.0.

hauksbee's own source code is Apache-2.0 (see LICENSE and NOTICE, next to
this file). This app wraps the default-shape build, whose optional avr
co-simulation backend STATICALLY LINKS libsimavr (GPL-3.0); statically
linking GPL-3.0 code makes the executable a combined work, so these binaries
are distributed under GPL-3.0. Running the app imposes no obligations at
all; GPL-3.0 constrains redistribution, not use.

Full GPL-3.0 text: LICENSE-GPL-3.0.txt, next to this file.
Corresponding source (GPL-3.0 section 6):
  https://github.com/hauksbee-dev/hauksbee   commit ${GIT_SHA}
  https://github.com/buserror/simavr         (tag: see scripts/install-sims.sh)
If you cannot take GPL code, use the -permissive tarball from the same
release: the same tool without the avr backend, licensed Apache-2.0.
EOF

log "Writing Info.plist"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>            <string>Hauksbee</string>
  <key>CFBundleDisplayName</key>     <string>Hauksbee</string>
  <key>CFBundleIdentifier</key>      <string>dev.hauksbee.app</string>
  <key>CFBundleExecutable</key>      <string>Hauksbee</string>
  <key>CFBundlePackageType</key>     <string>APPL</string>
  <key>CFBundleShortVersionString</key> <string>${VERSION}</string>
  <key>CFBundleVersion</key>         <string>${VERSION}</string>
  <key>CFBundleIconFile</key>        <string>hauksbee</string>
  <key>LSMinimumSystemVersion</key>  <string>11.0</string>
  <key>LSUIElement</key>             <false/>
  <key>NSHighResolutionCapable</key> <true/>
  <key>NSHumanReadableCopyright</key> <string>Apache-2.0</string>
</dict>
</plist>
EOF

# Icon, best-effort: rasterise frontend/public/favicon.svg with qlmanage (the
# Quick Look thumbnailer, present on every Mac), then iconutil it into an
# .icns. Any failure just means the app ships with the generic icon.
ICON_SVG="$HAUKSBEE_ROOT/frontend/public/favicon.svg"
if [ -f "$ICON_SVG" ] && have qlmanage && have sips && have iconutil; then
  log "Building the app icon from frontend/public/favicon.svg"
  ICONTMP="$STAGE/icon"
  mkdir -p "$ICONTMP/hauksbee.iconset"
  if qlmanage -t -s 1024 -o "$ICONTMP" "$ICON_SVG" >/dev/null 2>&1 \
     && [ -f "$ICONTMP/$(basename "$ICON_SVG").png" ]; then
    BASE_PNG="$ICONTMP/$(basename "$ICON_SVG").png"
    for size in 16 32 128 256 512; do
      sips -z "$size" "$size" "$BASE_PNG" \
        --out "$ICONTMP/hauksbee.iconset/icon_${size}x${size}.png" >/dev/null 2>&1
      sips -z "$((size * 2))" "$((size * 2))" "$BASE_PNG" \
        --out "$ICONTMP/hauksbee.iconset/icon_${size}x${size}@2x.png" >/dev/null 2>&1
    done
    if iconutil -c icns "$ICONTMP/hauksbee.iconset" \
         -o "$APP/Contents/Resources/hauksbee.icns" 2>/dev/null; then
      ok "Icon: hauksbee.icns"
    else
      warn "iconutil failed; shipping without an icon."
    fi
  else
    warn "qlmanage could not rasterise the SVG; shipping without an icon."
  fi
else
  warn "No icon toolchain / favicon.svg; shipping without an icon."
fi

# Optional codesigning (see header). Inside-out: the nested Mach-Os first,
# then the bundle, all with hardened runtime and a secure timestamp. --force
# replaces any ad-hoc signature swiftc may have left on the launcher.
SIGNED=0
if [ -n "${HAUKSBEE_SIGN_IDENTITY:-}" ]; then
  log "Codesigning with: $HAUKSBEE_SIGN_IDENTITY"
  codesign --force --options runtime --timestamp \
    --sign "$HAUKSBEE_SIGN_IDENTITY" "$APP/Contents/Resources/bin/hauksbee"
  codesign --force --options runtime --timestamp \
    --sign "$HAUKSBEE_SIGN_IDENTITY" "$APP/Contents/Resources/bin/hauksbee-ci"
  codesign --force --options runtime --timestamp \
    --sign "$HAUKSBEE_SIGN_IDENTITY" "$APP/Contents/Resources/bin/hauksbee-mcp"
  codesign --force --options runtime --timestamp \
    --sign "$HAUKSBEE_SIGN_IDENTITY" "$APP/Contents/MacOS/Hauksbee"
  codesign --force --options runtime --timestamp \
    --sign "$HAUKSBEE_SIGN_IDENTITY" "$APP"
  log "Verifying signature"
  codesign --verify --deep --strict --verbose=2 "$APP"
  ok "codesign --verify --deep --strict: passed"
  # spctl verdict, reported as-is: a signed-but-unnotarised app is REJECTED by
  # Gatekeeper policy. Do not treat that as a build failure, and do not claim
  # acceptance we did not observe.
  if SPCTL_OUT="$(spctl --assess --type execute -vv "$APP" 2>&1)"; then
    ok "spctl assessment: accepted"
    info "$SPCTL_OUT"
  else
    warn "spctl assessment (expected to reject until notarised):"
    warn "$SPCTL_OUT"
  fi
  SIGNED=1
fi

# Optional notarisation + stapling. Needs a signed app and notary credentials
# (keychain profile, or apple-id + team-id + app-specific password).
NOTARY_ARGS=()
if [ -n "${HAUKSBEE_NOTARY_PROFILE:-}" ]; then
  NOTARY_ARGS=(--keychain-profile "$HAUKSBEE_NOTARY_PROFILE")
elif [ -n "${HAUKSBEE_NOTARY_APPLE_ID:-}" ] && [ -n "${HAUKSBEE_NOTARY_TEAM_ID:-}" ] && [ -n "${HAUKSBEE_NOTARY_PASSWORD:-}" ]; then
  NOTARY_ARGS=(--apple-id "$HAUKSBEE_NOTARY_APPLE_ID" \
               --team-id "$HAUKSBEE_NOTARY_TEAM_ID" \
               --password "$HAUKSBEE_NOTARY_PASSWORD")
fi
if [ "${#NOTARY_ARGS[@]}" -gt 0 ]; then
  [ "$SIGNED" -eq 1 ] || die "notarisation requires HAUKSBEE_SIGN_IDENTITY (Apple rejects unsigned submissions)."
  log "Notarising (notarytool submit --wait)"
  NOTARY_ZIP="$STAGE/notary-submit.zip"
  ditto -c -k --keepParent "$APP" "$NOTARY_ZIP"
  xcrun notarytool submit "$NOTARY_ZIP" "${NOTARY_ARGS[@]}" --wait
  log "Stapling the notarisation ticket"
  xcrun stapler staple "$APP"
  ok "Notarised and stapled."
elif [ "$SIGNED" -eq 1 ]; then
  info "No notary credentials set; shipping signed but NOT notarised."
  info "Gatekeeper will still warn on first open (see SIGNING.md)."
fi

log "Zipping $NAME"
ZIP="$OUT_ABS/$NAME.zip"
# The zip and its .sha256 are one artifact: remove BOTH up front so a failure
# between the two writes can never leave a fresh zip next to a stale checksum,
# write the checksum in the same step that produced the zip, and verify the
# pair before claiming success.
rm -f "$ZIP" "$ZIP.sha256"
# ditto preserves the bundle structure and extended attributes the way
# Archive Utility expects; a plain `zip -r` can produce a bundle Finder
# quarantines more aggressively.
( cd "$STAGE" && ditto -c -k --keepParent "Hauksbee.app" "$ZIP" )
( cd "$OUT_ABS" && shasum -a 256 "$NAME.zip" > "$NAME.zip.sha256" )
( cd "$OUT_ABS" && shasum -a 256 -c "$NAME.zip.sha256" >/dev/null ) \
  || die "checksum self-verification failed for $ZIP"

ok "App:      $APP"
ok "Zip:      $ZIP"
ok "Checksum: $ZIP.sha256"
info "size: $(du -h "$ZIP" | cut -f1)"
if [ "$SIGNED" -eq 0 ]; then
  info "Unsigned: first open needs right-click > Open (see app/macos/SIGNING.md)."
fi
