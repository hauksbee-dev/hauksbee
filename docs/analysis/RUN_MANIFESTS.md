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
   the same value hashes at replay.

Only the fixed `hauksbee` and `hauksbee-ci` binaries can be replayed. A manifest
cannot nominate an arbitrary executable. The original `--emit-manifest` option
is removed from recorded argv, so replay cannot overwrite its evidence.

The replay runs from the project root (see Paths below) and is then compared
with the `outcome` the original run recorded: verdict, exit code, and the
gating findings (by stable finding-group fingerprint, or `spec[i]:<assertion>`
for `hauksbee-ci`). Timings and report text are not compared. The result goes to
stderr, so a replayed `--json` run keeps stdout:

```text
MATCH  verdict fail, exit 1, 1 gating finding (manifest sha256:690c...)
```

```text
DIFF  this run does not reproduce manifest sha256:2a8d...
  verdict: pass -> fail
  new gating finding: lint/unrouted_pad/serious/711e36fbcb84
```

`reproduce` exits 0 on `MATCH` and 1 on `DIFF`. An input whose bytes changed is
a `DIFF` too, reported without re-running. A manifest from before outcomes were
recorded has nothing to compare, so `reproduce` says so and returns the replayed
run's own exit code. If a backend's version or binary differs from the recorded
one, `reproduce` prints a note; it does not refuse, because the comparison above
is the real test.

The manifest is a verification artifact, not a container of the board or
firmware. The recorded input paths must still be available. This is deliberate:
designs and firmware are often confidential, and silently copying them into a
shareable artifact would be a privacy failure. To share a reproduction, share
the manifest and its inputs under the project's existing access policy.

## Contract (`schema_version = 2`)

Schema 2 adds `root_hint`, `backends` and `outcome`, and stores paths relative to
the project root. Schema 1 manifests (absolute paths, none of those fields) are
still read and verified.

The JSON fields are:

| Field | Meaning |
|---|---|
| `schema_version` | Manifest contract version. Unknown versions are refused. |
| `manifest_id` | SHA-256 of the complete document with this field empty. |
| `tool` | Binary name, workspace release version, and git revision when the build has one. Source-tarball builds honestly omit the unavailable revision. |
| `components` | Workspace versions for the engine, extractor, MCU bridge, model library, and solver. |
| `plugins` | Installed model-pack name, version, kind, and declared provenance. Source paths are omitted. |
| `build` | Compile target OS/architecture and the sorted feature set (`avr`, `renode`, `qemu`, `embed-web` as applicable). |
| `root_hint` | Names the project root that relative paths are measured from (its directory name and whether it is a git checkout or the working directory), without its location. |
| `environment` | Only behavior-changing selectors, with values represented as SHA-256 hashes. |
| `backends` | Each external program found when the manifest was written (`simavr`, `qemu-xtensa`, `qemu-riscv32`, `renode`, `ngspice`, `kicad-cli`) with `version` and `sha256_of_binary`, from the same discovery `hauksbee doctor` uses. simavr is linked in, so it has a source commit and no binary hash. |
| `inputs` | Sorted role/project-relative-path/kind/digest/size records; directory digests cover sorted relative paths and each file's bytes. |
| `invocation.argv` | Ordered arguments, with a portable tool name and without the emission flag. |
| `invocation.options` | Parsed, normalized settings including defaults, seeds (for `hauksbee-ci`, each spec's resolved ensemble seed count and mode), fidelity knobs, output modes, and DNP/solver-facing selections. |
| `reproduce` | The one-command replay form. |
| `outcome` | `{verdict, exit_code, record_sha256, gating_findings}` written when the run ends. It is not part of `manifest_id`: the manifest is the identity, the outcome is what `reproduce` compares with. `record_sha256` is `null` until the run record exists. |

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
manifests and capture files are included, plus each `variant` file and the same
implicit/explicit model, KiCad-project, and waiver inputs. Multi-spec roles carry
indices, so their identity cannot collapse.

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

Paths are portable. An input or path-valued argument under the project root (the
git checkout containing the working directory, else the working directory) is
stored relative to it, so a manifest replays from a colleague's checkout or a
CI workspace. A path under the home directory is stored as `~/...`; anything
else stays absolute. The original cwd is not stored separately. Treat a manifest
as project metadata: it contains no file contents, but a path can itself be
sensitive.

For external simulators, prefer an explicit allowlisted selector such as
`HAUKSBEE_RENODE`, `HAUKSBEE_QEMU_*`, `HAUKSBEE_PIO`, or `NGSPICE`. Its value is
verified without disclosure. A backend discovered only through ambient `PATH`
is not content-hashed by this schema, so the manifest should not be described as
a self-contained or bit-for-bit portable execution environment.
