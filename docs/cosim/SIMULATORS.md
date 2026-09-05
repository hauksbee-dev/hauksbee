# External simulator backends

| Backend | Chips | Emulator | Install |
|---|---|---|---|
| `simavr` | ATmega328P (any part simavr knows by name) | libsimavr, linked in-process (GPL-3.0) | `scripts/install-sims.sh --avr` (source build) |
| `renode` | STM32F072/F103/F4, nRF52840, SiFive FE310, RP2040 | headless Renode process over Monitor TCP + UART socket | `hauksbee install renode` |
| `qemu` | ESP32, ESP32-S3, ESP32-C3 | Espressif QEMU fork over QMP + gdbstub + UART socket | `hauksbee install esp-qemu` |

libsimavr is not vendored; build without it with
`cargo build -p hauksbee-engine --no-default-features --features renode,qemu`
(GPL-free: the other two backends link nothing). Renode and QEMU are never
bundled; Hauksbee detects an install and talks to it over sockets. Tests skip
when a backend is absent. RP2040 needs nothing extra: Hauksbee ships its own
platform, compiled by Renode at run time (about eight seconds of bring-up per
run).

## Install

```
hauksbee install esp-qemu [--yes]    # official prebuilt Espressif fork into ~/.hauksbee-qemu-esp/
hauksbee install renode   [--yes]    # Antmicro portable build into ~/renode-portable
scripts/install-sims.sh              # Renode + QEMU; --avr adds libsimavr
scripts/install-sims.sh --renode-only | --qemu-only | --check
scripts/install-sims.sh --qemu-patched-source   # see below
```

Both `install` subcommands prompt for consent (`--yes` skips it) and exit
"already installed" when the binary is discoverable. The QEMU installer
verifies the release's `qemu-<ver>-checksum.sha256` when it can fetch it and
says so on stderr when it cannot; each binary is accepted only if
`-machine help` lists `esp32`. On an ESP32 board, `hauksbee run --firmware`
offers the install inline on an interactive terminal. The script reads pinned
versions from `scripts/required-simulator-versions.env`.

**Patched QEMU source build.** Espressif's prebuilt discards `GPIO_OUT_REG`
state, so GPIO output observation falls back to a firmware RAM mailbox.
`--qemu-patched-source` fetches the pinned commit, applies
`scripts/qemu-patches/esp32-gpio-register-state.patch`, builds both
architectures, live-probes paired `gpio-out`/`gpio-enable` QOM properties, and
installs under `~/.hauksbee-qemu-esp-patched`. Ordinary ESP-IDF GPIO output is
then visible without a mailbox. GPIO input, SAR ADC and I2C/SPI remain mailbox
contracts ([MCU.md](MCU.md)).

## Discovery order

**Renode** (`find_renode`, `crates/hauksbee-mcu/src/renode/process.rs`):

1. `$HAUKSBEE_RENODE` (full path to the binary; must exist);
2. `renode` on `$PATH`;
3. `~/renode-portable/Renode.app/Contents/MacOS/renode`,
   `~/renode-portable/renode`, `~/renode_portable/renode`.

