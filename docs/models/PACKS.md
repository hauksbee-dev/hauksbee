# Model packs

A pack is a versioned, shareable bundle of model data. Distribution stays
deliberately plain: a git repo or a tarball. No signing, no registry.

## Layout

```
my-pack/
  pack.toml          # manifest (required)
  models/            # one or more [[models]] db TOML files (required, >= 1)
    parts.toml
  firmware/           # optional fixtures for the pack's own tests
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

Every field except `description` is required. Unknown fields are rejected, so
a typo does not vanish silently. Every `models/*.toml` file must pass the same
validation `hauksbee models lint` applies, including compiling any
`[models.logic]` block, before hauksbee installs anything. Each failure
category is a named error (`hauksbee-models/src/pack.rs`, `PackError`).

## CLI

```
hauksbee models add <path|url>   # validate + install + record
hauksbee models list             # what is installed
hauksbee models remove <name>    # uninstall
hauksbee models resolve <board>  # per component: which entry won, which layer
```

`add` accepts a pack directory, a local `.tar.gz`/`.tgz`/`.tar` archive
(unpacked with the system `tar`), or a git URL (shallow-cloned with the system
`git`). It refuses plain `http://` URLs. Installs land in
`~/.hauksbee/packs/<name>@<version>/` and are recorded in
`~/.hauksbee/packs.toml` (the lockfile-ish record, sibling of the packs dir).

### Publishing a drafted model

A model drafted from a datasheet (either with `hauksbee models extract` or from
the report page's "Draft a model from a datasheet", see
[MODELS.md](MODELS.md)) lands in `~/.hauksbee/models/` carrying the provenance
string `datasheet-extracted`. That is the same spelling `[pack] provenance`
accepts, so packaging one needs no relabelling: copy the TOML into a pack's
`models/` directory and set `provenance = "datasheet-extracted"` in
`pack.toml`.

Do not relabel it `vendor` or `hand-written` to make it look better. The
provenance is how anyone installing the pack knows which numbers were read off
a datasheet by a machine and are worth re-checking, and a pack that lies about
that is worse than one with no models in it.

## Resolution priority

Every model source has an explicit layer (`SourceLayer` in
`hauksbee-models/src/lib.rs`):

| layer | name in reports | priority |
|-------|-----------------|----------|
| built-in db | `builtin` | 0 |
| installed packs | `pack` | 10 |
| `~/.hauksbee/models` (where datasheet extraction writes) | `user-dir` | 20 |
| `~/.config/hauksbee/models` (your own models) | `user-config-dir` | 25 |
| `--models-dir <dir>` | `models-dir` | 30 |
| user SPICE cards | `spice` | 40 |

Six layers, not five: the two standing user directories are **distinct**.
`~/.config/hauksbee/models` sits above `~/.hauksbee/models` on purpose, so a
model you hand-corrected in your config directory deterministically wins over an
auto-extracted one carrying the same id. Collapsing them into "user model dirs"
would leave that ordering to load order, which is exactly the accident the
layering exists to prevent.

The higher layer wins outright. The specificity score only breaks ties within
a layer. Two packs shipping the same model id is a same-layer conflict:
hauksbee reports it loudly at load time, naming both packs, and never
resolves it silently. `hauksbee models resolve <board>` prints the winning
entry, layer, and origin per component, the pack author's debugging surface.
