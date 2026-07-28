# Release automation & binary licensing

Companion note to `.github/workflows/release.yml`. Covers the licensing
decision for prebuilt binaries, the feature-graph guard that keeps the
permissive build honest, the naming contract with the installer, the
aarch64-linux build mechanism, and the Windows status.

## 1. The licensing decision: ship both shapes, label both

hauksbee's source is Apache-2.0, and the `NOTICE` file rides with every
redistribution; that is the attribution mechanism. Its optional `avr` co-sim
backend **statically links libsimavr, which is GPL-3.0**. Statically linking
GPL code makes the *binary* a combined work: distributing it puts that download
under GPL-3.0, even though the source stays Apache-2.0.

**Decision: every release publishes both shapes, each labelled for what it is.**

| Download | Build | Binary licence | Who it is for |
| --- | --- | --- | --- |
| `hauksbee-<ver>-<suffix>.tar.gz` | default features (`avr` + `renode` + `qemu`), links libsimavr | **GPL-3.0** | Everyone installing the tool to use it. What `get-hauksbee.sh` fetches by default, what the README points at, and what `Hauksbee.app` contains |
| `hauksbee-<ver>-<suffix>-permissive.tar.gz` | `--no-default-features --features renode,qemu` | **Apache-2.0** | Redistributors, embedders, OEMs, anyone putting hauksbee inside something they ship. `get-hauksbee.sh --permissive` |

### Why the GPL shape is the default

The earlier position here was that the GPL-free shape was the only thing
shipped. That optimised for the wrong person.

- **The default download should serve the biggest funnel.** AVR / ATmega is the
  Arduino path: the largest, loudest, most bloggable slice of the audience. A
  one-line `curl | bash` install whose binary cannot simulate an ATmega328P is
  an install that fails the most common first thing anyone tries.
- **GPL-3.0 constrains distribution, not use.** For the person who downloads a
  tool and runs it on their board, commercial work included, a GPL binary
  carries no obligation whatsoever. The obligation lands only on someone who
  redistributes or embeds it.
- **The people that obligation actually lands on are a small, identifiable
  set**, and they are a licensing conversation rather than a `curl | bash`. They
  get a first-class artefact built for them, on every release, from the same
  commit: the `-permissive` tarball.
- **Nothing is implied.** Each tarball carries a `LICENSE-BINARY.txt` written by
  `scripts/bundle.sh` naming its licence in the first line, plus the
  corresponding-source pointers GPL-3.0 section 6 requires (this repo at the
  exact build commit, simavr at its pinned tag, the two scripts that reproduce
  the build, and a copy of the GPL-3.0 text). The installer prints the licence
  of what it just installed. The README states the two-download story where the
  download is offered.

Building from source is unchanged and needs no decision from anyone: a plain
`cargo build` still gives the default (AVR-capable) shape, and
`scripts/install-sims.sh --avr` still builds simavr on the user's own machine,
which is their combination rather than something this project distributed.

Both shapes are built on every matrix leg of `release.yml`, and the release
fails if either is not what its label claims. A silently-AVR-less "default"
download fails the build exactly as hard as a GPL leak into the permissive one,
because both are the download lying about itself. Two independent checks, for
the reason given in section 2.1.

## 2. The avr feature leak (found, fixed) and the standing GPL guard

This section is about the **permissive** shape only. The default shape is
supposed to link libsimavr; the guard below exists so that the permissive shape
cannot, by accident, do the same thing and still be labelled Apache-2.0.

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

- The permissive release binaries report `avr  disabled  compiled out` from
  `hauksbee doctor`, with the renode and qemu backends present. (The original
  `nm`-based measurement of this is no longer reproducible; see 2.1 for why,
  and for what replaced it.)
- The permissive shape builds with simavr discovery pointed at a nonexistent
  path (the guard below); it never even looks for libsimavr.
- Default-shape release binaries still link simavr (AVR available as before),
  and the full default test suites stay green.

### The standing GPL guard (expected green)

`ci.yml`, and the **permissive build step only** in `release.yml`, point
`SIMAVR_INCLUDE_DIR`/`SIMAVR_LIB_DIR` at a nonexistent path. A genuinely
avr-free build never reads them; any future feature-graph regression that drags
`avr` back in panics in `build.rs` **before any GPL code is linked**, turning
the release/CI red. A red guard means "fix the feature graph", never "weaken
the guard".

