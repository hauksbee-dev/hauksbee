#!/usr/bin/env bash
# build-watchy-s3.sh - rebuild the Watchy display-init firmware for esp32s3.
#
# Provenance for demo/firmware/watchy_display_init_s3/: the SAME source as
# testdata/firmware/watchy_display_init (unmodified), retargeted to esp32s3.
# The demo needs this because the watchy example board's MCU footprint is an
# ESP32-S3, so the engine's binder runs the qemu:esp32s3 machine; the esp32
# merged image in testdata boots nothing there (the S3 ROM reads the
# bootloader at 0x0, where the classic-esp32 image keeps 0xff padding).
#
# Requires esp-idf v5.x:  . ~/esp/esp-idf/export.sh && ./build-watchy-s3.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SRC="$ROOT/testdata/firmware/watchy_display_init"
OUT="$ROOT/demo/firmware/watchy_display_init_s3"

: "${IDF_PATH:?source esp-idf export.sh first (. ~/esp/esp-idf/export.sh)}"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cp -R "$SRC/main" "$SRC/CMakeLists.txt" "$WORK/"

cd "$WORK"
idf.py set-target esp32s3
idf.py build
cd build
esptool.py --chip esp32s3 merge_bin --fill-flash-size 4MB -o flash.bin @flash_args

mkdir -p "$OUT"
cp flash.bin watchy_display_init.elf "$OUT/"
echo "Wrote $OUT/flash.bin and $OUT/watchy_display_init.elf"
