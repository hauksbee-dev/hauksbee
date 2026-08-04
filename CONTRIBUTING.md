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

### Scenario QC

```bash
cargo build --release
qc/run.sh
```

Ten simulated engineering sessions, run end to end against the release
binaries: a first run on an unfamiliar board, a rail that sags and then does
not, a part swap, scaffolding a repo's gate and then desyncing it, two firmware
images with opposite verdicts, five kinds of wrong spec, an analysis refused
rather than faked, the JSON and JUnit exports, six unusable input files, and a
waiver from creation to expiry.

These check the thing no unit test looks at: what a person experiences. The
wording of a verdict, the exit code a pipeline branches on, whether a diagnosis
names the value it measured, whether an error message leaves you somewhere you
can act from. Each of those can be individually correct while the session as a
whole is unusable.

Each scenario is a directory under `qc/scenarios/` holding a `scenario.toml`
(persona, goal, success criteria, and the steps with their assertions) and an
`EXPECT.md` saying what the session should feel like and why each assertion is
the one it is. `qc/run.sh --scenario 04` runs one, `--list` shows what exists,
and every run writes a full transcript to `qc/results/<timestamp>/report.md`.
CI runs the suite in the `scenario-qc` job.

**A change to user-facing behaviour updates the scenarios in the same PR.** New
wording, a new exit code, a renamed section heading, a message that grew a
sentence: pin the new behaviour, in the commit that changes it. Never loosen an
assertion to turn a red run green. That is not a passing suite, it is a suite
that has stopped checking, and it fails silently forever after.

### The board corpus

hauksbee's checks are calibrated to stay quiet on hardware that is fine, and the
board corpus is how that is measured. A new or changed check earns its place by
being run against the corpus and shown not to fire on boards known to be good.
Several checks have that pinned as a silence gate that goes red on any fire; the
placeholder-value gate, for instance, sweeps every board file in the corpus (129
on the fetched layout) across four extraction paths and demands zero
medium-or-high findings. Add one for your check if it is the kind that can cry
wolf.

One caveat to know before you trust a green run: several of the older corpus
guards address boards by a hand-built `board-corpus/famous/<id>/...` layout that
`fetch-corpus.sh` does not produce, so they do not find their boards on a freshly
fetched corpus. Always run with `HAUKSBEE_REQUIRE_CORPUS=1`, which turns a missing
corpus into a failure rather than a silent skip.

The corpus is mostly public open hardware, so you can fetch it:

```bash
scripts/fetch-corpus.sh --dir "$PWD/board-corpus"
export HAUKSBEE_CORPUS_DIR=$PWD/board-corpus
cargo test --workspace
```

**Pass `--dir` explicitly.** A bare `scripts/fetch-corpus.sh` is supposed to read
`default_dir` from the manifest and land everything in `./board-corpus`, and on
macOS it does not: the manifest parser splits fields with `awk -F'\x1f'`, BSD awk
does not interpret `\x` escapes, so the default is read as empty and boards are
dropped at the repository root instead. You can see the symptom in `.gitignore`,
which lists root-level `crkbd/`, `lily58/` and friends so an accidental `git add`
cannot commit them. Boards that land there are invisible to every corpus test,
which is worse than not fetching them, because the suite then measures less than
it appears to.

Each board is pulled from its upstream at the revision `corpus.toml` pins. For
git-hosted boards the resolved commit is checked against that pin, so you get the
revision the gate was measured against. **Zip-hosted boards are not verified at
all**: `fetch_zip` is a `curl` and an `unzip`, and the sha256 recorded beside one
of those entries in `corpus.toml` is never checked. This repository does **not**
vendor the boards. They carry CC BY-SA, GPL-3.0, and CERN-OHL licences, and
fetching means you get each one from its author under that author's terms rather
than through us.

Expect a partial fetch. The manifest pins 29 boards, and around 23 or 24 land
on a good day: three are skipped by default because their licence could not be
confirmed, one upstream is prone to hanging, one has moved the revision the
manifest pins, and the occasional extra upstream flakes. That is fine and it
is not a broken checkout. A corpus test whose boards are absent skips and names
what is missing, with the `--only <id>` line that fetches it, rather than
failing in a way that reads as hauksbee being broken.

Without the corpus, corpus-dependent tests **report as passed**, not as ignored.
Rust has no runtime ignored state, so they early-return with a note on stderr and
`cargo test` counts them green. This is the trap the whole section is about: a
green suite on a machine with no corpus has measured nothing. Never read one as
evidence. Make it a hard failure instead:

```bash
HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace
```

**That command does not currently pass on a freshly fetched corpus, and the reason
is our bug, not your checkout.** Several corpus suites address boards by the
maintainers' `board-corpus/famous/<id>/...` layout described above, so with the
variable set they hard-fail on board *location* rather than on any check firing:
`known_faults` (3 failed), `strap_lint_corpus` (5 failed), `erc_contention_corpus`
(1 failed, and its message says "board-corpus is absent" when what is absent is
`board-corpus/famous`), plus one subtest each in `resource_conflict_corpus`,
`si_corpus` and `drc_corpus`. `.github/workflows/corpus-gate.yml` runs this command,
which is why that workflow has no green path today.

So: run it to see which suites are genuinely covered, and read a failure naming a
missing path as this known gap rather than as a regression you introduced. Reconciling
the two layouts behind `hauksbee_testkit::board_path` is the fix, and it is a good
first contribution.

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
