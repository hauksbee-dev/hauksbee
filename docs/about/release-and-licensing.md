# Release automation & binary licensing

Companion note to `.github/workflows/release.yml`. Covers the licensing
decision for prebuilt binaries, a known blocker that prevents that decision
from being realised today, the naming contract with the installer, the
aarch64-linux build mechanism, and the Windows status.

## 1. The licensing decision (GPL-free binaries)

hauksbee's source is Apache-2.0, and the `NOTICE` file rides with every
redistribution; that is the attribution mechanism. Its optional `avr` co-sim
backend **statically links libsimavr, which is GPL-3.0**. Statically linking
GPL code makes the *binary* a GPL derivative work: distributing it puts the
whole download under GPL-3.0, even though the source stays Apache-2.0. For a
`curl | bash` installer this is a real, irreversible consequence for the person
installing.

**Decision: release binaries ship in the GPL-free shape**: renode + qemu
backends only, no avr, no libsimavr:

```
cargo build --release --no-default-features --features renode,qemu
```

- AVR co-sim stays available to anyone who **builds from source** and runs
  `scripts/install-sims.sh --avr`, GPL simavr is then built on *their* machine
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

## 2. The avr feature leak (found, fixed) and the standing GPL guard

When release automation first landed, the shape `--no-default-features
--features renode,qemu` **still activated `avr` and still linked libsimavr**.
This was verified, not assumed: with simavr discovery pointed at a nonexistent
path the build panicked in `crates/hauksbee-mcu/build.rs:48` (*"the `avr`
co-sim feature needs a system libsimavr"*), and `nm` found 14 simavr C symbols +
42 `AvrMcu` symbols in each of `hauksbee` and `hauksbee-ci`.

### Root cause (for the next person who adds a hauksbee-* dependency)

1. `hauksbee-server` declared its `hauksbee-mcu` dependency **without**
   `default-features = false`, pulling mcu's default set (`avr, renode, qemu`).
2. `hauksbee-server/src/engine.rs` used `hauksbee_mcu::AvrMcu` unconditionally,
   so the crate could not even compile without `avr`.
3. Every shipped binary depends on `hauksbee-server`, and **cargo cannot
   override a dependency's `default-features` from the command line**, one
   missing `default-features = false` anywhere in the graph silently re-links
   GPL simavr no matter what flags the release build passes.

### The fix (landed on this branch)

- `crates/hauksbee-server/Cargo.toml`: `[features]` table (`default = ["avr",
  "renode", "qemu"]`, each forwarding to `hauksbee-mcu/*`); the mcu dep now sets
  `default-features = false`.
- `crates/hauksbee-server/src/engine.rs`: `McuDemoEngine` (the only AvrMcu user)
  is gated `#[cfg(feature = "avr")]`; `src/main.rs` (the AVR demo binary)
  compiles to an explanatory stub without `avr`; `tests/ws_roundtrip.rs` is
  gated on `avr`.
- `crates/hauksbee-engine/Cargo.toml`: the server dep sets `default-features =
  false`; `avr`/`renode`/`qemu` forward to both `hauksbee-mcu/*` and
  `hauksbee-server/*`.
- `crates/hauksbee-ci/Cargo.toml`: its own `[features]` table (so `--features
  renode,qemu` resolves against the `hauksbee-ci` package, previously a hard
  error); engine + server deps set `default-features = false`.

**Default behaviour is unchanged**: every crate's `default` still includes
`avr`, so a plain `cargo build --workspace`, the Docker images, and the demo
server keep AVR co-sim exactly as before. Verified after the fix:

- `nm` on the GPL-free release binaries: **0 simavr symbols, 0 AvrMcu symbols**
  in both `hauksbee` and `hauksbee-ci` (renode/qemu backends present).
- The GPL-free shape builds with simavr discovery pointed at a nonexistent
  path (the guard below); it never even looks for libsimavr.
- Default-features release binaries still link simavr (AVR available as before),
  and the full default test suites stay green.

### The standing GPL guard (expected green)

Both `release.yml` and `ci.yml` point `SIMAVR_INCLUDE_DIR`/`SIMAVR_LIB_DIR` at a
nonexistent path. A genuinely avr-free build never reads them; any future
feature-graph regression that drags `avr` back in panics in `build.rs` **before
any GPL code is linked**, turning the release/CI red. A red guard means "fix the
feature graph", never "weaken the guard". With the graph clean, releases flow
with **zero** system dependencies (no simavr, no libclang/bindgen, those are
only reached by the avr path).

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

## 5. Windows, evaluated, NOT promised

Windows is plausible but unexercised, so it is not a shipping target and the
installer refuses to pretend otherwise. What is actually known:

- **simavr (avr backend)**: builds on Windows in principle (MSYS2/MinGW), and the
  FFI in `hauksbee-mcu` is not inherently POSIX-only, but it is untested here and
  would need a Windows simavr build recipe + bindgen/libclang on Windows. Moot
  for release binaries anyway, since the shipping shape is avr-free.
- **Renode**: ships Windows builds, so the renode backend is plausible; the
  discovery paths in `install-sims.sh` and `scripts/doctor.sh` are POSIX-shaped
  and would need Windows equivalents.
- **QEMU (Espressif fork)**: Windows binaries exist; same discovery-path caveat.
- **Installer**: `get-hauksbee.sh` is bash; Windows would need a PowerShell
  installer and `.zip` assets (no `tar.gz`/`shasum` assumptions).
- **Toolchain**: MSVC vs GNU target, path handling, and CRLF concerns are all
  unexercised.

Windows stays off the promised-targets list until the port below exists and a
Windows runner keeps it green; a target nobody tests is a target that silently
breaks, and this project does not ship silent breakage.

### Want it? Point a coding agent at it

Nobody here has a Windows machine in the loop, which makes this a good
first-contribution shape: it is self-contained, the definition of done is
mechanical, and an agent can drive most of it. If you have Windows and an agent
(Claude Code, Codex, or similar), this prompt is a working starting point:

> Port hauksbee (github.com/ETM-Code/hauksbee) to Windows. Clone it, install a
> stable MSVC Rust toolchain, and get `cargo build --workspace
> --no-default-features --features renode,qemu` compiling; fix path handling,
> process spawning, and anything POSIX-shaped you hit, keeping changes
> `cfg`-gated or platform-neutral rather than forking behaviour. Then make
> `cargo test --workspace --no-default-features --features renode,qemu` green.
> Install Renode from its Windows portable zip and the Espressif QEMU Windows
> binaries, teach the discovery logic (`crates/hauksbee-mcu`, `scripts/doctor.sh`
> equivalents) where to find them, and prove firmware co-simulation end to end:
> `hauksbee run testdata/boards/stm32_bluepill_demo.kicad_pcb --firmware
> testdata/firmware/stm32_blinky/blinky.elf --headless --seconds 1` must report
> real net activity. Prove the web front door: `hauksbee serve` and a board drop
> in a browser. Write `scripts/get-hauksbee.ps1` mirroring the bash installer's
> naming contract from section 3 with `.zip` assets. Where a test is genuinely
> POSIX-only, gate it with `#[cfg(unix)]` and say why, never skip silently;
> hauksbee treats a test that cannot fail as worse than no test. Keep a log of
> every divergence you had to handle.

### If you get it working, ship it as a PR

A port that lives on one machine is a port the next Windows user has to redo,
so please upstream it. One PR is fine. It should contain:

1. The code changes, `cfg`-gated where platform-specific.
2. `scripts/get-hauksbee.ps1`, following the section 3 naming contract with a
   `windows-x86_64` suffix and `.zip` + `.sha256` assets.
3. A CI matrix entry (`windows-latest`) running fmt, clippy, and the test suite
   in the GPL-free shape, green.
4. The divergence log, as the PR description: what was POSIX-shaped, what you
   changed, what you gated and why. Evidence beats assertion; paste the co-sim
   output and a screenshot of the web front door.

Maintainer side, staged so nothing is promised before it is enforced: the CI
entry lands first and has to stay green on its own for a release cycle; then
`release.yml` grows the Windows matrix leg and the installer learns the new
suffix; only then does Windows join the promised targets in the README. The
`#[cfg(unix)]` gates from step 4 become the checklist for closing the gap to
full parity.
