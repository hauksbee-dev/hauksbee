# External simulator backends

> For the full capability map of hauksbee (MCU architecture coverage, which parts are proven, and the scope of the co-sim layer) see [`docs/about/CAPABILITIES.md`](../about/CAPABILITIES.md).

hauksbee's MCU co-simulation layer runs firmware against the solved analog
circuit in lockstep. Three backends cover the full supported architecture
range:

| Backend | Chips | Emulator | Install needed? |
|---------|-------|----------|-----------------|
| `simavr` | ATmega328P (AVR / Arduino) | libsimavr, linked in-process | **Yes** (source build), `scripts/install-sims.sh --avr` |
| `renode` | STM32 / nRF52840 / SiFive FE310 (RISC-V) / RP2040 | External headless Renode process | **Yes** |
| `qemu` | ESP32 / ESP32-S3 (Xtensa) / ESP32-C3 (RISC-V) | External Espressif QEMU process | **Yes** |

AVR links libsimavr from the system (GPL-3.0, deliberately not vendored in
this Apache-2.0 repo). Install it with `scripts/install-sims.sh --avr`, or
build without AVR through `cargo build -p hauksbee-engine
--no-default-features --features renode,qemu`. This document covers
installing Renode and the Espressif QEMU fork for the other two backends. See
[`docs/cosim/MCU.md`](MCU.md) for the full co-simulation architecture,
per-board recipes, and proven integration test results.

Two entries in that table need reading carefully:

- **simavr accepts any part name simavr knows.** `AvrMcu::new` passes the name
  straight through, so `atmega2560` or an ATtiny constructs and runs. Only the
  **ATmega328P port map** is auto-registered, though: pin hooks are installed
  for ports A through D, so a part with ports beyond those needs
  `register_port_hooks` called for them, and the shipped board recipes are
  ATmega328P.
- **RP2040 brings its own platform.** Renode ships no rp2040 platform, on 1.16.1
  or on `master`, so hauksbee carries one: the peripheral models are vendored C#
  that Renode compiles at run time, unpacked from the binary when the machine is
  created. Nothing extra to install, but each machine creation compiles about
  377 kB of C#, so an RP2040 run spends roughly eight seconds on bring-up before
  firmware executes. A bound RP2040 or Pico board is routed automatically; the
  proven and unproven features are itemized in
  [`docs/cosim/MCU.md`](MCU.md).

---

## Why the default downloads do not bundle them

Renode is MIT-licensed and ~150 MB. Espressif QEMU is GPL-2.0 and similarly
large. The normal tarballs and macOS app keep both as separately installed
programs so the everyday download stays small. The optional private `:full`
container deliberately bundles them for turnkey CI; it retains their exact
license texts and a corresponding-source offer under
`/usr/share/doc/hauksbee/third-party/`. Outside that image, the same "detect,
don't bundle" pattern used for the KiCad and ngspice oracles (see
[`docs/cosim/ORACLES.md`](ORACLES.md)) applies. Tests skip cleanly when a
separately installed binary is absent, rather than failing.

---

## Quick install

hauksbee can fetch either external backend itself. No shell script is needed:

```
hauksbee install esp-qemu          # prompts; add --yes for CI
hauksbee install renode
```

