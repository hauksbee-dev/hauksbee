#!/usr/bin/env bash
# sync-crate-assets.sh - refresh the crate-embedded mirrors of repo-level assets.
#
# `cargo package` ships only files under a crate's own directory, so assets a
# published crate embeds via include_str!/include_bytes! live under
# crates/<crate>/assets/. Most are mirrors of authoritative copies elsewhere.
# The exceptions: install-sims.sh and simulator-provenance.py are
# authoritative under hauksbee-engine/assets/scripts, with small repo-root
# scripts/ forwarders, and the bundled sensor specs are authoritative under
# hauksbee-server/assets/sensor-specs. The remaining
# pairs below copy authoritative over mirror, and packaged_asset_sync.rs catches
# drift.
#
# Idempotent; run from anywhere inside the repo.
set -euo pipefail
cd "$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"

# authoritative -> mirror
pairs=(
  "scripts/common.sh:crates/hauksbee-engine/assets/scripts/common.sh"
  "scripts/required-simulator-versions.env:crates/hauksbee-engine/assets/scripts/required-simulator-versions.env"
  "scripts/renode-checksums.txt:crates/hauksbee-engine/assets/scripts/renode-checksums.txt"
  "scripts/espressif-qemu-checksums.txt:crates/hauksbee-engine/assets/scripts/espressif-qemu-checksums.txt"
  "scripts/simavr-payload-provenance.sh:crates/hauksbee-engine/assets/scripts/simavr-payload-provenance.sh"
  "scripts/install-sims-windows.ps1:crates/hauksbee-engine/assets/scripts/install-sims-windows.ps1"
  "crates/hauksbee-ci/examples/boards/blinky.kicad_pcb:crates/hauksbee-engine/assets/examples/blinky.kicad_pcb"
  "examples/decks/rlc_ringdown.cir:crates/hauksbee-engine/assets/examples/rlc_ringdown.cir"
  "testdata/firmware/demo/demo.hex:crates/hauksbee-ci/assets/firmware/demo.hex"
  "crates/hauksbee-server/assets/sensor-specs/mcp4728.toml:crates/hauksbee-cosim/assets/sensor-specs/mcp4728.toml"
)

changed=0
for pair in "${pairs[@]}"; do
  src="${pair%%:*}"; dst="${pair##*:}"
  [ -f "$src" ] || { echo "missing authoritative file: $src" >&2; exit 1; }
  if ! cmp -s "$src" "$dst"; then
    mkdir -p "$(dirname "$dst")"
    cp "$src" "$dst"
    echo "synced $dst"
    changed=1
  fi
done
[ "$changed" = 1 ] || echo "all crate asset mirrors already in sync"
