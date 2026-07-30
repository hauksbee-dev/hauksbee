# Contributing to hauksbee

Thanks for looking. This document covers getting a build, running the tests
(including the parts that need boards we cannot ship), and what a change must
clear before it lands.

## Getting a build

```bash
git clone https://github.com/hauksbee-dev/hauksbee
cd hauksbee
cargo build --workspace
```

`rust-toolchain.toml` pins the toolchain version, so rustup fetches the right
one automatically. The default build needs no system dependencies.

hauksbee detects optional co-simulation backends (simavr for AVR, Renode for
ARM, QEMU for Espressif) at runtime. The build does not need them. `hauksbee
doctor` reports which ones this machine has, and the web UI offers one-click
installs for the missing ones. See `docs/cosim/SIMULATORS.md`.

**Licensing note:** hauksbee is Apache-2.0, and the `NOTICE` file must ride
along with any redistribution. The `avr` feature links the GPL `libsimavr`, so
the default build shape is GPL-encumbered. The GPL-free shape is
`--no-default-features --features renode,qemu`. CI checks that this shape
stays genuinely avr-free rather than merely compiling. Read
`docs/about/release-and-licensing.md` before you touch the feature graph.

## Running the tests

```bash
cargo test --workspace                    # everything that runs anywhere
cargo test --workspace --features avr     # adds the AVR co-simulation suites
```

Some tests need real boards. See below.

### The board corpus

hauksbee's central claim is that its checks produce zero false positives across
a large corpus of real hardware. That corpus is mostly public open hardware.
You can reproduce it:

```bash
scripts/fetch-corpus.sh                   # fetches into ./board-corpus
export HAUKSBEE_CORPUS_DIR=$PWD/board-corpus
cargo test --workspace --features avr
```

The fetch script pulls each board from its upstream project at a pinned commit
and checks it against a checksum, so the corpus you get is the corpus the gate
was measured against. This repository does **not** vendor the boards. They
carry CC BY-SA, GPL-3.0, and CERN-OHL licences, and fetching means you get
each one from its author under that author's terms rather than through us.

Expect a partial fetch. Around 23 of the 28 boards land on a good day: one
upstream hangs, one moved the revision its pin names, and two are skipped by
default because their licence could not be confirmed. That is fine and it is
not a broken checkout. A corpus test whose boards are absent skips and names
what is missing, with the `--only <id>` line that fetches it, rather than
failing in a way that reads as hauksbee being broken.

Without the corpus at all, corpus-dependent tests report as ignored, never as
passed. To make a missing or thin corpus a hard failure instead (recommended in
CI):

```bash
HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace --features avr
```

A handful of boards carry no redistribution rights at all, so they are absent
from the manifest. Nothing in the public test suite depends on them.

## What a change has to clear

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

CI runs the same four checks. `cargo doc` renders the module headers as
documentation, so `cargo doc --open` is how you read them the way a reader
will. Beyond that:

**A check that fires must be right.** The corpus gate exists because a hardware
tool that cries wolf gets switched off, and a switched-off tool catches nothing.
If you add or widen a check, run it across the corpus and show it stays quiet on
boards that are fine. A new check that lights up on healthy boards will not
land, however well-built it is. "It found a real issue on one board" does not
excuse noise on twenty others.

**Never report a result you cannot stand behind.** The codebase separates "this
failed" from "this could not be answered". A run that cannot produce a
meaningful answer exits 3 rather than 0, and a report that would be misleading
says so instead of rendering. If your change can produce a number that might be
wrong, make it say so.

**Tests must be able to fail.** A test that returns early when a fixture is
missing, and so reports success having checked nothing, is worse than no test.
Use `#[ignore]` with a reason if something is genuinely optional.

**Comments explain why.** The what is in the code. Existing comments record the
reasoning and the failure that motivated a piece of design. Match that style.
House rule: no em dashes.

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
| `hauksbee-engine` | Binds it together: the checks, the reports, the CLI |
| `hauksbee-ci` | Declarative TOML specs and assertions for pipelines |
| `hauksbee-server` | The web front door |

## The CLA

First-time contributors sign a contributor licence agreement (`CLA.md`) by
replying to a bot comment on their first pull request. It takes one sentence
and one click. Short version: you keep your copyright, the project gets a
licence broad enough to keep its options open, and the document states the
reasoning plainly rather than burying it in legalese.

## Reporting bugs

A board file that reproduces the problem is worth more than any description. If
you cannot share the board, a minimal reconstruction is the next best thing.
`hauksbee to-code <board>` produces an editable text form that is often small
enough to paste.

For security issues, see `SECURITY.md`. Do not open a public issue.

`CODE_OF_CONDUCT.md` covers participation.
