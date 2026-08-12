# Release automation & binary licensing

Companion note to `.github/workflows/release.yml`. Covers the licensing
decision for prebuilt binaries, the feature-graph guard that keeps the
permissive build honest, the naming contract with the installer, the
aarch64-linux build mechanism, the Windows status, the open-core commitment,
and the provenance of the Altium ingest.

For the short answer without the reasoning, one row per artifact, see
[`COMPLIANCE.md`](../../COMPLIANCE.md) at the repository root.

## 1. The licensing decision: ship both shapes, label both

hauksbee's source is Apache-2.0, and the `NOTICE` file rides with every
redistribution. That is the attribution mechanism. Its optional `avr` co-sim
backend **statically links libsimavr, which is GPL-3.0**. Statically linking
GPL code makes the *binary* a combined work: distributing it puts that download
under GPL-3.0, even though the source stays Apache-2.0.

**Decision: every release publishes both shapes, each labelled for what it is.**

| Download | Build | Binary licence | Who it is for |
| --- | --- | --- | --- |
| `hauksbee-<ver>-<suffix>.tar.gz` | default features (`avr` + `renode` + `qemu`), links libsimavr | **GPL-3.0** | Everyone installing the tool to use it. What `get-hauksbee.sh` fetches by default, what the README points at, and what `Hauksbee.app` contains |
| `hauksbee-<ver>-<suffix>-permissive.tar.gz` | `--no-default-features --features renode,qemu` | **Apache-2.0** | Redistributors, embedders, OEMs, anyone putting hauksbee inside something they ship. `get-hauksbee.sh --permissive` |

### Why the GPL shape is the default

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
  get a dedicated artifact built for them, on every release, from the same
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
supposed to link libsimavr. The guard below exists so that the permissive shape
cannot, by accident, do the same thing and still be labeled Apache-2.0.

