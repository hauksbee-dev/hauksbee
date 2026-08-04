# Make a model pack: from a directory to `models add`

**Goal.** Bundle model data (`[[models]]` entries, including sensor and logic
specs) into a versioned pack. Share it as a git repo or tarball, install it
with `hauksbee models add`, and debug it with `hauksbee models resolve`. The
format reference is [docs/models/PACKS.md](../models/PACKS.md). This is the
narrative version: build one from scratch and run the whole lifecycle. The
implementation is `crates/hauksbee-models/src/pack.rs` (format + store) and
`crates/hauksbee-engine/src/commands/models.rs` (CLI).

**What you need:** a working model TOML (from
[add-a-logic-ic.md](add-a-logic-ic.md) or [add-a-sensor.md](add-a-sensor.md))
and the `hauksbee` binary.

## Step 1, the directory

```
acme-logic/
  pack.toml          # manifest (required)
  models/            # >= 1 [[models]] db TOML file (required)
    parts.toml
  firmware/          # optional fixtures for the pack's own tests
```

## Step 2, the manifest

```toml
[pack]
name = "acme-logic"              # [a-z0-9._-]; becomes the install dir name
version = "1.0.0"                # x.y.z, digits only
license = "MIT"
min_hauksbee_version = "0.1.0"   # oldest hauksbee this pack works with
provenance = "hand-written"      # hand-written | datasheet-extracted | vendor
description = "ACME's 74HC coverage"   # the only optional field
```

Every other field is required, and **hauksbee rejects unknown fields**: a
typo'd `licence =` fails with a named error instead of vanishing.

> **Why `provenance` is mandatory.** A human typed a pack's numbers from a
> datasheet, an LLM extracted them, or the vendor shipped them, and a
> reviewer of `models list` output deserves to see which at a glance, because
> the three fail differently. You declare it, hauksbee does not infer it, and
> it carries into the install record. (Also deliberate: **no signing, no
> registry.** Distribution is a git URL or a tarball whose provenance you
> already trust. A registry would add an authority without adding a
> verification.)

## Step 3, the model files

`models/parts.toml` is an ordinary db file, in the same format as the builtin
`crates/hauksbee-models/db/*.toml`:

```toml
[[models]]
id = "acme-7400"
kind = "digital"
description = "quad 2-input NAND"

[models.match]
value_re = "(?i)^ACME7400"

[models.params]
voh = 4.4
vol = 0.1
vih = 3.15
vil = 1.35
tpd_s = 1.5e-8

[models.logic]
inputs  = ["a1", "b1"]
outputs = ["y1"]
[models.logic.comb]
"y1" = "!(a1 & b1)"
```

## Step 4, install, list, inspect, remove

```
$ hauksbee models add ./acme-logic
installed pack 'acme-logic@1.0.0' (1 model file(s), provenance: hand-written) into /home/you/.hauksbee/packs/acme-logic@1.0.0
recorded in /home/you/.hauksbee/packs.toml

$ hauksbee models list
installed packs (/home/you/.hauksbee/packs):
  acme-logic@1.0.0  license=MIT  provenance=hand-written  source=./acme-logic

$ hauksbee models remove acme-logic
removed pack 'acme-logic@1.0.0'
```

`add` also accepts a `.tar.gz`/`.tgz`/`.tar` archive (unpacked with the system
`tar`) or a git URL (`git@…`, `ssh://…`, `…​.git`, any `https://…`,
shallow-cloned with the system `git`). hauksbee refuses plain `http://`: no
HTTP client ships in hauksbee, and an unencrypted model source is a bad
default.

**Validation happens before anything is copied.** `Pack::load` checks the
manifest field by field (each failure a named `PackError`), requires at least
one `models/*.toml`, and runs every entry through the same validation
`models lint` applies, including *compiling* every `[models.logic]` block
through the engine's bind path. A pack that installs is a pack that loads.
If any file fails, nothing lands.

## Step 5, where your pack sits: the priority layers

Every model source has an explicit layer (`SourceLayer` in
`crates/hauksbee-models/src/lib.rs`):

