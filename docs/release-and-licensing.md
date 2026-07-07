# Release automation & binary licensing

Companion note to `.github/workflows/release.yml` and `07-ux-and-integrations.md`
§6. Covers the licensing decision for prebuilt binaries, a known blocker that
prevents that decision from being realised today, the naming contract with the
installer, the aarch64-linux build mechanism, and the Windows investigation.

## 1. The licensing decision (MIT-clean binaries)

hauksbee's source is MIT. Its optional `avr` co-sim backend **statically links
libsimavr, which is GPL-3.0**. Statically linking GPL code makes the *binary* a
GPL derivative work: distributing it puts the whole download under GPL-3.0, even
though the source stays MIT. For a `curl | bash` installer this is a real,
irreversible consequence for the person installing.

**Decision: release binaries ship in the MIT-clean shape** — renode + qemu
backends only, no avr, no libsimavr:

```
cargo build --release --no-default-features --features renode,qemu
```

- AVR co-sim stays available to anyone who **builds from source** and runs
  `scripts/install-sims.sh --avr` — GPL simavr is then built on *their* machine
  (their combination, their choice), never something the project distributes.
- The full GPL-encumbered shape (option A) is preserved as a **commented-out
  variant** at the bottom of `release.yml`. It is not the shipping default and
  must not be enabled without an explicit, documented decision to ship GPL
  binaries.
- This position is consistent with what the code already states:
  `scripts/install-sims.sh` and `crates/hauksbee-mcu/build.rs` both document that
  simavr is GPL-3.0 and deliberately *not* vendored, and `hauksbee-engine`'s
  Cargo manifest comment already calls `--no-default-features --features
  renode,qemu` "the GPL-free build". The release workflow makes that stance the
  shipping default rather than an aspiration.

## 2. ⚠ Blocker: the MIT-clean shape is not actually avr-free today

The shape `--no-default-features --features renode,qemu` **still activates `avr`
and still links libsimavr** in this workspace. This was verified, not assumed:

- With simavr discovery pointed at a nonexistent path, an engine-alone
  `--no-default-features --features renode,qemu` build **panics** in
  `crates/hauksbee-mcu/build.rs:48`: *"the `avr` co-sim feature needs a system
  libsimavr"* — proving `avr` is active under those flags.
- `nm` on a two-binary release build (`-p hauksbee-engine -p hauksbee-ci
  --no-default-features --features renode,qemu`) finds **14 simavr C symbols +
  42 `AvrMcu` symbols in each of `hauksbee` and `hauksbee-ci`**.
- `cargo tree -e features` shows `hauksbee-mcu feature "avr"` and `"default"`
  activated under those flags.

### Root cause

1. `crates/hauksbee-server/Cargo.toml`: the `hauksbee-mcu` dependency is declared
   **without** `default-features = false`, so it pulls mcu's default feature set
   (`avr, renode, qemu`).
2. `crates/hauksbee-server/src/engine.rs`: `hauksbee_mcu::AvrMcu::…` is used
   **unconditionally** (no `#[cfg(feature = "avr")]`), so the server crate does
   not even compile without `avr`.
3. Every shipped binary depends on `hauksbee-server` (`hauksbee-engine` and
   `hauksbee-ci` both), so `avr` is dragged into every build regardless of the
   command-line feature flags — cargo cannot override a dependency's
   `default-features` from the CLI.

`hauksbee-engine` itself is correct (it declares `hauksbee-mcu = { …,
default-features = false }`); the leak is entirely via the `server` crate, plus
`hauksbee-ci`'s own `hauksbee-engine`/`hauksbee-server` deps not disabling
defaults.

### The fix (source change — out of scope for release automation)

Three files, no logic changes beyond feature-gating:

- `crates/hauksbee-server/Cargo.toml`: `hauksbee-mcu = { …, default-features =
  false }`; add a `[features]` table forwarding `avr`/`renode`/`qemu` to
  `hauksbee-mcu/*` (and a `default` that does **not** include `avr`).
- `crates/hauksbee-server/src/engine.rs`: gate the `AvrMcu` construction behind
  `#[cfg(feature = "avr")]` (and provide a non-avr code path / error).
- `crates/hauksbee-ci/Cargo.toml`: `default-features = false` on its
  `hauksbee-engine` and `hauksbee-server` deps, plus forwarding features so
  `--features renode,qemu` resolves against the `hauksbee-ci` package (today it
  errors: *"none of the selected packages contains these features"*).

This touches product source and changes the default build/runtime behaviour
(default builds and the Docker `full`/`slim` images currently rely on `avr`
being on), so it belongs to a source-owning change, not this
release-automation work.

### How the workflow behaves until the fix lands

Both `release.yml` and `ci.yml` fail **fast and loudly** rather than ship a
mislabeled GPL binary: they point `SIMAVR_INCLUDE_DIR`/`SIMAVR_LIB_DIR` at a
nonexistent path. A genuinely avr-free build ignores them; a leaky one panics in
`build.rs`. So a red release/CI today means "the GPL-free shape is not clean
yet", and no GPL-encumbered binary is ever built or attached. The moment the
source fix lands, the guard passes and releases flow with **zero** system
dependencies (no simavr, no libclang/bindgen — those are only reached by the
avr path).

## 3. Naming contract with `scripts/get-hauksbee.sh`

The installer and `scripts/bundle.sh` agree on this exact shape (do not drift):

| Piece            | Value                                             |
| ---------------- | ------------------------------------------------- |
| version          | tag with leading `v` stripped (e.g. `0.1.0`)      |
| target suffix    | `linux-x86_64`, `linux-aarch64`, `darwin-arm64`, `darwin-x86_64` |
| asset base name  | `hauksbee-<version>-<suffix>`                      |
| tarball          | `<base>.tar.gz`                                    |
| checksum         | `<base>.tar.gz.sha256` (relative basename, `shasum -a 256` format) |
| tarball layout   | `<base>/bin/hauksbee` and `<base>/bin/hauksbee-ci` (mode 0755) |

`get-hauksbee.sh` was extended in this change to accept all four suffixes; it
previously rejected `linux-aarch64` and `darwin-x86_64` because the old matrix
built only two targets.

## 4. aarch64-linux mechanism

Built **natively** on GitHub's `ubuntu-24.04-arm` runner (and Intel macOS on
`macos-13`). No `cross`, no QEMU userland emulation. This matches the honest,
tested, no-cross-compilation philosophy the release workflow has always stated:
every artifact is produced on its own architecture, so nothing ships that no
runner actually executed.

## 5. Windows — evaluated, NOT promised

Per §6, Windows is evaluated as an investigation, not a shipping target:

- **simavr (avr backend)**: builds on Windows in principle (MSYS2/MinGW), and the
  FFI in `hauksbee-mcu` is not inherently POSIX-only, but it is untested here and
  would need a Windows simavr build recipe + bindgen/libclang on Windows. Moot
  for release binaries anyway, since the shipping shape is avr-free.
- **Renode**: ships Windows builds, so the renode backend is plausible; the
  discovery paths in `install-sims.sh` are POSIX-shaped and would need Windows
  equivalents.
- **QEMU (Espressif fork)**: Windows binaries exist; same discovery-path caveat.
- **Installer**: `get-hauksbee.sh` is bash; Windows would need a PowerShell
  installer and `.zip` assets (no `tar.gz`/`shasum` assumptions).
- **Toolchain**: MSVC vs GNU target, path handling, and CRLF concerns are all
  unexercised.

Recommendation: defer Windows to its own tracked task with a real Windows runner
in the matrix and a PowerShell installer; do not add it to the promised targets
until a persona-style run passes on Windows.
