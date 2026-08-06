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

### Frontend checks

```bash
cd frontend
bun run lint
bun run test:unit
bun run build
bun run test:e2e
```

The end-to-end command runs the Layers-dismissal, saved-session/export, and 3D
idle/responsiveness flows in sequence. It starts one fixture server on an
operating-system-selected port and always stops it after the run. Screenshots
and downloads go under `frontend/test-results/e2e/`. Set `HB_E2E_BASE` only
when running an individual flow against an already-running real server.

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
`frontend-quality` job and uploads the browser-test artifacts on a failure. It
serves
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
placeholder-value gate, for instance, sweeps 470 board files across four
extraction paths and demands zero medium-or-high findings that are not a recorded,
dated exception. Add one for your check if it is the kind that can cry wolf.

A silence gate's input set is hardware known to be fine, which is narrower than
the corpus. Four entries are fetched and parsed for format coverage but excluded
from the silence gates, and `corpus.toml` says why per entry: KiCad's own demo
projects and the CATs Eurosynth modules were never manufactured products, and the
Olimex ESP32-PoE and Duet 2 boards did ship but the shorts check fires on them
with the finding not yet adjudicated. Each exclusion is printed per board as a
`NOT KNOWN-GOOD` line beside the `SCANNED` counts, because a gate that quietly
narrowed its own input set is the same failure as one that scanned nothing.

An exception is not an allowlist entry. It names the board and the parts, says why
the finding is *right*, and carries an expiry after which the gate goes red again,
the same discipline `hauksbee-waivers.toml` imposes on a user. If a gate is red
because a check found something real on a corpus board, that is the answer: record
it with its evidence. Never widen a threshold until it disappears.

Always run with `HAUKSBEE_REQUIRE_CORPUS=1`, which turns a missing corpus into a
failure rather than a silent skip.

The corpus is mostly public open hardware, so you can fetch it:

```bash
scripts/fetch-corpus.sh
export HAUKSBEE_CORPUS_DIR=$PWD/board-corpus
HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace
```

Each board is pulled from its upstream at the revision `corpus.toml` pins. For
git-hosted boards that pin is a full 40-character commit sha and the resolved
commit is checked against it; for zip-hosted boards the archive's sha256 is
checked against the manifest, and the fetch refuses to download one that has no
hash recorded. Either way you get the revision the gate was measured against.
This repository does **not** vendor the boards. They carry CC BY-SA, GPL-3.0,
CERN-OHL-S, CERN-OHL-W, CERN-OHL-P, TAPR-OHL, Apache-2.0 and MIT terms, and
fetching means you get each one from its author under that author's terms rather
than through us.

The fetch ends by running `scripts/check-corpus.py`, which reads the manifest
back against the tree that landed and fails if they disagree. That is not
belt-and-braces: `subdir = "demos"` sat on the KiCad entry from the day it was
added and nothing acted on it, so the fetch pulled KiCad's `qa/` tree and the
zero-shorts gate spent that whole time grading itself on boards whose purpose is
to reproduce KiCad bugs. The check fails on a field nothing honours, an
abbreviated pin, an entry with no declared axes or `expect` paths, and an entry
whose landed files do not match its declaration. Run it on its own with
`python3 scripts/check-corpus.py --manifest-only` before you have a corpus.

Adding a board means adding all of it: the upstream, a full commit sha, the
licence **as you read it in the upstream's own bytes at that revision**, the
`axes` it covers, and at least one `expect` path. A licence you inferred from the
vendor's other repositories is not established; set `license_confirmed = false`
and it stays out of the default fetch until somebody establishes it. Two entries
are in that state today for exactly that reason.

Expect 47 of the manifest's 50 entries, which is 302 layout files, 507 schematics,
41 netlists and 606 gerber films in 531 MB. The three skipped by default are the
two ClockworkPi uConsole boards and the SparkFun MicroMod nRF52840, whose licences
could not be established; `--include-unconfirmed` fetches them if you read the
manifest and decide for yourself. A corpus test whose boards are absent skips and
names what is missing, with the `--only <id>` line that fetches it, rather than
failing in a way that reads as hauksbee being broken.

