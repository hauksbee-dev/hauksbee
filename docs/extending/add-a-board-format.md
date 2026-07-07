# Add a board format: a toy reader

**Goal.** Teach hauksbee to read a board/netlist file format it doesn't know,
by implementing one small trait and registering it. This is the first
extension type in these walkthroughs that requires Rust — but it is Rust
against a stable two-method surface in a fork, not a core edit. The trait and
registry live in `crates/hauksbee-extract/src/reader.rs`; the proving test
pattern is `crates/hauksbee-extract/tests/reader_matrix.rs`.

**Honesty up front: this is a fork-with-one-registration-line story, not a
plugin ABI.** A dynamic `.so` plugin interface is deliberately out of scope —
Rust's unstable ABI makes one a maintenance sink (`reader.rs` module docs,
plan 06 §4). You fork, add a file, add one registration line, and carry a
small diff.

## How the registry works

Every format is a `BoardReader` that owns its own detection and its own parse:

```rust
pub trait BoardReader: Send + Sync {
    fn name(&self) -> &str;                                   // "kicad-pcb"
    fn detects(&self, bytes: &[u8], path: Option<&Path>) -> bool;
    fn read(&self, bytes: &[u8], path: Option<&Path>) -> Result<ExtractedBoard, ReadError>;
    fn is_binary(&self) -> bool { false }                     // OLE2 etc.
}
```

`Registry::builtin()` holds the six shipped readers (altium, eagle,
kicad-netlist, kicad-schematic, kicad-pcb, ipc-d356) in a documented order;
detection walks front-to-back and the first reader to claim the bytes wins.
When nothing matches, the error *enumerates every reader that was tried* — the
replacement for the old sniff ladder's arbitrary fallback.

> **Why `register` prepends.** The builtins are mutually exclusive by
> construction (distinct magics; the Altium OLE2 container can't appear in
> text), so order among them never matters. It only matters when a
> *third-party* reader overlaps a builtin — and in that case the fork author
> is the one who knows best, so `Registry::register` inserts at the **front**,
> letting a fork deliberately shadow a builtin (say, a stricter KiCad variant
> reader). Shadowing is a feature here, not an accident to prevent.

## Step 1 — implement the trait

The toy reader from the shipped test
(`third_party_reader_registers_and_wins`), verbatim:

```rust
use hauksbee_extract::reader::{BoardReader, ReadError, Registry};
use hauksbee_extract::ExtractedBoard;
use std::path::Path;

struct ToyReader;

impl BoardReader for ToyReader {
    fn name(&self) -> &str { "toy" }

    fn detects(&self, bytes: &[u8], _p: Option<&Path>) -> bool {
        bytes.starts_with(b"TOYBOARD")
    }

    fn read(&self, _b: &[u8], _p: Option<&Path>) -> Result<ExtractedBoard, ReadError> {
        Ok(ExtractedBoard { name: "toy".into(), nets: vec![], components: vec![] })
    }
}
```

For a real format, `read` fills `ExtractedBoard`'s nets and components — study
`EagleReader`/`from_eagle_brd` for an XML format or `Ipc356Reader` for a
fixed-column one. `ReadError` is an alias for the crate's `ExtractError`, so
existing parse-error conventions apply unchanged.

The two contracts that matter:

- **`detects` must be cheap** — a magic/structural prefix check, never a full
  parse. The builtins scan a 2 KiB window (`MAGIC_WINDOW`) for text magics;
  IPC-D-356 scans 64 KiB for its `3xx` records because real fab headers push
  them past the first line.
- **`detects` must not false-positive on other formats' files.** This is not
  advisory: the detection-matrix test enforces it pairwise across every
  fixture in the repo (below).

**Trap — detection by path is a tie-break, never authority.** The `path`
argument is a filename *hint*; content sniffing is authoritative and none of
the builtin readers use the path for detection at all. A reader that claims
files by extension alone will claim another format's file the moment someone
renames it, and the matrix test will (rightly) fail you.

## Step 2 — register it

```rust
let mut registry = Registry::builtin();
registry.register(Box::new(ToyReader));   // consulted before the builtins
let board = registry.read(&bytes, Some(path))?;
```

That is the entire integration. In a fork you place the registration where
your entry point builds its registry.

## Step 3 — the test that proves it

Two layers, both in `crates/hauksbee-extract/tests/reader_matrix.rs`:

1. **Your reader registers and wins** — the shape of
   `third_party_reader_registers_and_wins`: register the reader, feed it a
   sample, assert it claims it, then assert a builtin format *still routes
   correctly* with your reader present (the shadowing you didn't intend is
   the bug this catches).
2. **The detection matrix** — `detection_matrix_every_fixture` sweeps every
   board/netlist fixture committed to the repo plus synthesized Eagle/Altium
   samples, asserting (a) *exactly one* reader claims each file (pairwise
   no-false-positive) and (b) the winner matches the legacy sniff's routing
   (no behaviour change). Add a fixture of your format to the corpus and
   extend the expectation; if your `detects` overlaps anything, this is the
   test that says so, with a printed matrix naming the offender.

```
cargo test -p hauksbee-extract --test reader_matrix
```

Green looks like:

```
test unrecognized_error_enumerates_readers ... ok
test third_party_reader_registers_and_wins ... ok
test detection_matrix_every_fixture ... ok

test result: ok. 3 passed; 0 failed
```

(with the full fixture-by-reader matrix printed to stderr under
`--nocapture`).

## Limitations, stated

- **No dynamic loading.** Covered above; a fork with one registration line is
  the supported story, and the trait is kept small precisely so that diff
  stays small across rebases.
- **Binary formats** must override `is_binary()` — the byte-input entry point
  (`ExtractedBoard::from_auto_bytes`) only lets binary readers claim raw
  bytes, so a text file handed to it falls through to the text sniffer
  instead of being force-parsed as a container.
- What `ExtractedBoard` can express (nets, components, pads) bounds what your
  reader can extract; geometry-only formats like Gerber go through the
  separate `gerber` pipeline, not a `BoardReader`.

---

See [docs/BOARD_AS_CODE.md](../BOARD_AS_CODE.md) and
[docs/SCHEMATICS.md](../SCHEMATICS.md) for what happens to an
`ExtractedBoard` after your reader produces one.
