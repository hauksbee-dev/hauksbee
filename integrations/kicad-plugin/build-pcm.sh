#!/usr/bin/env bash
# Build the KiCad PCM (Plugin and Content Manager) package for the hauksbee-ci
# plugin.
#
# PCM zip layout (https://dev-docs.kicad.org/en/addons/):
#   metadata.json          at the zip root
#   plugins/               the python package KiCad extracts into 3rdparty/plugins/
#   resources/icon.png     64x64 PCM listing icon (optional; skipped if absent)
#
# The script only COPIES the plugin sources into a staging directory; it never
# moves or renames them, so the symlink dev-install documented in README.md
# keeps working. Output lands in dist/ (gitignored via the repo root dist/
# pattern), and the download_* fields a registry listing needs are computed
# and printed at the end.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../.." && pwd)"
dist="$here/dist"

# --- version: single source of truth is the workspace Cargo.toml -------------
# [workspace.package] version, inherited by crates/hauksbee-ci.
version="$(awk '/^\[workspace\.package\]/{ws=1;next} /^\[/{ws=0} ws && /^version *=/{gsub(/[" ]/,"",$3); print $3; exit}' "$repo_root/Cargo.toml")"
if [ -z "$version" ]; then
    echo "error: could not read [workspace.package] version from $repo_root/Cargo.toml" >&2
    exit 1
fi

meta_version="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["versions"][0]["version"])' "$here/metadata.json")"
if [ "$version" != "$meta_version" ]; then
    echo "error: workspace Cargo.toml version ($version) != metadata.json versions[0].version ($meta_version)." >&2
    echo "       Update integrations/kicad-plugin/metadata.json before packaging." >&2
    exit 1
fi

# --- stage the PCM layout -----------------------------------------------------
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/plugins"
cp "$here/__init__.py" "$here/hauksbee_ci_core.py" "$here/hauksbee_ci_action.py" "$stage/plugins/"
cp "$here/metadata.json" "$stage/metadata.json"

if [ -f "$here/icon.png" ]; then
    mkdir -p "$stage/resources"
    cp "$here/icon.png" "$stage/resources/icon.png"
else
    echo "note: no icon.png found, packaging without a PCM listing icon."
fi

# --- zip ------------------------------------------------------------------------
mkdir -p "$dist"
zip_name="hauksbee-ci-pcm-v$version.zip"
zip_path="$dist/$zip_name"
rm -f "$zip_path"
contents=(metadata.json plugins)
[ -d "$stage/resources" ] && contents+=(resources)
# -X strips platform extra fields so the archive hashes reproducibly-ish.
(cd "$stage" && zip -q -r -X "$zip_path" "${contents[@]}")

# --- listing fields -------------------------------------------------------------
if command -v shasum >/dev/null 2>&1; then
    sha256="$(shasum -a 256 "$zip_path" | awk '{print $1}')"
else
    sha256="$(sha256sum "$zip_path" | awk '{print $1}')"
fi
download_size="$(wc -c < "$zip_path" | tr -d ' ')"
install_size="$(find "$stage" -type f ! -name metadata.json -exec wc -c {} + | awk 'END{print $1}')"

echo
echo "built:         $zip_path"
unzip -l "$zip_path"
echo
echo "sha256:        $sha256"
echo "download size: $download_size bytes"
echo "install size:  $install_size bytes"
echo
echo "versions[] entry for the registry listing (fill in the real release URL):"
python3 - "$here/metadata.json" "$sha256" "$download_size" "$install_size" "$version" "$zip_name" <<'PY'
import json, sys
meta_path, sha256, dl_size, inst_size, version, zip_name = sys.argv[1:7]
entry = dict(json.load(open(meta_path))["versions"][0])
entry.update(
    download_url=f"https://github.com/hauksbee-dev/hauksbee/releases/download/v{version}/{zip_name}",
    download_sha256=sha256,
    download_size=int(dl_size),
    install_size=int(inst_size),
)
print(json.dumps(entry, indent=4))
PY