On Windows: `HAUKSBEE_RENODE`, `Renode.exe`/`renode.exe` on `PATH`,
`%USERPROFILE%\renode-portable` (top level or `bin\`), then
`%ProgramFiles%\Renode` and `%LOCALAPPDATA%\Programs\Renode`.

**Espressif QEMU** (`find_qemu(arch)`, `crates/hauksbee-mcu/src/qemu/process.rs`),
for each of `qemu-system-xtensa` (ESP32/S3) and `qemu-system-riscv32` (C3):

1. `$HAUKSBEE_QEMU_XTENSA` / `$HAUKSBEE_QEMU_RISCV32` (taken as-is; the only
   slot without the esp32-machine check);
2. `$HAUKSBEE_QEMU_DIR/bin/<name>`;
3. `~/.hauksbee-qemu-esp-patched/qemu/bin/<name>`, then
   `~/.hauksbee-qemu-esp/qemu/bin/<name>` (legacy `~/.galvani-qemu-esp` still
   honoured);
4. `<idf-tools-root>/tools/qemu-*/<ver>/qemu/bin/<name>` for
   `$IDF_TOOLS_PATH`, `~/.espressif`, and on Windows `C:\Espressif`;
5. `<name>` on `$PATH`.

Slots 2-5 accept a candidate only if `<binary> -machine help` lists `esp32`.
Homebrew's mainline `qemu-system-xtensa` (`lx60`/`kc705`/`sim` only) is
rejected wherever it sits.

| Variable | Purpose |
|---|---|
| `HAUKSBEE_RENODE` | full path to `renode` |
| `HAUKSBEE_QEMU_XTENSA` | full path to `qemu-system-xtensa` (Espressif fork) |
| `HAUKSBEE_QEMU_RISCV32` | full path to `qemu-system-riscv32` (Espressif fork) |
| `HAUKSBEE_QEMU_DIR` | directory whose `bin/` holds both |

## Verify

```
$ hauksbee doctor --backends
avr	builtin	simavr linked into this binary; source commit ...
qemu-xtensa	ok	/Users/you/.hauksbee-qemu-esp/qemu/bin/qemu-system-xtensa
qemu-riscv32	ok	/Users/you/.hauksbee-qemu-esp/qemu/bin/qemu-system-riscv32
renode	ok	/Users/you/renode-portable/Renode.app/Contents/MacOS/renode
```

`doctor` runs the engine's own discovery, so it cannot disagree with a co-sim.
On a TTY it prints a table; piped, one `NAME<TAB>STATUS<TAB>PATH-OR-HINT` line
per backend on stdout; `--json` gives `{"backends":[{name,status,available,...}]}`.
Integration tests: `cargo test -p hauksbee-engine --test stm32_renode_cosim`
and `--test esp32_qemu_cosim`.

## Manual install

**Renode, macOS**: download `renode-<ver>-dotnet.osx-{arm64,x86_64}-portable.dmg`
from github.com/renode/renode/releases, then

```
hdiutil attach renode-<ver>-dotnet.osx-arm64-portable.dmg -mountpoint /tmp/renode_mnt -nobrowse
mkdir -p ~/renode-portable && ditto /tmp/renode_mnt/Renode.app ~/renode-portable/Renode.app
hdiutil detach /tmp/renode_mnt
xattr -dr com.apple.quarantine ~/renode-portable/Renode.app
```

Use `ditto`, not `cp -R` (it breaks symlinks inside the bundle), and remove
the quarantine flag or Gatekeeper blocks the first launch.

**Renode, Linux**: extract `renode-<ver>.linux-portable-dotnet.tar.gz` (or the
`linux-arm64` asset) with `tar xzf ... -C ~/renode-portable --strip-components=1`.

**Renode, Windows**: the `windows-portable-dotnet.zip` under
`%USERPROFILE%\renode-portable`, or the `.msi`.

**Espressif QEMU**: with ESP-IDF present,
`python3 $IDF_PATH/tools/idf_tools.py install qemu-xtensa qemu-riscv32` (slot 4).
Otherwise download both `qemu-{xtensa,riscv32}-softmmu-<ver>-<platform>.tar.xz`
assets from github.com/espressif/qemu/releases (asset names use underscores
where the tag uses dashes) and extract each with `tar -xJf ... --strip-components=1`
into `~/.espressif/tools/qemu-<tool>/<ver>/qemu/` (slot 4) or copy both
binaries into `~/.hauksbee-qemu-esp/qemu/bin/` (slot 3). On Windows the same
layouts apply with `.exe`, or the idf-tools tree under `C:\Espressif\tools\`.

Confirm the fork: `qemu-system-xtensa -machine help | grep esp32`.
