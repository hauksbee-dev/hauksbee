#!/usr/bin/env bash
# sync-crate-assets.sh - refresh the crate-embedded mirrors of repo-level assets.
#
# `cargo package` ships only files under a crate's own directory, so every
# asset a published crate embeds via include_str!/include_bytes! lives in a
# mirror under crates/<crate>/assets/. The AUTHORITATIVE copy of each asset
# stays where it is maintained and consumed (scripts/, testdata/,
# crates/hauksbee-ci/examples/, examples/); this script copies authoritative
# over mirror, and each crate's tests/packaged_asset_sync.rs fails the build
# whenever a mirror drifts, pointing here.
#
# Idempotent; run from anywhere inside the repo.
set -euo pipefail
cd "$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"

# authoritative -> mirror
pairs=(
  "scripts/install-sims.sh:crates/hauksbee-engine/assets/scripts/install-sims.sh"
  "scripts/common.sh:crates/hauksbee-engine/assets/scripts/common.sh"
  "scripts/required-simulator-versions.env:crates/hauksbee-engine/assets/scripts/required-simulator-versions.env"
  "scripts/renode-checksums.txt:crates/hauksbee-engine/assets/scripts/renode-checksums.txt"
  "scripts/espressif-qemu-checksums.txt:crates/hauksbee-engine/assets/scripts/espressif-qemu-checksums.txt"
  "scripts/simulator-provenance.py:crates/hauksbee-engine/assets/scripts/simulator-provenance.py"
  "scripts/simavr-payload-provenance.sh:crates/hauksbee-engine/assets/scripts/simavr-payload-provenance.sh"
  "scripts/install-sims-windows.ps1:crates/hauksbee-engine/assets/scripts/install-sims-windows.ps1"
  "crates/hauksbee-ci/examples/boards/blinky.kicad_pcb:crates/hauksbee-engine/assets/examples/blinky.kicad_pcb"
  "examples/decks/rlc_ringdown.cir:crates/hauksbee-engine/assets/examples/rlc_ringdown.cir"
  "testdata/firmware/demo/demo.hex:crates/hauksbee-ci/assets/firmware/demo.hex"
  "testdata/sensor-specs/mcp4728.toml:crates/hauksbee-engine/assets/sensor-specs/mcp4728.toml"
)
for name in lm75 bma423_chip_id bme280 mpu6050 ads1115 ina219 mcp4728 icm42605; do
  pairs+=("testdata/sensor-specs/${name}.toml:crates/hauksbee-server/assets/sensor-specs/${name}.toml")
done

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