The fetched layout and the hand-built `board-corpus/famous/<id>/...` layout the
maintainers use are both accepted. Address a board through
`hauksbee_testkit::corpus_board` (one path) or `corpus_board_any` (alternates when
the two corpora hold different upstream revisions), and a sweep's root through
`corpus_boards_root`, never by joining `famous` yourself. Joining it directly is
what used to make corpus guards skip for everyone who followed these instructions.

`corpus_dir` finds the corpus from a git worktree as well as from the checkout: it
walks up from the crate it was compiled in, and reads the `.git` file to reach the
main worktree when the worktree lives outside the checkout. It used to check the
repository root and its parent and nothing else, so from
`<checkout>/.claude/worktrees/<name>` no corpus resolved, every corpus gate
skipped, and the skip read as a pass. If you work in a worktree, read the
`SCANNED` lines rather than the green tick.

Without the corpus, corpus-dependent tests **report as passed**, not as ignored.
Rust has no runtime ignored state, so they early-return with a note on stderr and
`cargo test` counts them green. This is the trap the whole section is about: a
green suite on a machine with no corpus has measured nothing. Never read one as
evidence. Make it a hard failure instead:

```bash
HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace
```

Every corpus gate prints what it covered, so add `--nocapture` and read the
`SCANNED  <gate>: N board(s)` lines. A gate that scanned zero fails outright:
`hauksbee_testkit::scanned` refuses the pass, because a gate that opened no board
has proved nothing and a green tick next to it is a lie about coverage.
`.github/workflows/corpus-gate.yml` runs the whole thing this way nightly and
lifts those counts into the run summary.

Three board families are outside the public manifest and have their own opt-in
flags, so requiring the public corpus does not make the nightly red by
construction: `HAUKSBEE_REQUIRE_ALTIUM_CORPUS`, `HAUKSBEE_REQUIRE_HUNT_CORPUS` and
`HAUKSBEE_REQUIRE_UCONSOLE_CORPUS`. Absent them, those suites report `NOT RUN` on
stderr with what is missing. They never pass quietly.

A handful of boards carry no redistribution rights at all, so they are absent
from the manifest. Nothing in the public test suite depends on them.

## Adding support for hardware we do not have

Most of what hauksbee knows about the physical world is data, and adding to it is
a welcomed contribution rather than a core-team activity. Each route has a
walkthrough that starts from a real datasheet or file format and ends with the
test that proves the extension works, and [docs/extending/README.md](docs/extending/README.md)
indexes them all with the cost of each.

The commonest ones:

- a part model (LDO, op-amp, diode, BJT, MOSFET, comparator): one `[[models]]`
  entry, no Rust;
- an I2C or SPI sensor with a register map: one TOML file, no Rust;
- an MCU variant of a family already supported: two TOML files, no recompile,
  [docs/extending/add-an-mcu-variant.md](docs/extending/add-an-mcu-variant.md);
- an MCU family we do not support at all: still TOML when the emulator models
  the part, and [docs/extending/add-a-microcontroller.md](docs/extending/add-a-microcontroller.md)
  is honest about the tier where it stops being. That page ends with a checklist
  a pull request is graded against, and a plain statement of what the maintainer
  will and will not maintain afterwards.

Two rules apply to all of them, and they are the same rules the rest of this page
states. A capability is claimed only at the tier a test proves, using the
proven-end-to-end / boot-only / absent vocabulary
[docs/cosim/MCU.md](docs/cosim/MCU.md) uses. And anything vendored from another
project carries a `README.md` beside it recording origin, upstream commit,
licence and refresh procedure, with the licence text checked in;
`crates/hauksbee-mcu/db/mcu/rp2040/` is the worked pattern.

## What a change has to clear

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

CI runs the same four checks. `cargo doc` renders the module headers as
documentation, so `cargo doc --open` is how you read them the way a reader
will. A change that touches `frontend/` also has to clear lint, unit tests, the
production build, the end-to-end browser flows, and visual lint (above). Beyond
that:

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
