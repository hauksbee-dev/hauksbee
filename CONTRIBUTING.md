# Contributing to hauksbee

Thanks for looking. This document covers getting a build, running the tests
(including the parts that need boards we cannot ship), and what a change is
expected to clear before it lands.

## Getting a build

```bash
git clone https://github.com/ETM-Code/hauksbee
cd hauksbee
cargo build --workspace
```

The toolchain is pinned in `rust-toolchain.toml`, so rustup will fetch the right
version automatically. No system dependencies are needed for the default build.

Optional co-simulation backends (simavr for AVR, Renode for ARM, QEMU for
Espressif) are detected at runtime, not required to build. `hauksbee doctor`
reports which ones this machine has, and the web UI offers one-click installs
for the missing ones. See `docs/cosim/SIMULATORS.md`.

**Licensing note:** the `avr` feature links the GPL `libsimavr`, so the default
build shape is GPL-encumbered. The MIT-clean shape is
`--no-default-features --features renode,qemu`, and CI asserts that it stays
genuinely avr-free rather than merely compiling. See
`docs/about/release-and-licensing.md` before touching the feature graph.

## Running the tests

```bash
cargo test --workspace                    # everything that runs anywhere
cargo test --workspace --features avr     # adds the AVR co-simulation suites
```

Some tests need real boards. See below.

### The board corpus

hauksbee's central claim is that its checks produce zero false positives across
a large corpus of real hardware. That corpus is mostly public open hardware, and
you can reproduce it:

```bash
scripts/fetch-corpus.sh                   # fetches into ./board-corpus
export HAUKSBEE_CORPUS_DIR=$PWD/board-corpus
cargo test --workspace --features avr
```

Each board is fetched from its upstream project at a pinned commit and verified
against a checksum, so the corpus you get is the corpus the gate was measured
against. The boards are **not** vendored into this repository: they carry
CC BY-SA, GPL-3.0, and CERN-OHL licences, and fetching means you obtain each one
from its author under that author's terms rather than through us.

Without the corpus, corpus-dependent tests report as ignored, never as passed.
If you want a missing corpus to be a hard failure instead (recommended in CI):

```bash
HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace --features avr
```

A handful of boards cannot be redistributed at all and are absent from the
manifest. Nothing in the public test suite depends on them.

## What a change has to clear

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same three. Beyond that:

**A check that fires must be right.** The corpus gate exists because a hardware
tool that cries wolf gets switched off, and a switched-off tool catches nothing.
If you add or widen a check, run it across the corpus and show it stays quiet on
boards that are fine. A new check that lights up on healthy boards will not land
however elegant it is, and "it found a real issue on one board" does not excuse
noise on twenty others.

**Never report a result you cannot stand behind.** The codebase separates "this
failed" from "this could not be answered": a run that cannot produce a
meaningful answer exits 3 rather than 0, and a report that would be misleading
says so instead of rendering. If your change can produce a number that might be
wrong, make it say so.

**Tests must be able to fail.** A test that returns early when a fixture is
missing, and so reports success having checked nothing, is worse than no test.
Use `#[ignore]` with a reason if something is genuinely optional.

**Comments explain why.** The what is in the code. Existing comments record the
reasoning and the failure that motivated a piece of design; match that. House
style has no em dashes.

## Where things live

`docs/START_HERE.md` is the entry point and `docs/DOCS_MAP.md` indexes the rest.
The short version:

| Crate | Job |
|---|---|
| `hauksbee-extract` | Turn a PCB, schematic, netlist, gerber, or Altium file into a circuit |
| `hauksbee-models` | Resolve parts to device models |
| `hauksbee-ir` | Circuit representation and the SPICE front-end |
| `hauksbee-solve` | Sparse MNA solver: DC, transient, AC |
| `hauksbee-mcu` | Emulator backends for firmware co-simulation |
| `hauksbee-engine` | Binds it together; the checks, the reports, the CLI |
| `hauksbee-ci` | Declarative TOML specs and assertions for pipelines |
| `hauksbee-server` | The web front door |

## Reporting bugs

A board file that reproduces the problem is worth more than any description. If
you cannot share the board, a minimal reconstruction is the next best thing, and
`hauksbee to-code <board>` produces an editable text form that is often small
enough to paste.

For security issues, see `SECURITY.md`; please do not open a public issue.
