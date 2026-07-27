# External simulator backends

> For the full capability map of hauksbee (MCU architecture coverage, which parts are proven, and the scope of the co-sim layer) see [`docs/about/CAPABILITIES.md`](../about/CAPABILITIES.md).

hauksbee's MCU co-simulation layer runs firmware against the solved analog
circuit in lockstep. Three backends cover the full supported architecture range:

| Backend | Chips | Emulator | Install needed? |
|---------|-------|----------|-----------------|
| `simavr` | ATmega328P (AVR / Arduino) | libsimavr, linked in-process | **Yes** (source build), `scripts/install-sims.sh --avr` |
| `renode` | STM32 / nRF52840 / SiFive FE310 (RISC-V) / RP2040 | External headless Renode process | **Yes** |
| `qemu` | ESP32 / ESP32-S3 (Xtensa) / ESP32-C3 (RISC-V) | External Espressif QEMU process | **Yes** |

AVR links libsimavr from the system (GPL-3.0, deliberately not vendored in this
Apache-2.0 repo): install it with `scripts/install-sims.sh --avr`, or build without AVR
via `cargo build -p hauksbee-engine --no-default-features --features renode,qemu`.
This document covers installing Renode and the Espressif QEMU fork for the other
two. See [`docs/cosim/MCU.md`](MCU.md) for the full co-simulation
architecture, per-board recipes, and proven integration test results.

---

## Why these are not bundled

Renode is EPL-licensed and ~150 MB. Espressif QEMU is GPL-2.0 and similarly
large. Vendoring either into hauksbee would bloat the distribution, impose
redistribution obligations, and undercut the core premise (PCB CI from the
design files, no bulky EDA toolchain required). The same "detect, don't bundle"
pattern used for the KiCad and ngspice oracles (see [`docs/cosim/ORACLES.md`](ORACLES.md))
applies here. hauksbee locates an externally installed binary and uses it on
demand; tests skip cleanly when the binary is absent rather than failing.

---

## Quick install

For the ESP32-family backend, hauksbee can fetch the Espressif QEMU fork
itself, no shell script needed:

```
hauksbee install esp-qemu          # prompts; add --yes for CI
```

