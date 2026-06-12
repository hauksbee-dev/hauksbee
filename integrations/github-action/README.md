# galvani-ci GitHub Action

A composite action that runs [`galvani-ci`](../../crates/galvani-ci) on your
board and firmware, so every layout change boots the firmware on the emulated
PCB and asserts the rails come up, the UART says hello, and the LED blinks,
before anything reaches a bench.

## Usage

In your hardware repo, add `.github/workflows/galvani-ci.yml`. The minimal job:

```yaml
- uses: actions/checkout@v4
- uses: galvani/galvani/integrations/github-action@main
  with:
    spec: ci/power-up.toml          # your checked-in galvani-ci spec
    junit: galvani-ci-results.xml   # JUnit XML written here (optional)
```

See [`example-workflow.yml`](./example-workflow.yml) for a full workflow,
including a `matrix` that runs several specs in parallel and a commented-out
firmware-build step.

## Inputs

| Input          | Required | Default            | Description                                         |
| -------------- | -------- | ------------------ | --------------------------------------------------- |
| `spec`         | yes      | -                  | Path to the TOML spec (relative to your repo root). |
| `junit`        | no       | `galvani-ci-results.xml` | JUnit XML output path; empty to skip.         |
| `galvani-ref`  | no       | `main`             | git ref of galvani to build galvani-ci from.        |
| `galvani-repo` | no       | `galvani/galvani`  | owner/name of the galvani repo.                     |

## Outputs

| Output   | Description                                  |
| -------- | -------------------------------------------- |
| `passed` | `true` if every assertion passed.            |
| `junit`  | Path to the JUnit XML that was written.      |

## How it works

1. Checks out galvani into `.galvani/` at `galvani-ref`.
2. Installs a stable Rust toolchain.
3. Caches `~/.cargo` and `.galvani/target`, keyed on the galvani ref and its
   `Cargo.lock`, so only the first run pays the compile cost.
4. Builds `galvani-ci` in release.
5. Runs your spec. Because `GITHUB_ACTIONS` is set, galvani-ci emits
   `::error` / `::notice` annotations inline, and the JUnit XML is published to
   the Checks tab via `mikepenz/action-junit-report`.
6. The job's exit code is galvani-ci's: green if every assertion passed.

For a v1 this builds from source. To skip the build, publish prebuilt
`galvani-ci` binaries as release assets and swap the build step for a download;
the cache key already isolates per-ref so the migration is local to this file.
