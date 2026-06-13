# galvani-ci GitHub Action

A composite action that runs [`galvani-ci`](../../crates/galvani-ci) on your
board and firmware, so every layout change boots the firmware on the emulated
PCB and asserts the rails come up, the UART says hello, and the LED blinks,
before anything reaches a bench.

## Usage

In your hardware repo, add `.github/workflows/galvani-ci.yml`. The minimal job:

```yaml
- uses: actions/checkout@v4
- uses: ETM-Code/galvani/integrations/github-action@main
  with:
    spec: ci/power-up.toml          # your checked-in galvani-ci spec
    junit: galvani-ci-results.xml   # JUnit XML written here (optional)
```

See [`example-workflow.yml`](./example-workflow.yml) for a full workflow,
including a `matrix` that runs several specs in parallel and a commented-out
firmware-build step.

## Inputs

| Input             | Required | Default                  | Description                                                                 |
| ----------------- | -------- | ------------------------ | --------------------------------------------------------------------------- |
| `spec`            | yes      | -                        | Path to the TOML spec (relative to your repo root).                         |
| `junit`           | no       | `galvani-ci-results.xml` | JUnit XML output path; empty to skip.                                       |
| `galvani-ref`     | no       | `main`                   | git ref of galvani to build galvani-ci from (fallback build).               |
| `galvani-repo`    | no       | `ETM-Code/galvani`       | owner/name of the galvani repo (release download + fallback build).         |
| `galvani-version` | no       | (empty)                  | Release version to download a prebuilt binary from; empty auto-detects.     |
| `prefer-prebuilt` | no       | `true`                   | Download a prebuilt release binary when available, else build from source.  |

## Outputs

| Output   | Description                                  |
| -------- | -------------------------------------------- |
| `passed` | `true` if every assertion passed.            |
| `junit`  | Path to the JUnit XML that was written.      |

## How it works

1. **Prefers a prebuilt binary.** When `prefer-prebuilt` is `true` (the
   default), the action downloads the `galvani-ci` binary from a matching
   GitHub Release asset (`galvani-<version>-<os>-<arch>.tar.gz`, produced by
   `.github/workflows/release.yml`). It maps the runner to the right
   os-arch label and uses `galvani-version`, or the release that matches a
   tagged `galvani-ref`, or the latest release. No compile, runs in seconds.
2. **Falls back to building from source** only when no matching prebuilt asset
   exists: it checks galvani out into `.galvani/` at `galvani-ref`, installs a
   stable Rust toolchain, caches `~/.cargo` and `.galvani/target` (keyed on the
   ref and `Cargo.lock`), and builds `galvani-ci` in release.
3. Runs your spec. Because `GITHUB_ACTIONS` is set, galvani-ci emits
   `::error` / `::notice` annotations inline, and the JUnit XML is published to
   the Checks tab via `mikepenz/action-junit-report`.
4. The job's exit code is galvani-ci's: green if every assertion passed.

To make the prebuilt path available, push a `vX.Y.Z` tag so the release
workflow attaches binaries. Until a release exists the action simply builds
from source, so it works on day one with no release published.
