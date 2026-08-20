# Add a board format: a toy reader

**Goal.** Teach hauksbee to read a board or netlist file format it does not
know. Implement one small trait and register it. This is the first extension
type in these walkthroughs that needs Rust. It uses Rust against a stable
two-method surface in a fork, not a core edit. The trait and registry live in
`crates/hauksbee-extract/src/reader.rs`. The proving test pattern is
`crates/hauksbee-extract/tests/reader_matrix.rs`.

**Fork with one registration line, not a plugin ABI.** A dynamic `.so`
plugin interface is out of scope on purpose. Rust's unstable ABI makes one a
maintenance sink (see the `reader.rs` module docs). You fork, add a file, add
one registration line, and carry a small diff.

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

`Registry::builtin()` holds nine shipped readers in a documented order:
Altium, ODB++, Protel ASCII, IPC-2581, Eagle, KiCad netlist, KiCad schematic,
KiCad PCB, and IPC-D-356. Detection walks front to back, and the first reader
to claim the bytes wins. When nothing matches, the error lists every reader it
tried.

> **Why `register` prepends.** The builtins are mutually exclusive by
> construction: they have distinct magics, and the Altium OLE2 container
> cannot appear in text. So order among them never matters. It matters only
> when a *third-party* reader overlaps a builtin. In that case, the fork
> author knows best, so `Registry::register` inserts at the **front**. This
> lets a fork deliberately shadow a builtin, for example a stricter KiCad
> variant reader. Shadowing is a feature here, not a bug to prevent.

## Step 1, implement the trait

This is the toy reader from the shipped test
(`third_party_reader_registers_and_wins`), shown verbatim:

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

For a real format, `read` fills `ExtractedBoard`'s nets and components.
Study `EagleReader` or `from_eagle_brd` for an XML format, or `Ipc356Reader`
for a fixed-column format. `ReadError` is an alias for the crate's
`ExtractError`, so existing parse-error conventions still apply.

Two contracts matter:

- **`detects` must be cheap.** Use a magic or structural prefix check, never
  a full parse. The builtins scan a 2 KiB window (`MAGIC_WINDOW`) for text
  magics. IPC-D-356 scans 64 KiB for its `3xx` records, because real fab
  headers push them past the first line.
- **`detects` must not false-positive on other formats' files.** This rule
  is not optional. The detection-matrix test enforces it pairwise across
  every fixture in the repo (see below).

**Trap: path-based detection is a tie-break, never authority.** The `path`
argument is only a filename hint. Content sniffing is authoritative, and
none of the builtin readers use the path for detection. A reader that claims
files by extension alone will claim another format's file the moment
someone renames it. The matrix test will fail you, correctly.

## Step 2, register it

```rust
let mut registry = Registry::builtin();
registry.register(Box::new(ToyReader));   // consulted before the builtins
let board = registry.read(&bytes, Some(path))?;
```

That is the whole integration. In a fork, place the registration where your
entry point builds its registry.

## Step 3: the test that proves it

Two layers, both in `crates/hauksbee-extract/tests/reader_matrix.rs`:

1. **Your reader registers and wins.** This follows the shape of
   `third_party_reader_registers_and_wins`: register the reader, feed it a
   sample, and confirm it claims the sample. Then confirm a builtin format
   still routes correctly with your reader present. This step catches
   unintended shadowing.
2. **The detection matrix.** `detection_matrix_every_fixture` sweeps every
   board or netlist fixture in the repo, plus synthesized Eagle and Altium
   samples. It checks that exactly one reader claims each file, with no false
   positives, and that the winner is the expected reader for that fixture. Add
   a fixture of your format to the corpus and extend the expectation. If your
   `detects` overlaps another reader, this test finds it and prints a matrix
   that names the offender.

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

The full fixture-by-reader matrix prints to stderr under `--nocapture`.

## Limitations

- **No dynamic loading.** As covered above, a fork with one registration
  line is the supported approach. The trait stays small on purpose, so the
  diff stays small across rebases.
- **Binary formats** must override `is_binary()`. The byte-input entry point
  (`ExtractedBoard::from_auto_bytes`) lets only binary readers claim raw
  bytes. A text file handed to it falls through to the text sniffer instead
  of a forced parse as a container.
- What `ExtractedBoard` can express (nets, components, pads) bounds what
  your reader can extract. Geometry-only formats like Gerber go through the
  separate `gerber` pipeline, not a `BoardReader`.

---

See [docs/ingest/BOARD_AS_CODE.md](../ingest/BOARD_AS_CODE.md) and
[docs/ingest/SCHEMATICS.md](../ingest/SCHEMATICS.md) for what happens to an
`ExtractedBoard` after your reader produces one.