The guard's job is unchanged by the two-shape decision; only its scope is now
explicit. The default build step in `release.yml` deliberately does *not* set
those variables: it points them at the real prefix that
`scripts/install-sims.sh --avr` installed into, because that build is meant to
link simavr. The permissive shape still needs **zero** system dependencies (no
simavr, no libclang/bindgen, those are only reached by the avr path); the
default shape needs libsimavr, libelf, zlib and libclang, which the release
workflow installs per runner OS before building it.

Two build steps, two shapes, one commit.

### 2.1 Why the shape check is behavioural, not a symbol scan

The obvious check is `nm | grep simavr`. It does not work here, and it fails in
the direction that matters: silently passing.

The workspace sets `strip = true` in `[profile.release]`, so rustc strips at
link time. Run `nm` on a release binary of *either* shape and you get zero
simavr and zero `AvrMcu` symbols. Measured on darwin-arm64, on freshly linked
binaries, before `bundle.sh`'s own `strip` step ever runs. A symbol-table check
would therefore report "clean" for a GPL-linked binary, which is worse than no
check at all. (The 14-plus-42 symbol counts recorded above date from before the
release profile stripped, and cannot be reproduced on a current release build.)

So the check asks the binary what it can do. `hauksbee doctor` reports the
co-sim backends the build can actually reach, and its `avr` line is decided at
compile time:

| Shape | `hauksbee doctor` avr line |
| --- | --- |
| default | `avr` `builtin` `simavr linked into this binary` |
| permissive | `avr` `disabled` `compiled out; rebuild with the default features + libsimavr` |

`scripts/bundle.sh` runs that check on every build and **refuses to package**
a bundle whose answer does not match its `--shape`, in either direction. It
lives in `bundle.sh` rather than only in the workflow so a local build gets the
same guarantee as a release build, and so no tarball can exist without having
passed.

`release.yml` then re-runs the same check independently on the extracted
tarball contents (what is actually about to be uploaded, not what happened to be
in `target/release`), greps each `LICENSE-BINARY.txt` for its licence string so
the two labels cannot be swapped, and checks that `LICENSE`, `NOTICE` and, for
the default shape, `LICENSE-GPL-3.0.txt` are present and non-empty.

## 3. Naming contract with `scripts/get-hauksbee.sh`

The installer and `scripts/bundle.sh` agree on this exact shape (do not drift):

| Piece            | Value                                             |
| ---------------- | ------------------------------------------------- |
| version          | tag with leading `v` stripped (e.g. `0.1.0`)      |
| target suffix    | `linux-x86_64`, `linux-aarch64`, `darwin-arm64`, `darwin-x86_64` |
| asset base name (default shape) | `hauksbee-<version>-<suffix>`               |
| asset base name (permissive shape) | `hauksbee-<version>-<suffix>-permissive` |
| tarball          | `<base>.tar.gz`                                    |
| checksum         | `<base>.tar.gz.sha256` (relative basename, `shasum -a 256` format) |
| tarball layout   | `<base>/bin/hauksbee` and `<base>/bin/hauksbee-ci` (mode 0755) |
| licence file     | `<base>/LICENSE-BINARY.txt`, plus `<base>/LICENSE` and `<base>/NOTICE`; the default shape also carries `<base>/LICENSE-GPL-3.0.txt` |
| macOS app        | `<base>-app.zip` + `<base>-app.zip.sha256` (darwin suffixes only, default shape only; contains `Hauksbee.app`, built by `app/macos/build-app.sh`; unsigned, see `app/macos/SIGNING.md`) |

The `-permissive` suffix is part of the base name, so it appears in the tarball
name, the checksum name **and** the directory inside the tarball. Both
`scripts/bundle.sh --shape permissive` and `get-hauksbee.sh --permissive`
derive it the same way; do not let them drift.

`LICENSE-BINARY.txt` is the per-download licence statement, generated by
`bundle.sh` from the shape being built. Its first line names the licence
(`UNDER GPL-3.0` or `UNDER APACHE-2.0`), and the release workflow greps for
exactly those strings before publishing, so a shape whose label was swapped
never reaches a release page.

`get-hauksbee.sh` accepts all four suffixes; it previously rejected
`linux-aarch64` and `darwin-x86_64` because the old matrix built only two
targets. It installs the default shape unless given `--permissive`, and prints
the licence of what it installed as the last line of its summary.

