# hauksbee-ci GitHub Action

A composite action that runs [`hauksbee-ci`](https://docs.hauksbee.dev/docs/ci/ci) on your
board and firmware, so every layout change boots the firmware on the emulated
PCB and asserts the rails come up, the UART says hello, and the LED blinks,
before anything reaches a bench.

## Usage

In your hardware repo, add `.github/workflows/hauksbee-ci.yml`. No secret and
no token are needed: the Action, its release assets, and its images are
public.

```yaml
permissions:
  contents: read
  checks: write   # lets the action publish the JUnit results to the Checks tab

steps:
  - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0
    with:
      persist-credentials: false
  - uses: hauksbee-dev/hauksbee/integrations/github-action@REPLACE_WITH_RELEASE_COMMIT_SHA
    with:
      hauksbee-ref: REPLACE_WITH_RELEASE_COMMIT_SHA
      hauksbee-version: v0.1.0
      spec: ci/power-up.toml          # your checked-in hauksbee-ci spec
      junit: hauksbee-ci-results.xml   # JUnit XML written here (optional)
```

Pin the `@ref` to the release's full commit SHA, never `main` or a tag: a tag
can move after review, while an object ID cannot redirect the Action code.
Pass the same SHA as `hauksbee-ref` and the human release tag as
`hauksbee-version`; the Action refuses assets whose tag does not resolve to
that SHA.

Set `timeout-minutes` on every consumer job (the generated workflow uses 45).
Spec duration and ensemble size are intentionally user-controlled; without a
job deadline, a typo can consume the hosted runner's much larger default
budget before GitHub stops it.

See [`example-workflow.yml`](./example-workflow.yml) for a full workflow,
including a `matrix` that runs several specs in parallel and a commented-out
firmware-build step.

### Several specs, one invocation

`specs` takes a newline- or space-separated list of paths and/or globs. All of
them fan into ONE `hauksbee-ci run` invocation, which writes one merged JUnit
file (a `<testsuite>` per spec) and exits with the worst severity across the
set (3 invalid > 2 spec error > 1 assertion failed > 0 green):

```yaml
- uses: hauksbee-dev/hauksbee/integrations/github-action@REPLACE_WITH_RELEASE_COMMIT_SHA
  with:
    specs: ci/*.toml
```

### Board check mode

`mode: check` skips specs entirely and runs the engine's static gate,
`hauksbee run <board> --check --strict --junit <path>`: DRC shorts, netlint,
SI, the USB-C check, gated strictly, with inline annotations:

```yaml
- uses: hauksbee-dev/hauksbee/integrations/github-action@REPLACE_WITH_RELEASE_COMMIT_SHA
  with:
    mode: check
    board: hardware/board.kicad_pcb
```

### Auto-detection

With no `spec`, `specs`, or `board` at all, the action detects what to run:
exactly one Hauksbee TOML spec (a file with a top-level `board` key) in `ci/`
or the repository root runs as a spec. With no spec, exactly one supported
board file (`.kicad_pcb`, `.kicad_sch`, `.net`, `.brd`, `.PcbDoc`, `.d356` or
`.board`) runs as a check. Anything ambiguous fails with a list of what was
found, rather than silently changing which gate runs.

## Inputs

| Input             | Required | Default                  | Description                                                                 |
| ----------------- | -------- | ------------------------ | --------------------------------------------------------------------------- |
| `spec`            | no       | -                        | Path to a single TOML spec (relative to your repo root). Mutually exclusive with `specs`. |
| `specs`           | no       | -                        | Newline- or space-separated spec paths and/or globs; all run in one merged invocation. |
| `board`           | no       | -                        | Board file for `mode: check`.                                               |
| `mode`            | no       | `auto`                   | `spec` runs hauksbee-ci; `check` runs `hauksbee run <board> --check --strict`; `auto` infers, or detects when nothing is given. |
| `junit`           | no       | `hauksbee-ci-results.xml` | JUnit XML output path; empty to skip.                                       |
| `publish-report`  | no       | `true`                   | Publish the JUnit XML to the Checks tab. Set `false` in a job that lacks `checks: write` (fork PRs) and publish from a separate `workflow_run` workflow instead. |
| `hauksbee-ref`     | no       | `main`                   | git ref of hauksbee to build hauksbee-ci from (fallback build).               |
| `hauksbee-repo`    | no       | `hauksbee-dev/hauksbee`       | owner/name of the hauksbee repo (release download + fallback build).         |
| `hauksbee-token`   | no       | `github.token`           | Optional credential. The repo, releases, and images are public, so the calling workflow's automatic token suffices; set this only for a private mirror or GitHub Enterprise. |
| `registry-user`    | no       | derived                  | GHCR username for `use-image`; App installation tokens derive `x-access-token`, PATs derive `github.actor`. Set it when a PAT belongs to another user. |
| `hauksbee-version` | with `use-image`; otherwise no | (empty) | Release version for a prebuilt binary or image. Image mode uses its immutable Docker digest record; non-image mode can auto-detect. |
| `prefer-prebuilt` | no       | `true`                   | Download a prebuilt release binary when available, else build from source.  |
| `use-image`       | no       | `false`                  | Run from the published Docker image instead of a binary; skips the download and build paths entirely. |
| `image`           | no | `ghcr.io/hauksbee-dev/hauksbee:slim` | `slim`/`full` selector or immutable canonical digest. The Action resolves selectors through the selected release's immutable Docker manifest and verifies its OCI revision. |

The source fallback deliberately builds `--no-default-features --features
renode,qemu`, because stock hosted runners do not carry the GPL system
libsimavr dependency. AVR co-simulation therefore requires a prebuilt bundle
or image; selecting AVR after a source fallback fails explicitly rather than
pretending that backend is present.

## Outputs

| Output   | Description                                  |
| -------- | -------------------------------------------- |
| `passed` | `true` if every assertion passed.            |
| `junit`  | Path to the JUnit XML that was written.      |

## Fork PRs and the Checks tab

The Action itself runs fine on fork PRs: everything it downloads is public.
What a fork PR's workflow does not get is `checks: write`, so publishing the
JUnit report to the Checks tab fails there. Run the check with
`publish-report: false` and publish from a separate trusted workflow:

```yaml
# hauksbee-ci.yml (pull_request)
- uses: hauksbee-dev/hauksbee/integrations/github-action@REPLACE_WITH_RELEASE_COMMIT_SHA
  with:
    spec: ci/power-up.toml
    publish-report: false
- uses: actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02 # v4.6.2
  if: always()
  with:
    name: hauksbee-ci-results
    path: hauksbee-ci-results.xml
```

A second workflow, triggered by `workflow_run`, runs in the base repo's
context with real write permissions, downloads that artifact, and publishes
it:

```yaml
# hauksbee-ci-report.yml (workflow_run; has the base repo's token)
on:
  workflow_run:
    workflows: [hardware-ci]
    types: [completed]
permissions:
  actions: read
  checks: write
jobs:
  report:
    runs-on: ubuntu-latest
    steps:
      - uses: mikepenz/action-junit-report@db71d41eb79864e25ab0337e395c352e84523afe # v4.3.1
        with:
          commit: ${{ github.event.workflow_run.head_sha }}
          report_paths: "**/hauksbee-ci-results*.xml"
          check_name: "hauksbee-ci (hardware)"
```

(`mikepenz/action-junit-report` fetches the artifact itself on
`workflow_run` events; see its docs for the artifact-name knobs.)
The publishing workflow runs in the base repository's context with real write
permissions; the fork's code never executes there.

## How it works

1. **Resolves what to run.** `spec`/`specs` (globs expanded) run through
   `hauksbee-ci`; `board` runs through `hauksbee run --check --strict`;
   given neither, it auto-detects as described above. Contradictory inputs
   fail loudly instead of guessing.
2. **Prefers a prebuilt binary.** When `prefer-prebuilt` is `true` (the
   default), the action downloads the binaries from a matching GitHub Release
   asset (`hauksbee-<version>-<os>-<arch>.tar.gz`, produced by
   `.github/workflows/release.yml`). It maps the runner to the right
   os-arch label and uses `hauksbee-version`, or the release that matches a
   tagged `hauksbee-ref`, or the latest release. Archives are verified and
   extracted in per-job runner storage; they are deliberately never put in
   the consuming repository's restorable Actions cache, which fork workflows
   could poison. No compile is needed when a matching immutable release asset
   exists.
3. **Falls back to building from source** only when no matching prebuilt asset
   exists: it checks hauksbee out into `.hauksbee/` at `hauksbee-ref`, installs a
   stable Rust toolchain, caches only Cargo registry/git downloads, and
   builds the needed binary in release. The build output is not cached where
   fork workflows could restore it.
4. Runs your specs (or the board check). Because `GITHUB_ACTIONS` is set, the
   binary emits `::error` / `::notice` annotations inline, and the JUnit XML
   is published to the Checks tab via `mikepenz/action-junit-report` (unless
   `publish-report: false`).
5. The job's exit code is the binary's: green only when the check passed.

All third-party actions used internally are pinned to full commit SHAs.

To make the prebuilt path available, push a `vX.Y.Z` tag so the release
workflow attaches binaries. Before a release exists the Action can still build
the Renode/QEMU or static-check paths from source. AVR co-simulation needs the
prebuilt bundle or image described above; it fails explicitly rather than
presenting a source-only run as equivalent.
