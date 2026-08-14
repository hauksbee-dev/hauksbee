# hauksbee-ci GitHub Action

A composite action that runs [`hauksbee-ci`](https://docs.hauksbee.dev/docs/ci/ci) on your
board and firmware, so every layout change boots the firmware on the emulated
PCB and asserts the rails come up, the UART says hello, and the LED blinks,
before anything reaches a bench.

## Usage

The Action and its release assets live in a private repository. Create a secret
named `HAUKSBEE_READ_TOKEN` containing either a fine-grained personal access token
or a GitHub App installation token authorised for
`hauksbee-dev/hauksbee` with **Contents: read**. If `use-image` is enabled, the
credential also needs **Packages: read**. Do not put the credential in a URL,
workflow file, Action input default, log statement, or checked-in configuration.

The consumer repository's automatic `github.token` is scoped to that consumer;
it cannot authenticate a checkout or release download from a different private
repository. In your hardware repo, add `.github/workflows/hauksbee-ci.yml` and
check out the pinned Action code with the authorised credential before invoking
it as a local Action:

```yaml
permissions:
  contents: read
  checks: write   # lets the action publish the JUnit results to the Checks tab

steps:
  - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0
    with:
      persist-credentials: false
  - name: Fetch the private hauksbee Action
    uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0
    with:
      repository: hauksbee-dev/hauksbee
      ref: REPLACE_WITH_RELEASE_COMMIT_SHA
      path: .hauksbee-action
      token: ${{ secrets.HAUKSBEE_READ_TOKEN }}
      persist-credentials: false
  - uses: ./.hauksbee-action/integrations/github-action
    with:
      hauksbee-token: ${{ secrets.HAUKSBEE_READ_TOKEN }}
      hauksbee-ref: REPLACE_WITH_RELEASE_COMMIT_SHA
      hauksbee-version: v0.1.0
      spec: ci/power-up.toml          # your checked-in hauksbee-ci spec
      junit: hauksbee-ci-results.xml   # JUnit XML written here (optional)
```

Pin the private checkout's `ref` to the release's full commit SHA, never
`main` or a tag: a tag can move after review, while an object ID cannot redirect
the credential-bearing Action code. Pass the same SHA as `hauksbee-ref` and the
human release tag as `hauksbee-version`; the Action refuses assets whose tag
does not resolve to that SHA. Repositories owned by the same GitHub
organisation may alternatively use the direct `owner/repo/path@ref` form after
an administrator enables private Action sharing, but the explicit checkout
above is the portable cross-repository credential contract.

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
- uses: ./.hauksbee-action/integrations/github-action
  with:
    hauksbee-token: ${{ secrets.HAUKSBEE_READ_TOKEN }}
    specs: ci/*.toml
```

### Board check mode

`mode: check` skips specs entirely and runs the engine's static gate,
`hauksbee run <board> --check --strict --junit <path>`: DRC shorts, netlint,
SI, the USB-C check, gated strictly, with inline annotations:

```yaml
- uses: ./.hauksbee-action/integrations/github-action
  with:
    hauksbee-token: ${{ secrets.HAUKSBEE_READ_TOKEN }}
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
| `publish-report`  | no       | `true`                   | Publish the JUnit XML to the Checks tab. Set `false` only in a trusted job that lacks `checks: write`; fork PRs fail earlier because the private read token is withheld. |
| `hauksbee-ref`     | no       | `main`                   | git ref of hauksbee to build hauksbee-ci from (fallback build).               |
| `hauksbee-repo`    | no       | `hauksbee-dev/hauksbee`       | owner/name of the hauksbee repo (release download + fallback build).         |
| `hauksbee-token`   | yes      | -                        | Fine-grained PAT or GitHub App installation token authorised for the private repository with Contents: read; add Packages: read for `use-image`. |
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

GitHub does not pass `HAUKSBEE_READ_TOKEN` to an untrusted fork PR. That is the
correct boundary: do not expose a credential that can read private Hauksbee
source or assets to fork-controlled workflow code. The private Action therefore
fails closed on fork PRs. Run it on trusted branches, or have a maintainer apply
the change to a trusted branch after review; do not switch to
`pull_request_target` and execute fork content with the secret.

For trusted PRs, publishing the JUnit report also needs `checks: write`. If a
workflow intentionally lacks that permission, the hardware check can still run
with `publish-report: false` and a separate trusted workflow can publish XML.

For a trusted same-repository PR whose check job deliberately has no
`checks: write`, the two-workflow reporting pattern is:

```yaml
# hauksbee-ci.yml (pull_request; trusted same-repository branches only)
- uses: ./.hauksbee-action/integrations/github-action
  with:
    hauksbee-token: ${{ secrets.HAUKSBEE_READ_TOKEN }}
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
This does not make an untrusted fork runnable: fork PRs receive no private
Hauksbee credential and stop before producing the XML, by design.

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
   tagged `hauksbee-ref`, or the latest release. Private archives are verified
   and extracted in per-job runner storage; they are deliberately never put in
   the consuming repository's restorable Actions cache. No compile is needed
   when a matching immutable release asset exists.
3. **Falls back to building from source** only when no matching prebuilt asset
   exists: it checks hauksbee out into `.hauksbee/` at `hauksbee-ref`, installs a
   stable Rust toolchain, caches only public Cargo registry/git downloads, and
   builds the needed binary in release. The private source build output is not
   cached where fork workflows could restore it.
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