`scripts/get-hauksbee.ps1` is the Windows counterpart, holding the same
contract with a `windows-x86_64` suffix and `.zip` + `.zip.sha256` assets
(`Get-FileHash` for verification, `bin\hauksbee.exe` + `bin\hauksbee-ci.exe`
inside). It exists ahead of any published Windows asset so the contract cannot
drift when that leg lands; until then its download step honestly 404s, and
Windows remains not a promised target (section 5).

The full asset list for a release of version `V`, 20 files:

- 4 targets x `hauksbee-V-<suffix>.tar.gz` + `.sha256`
- 4 targets x `hauksbee-V-<suffix>-permissive.tar.gz` + `.sha256`
- 2 darwin targets x `hauksbee-V-<suffix>-app.zip` + `.sha256`

## 4. aarch64-linux mechanism

Built **natively** on GitHub's `ubuntu-24.04-arm` runner (and Intel macOS on
`macos-13`). No `cross`, no QEMU userland emulation. This matches the honest,
tested, no-cross-compilation philosophy the release workflow has always stated:
every artifact is produced on its own architecture, so nothing ships that no
runner actually executed.

## 5. Windows, evaluated, NOT promised

Windows is close but unproven on real hardware, so it is not a shipping target
and the installer refuses to pretend otherwise. Measured baseline (updated
2026-07-28; cross-compiled `x86_64-pc-windows-gnu` from macOS with mingw-w64,
exercised under Wine 9.0):

**Verified under Wine today** (every claim below was executed, not assumed):

- **The permissive shape cross-compiles clean**: zero errors, zero warnings,
  all three binaries (`hauksbee.exe`, `hauksbee-ci.exe`, `hauksbee-mcp.exe`).
  The engine and CI binaries are pure Rust. `hauksbee-mcp` now links the
  vendored QuickJS (C, MIT) via `rquickjs`, so cross-building IT needs a C
  cross-compiler and bindgen headers:
  `CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc`,
  `AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar`, and
  `BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu` pointing `--sysroot` and
  `-I` at the mingw sysroot include dir. On a native MSVC toolchain none of
  that applies; QuickJS builds with the platform compiler.
- **The CLI does real work**: `doctor`; `run --check` (bind report, DRC, lint,
  signal integrity) in both human and `--json` form, and the JSON is clean
  (LF-only, no CRLF, no unescaped backslash paths); `to-code` / `from-code` /
  `check-code` round trip including a transient solve.
- **`hauksbee-ci` behaves**: a green spec exits 0, a failing assertion exits
  1, a missing spec exits 2, and `--junit` writes well-formed XML in all
  shapes (verified by parsing it back).
- **`hauksbee-mcp` speaks MCP over stdio**: initialize handshake, tools/list,
  and a real `analyze_board` call, with both LF and CRLF framed input;
  output is LF-only JSON-RPC lines either way.
- **`serve` carries the full web surface**, driven from a real host browser
  against the Wine server: board upload through the actual UI, the full
  report, the checks panel running a spec end to end (the server stages
  uploads and shells the sibling `hauksbee-ci.exe`), a live transient session
  over WebSocket, and the honest refusal paths (firmware co-sim without a
  backend returns the discovery guidance; dependency auto-install refuses on
  Windows with manual instructions).
- **Discovery knows the Windows conventions**: `Renode.exe` in the
  `%USERPROFILE%\renode-portable` zip layout (verified resolving under Wine),
  plus `%ProgramFiles%\Renode` and `%LOCALAPPDATA%\Programs\Renode`;
  Espressif QEMU under `%USERPROFILE%\.hauksbee-qemu-esp`, `IDF_TOOLS_PATH`,
  `%USERPROFILE%\.espressif` and `C:\Espressif` (the ESP-IDF Windows
  installer default), with `.exe` names and `.exe`-aware PATH search. The
  layout logic is platform-neutral and unit-tested in the native suite
  (`crates/hauksbee-mcu`, `discovery_tests`), so a regression shows up
  without a Windows machine.
- **Emulator children die with their owner**: co-sim children are spawned
  into their own process group and tree-killed on teardown, and a
  SIGTERM/SIGINT to the owning process reaps them via a signal-safe registry
  (`hauksbee_mcu::children`). On Windows the teardown path is
  `taskkill /T /F`; making a hard `TerminateProcess` of the parent cascade
  needs a Job object, which is still native-Windows work.

**Still needs a native Windows runner** (Wine cannot prove these):

- The test suite itself, console/TTY behaviour, real NTFS semantics,
  Defender, and anything timing-sensitive.
- Co-sim end to end with the real Renode and Espressif QEMU Windows builds
  (discovery is taught and tested; an actual firmware run is not).