`install esp-qemu` downloads Espressif's **official** prebuilt
`qemu-system-xtensa` and `qemu-system-riscv32` from
[github.com/espressif/qemu/releases](https://github.com/espressif/qemu/releases),
unpacks the fork into `~/.hauksbee-qemu-esp/` (discovery slot 3 below), and
accepts each binary only after the same esp32-machine check the co-sim
applies. `install renode` fetches Antmicro's published Renode portable build
and unpacks it into `~/renode-portable`, the same flow as
`scripts/install-sims.sh --renode-only`. In these installer flows each remains a
separate program hauksbee talks to over sockets. Both subcommands prompt for consent and
take `--yes` to skip it, and both report "already installed" and exit when the
binary is already discoverable.

Archive integrity is best-effort to *obtain* and strict once obtained. The
installer fetches the release's `qemu-<ver>-checksum.sha256` manifest; when it
has a hash for an archive it verifies it and a mismatch aborts the install
("sha256 MISMATCH ... Refusing to install it"). When the manifest cannot be
fetched, or carries no line for that asset, the installer says so on stderr and
proceeds **without** hash verification, leaving TLS and the esp32-machine check
as the gates. So do not read "verifies the sha256" as unconditional: an offline
or rate-limited GitHub degrades it, loudly, rather than failing the install.

On an ESP32-family board, `hauksbee run --firmware` offers the same install
inline when it finds the emulator missing on an interactive terminal (declining
keeps the loud install-guidance error). Every "not found" error from either
backend names its `hauksbee install` subcommand, so the fix is in the message.

To install everything at once (Renode + QEMU, optionally AVR), run:

```
scripts/install-sims.sh
```

That is all it takes. The script detects your OS and architecture, resolves
the latest release through the GitHub API, downloads the portable builds,
places them exactly where hauksbee auto-discovers them, and verifies the
result. Re-running is safe: if a backend is already present, the script
skips it.

To install only one backend:

```
scripts/install-sims.sh --renode-only
scripts/install-sims.sh --qemu-only
```

To check whether hauksbee can find the simulators without installing
anything:

```
scripts/install-sims.sh --check
```

The `--check` flag mirrors the exact discovery logic in the Rust source.
Exit 0 means both (requested) backends are discoverable. A non-zero exit
means at least one is missing.

---

## Discovery order

### Renode

hauksbee calls `find_renode()` in
`crates/hauksbee-mcu/src/renode/process.rs`. It checks, in order:

1. `$HAUKSBEE_RENODE`. If set, it must name the full path to the `renode`
   binary. hauksbee uses it directly and fails clearly if the path does not
   exist.
2. `renode` on `$PATH`.
3. Conventional portable install locations under `$HOME`:
   - `~/renode-portable/Renode.app/Contents/MacOS/renode` (macOS app bundle)
   - `~/renode-portable/renode` (Linux extracted tarball)
   - `~/renode_portable/renode` (underscore variant, for compatibility)

The installer places the app bundle at `~/renode-portable/Renode.app`, which
hits case 3 on macOS, and extracts the tarball to `~/renode-portable/` on
Linux, which also hits case 3.

### Espressif QEMU

hauksbee calls `find_qemu(arch)` in
`crates/hauksbee-mcu/src/qemu/process.rs`. For each of `qemu-system-xtensa`
(ESP32 / ESP32-S3) and `qemu-system-riscv32` (ESP32-C3), it checks:

1. `$HAUKSBEE_QEMU_XTENSA` / `$HAUKSBEE_QEMU_RISCV32`, the full path to the
   binary. hauksbee uses it directly if it exists, and errors naming the
   variable if it does not. This is the one slot that takes your word for it:
   the esp-fork check below is **not** applied, so an override pointed at
   mainline QEMU will be accepted here and fail later at boot.
2. `$HAUKSBEE_QEMU_DIR/bin/<name>`, the `bin/` directory of the unpacked
   fork.
3. `~/.hauksbee-qemu-esp/qemu/bin/<name>`, both the conventional manual-unpack
   location and where `hauksbee install esp-qemu` puts it. The legacy
   `~/.galvani-qemu-esp/qemu/bin/<name>` is still honoured right after it, so
   a fork unpacked before the rename keeps resolving.
4. `<idf-tools-root>/tools/qemu-*/<ver>/qemu/bin/<name>`, the location
   ESP-IDF's `idf_tools.py install qemu-xtensa qemu-riscv32` uses. The roots
   tried, in order, are `$IDF_TOOLS_PATH`, `~/.espressif`, and on Windows
   `C:\Espressif`.
5. `<name>` on `$PATH`.

**Every slot except the explicit per-arch override in slot 1 is gated on the
esp-fork check**, not just `$PATH`: a candidate is accepted only if running
`<binary> -machine help` lists `esp32`. Homebrew's mainline
`qemu-system-xtensa` shows only `lx60`/`kc705`/`sim` and gets rejected wherever
it sits, including under `$HAUKSBEE_QEMU_DIR` or an idf-tools tree. This guard
is not optional: mainline QEMU cannot boot an ESP32 image.

The installer uses `idf_tools.py` when it finds an ESP-IDF checkout
(`~/esp/esp-idf`, `$IDF_PATH`, or `~/.espressif`), which puts binaries in
discovery slot 4. Otherwise it downloads directly from
[github.com/espressif/qemu/releases](https://github.com/espressif/qemu/releases)
and places them in `~/.espressif/tools/qemu-xtensa/<ver>/qemu/bin/`, which
also hits slot 4.

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

Ask the binary itself. `hauksbee doctor --backends` runs the engine's own
`find_qemu` / `find_renode`, so it can never disagree with what a co-sim would
resolve, and prints one tab-separated `NAME<TAB>STATUS<TAB>PATH-OR-HINT` line
per backend (add `--json` for machine consumption). On this machine:

```
$ hauksbee doctor --backends
hauksbee co-sim backends (resolved by the engine's own discovery)
    avr           ATmega / ATtiny firmware co-sim
avr	builtin	simavr linked into this binary; source commit f44723e8c42431136d5b4de81f789ded56d7e8fa
    qemu-xtensa   ESP32 / ESP32-S3 firmware co-sim (Espressif QEMU fork)
qemu-xtensa	ok	/Users/you/.hauksbee-qemu-esp/qemu/bin/qemu-system-xtensa
    qemu-riscv32  ESP32-C3 firmware co-sim (Espressif QEMU fork)
qemu-riscv32	ok	/Users/you/.hauksbee-qemu-esp/qemu/bin/qemu-system-riscv32
    renode        STM32 / nRF52 / RISC-V firmware co-sim
renode	ok	/Users/you/renode-portable/Renode.app/Contents/MacOS/renode
```

The indented lines are the human header on stderr; the flush lines are the
machine-readable report on stdout. A backend that resolves to mainline QEMU is
reported absent here exactly as the co-sim rejects it. Anything missing is one
command away: `hauksbee install renode` or `hauksbee install esp-qemu`.

The shell script has an equivalent check mode:

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

Run the integration tests. They skip cleanly if the emulator is absent:

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

Without this step, Gatekeeper blocks the binary on first launch with "cannot
be opened because the developer cannot be verified." The quarantine flag is a
per-file extended attribute. Removing it does not disable system-wide
Gatekeeper.

If you installed Renode yourself and hauksbee hangs or fails to spawn it, run
the `xattr` command above manually.

The installer also uses `ditto` (not `cp -R`) to copy the app bundle.
`ditto` preserves extended attributes and the symlinks inside the bundle
that `cp -R` silently breaks on macOS (this causes a dylib permission error
at runtime).

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

1. Download `renode-<ver>.linux-portable-dotnet.tar.gz` from the releases
   page.
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

Run `scripts\install-sims-windows.ps1 -RenodeOnly`. It downloads the pinned
portable release, verifies the repository-recorded SHA-256, probes
`Renode.exe`, and transactionally installs it under
`%USERPROFILE%\renode-portable`. A manual alternative is to download the same
`windows-portable-dotnet.zip` or use the `.msi` installer. Discovery checks,
in order: `HAUKSBEE_RENODE`,
`Renode.exe`/`renode.exe` on `PATH`, the `%USERPROFILE%\renode-portable` zip
layout (`Renode.exe` at the top or under `bin\`), then the installer trees
under `%ProgramFiles%\Renode` and `%LOCALAPPDATA%\Programs\Renode`. These
lookups are unit-tested on every OS. Both the ordinary Windows CI lane and the
release job install that exact archive and require the named RP2040
firmware-to-ADC flow; a missing backend, missing firmware, `SKIP:`, wrong SHA,
or absent one-test pass record fails the job.

---

### Espressif QEMU

**Via ESP-IDF (recommended if you already have it)**

If `~/esp/esp-idf`, `$IDF_PATH`, or `~/.espressif/idf_tools.py` is present:

```
python3 ~/esp/esp-idf/tools/idf_tools.py install qemu-xtensa qemu-riscv32
```

This places the binaries in `~/.espressif/tools/qemu-xtensa/<ver>/qemu/bin/`
and `~/.espressif/tools/qemu-riscv32/<ver>/qemu/bin/`, which hauksbee
discovers automatically (slot 4).

**Direct download (no ESP-IDF)**

1. Go to [github.com/espressif/qemu/releases](https://github.com/espressif/qemu/releases)
   and find the latest release. Tags look like `esp-develop-9.2.2-20260417`;
   the asset names carry the same version with **underscores**
   (`esp_develop_9.2.2_20260417`), so the tag is not a substring of the asset.
2. Download the asset for your platform for **both** `qemu-xtensa` and
   `qemu-riscv32`. Every current asset is `.tar.xz`:

   | Platform | Asset |
   |----------|-------|
   | macOS arm64 | `qemu-<tool>-softmmu-<ver>-aarch64-apple-darwin.tar.xz` |
   | macOS x86_64 | `qemu-<tool>-softmmu-<ver>-x86_64-apple-darwin.tar.xz` |
   | Linux x86_64 | `qemu-<tool>-softmmu-<ver>-x86_64-linux-gnu.tar.xz` |
   | Linux arm64 | `qemu-<tool>-softmmu-<ver>-aarch64-linux-gnu.tar.xz` |
   | Windows x86_64 | `qemu-<tool>-softmmu-<ver>-x86_64-w64-mingw32.tar.xz` |

   `<tool>` is `xtensa` or `riscv32`; `<ver>` is the underscored version. The
   release also publishes `qemu-<ver>-checksum.sha256`, which lists
   `<sha256hex> *<asset-name>` for each. Upstream has changed both the
   separator convention and the compression (`.tar.bz2` to `.tar.xz`) across
   releases, which is why `hauksbee install esp-qemu` resolves the name by
   listing the release's published assets rather than constructing it. Check
   the release page before assuming a suffix.
3. Extract each into `~/.espressif/tools/<tool-name>/<ver>/qemu/`. The
   tarball has a top-level `qemu/` directory. Extract one level up and strip
   it. `.tar.xz` needs `-J`, not `-z` or `-j`:

   ```
   DEST=~/.espressif/tools/qemu-xtensa/<ver>/qemu
   mkdir -p "$DEST"
   tar -xJf qemu-xtensa-softmmu-<ver>-aarch64-apple-darwin.tar.xz \
     -C "$DEST" --strip-components=1

   DEST=~/.espressif/tools/qemu-riscv32/<ver>/qemu
   mkdir -p "$DEST"
   tar -xJf qemu-riscv32-softmmu-<ver>-aarch64-apple-darwin.tar.xz \
     -C "$DEST" --strip-components=1
   ```

   GNU tar and bsdtar both sniff the compression, so a bare `tar -xf` also
   works; `-J` is explicit about what the archive is.

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

**Windows**: the same two layouts work with backslashes and `.exe`. Unpack
into `%USERPROFILE%\.hauksbee-qemu-esp\qemu\bin\` (so
`qemu-system-xtensa.exe` and `qemu-system-riscv32.exe` sit there), or use the
idf-tools tree under `%USERPROFILE%\.espressif\tools\`, `C:\Espressif\tools\`
(the ESP-IDF Windows installer default), or wherever `IDF_TOOLS_PATH` points.
`scripts\install-sims-windows.ps1 -QemuOnly` is the release-evidence route: it
downloads both exact Windows archives, verifies repository-recorded SHA-256
values, rejects unsafe archive paths, checks each binary's ESP32 machine list,
and transactionally installs the shared tree. `hauksbee install esp-qemu --yes`
invokes this same pinned PowerShell route as the interactive front door and is
also exercised through the production dependency installer; the direct script
remains the auditable release-evidence entry point. Native CI then requires one
Xtensa firmware/I2C/GPIO flow and one RISC-V
firmware/UART/current/GPIO-circuit flow.
