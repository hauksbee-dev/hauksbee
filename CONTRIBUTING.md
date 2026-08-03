# Contributing to hauksbee

Thanks for looking. This document covers getting a build, running the tests
(including the parts that need boards we cannot ship), and what a change must
clear before it lands.

## Getting a build

```bash
git clone https://github.com/hauksbee-dev/hauksbee
cd hauksbee
scripts/install-sims.sh --avr    # libsimavr, needed by the default `avr` feature
cargo build --workspace
```

`rust-toolchain.toml` pins the toolchain version, so rustup fetches the right
one automatically. The default features include the AVR backend, which links
libsimavr and needs libelf, zlib, and libclang at build time;
`scripts/install-sims.sh --avr` sets that up. To build without any of it:

```bash
cargo build --workspace --no-default-features --features renode,qemu
```

The other co-simulation backends (Renode for ARM, QEMU for Espressif) are
detected at runtime, so the build does not need them. `hauksbee doctor`
reports which ones this machine has, and the web UI offers one-click installs
for the missing ones. See `docs/cosim/SIMULATORS.md`.

**Licensing note:** hauksbee is Apache-2.0, and the `NOTICE` file must ride
along with any redistribution. The `avr` feature links the GPL `libsimavr`, so
the default build shape is GPL-encumbered. The GPL-free shape is
`--no-default-features --features renode,qemu`. CI checks that this shape
stays genuinely avr-free rather than merely compiling. Read
`docs/about/release-and-licensing.md` before you touch the feature graph.

## Running the tests

```bash
cargo test --workspace                    # everything, including the AVR co-simulation suites (`avr` is a default feature)
```

Some tests need real boards. See below.

`scripts/test-install-mock.sh` proves the release installer's whole
download/verify/install flow against a local mock of GitHub's release
endpoints. It builds the real bundle, so it needs cargo and python3.

### Visual lint

```bash
cd frontend && bun run build && bun run visual-lint
```

The web UI's layout is measured in a real browser, one pass per viewport:
360x780, 768x1024 and 1440x900, plus 320x568 and 1920x600 as the stress ratios.
Five rules fire on what the layout actually did, not on what the CSS meant:

- the page never scrolls sideways (a table or code block may scroll inside its
  own `overflow-x` wrapper; the page may not),
- no button, badge, input, or image escapes the box that is supposed to hold it,
- no single-line text is cut off (deliberate `text-overflow: ellipsis`
  truncation is allowed, and reported as a note so you can see what is hidden),
- every visible image has loaded and is rendered at its natural aspect,
- nothing sticky or fixed sits on top of a control.

Violations print one line each with the surface, the viewport, the DOM path, the
rule and the measured numbers, and the offending state is screenshotted to
`frontend/test-results/visual-lint/`. CI runs the same command in the
`frontend-visual-lint` job and uploads those screenshots on a failure. It serves
`frontend/dist` with `tests/visual-lint/fixture-server.ts`, which replays real
`hauksbee serve` responses from `tests/visual-lint/fixtures/`, so no engine
build is needed; point the lint at a real server with
`HB_LINT_BASE=http://127.0.0.1:3001` when you want the round trip, and re-record
the fixtures with `tests/visual-lint/capture-fixtures.ts` when a response shape
changes.

**A new UI surface gets an array entry.** `tests/visual-lint/surfaces.ts` is the
list of what gets looked at; a surface with no entry is a surface nobody
measures. Add one, with the clicks a user would make to reach it. Never open the
viewer's 3D tab from the lint: three.js on a GPU-less CI runner wedges the page.

### The board corpus

hauksbee's central claim is that its checks produce zero false positives across
a large corpus of real hardware. That corpus is mostly public open hardware.
You can reproduce it:

```bash
scripts/fetch-corpus.sh                   # fetches into ./board-corpus
export HAUKSBEE_CORPUS_DIR=$PWD/board-corpus
cargo test --workspace
```

The fetch script pulls each board from its upstream project at a pinned commit
and checks it against a checksum, so the corpus you get is the corpus the gate
was measured against. This repository does **not** vendor the boards. They
carry CC BY-SA, GPL-3.0, and CERN-OHL licences, and fetching means you get
each one from its author under that author's terms rather than through us.

Expect a partial fetch. The manifest pins 28 boards, and around 23 or 24 land
on a good day: two are skipped by default because their licence could not be
confirmed, one upstream is prone to hanging, one has moved the revision the
manifest pins, and the occasional extra upstream flakes. That is fine and it
is not a broken checkout. A corpus test whose boards are absent skips and names
what is missing, with the `--only <id>` line that fetches it, rather than
failing in a way that reads as hauksbee being broken.

Without the corpus at all, corpus-dependent tests report as ignored, never as
passed. To make a missing or thin corpus a hard failure instead (recommended in
CI):

```bash
HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace
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
will. A change that touches `frontend/` also has to clear the visual lint
(above). Beyond that:

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

**Anything a user reads follows the style contract.** `docs/STYLE.md` is one
page and it is enforced in review: the three-part error template (and the rule
against printing an internal identifier the user did not type), the verdict
lexicon (a run is GREEN or RED, an assertion passes or fails, a binding is
`exact`/`family`/`guessed`/`unresolved`, and no synonyms on any surface), table
style and deterministic sort, `snake_case` spec vocabulary against `kebab-case`
flags, stating an untrustworthy result in the same breath as the result with its
blast radius and remedy, and the rule that a fact appears once per report at its
highest-value position. If your change adds a surface, read that page first.

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
| `hauksbee-mcp` | The stdio MCP server: the analyse/check/decompile flow as tools for AI agents |
| `hauksbee-testkit` | Shared test plumbing: locates test assets and fixtures for the suites |

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
