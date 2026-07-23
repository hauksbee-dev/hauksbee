# Model packs

A pack is a versioned, shareable bundle of model data (06-extensibility-sdk
§3). Distribution is deliberately plain: a git repo or a tarball. No signing,
no registry.

## Layout

```
my-pack/
  pack.toml          # manifest (required)
  models/            # one or more [[models]] db TOML files (required, >= 1)
    parts.toml
  firmware/          # optional fixtures for the pack's own tests
```

## pack.toml

```toml
[pack]
name = "acme-sensors"            # [a-z0-9._-]; becomes the install dir name
version = "1.2.0"                # x.y.z, digits only
license = "MIT"
min_hauksbee_version = "0.1.0"   # oldest hauksbee this pack works with
provenance = "hand-written"      # hand-written | datasheet-extracted | vendor
description = "ACME's sensor line"   # optional
```

Every field except `description` is required; unknown fields are rejected
(typos must not vanish silently). Every `models/*.toml` file must pass the
same validation `hauksbee models lint` applies — including compiling any
`[models.logic]` block — before anything is installed. Each failure category
is a named error (`hauksbee-models/src/pack.rs`, `PackError`).

## CLI

```
hauksbee models add <path|url>   # validate + install + record
hauksbee models list             # what is installed
hauksbee models remove <name>    # uninstall
hauksbee models resolve <board>  # per component: which entry won, which layer
```

`add` accepts a pack directory, a local `.tar.gz`/`.tgz`/`.tar` archive
(unpacked with the system `tar`), or a git URL (shallow-cloned with the system
`git`). Plain `http://` URLs are refused. Installs land in
`~/.hauksbee/packs/<name>@<version>/` and are recorded in
`~/.hauksbee/packs.toml` (the lockfile-ish record, sibling of the packs dir).

## Resolution priority

Every model source has an explicit layer (`SourceLayer` in
`hauksbee-models/src/lib.rs`):

| layer            | priority |
|------------------|----------|
| built-in db      | 0        |
| installed packs  | 10       |
| user model dirs  | 20       |
| `--models-dir`   | 30       |
| user SPICE cards | 40       |

The higher layer wins outright; the specificity score only breaks ties within
a layer. Two packs shipping the same model id is a same-layer conflict:
reported loudly at load, naming both packs, never silently resolved.
`hauksbee models resolve <board>` prints the winning entry, layer, and origin
per component — the pack author's debugging surface.