| layer | priority |
|---|---|
| built-in db | 0 |
| **installed packs** | **10** |
| `~/.hauksbee/models` | 20 |
| `~/.config/hauksbee/models` | 25 |
| `--models-dir` flag | 30 |
| user SPICE cards | 40 |

The higher layer wins outright. The specificity score only breaks ties
*within* a layer. So your pack overrides any builtin it names, and a user
who disagrees with your pack overrides it from their model dir without
touching your files.

**Trap, same-layer conflicts.** Two installed packs shipping the same model
id cannot be ordered by priority (same layer), and would otherwise win by
load order, by accident. The library reports the conflict loudly at load,
naming both packs, and never resolves it silently. If you see that warning,
one of the two packs has to go, or rename its entry: that is the design, not
a bug.

## Step 6, the debugging surface

When a board does not bind what you expected:

```
$ hauksbee models resolve my_board.kicad_pcb
layer priority: builtin(0) < pack(10) < user-dir(20) < user-config-dir(25) < models-dir(30) < spice(40); specificity breaks ties within a layer
┌─────┬──────────┬────────────┬─────────────────┬──────────────────┐
│ Ref │ Value    │ Model      │ Layer           │ Origin           │
├─────┼──────────┼────────────┼─────────────────┼──────────────────┤
│ R1  │ 10k      │ r_fallback │ engine-fallback │ engine-fallback  │
│ U3  │ ACME7400 │ acme-7400  │ pack(10)        │ acme-logic@1.0.0 │
│ D1  │ 1N4148   │ 1n4148     │ builtin(0)      │ diodes           │
└─────┴──────────┴────────────┴─────────────────┴──────────────────┘
```

This shows, per component, which entry won, from which layer, from which
source. The rows are ordered for reading rather than in board order: anything
`UNRESOLVED` comes first, then `engine-fallback`, then the layers from
most-user-supplied down to `builtin`, with natural reference order inside each
group. So the parts you most likely need to act on are at the top of the output,
not scattered through it. Four things to read off it:

- The **Layer** column prints the layer name with its priority, so `pack(10)`
  and `builtin(0)` rather than bare names. Priorities are ordered, so you can
  see at a glance who could have overridden whom.
- **Origin** for a pack entry is `<name>@<version>`, not a path inside the pack.
  For a built-in it is the db file's stem (`diodes`), with no `.toml`.
- `engine-fallback` is not a layer at all. It means no model entry claimed the
  part and the engine stamped a generic device from the Value field instead
  (`r_fallback` for a plain resistance). Seeing it against a part your pack was
  supposed to claim is the signal that your `match.value_re` did not hit.

- An `UNRESOLVED` row means no entry claimed the part *and* the engine had no
  fallback for it either, so it binds OPEN. Those are the rows at the top.

The banner line is the full priority list, `user-config-dir(25)` included, so the
ordering you see there is the ordering the resolver actually applied.

Run this command first when a pack "does not work". Nine times out of ten the
entry lost a specificity tie inside its layer, or the `match.value_re` does not
hit the board's Value field.

## The test that proves it

The lifecycle above *is* the acceptance test, pinned in
`crates/hauksbee-models/tests/suite/`: `pack_format.rs` (manifest validation,
every named error, 14 tests), `pack_store.rs`
(`install_list_remove_round_trip`, `install_refuses_invalid_pack_before_copying`,
2 tests), `pack_layering.rs` (pack-beats-builtin, user-dir-beats-pack,
same-layer conflict reporting, 6 tests), plus the CLI-level resolve report in
`crates/hauksbee-engine/tests/models_resolve_layers.rs`.

Everything under `tests/suite/` compiles into one test binary named `suite`
(`tests/suite/main.rs` is its entry point), so there is one command, not three:

```
cargo test -p hauksbee-models --test suite pack
```

A green run:

```
running 22 tests
......................
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 60 filtered out; finished in 0.04s
```

Drop the `pack` filter to run all 82 tests in the suite.

And for *your* pack, the proof is the live transcript in step 4 plus a
`models resolve` against a board that names your parts, showing them win at
the `pack` layer.

---

Build the contents first: [add-a-sensor.md](add-a-sensor.md),
[add-a-logic-ic.md](add-a-logic-ic.md). Format reference:
[docs/models/PACKS.md](../models/PACKS.md).