- The `windows-msvc` target, which is what a real port should ship (no mingw
  runtime DLLs, the native toolchain on `windows-latest`). `windows-gnu` is
  the cross-check target from mac/Linux only. Note the pinned toolchain:
  `rustup target add` must be applied to the toolchain from
  `rust-toolchain.toml`, not whichever is globally active.
- A real installer run: `scripts/get-hauksbee.ps1` exists, follows the
  section 3 naming contract with a `windows-x86_64` suffix and `.zip` +
  `.sha256` assets, and was validated with PowerShell 7 on macOS (parses
  clean; full download-verify-extract-install dry run against a mock release
  server, including the checksum-mismatch refusal). It has not run on a real
  Windows box, and there are no published Windows release assets for it to
  fetch yet.
- **simavr (avr backend)**: a Windows avr build would need an MSYS2 simavr
  recipe plus bindgen, which nobody has written. Confirmed concretely: the
  default-shape cross-build stops in `hauksbee-mcu/build.rs` because
  pkg-config cannot provide libelf/simavr for the target. A first Windows
  release would therefore ship the permissive shape only, and would have to
  say so; the default (AVR-bearing) shape is a later step, not part of the
  port.

Windows stays off the promised-targets list until the port below exists and a
Windows runner keeps it green; a target nobody tests is a target that silently
breaks, and this project does not ship silent breakage.

### Want it? Point a coding agent at it

Nobody here has a Windows machine in the loop, which makes this a good
first-contribution shape: it is self-contained, the definition of done is
mechanical, and an agent can drive most of it. If you have Windows and an agent
(Claude Code, Codex, or similar), this prompt is a working starting point:

> Port hauksbee (github.com/ETM-Code/hauksbee) to Windows. Clone it, install a
> stable MSVC Rust toolchain (add the target to the toolchain rust-toolchain.toml
> pins), and confirm `cargo build --workspace --no-default-features --features
> renode,qemu` compiles; the cross-compile baseline in section 5 says it should,
> cleanly. The real work starts at making `cargo test --workspace
> --no-default-features --features renode,qemu` green on a native runner,
> keeping any platform change `cfg`-gated or platform-neutral rather than
> forking behaviour.
> Install Renode from its Windows portable zip and the Espressif QEMU Windows
> binaries; discovery already knows the Windows-conventional locations
> (`crates/hauksbee-mcu`, see the `discovery_tests` modules), so `hauksbee
> doctor` should find both once unpacked, and any location it misses is a bug
> to fix in those candidate lists, not to work around. Then prove firmware
> co-simulation end to end: `hauksbee run
> testdata/boards/stm32_bluepill_demo.kicad_pcb --firmware
> testdata/firmware/stm32_blinky/blinky.elf --headless --seconds 1` must report
> real net activity. Prove the web front door natively: `hauksbee serve` and a
> board drop in a browser (the same flow is already Wine-verified, so a native
> failure is a real finding). Run `scripts/get-hauksbee.ps1` against a real
> release once Windows assets exist, or against a mock (`HAUKSBEE_API_BASE` /
> `HAUKSBEE_RELEASES_BASE` overrides). Where a test is genuinely POSIX-only,
> gate it with `#[cfg(unix)]` and say why, never skip silently; hauksbee
> treats a test that cannot fail as worse than no test. Keep a log of every
> divergence you had to handle.

### If you get it working, ship it as a PR

A port that lives on one machine is a port the next Windows user has to redo,
so please upstream it. One PR is fine. It should contain:

1. The code changes, `cfg`-gated where platform-specific.
2. Any fixes `scripts/get-hauksbee.ps1` needed on a real Windows box (the
   script exists and follows the section 3 naming contract with a
   `windows-x86_64` suffix and `.zip` + `.sha256` assets, but has only been
   dry-run against a mock from macOS).
3. A CI matrix entry (`windows-latest`) running fmt, clippy, and the test suite
   in the permissive shape, green.
4. The divergence log, as the PR description: what was POSIX-shaped, what you
   changed, what you gated and why. Evidence beats assertion; paste the co-sim
   output and a screenshot of the web front door.

Maintainer side, staged so nothing is promised before it is enforced: the CI
entry lands first and has to stay green on its own for a release cycle; then
`release.yml` grows the Windows matrix leg and the installer learns the new
suffix; only then does Windows join the promised targets in the README. The
`#[cfg(unix)]` gates from step 4 become the checklist for closing the gap to
full parity.