This downloads Espressif's **official** prebuilt `qemu-system-xtensa` and
`qemu-system-riscv32` from
[github.com/espressif/qemu/releases](https://github.com/espressif/qemu/releases),
verifies each archive's sha256 against the release's checksum manifest,
unpacks into `~/.hauksbee-qemu-esp/` (discovery slot 3 below), and accepts
each binary only after the same esp32-machine check the co-sim applies.
Nothing is bundled: the fork is a separate GPL-2.0 program hauksbee talks to
over sockets. `hauksbee run --firmware` on an ESP32-family board offers the
same install inline when it finds the emulator missing on an interactive
terminal (declining keeps the loud install-guidance error).

For everything at once (Renode + QEMU, optionally AVR):

```
scripts/install-sims.sh
```

That is it. The script detects your OS and architecture, resolves the latest
release via the GitHub API, downloads the portable builds, puts them exactly
where hauksbee auto-discovers them, and verifies the result. Re-running is safe:
if a backend is already present it is skipped.

To install only one backend:

```
scripts/install-sims.sh --renode-only
scripts/install-sims.sh --qemu-only
```

To check whether hauksbee can find the simulators without installing anything:

```
scripts/install-sims.sh --check
```

The `--check` flag mirrors the exact discovery logic in the Rust source. Exit 0
means both (requested) backends are discoverable; non-zero means at least one
is missing.

---

## Discovery order

### Renode

hauksbee calls `find_renode()` in
`crates/hauksbee-mcu/src/renode/process.rs`. It checks, in order:

1. `$HAUKSBEE_RENODE`, if set, it must be the full path to the `renode`
   binary. hauksbee uses it directly and fails clearly if the path does not
   exist.
2. `renode` on `$PATH`.
3. Conventional portable install locations under `$HOME`:
   - `~/renode-portable/Renode.app/Contents/MacOS/renode` (macOS app bundle)
   - `~/renode-portable/renode` (Linux extracted tarball)
   - `~/renode_portable/renode` (underscore variant, for compatibility)

The installer places the app bundle at `~/renode-portable/Renode.app`, which
hits case 3 on macOS, and extracts the tarball to `~/renode-portable/` on Linux,
which hits case 3 for Linux.

### Espressif QEMU

hauksbee calls `find_qemu(arch)` in
`crates/hauksbee-mcu/src/qemu/process.rs`. For each of `qemu-system-xtensa`
(ESP32 / ESP32-S3) and `qemu-system-riscv32` (ESP32-C3), it checks:

1. `$HAUKSBEE_QEMU_XTENSA` / `$HAUKSBEE_QEMU_RISCV32`, full path to the
   binary; used directly if set.
2. `$HAUKSBEE_QEMU_DIR/bin/<name>`, points at the `bin/` directory of the
   unpacked fork.
3. `~/.hauksbee-qemu-esp/qemu/bin/<name>`; the conventional manual-unpack
   location.
4. `~/.espressif/tools/qemu-*/<ver>/qemu/bin/<name>`; the location used by
   ESP-IDF's `idf_tools.py install qemu-xtensa qemu-riscv32`.
5. `<name>` on `$PATH`, accepted **only if** it is the Espressif fork. The
   check runs `qemu-system-xtensa -machine help` and looks for `esp32` in the
   output. Homebrew's mainline `qemu-system-xtensa` lists only `lx60`/`kc705`/
   `sim` and is rejected. This guard is not optional: mainline QEMU cannot boot
   an ESP32 image.

The installer uses `idf_tools.py` if an ESP-IDF checkout is found
(`~/esp/esp-idf`, `$IDF_PATH`, or `~/.espressif`), which puts binaries in
discovery slot 4. Otherwise it downloads directly from
[github.com/espressif/qemu/releases](https://github.com/espressif/qemu/releases)
and places them in `~/.espressif/tools/qemu-xtensa/<ver>/qemu/bin/`, which also
hits slot 4.

### Env-var overrides

Set these to point hauksbee at a custom install location without touching the
conventional paths:

| Variable | Purpose |
|----------|---------|
| `HAUKSBEE_RENODE` | Full path to the `renode` binary |
| `HAUKSBEE_QEMU_XTENSA` | Full path to `qemu-system-xtensa` (Espressif fork) |
| `HAUKSBEE_QEMU_RISCV32` | Full path to `qemu-system-riscv32` (Espressif fork) |
| `HAUKSBEE_QEMU_DIR` | Directory whose `bin/` subdirectory contains both binaries |

---

## Verifying the install

Run the check mode:

```
scripts/install-sims.sh --check
```

For QEMU specifically, confirm the Espressif fork is in place:

```
~/.espressif/tools/qemu-xtensa/<ver>/qemu/bin/qemu-system-xtensa -machine help | grep esp32
```

You should see at least `esp32` and `esp32s3` in the output. If the output is
empty or shows only `lx60`/`kc705`/`sim`, you have mainline QEMU, not the
Espressif fork, and hauksbee will reject it.

Run the integration tests (they skip cleanly if the emulator is absent):

```
cargo test -p hauksbee-engine --test stm32_renode_cosim -- --nocapture
cargo test -p hauksbee-engine --test esp32_qemu_cosim  -- --nocapture
```

---

## macOS Gatekeeper note

The Renode DMG is not notarized by default. After copying the app bundle, the
installer runs:

```
xattr -dr com.apple.quarantine ~/renode-portable/Renode.app
```

Without this, Gatekeeper blocks the binary on first launch with "cannot be
opened because the developer cannot be verified." The quarantine flag is a
per-file extended attribute; removing it does not disable system-wide Gatekeeper.

If you installed Renode yourself and hauksbee hangs or fails to spawn it, run
the `xattr` command above manually.

The installer also uses `ditto` (not `cp -R`) to copy the app bundle. `ditto`
preserves extended attributes and the symlinks inside the bundle that `cp -R`
silently breaks on macOS (resulting in a dylib permission error at runtime).

---

## Manual install

If you prefer not to run the script, the steps below are what it does.

### Renode

**macOS (arm64)**

1. Download `renode-<ver>-dotnet.osx-arm64-portable.dmg` from
   [github.com/renode/renode/releases](https://github.com/renode/renode/releases).
2. Mount it and copy the app bundle:

   ```
   hdiutil attach renode-<ver>-dotnet.osx-arm64-portable.dmg \
     -mountpoint /tmp/renode_mnt -nobrowse
   mkdir -p ~/renode-portable
   ditto /tmp/renode_mnt/Renode.app ~/renode-portable/Renode.app
   hdiutil detach /tmp/renode_mnt
   xattr -dr com.apple.quarantine ~/renode-portable/Renode.app
   ```

3. Verify: `~/renode-portable/Renode.app/Contents/MacOS/renode --version`

**macOS (x86_64)**

Same as above, but use the `osx-x86_64-portable.dmg` asset.

**Linux (x86_64)**

1. Download `renode-<ver>.linux-portable-dotnet.tar.gz` from the releases page.
2. Extract:

   ```
   mkdir -p ~/renode-portable
   tar xzf renode-<ver>.linux-portable-dotnet.tar.gz \
     -C ~/renode-portable --strip-components=1
   ```

3. Verify: `~/renode-portable/renode --version`

**Linux (arm64)**

Same as x86_64 above, but use the `linux-arm64-portable-dotnet.tar.gz` asset.

**Windows**

Download `renode-<ver>.windows-portable-dotnet.zip`, extract it, and either add
the `renode.exe` to `PATH` or set `HAUKSBEE_RENODE` to its full path. The
installer script does not cover Windows (it is bash); this step must be done
manually. Windows as a whole is untested territory: if you want to change that,
`docs/about/release-and-licensing.md` section 5 has a ready-made prompt for
pointing a coding agent at the port, and an invitation to PR the result.

---

### Espressif QEMU

**Via ESP-IDF (recommended if you already have it)**

If `~/esp/esp-idf`, `$IDF_PATH`, or `~/.espressif/idf_tools.py` is present:

```
python3 ~/esp/esp-idf/tools/idf_tools.py install qemu-xtensa qemu-riscv32
```

This places the binaries in `~/.espressif/tools/qemu-xtensa/<ver>/qemu/bin/`
and `~/.espressif/tools/qemu-riscv32/<ver>/qemu/bin/`, which hauksbee discovers
automatically (slot 4).

**Direct download (no ESP-IDF)**

1. Go to [github.com/espressif/qemu/releases](https://github.com/espressif/qemu/releases)
   and find the latest release (tags like `esp-develop-9.0.0-20240606`).
2. Download the asset for your platform for **both** `qemu-xtensa` and
   `qemu-riscv32`:

   | Platform | Asset suffix |
   |----------|-------------|
   | macOS arm64 | `aarch64-apple-darwin.tar.bz2` |
   | macOS x86_64 | `x86_64-apple-darwin.tar.bz2` |
   | Linux x86_64 | `x86_64-linux-gnu.tar.bz2` |
   | Linux arm64 | `aarch64-linux-gnu.tar.bz2` |

3. Extract each into `~/.espressif/tools/<tool-name>/<ver>/qemu/`. The tarball
   has a top-level `qemu/` directory; extract one level up and strip it:

   ```
   DEST=~/.espressif/tools/qemu-xtensa/<ver>/qemu
   mkdir -p "$DEST"
   tar xjf qemu-xtensa-softmmu-<tag>-aarch64-apple-darwin.tar.bz2 \
     -C "$DEST" --strip-components=1

   DEST=~/.espressif/tools/qemu-riscv32/<ver>/qemu
   mkdir -p "$DEST"
   tar xjf qemu-riscv32-softmmu-<tag>-aarch64-apple-darwin.tar.bz2 \
     -C "$DEST" --strip-components=1
   ```

4. Alternatively, unpack both into `~/.hauksbee-qemu-esp/qemu/` (slot 3):

   ```
   mkdir -p ~/.hauksbee-qemu-esp/qemu/bin
   # copy qemu-system-xtensa and qemu-system-riscv32 into that bin/ dir
   ```

5. Verify the Espressif fork:

   ```
   ~/.espressif/tools/qemu-xtensa/<ver>/qemu/bin/qemu-system-xtensa \
     -machine help | grep esp32
   ```