When release automation first landed, the shape `--no-default-features
--features renode,qemu` **still activated `avr` and still linked libsimavr**.
This was verified, not assumed: with simavr discovery pointed at a nonexistent
path the build panicked in `crates/hauksbee-mcu/build.rs:48` (*"the `avr`
co-sim feature needs a system libsimavr"*), proving the feature was active in
a shape that promised to exclude it.

### Root cause (for the next person who adds a hauksbee-* dependency)

1. `hauksbee-server` declared its `hauksbee-mcu` dependency **without**
   `default-features = false`, pulling mcu's default set (`avr, renode, qemu`).
2. `hauksbee-server/src/engine.rs` used `hauksbee_mcu::AvrMcu` unconditionally,
   so the crate could not even compile without `avr`.
3. Every shipped binary depends on `hauksbee-server`, and **cargo cannot
   override a dependency's `default-features` from the command line**, one
   missing `default-features = false` anywhere in the graph silently re-links
   GPL simavr no matter what flags the release build passes.

### The fix

- `crates/hauksbee-server/Cargo.toml`: `[features]` table (`default = ["avr",
  "renode", "qemu"]`, each forwarding to `hauksbee-mcu/*`). The mcu dep now sets
  `default-features = false`.
- `crates/hauksbee-server/src/engine.rs`: `McuDemoEngine` (the only AvrMcu user)
  is gated `#[cfg(feature = "avr")]`. `src/main.rs` (the AVR demo binary)
  compiles to an explanatory stub without `avr`. `tests/ws_roundtrip.rs` is
  gated on `avr`.
- `crates/hauksbee-engine/Cargo.toml`: the server dep sets `default-features =
  false`. `avr`/`renode`/`qemu` forward to both `hauksbee-mcu/*` and
  `hauksbee-server/*`.
- `crates/hauksbee-ci/Cargo.toml`: its own `[features]` table (so `--features
  renode,qemu` resolves against the `hauksbee-ci` package, previously a hard
  error). Engine and server deps set `default-features = false`.

**Default behaviour is unchanged**: every crate's `default` still includes
`avr`, so a plain `cargo build --workspace`, the Docker images, and the demo
server keep AVR co-sim exactly as before. Verified after the fix:

- The permissive release binaries report `avr` as `disabled`, "not in this
  build (the permissive, Apache-2.0 download drops the GPL simavr backend)",
  from `hauksbee doctor`, with the renode and qemu backends present.
- The permissive shape builds with simavr discovery pointed at a nonexistent
  path (the guard below). It never even looks for libsimavr.
- Default-shape release binaries still link simavr (AVR available as before),
  and the full default test suites stay green.

### The standing GPL guard (expected green)

`ci.yml`, and the **permissive build step only** in `release.yml`, point
`SIMAVR_INCLUDE_DIR`/`SIMAVR_LIB_DIR` at a nonexistent path. A genuinely
avr-free build never reads them. Any future feature-graph regression that drags
`avr` back in panics in `build.rs` **before any GPL code is linked**, turning
the release/CI red. A red guard means "fix the feature graph," never "weaken
the guard."

The guard's job is unchanged by the two-shape decision. Only its scope is now
explicit. The default build step in `release.yml` deliberately does *not* set
those variables: it points them at the real prefix that
`scripts/install-sims.sh --avr` installed into, because that build is meant to
link simavr. The permissive shape still needs **zero** system dependencies (no
simavr, no libclang/bindgen, those are only reached by the avr path). The
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
check at all.

So the check asks the binary what it can do. `hauksbee doctor` reports the
co-sim backends the build can actually reach, and its `avr` line is decided at
compile time:

| Shape | `hauksbee doctor` avr line |
| --- | --- |
| default | `avr` `builtin` "simavr linked into this binary" |
| permissive | `avr` `disabled` "not in this build (the permissive, Apache-2.0 download drops the GPL simavr backend). For AVR co-sim, install the default (GPL-3.0) download instead, or build from source with libsimavr (scripts/install-sims.sh --avr)" |

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
| tarball layout   | `<base>/bin/hauksbee`, `<base>/bin/hauksbee-ci` and `<base>/bin/hauksbee-mcp` (three binaries, mode 0755) |
| licence file     | `<base>/LICENSE-BINARY.txt`, plus `<base>/LICENSE` and `<base>/NOTICE`. The default shape also carries `<base>/LICENSE-GPL-3.0.txt` |
| macOS app        | `<base>-app.zip` + `<base>-app.zip.sha256` (darwin suffixes only, default shape only, contains `Hauksbee.app`, built by `app/macos/build-app.sh`, signed and notarized, see `app/macos/SIGNING.md`) |
| Windows          | `hauksbee-<version>-windows-x86_64-permissive.zip` + `.zip.sha256`; contains the same three binaries with `.exe` suffixes and an Apache-2.0 `LICENSE-BINARY.txt` |

The `-permissive` suffix is part of the base name, so it appears in the tarball
name, the checksum name **and** the directory inside the tarball. Both
`scripts/bundle.sh --shape permissive` and `get-hauksbee.sh --permissive`
derive it the same way. Do not let them drift.

`LICENSE-BINARY.txt` is the per-download licence statement, generated by
`bundle.sh` from the shape being built. Its first line names the licence
(`UNDER GPL-3.0` or `UNDER APACHE-2.0`), and the release workflow greps for
exactly those strings before publishing, so a shape whose label was swapped
never reaches a release page.

`get-hauksbee.sh` accepts all four suffixes. It previously rejected
`linux-aarch64` and `darwin-x86_64` because the old matrix built only two
targets. It installs the default shape unless given `--permissive`, and prints
the licence of what it installed as the last line of its summary.

`scripts/get-hauksbee.ps1` is the Windows counterpart, holding the same
contract with a `windows-x86_64` suffix and `.zip` + `.zip.sha256` assets
(`Get-FileHash` for verification and all three `.exe` binaries inside). Windows
has one shape rather than two: the installer always selects `-permissive`, says
that AVR is disabled before download, and the bundle refuses any binary whose
`doctor` output does not say the same thing.

The full asset list for a release of version `V`, 24 files:

- 4 targets x `hauksbee-V-<suffix>.tar.gz` + `.sha256`
- 4 targets x `hauksbee-V-<suffix>-permissive.tar.gz` + `.sha256`
- 2 darwin targets x `hauksbee-V-<suffix>-app.zip` + `.sha256`
- 1 Windows target x `hauksbee-V-windows-x86_64-permissive.zip` + `.sha256`
- 1 platform-neutral KiCad PCM zip + `.sha256`

### 3.1 macOS signing, stated plainly

The claim the user-facing docs make, and the one this workflow enforces:
every macOS release binary is signed with a Developer ID identity.
`Hauksbee.app` is signed and notarised with the ticket stapled, and the
release workflow refuses to publish an app zip that is not, so the app opens
on a double-click with no Gatekeeper warning. The tarball binaries are signed
too, and notarised from launch onward; a bare command-line binary cannot
carry a stapled ticket, so Gatekeeper confirms the notarisation online on
first run, and a tarball fetched through a browser opens cleanly. Only a
pre-release or locally built unsigned bundle still needs the one-time `xattr
-d com.apple.quarantine` fallback on the installed binaries, while a copy
installed by `get-hauksbee.sh` never carries the quarantine flag at all.

The gates live in `release.yml`. The "Gate the app zip on signing +
notarisation (macOS only)" step runs `codesign --verify --deep --strict`,
`spctl -a -t exec`, and `xcrun stapler validate` on the `Hauksbee.app` inside
the zip, and fails the release if any of the three does. The "Verify the two
shapes" step additionally runs `codesign --verify --strict` on every binary
inside both darwin tarballs and fails the job if any is unsigned;
notarisation of those binaries is required at launch on the same terms as
the app gate. `get-hauksbee.sh` prints the quarantine fallback itself on
Darwin for pre-release bundles, so the tarball side of the story is told by
the installer as well as by the docs. Mechanics and credentials:
`app/macos/SIGNING.md`.

## 4. aarch64-linux mechanism

Built **natively** on GitHub's `ubuntu-24.04-arm` runner (and Intel macOS on
`macos-13`). No `cross`, no QEMU userland emulation. This matches the honest,
tested, no-cross-compilation philosophy the release workflow has always stated:
every artifact is produced on its own architecture, so nothing ships that no
runner actually executed.

