# hauksbee-ci GitHub Action

A composite action that runs [`hauksbee-ci`](https://docs.hauksbee.dev/docs/ci/ci) on your
board and firmware, so every layout change boots the firmware on the emulated
PCB and asserts the rails come up, the UART says hello, and the LED blinks,
before anything reaches a bench.

## Usage

In your hardware repo, add `.github/workflows/hauksbee-ci.yml`. The minimal job:

```yaml
permissions:
  contents: read
  checks: write   # lets the action publish the JUnit results to the Checks tab

steps:
  - uses: actions/checkout@v4
  - uses: hauksbee-dev/hauksbee/integrations/github-action@v0.1.0
    with:
      spec: ci/power-up.toml          # your checked-in hauksbee-ci spec
      junit: hauksbee-ci-results.xml   # JUnit XML written here (optional)
```

Pin the action to a released tag (`@v0.1.0`), not `@main`: a tag names the
exact action code your pipeline runs, where `@main` silently moves under you.
Stricter still is pinning the full commit SHA
(`...github-action@<full-sha> # v0.1.0`), which no tag move can redirect.

See [`example-workflow.yml`](./example-workflow.yml) for a full workflow,
including a `matrix` that runs several specs in parallel and a commented-out
firmware-build step.

### Several specs, one invocation

`specs` takes a newline- or space-separated list of paths and/or globs. All of
them fan into ONE `hauksbee-ci run` invocation, which writes one merged JUnit
file (a `<testsuite>` per spec) and exits with the worst severity across the
set (3 invalid > 2 spec error > 1 assertion failed > 0 green):

```yaml
- uses: hauksbee-dev/hauksbee/integrations/github-action@v0.1.0
  with:
    specs: ci/*.toml
```

### Board check mode

`mode: check` skips specs entirely and runs the engine's static gate,
`hauksbee run <board> --check --strict --junit <path>`: DRC shorts, netlint,
SI, the USB-C check, gated strictly, with inline annotations:

```yaml
- uses: hauksbee-dev/hauksbee/integrations/github-action@v0.1.0
  with:
    mode: check
    board: hardware/board.kicad_pcb
```

### Auto-detection

With no `spec`, `specs`, or `board` at all, the action detects what to run:
exactly one `*.toml` in `ci/` runs as a spec; failing that, exactly one
`*.kicad_pcb` in the repo runs as a check. Anything ambiguous fails with a
list of what was found, rather than guessing.

## Inputs

| Input             | Required | Default                  | Description                                                                 |
| ----------------- | -------- | ------------------------ | --------------------------------------------------------------------------- |
| `spec`            | no       | -                        | Path to a single TOML spec (relative to your repo root). Mutually exclusive with `specs`. |
| `specs`           | no       | -                        | Newline- or space-separated spec paths and/or globs; all run in one merged invocation. |
| `board`           | no       | -                        | Board file for `mode: check`.                                               |
| `mode`            | no       | `auto`                   | `spec` runs hauksbee-ci; `check` runs `hauksbee run <board> --check --strict`; `auto` infers, or detects when nothing is given. |
| `junit`           | no       | `hauksbee-ci-results.xml` | JUnit XML output path; empty to skip.                                       |
| `publish-report`  | no       | `true`                   | Publish the JUnit XML to the Checks tab. Set `false` on fork PRs (see below). |
| `hauksbee-ref`     | no       | `main`                   | git ref of hauksbee to build hauksbee-ci from (fallback build).               |
| `hauksbee-repo`    | no       | `hauksbee-dev/hauksbee`       | owner/name of the hauksbee repo (release download + fallback build).         |
| `hauksbee-version` | no       | (empty)                  | Release version to download a prebuilt binary from; empty auto-detects.     |
| `prefer-prebuilt` | no       | `true`                   | Download a prebuilt release binary when available, else build from source.  |
| `use-image`       | no       | `false`                  | Run from the published Docker image instead of a binary; skips the download and build paths entirely. |
| `image`           | no       | `ghcr.io/hauksbee-dev/hauksbee:slim` | Image to run when `use-image` is true. Use `ghcr.io/hauksbee-dev/hauksbee:full` for STM32 / ESP32 co-sim or autorouting. |

## Outputs

| Output   | Description                                  |
| -------- | -------------------------------------------- |
| `passed` | `true` if every assertion passed.            |
| `junit`  | Path to the JUnit XML that was written.      |

## Fork PRs and the Checks tab

Publishing the JUnit report needs `checks: write`, and on a `pull_request`
run triggered from a FORK the token is read-only no matter what the
`permissions:` block asks for. The publish step then fails (or is skipped),
even though the hardware check itself ran fine.

The standard fix is the two-workflow pattern. The `pull_request` workflow
runs the check with `publish-report: false` and uploads the XML as an
artifact:

```yaml
# hauksbee-ci.yml (pull_request; runs with the fork's read-only token)
- uses: hauksbee-dev/hauksbee/integrations/github-action@v0.1.0
  with:
    spec: ci/power-up.toml
    publish-report: false
- uses: actions/upload-artifact@v4
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
  checks: write
jobs:
  report:
    runs-on: ubuntu-latest
    steps:
      - uses: mikepenz/action-junit-report@v4
        with:
          commit: ${{ github.event.workflow_run.head_sha }}
          report_paths: "**/hauksbee-ci-results*.xml"
          check_name: "hauksbee-ci (hardware)"
```

(`mikepenz/action-junit-report` fetches the artifact itself on
`workflow_run` events; see its docs for the artifact-name knobs.)

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
   tagged `hauksbee-ref`, or the latest release. The downloaded bundle is
   cached keyed on the resolved release tag, so a warm run skips even the
   download. No compile, runs in seconds.
3. **Falls back to building from source** only when no matching prebuilt asset
   exists: it checks hauksbee out into `.hauksbee/` at `hauksbee-ref`, installs a
   stable Rust toolchain, caches `~/.cargo` and `.hauksbee/target` (keyed on the
   ref and `Cargo.lock`), and builds the needed binary in release.
4. Runs your specs (or the board check). Because `GITHUB_ACTIONS` is set, the
   binary emits `::error` / `::notice` annotations inline, and the JUnit XML
   is published to the Checks tab via `mikepenz/action-junit-report` (unless
   `publish-report: false`).
5. The job's exit code is the binary's: green only when the check passed.

All third-party actions used internally are pinned to full commit SHAs.

To make the prebuilt path available, push a `vX.Y.Z` tag so the release
workflow attaches binaries. Until a release exists the action simply builds
from source, so it works on day one with no release published.
