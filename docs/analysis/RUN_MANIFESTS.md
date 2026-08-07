# Immutable run manifests

`hauksbee run` and `hauksbee-ci run` can write a content-addressed record of
exactly what they were asked to run:

```bash
hauksbee run board.kicad_pcb --check --strict \
  --emit-manifest run.manifest.json

hauksbee-ci run ci/power-up.toml --seed 8 \
  --emit-manifest power-up.manifest.json
```

The receipt is printed on stderr, including its `sha256:...` manifest ID, so
`--json` stdout remains one parseable report object. Emission happens before the
analysis starts. A red, invalid, or interrupted run therefore still leaves the
request needed to investigate it.

The file is immutable by contract. Hauksbee creates it atomically and refuses
to replace an existing path. It also refuses to put a manifest inside a
directory the same manifest hashes: writing the file there would immediately
change that directory's digest.

## Reproduce in one command

```bash
hauksbee reproduce run.manifest.json
```

Before launching anything, `reproduce` verifies all four boundaries:

1. the JSON content still computes to its recorded `manifest_id`;
2. every file and directory input still has the recorded SHA-256 digest, size,
   kind, and (for directories) file count;
3. the current tool version and git revision match the recorded build; and
4. behavior-changing environment selectors which were set during capture have
   the same value hashes now.

Only the fixed `hauksbee` and `hauksbee-ci` binaries can be replayed. A manifest
cannot nominate an arbitrary executable. The original `--emit-manifest` option
is removed from recorded argv, so replay cannot overwrite its evidence. The
child's original exit code is returned.

The manifest is a verification artifact, not a container of the board or
firmware. The recorded input paths must still be available. This is deliberate:
designs and firmware are often confidential, and silently copying them into a
shareable artifact would be a privacy failure. To share a reproduction, share
the manifest and its inputs under the project's existing access policy.

## Contract (`schema_version = 1`)

The JSON fields are:

| Field | Meaning |
|---|---|
| `schema_version` | Manifest contract version. Unknown versions are refused. |
| `manifest_id` | SHA-256 of the complete document with this field empty. |
| `tool` | Binary name, workspace release version, and git revision when the build has one. Source-tarball builds honestly omit the unavailable revision. |
| `components` | Workspace versions for the engine, extractor, MCU bridge, model library, and solver. |
| `plugins` | Installed model-pack name, version, kind, and declared provenance. Source paths are omitted. |
| `build` | Compile target OS/architecture and the sorted feature set (`avr`, `renode`, `qemu`, `embed-web` as applicable). |
| `environment` | Only behavior-changing selectors, with values represented as SHA-256 hashes. |
| `inputs` | Sorted role/canonical-path/kind/digest/size records; directory digests cover sorted relative paths and each file's bytes. |
| `invocation.argv` | Ordered arguments, with a portable tool name and without the emission flag. |
| `invocation.options` | Parsed, normalized settings including defaults, seeds, fidelity knobs, output modes, and DNP/solver-facing selections. |
| `reproduce` | The one-command replay form. |

Serialization is stable: struct order is fixed; options/components are sorted
maps; input, plugin, feature, and environment inventories are sorted before the
ID is computed; no timestamp is included.

## What is hashed

For `hauksbee run`, inputs include the board (a directory is hashed recursively),
firmware source and resolved image when they differ, BOM, placement, as-built
overlay, explicit model directory, installed model packs, both user model
directories, a sibling KiCad project file, and a board-local
`hauksbee-waivers.toml` when present.

For `hauksbee-ci run`, each spec and its resolved board, firmware, as-built
overlay, MCU descriptor directory, external sensor specs, hardware-trace
manifests and capture files are included, plus the same implicit/explicit model,
KiCad-project, and waiver inputs. Multi-spec roles carry indices, so their
identity cannot collapse.

Built-in models and solver code are pinned by the tool git revision and the
component versions rather than duplicated as input files.

A replay deliberately re-evaluates time-based waiver expiry against the date of
the replay. The manifest does not freeze or resurrect a waiver, so a historical
verdict can change after that waiver expires.

## Privacy and honesty boundary

The manifest never records timestamps, hostname, username, current directory,
`PATH`, debug switches, API credentials, tokens, or secrets. The allowlisted
environment selectors are model/backend resolution controls such as
`HAUKSBEE_MCU_DIR`, `HAUKSBEE_RENODE`, `HAUKSBEE_QEMU_*`, `HAUKSBEE_PIO`, and
`NGSPICE`; only their value digests are stored. A mismatch names the variable,
but never reveals either value.

Canonical absolute paths for inputs do appear because replay must not depend on
the directory it is launched from, and mismatch diagnostics must name the file.
Path-valued argv is made absolute for the same reason; the original cwd is not
stored separately. Treat a manifest as project metadata: it contains no file
contents, but a path (including a home-directory name) can itself be sensitive.

For external simulators, prefer an explicit allowlisted selector such as
`HAUKSBEE_RENODE`, `HAUKSBEE_QEMU_*`, `HAUKSBEE_PIO`, or `NGSPICE`. Its value is
verified without disclosure. A backend discovered only through ambient `PATH`
is not content-hashed by this schema, so the manifest should not be described as
a self-contained or bit-for-bit portable execution environment.