## 5. Windows x86_64

Windows is a native release target in one deliberately narrower shape. The
`windows-latest` gate builds and tests `hauksbee`, `hauksbee-ci`, and the MCU
process layer with MSVC, builds the embedded web app, launches the release-mode
`hauksbee.exe`, drops a real Board-as-Code file through Chromium, and retains
the screenshot/report/server logs. The release job repeats the native tests,
packages all three executables, verifies the checksum and extracted contents,
and runs `doctor` from the packaged binary before upload. Both jobs install
repository-checksummed Renode and Espressif QEMU archives and reject a release
unless the exact RP2040, Xtensa and RISC-V firmware-through-emulator tests pass
without `SKIP:`. A local GNU
cross-check is useful compiler coverage; it is not substituted for this native
gate.

The platform differences are explicit:

- Windows ships only `--no-default-features --features renode,qemu`. AVR is
  disabled because the repository has no supported native MSVC libsimavr
  build. Building libsimavr under MSYS2 and proving it through the same native
  tests is the unlocking path for an AVR-enabled Windows artifact.
- Renode and Espressif QEMU discovery includes their conventional Windows
  locations. The child is created suspended, assigned to a Win32 Job Object
  with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and only then resumed; the child
  cannot execute or create a descendant before assignment. Native regressions
  cover direct close and hard parent termination after assignment with an
  immediate grandchild. The stable Rust process API cannot assign the Job
  atomically at creation: a hard kill in the narrow spawn-to-assignment window
  can leave the suspended direct child, which has not executed or created
  descendants. That bounded Windows limitation is explicit, not claimed away.
  Availability still comes from `hauksbee doctor`; a missing executable is
  named, never substituted.
- Host serial uses loopback TCP. Windows has no Unix pseudo-terminal in this
  implementation, so requesting `pty` refuses and names
  `--serial-transport tcp` as the working route.
- There is no Windows desktop shell wrapper. The supported front door is
  `hauksbee.exe serve --open` (or `--no-open` for automation), which opens the
  same embedded browser UI the native drag-and-drop gate exercises.

## 6. The core stays open

The Apache-2.0 core of hauksbee (extraction, models, solver, checks, co-sim,
CI runner, MCP server, everything in this repository) stays open, under
Apache-2.0, permanently. Anything sold around it is additive: hosting,
support, integrations. No existing capability moves behind a paywall, and no
release removes from the open core something an earlier release shipped in
it. Contributions are accepted under a CLA (`CLA.md`), and this section is
the commitment that the CLA's licensing-flexibility grant is not a lever for
closing the core.

## 7. Provenance of the Altium ingest

The `.PcbDoc` reader's binary record layouts are ported field-by-field from
KiCad's Altium importer, which is GPL-3.0 (the exact source files are named
in [`docs/ingest/ALTIUM.md`](../ingest/ALTIUM.md)). No KiCad code is copied
into this repository; what was taken is the description of the on-disk
record structures: field order, offsets, sizes, and enum meanings.

Whether a field-by-field port of record layouts makes the reader a derived
work of the GPL importer is a question this project treats as open rather
than settled. The current position is that file-format structures and
offsets are facts about Altium's format rather than KiCad's copyrightable
expression, which is why the reader ships in the Apache-2.0 core; but that
position is a legal judgment, not a certainty, and it is under legal review
before launch. If the review concludes otherwise, the reader moves to the
GPL shape or gets re-derived from the format itself. `NOTICE` carries a
KiCad attribution line for the record layouts either way.

## 8. Provenance of the vendored RP2040 platform

RP2040 co-sim is the one platform Renode does not supply, so hauksbee carries
its own, and unlike everything else in section 7 this one is *copied* rather
than ported. Both release shapes embed it, so both redistribute it:

| What | Upstream | Licence |
|---|---|---|
| Renode peripheral models (C# sources) | `matgla/Renode_RP2040`, at a pinned commit | MIT |
| `RP2040.svd` (register tags) | `raspberrypi/pico-sdk`, tag `2.1.1` | BSD-3-Clause |
| `bootrom.elf` (boot ROM image) | `raspberrypi/pico-bootrom-rp2040`, release `b2` | BSD-3-Clause |

All three are permissive, so neither shape's licence position changes: the
permissive tarball stays Apache-2.0-compatible. Two obligations follow and are
met in the tree rather than left implicit. The licence texts ship beside the
files in `crates/hauksbee-mcu/db/mcu/rp2040/`, and `NOTICE` carries the
attribution, which is what travels with a redistributed binary.

The C# sources are byte-for-byte copies and are deliberately never edited: a
fix goes upstream and the file is re-copied, so the directory cannot quietly
become a fork whose provenance line has stopped being true. The exact commit,
the tags, and the refresh procedure are in that directory's `README.md`.
