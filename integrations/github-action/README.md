# hauksbee-ci GitHub Action

A composite action that runs [`hauksbee-ci`](../../crates/hauksbee-ci) on your
board and firmware, so every layout change boots the firmware on the emulated
PCB and asserts the rails come up, the UART says hello, and the LED blinks,
before anything reaches a bench.

## Usage

In your hardware repo, add `.github/workflows/hauksbee-ci.yml`. The minimal job:

```yaml
- uses: actions/checkout@v4
- uses: ETM-Code/hauksbee/integrations/github-action@main
  with:
    spec: ci/power-up.toml          # your checked-in hauksbee-ci spec
    junit: hauksbee-ci-results.xml   # JUnit XML written here (optional)
```

See [`example-workflow.yml`](./example-workflow.yml) for a full workflow,
including a `matrix` that runs several specs in parallel and a commented-out
firmware-build step.

## Inputs

| Input             | Required | Default                  | Description                                                                 |
| ----------------- | -------- | ------------------------ | --------------------------------------------------------------------------- |
| `spec`            | yes      | -                        | Path to the TOML spec (relative to your repo root).                         |
| `junit`           | no       | `hauksbee-ci-results.xml` | JUnit XML output path; empty to skip.                                       |
| `hauksbee-ref`     | no       | `main`                   | git ref of hauksbee to build hauksbee-ci from (fallback build).               |
| `hauksbee-repo`    | no       | `ETM-Code/hauksbee`       | owner/name of the hauksbee repo (release download + fallback build).         |
| `hauksbee-version` | no       | (empty)                  | Release version to download a prebuilt binary from; empty auto-detects.     |
| `prefer-prebuilt` | no       | `true`                   | Download a prebuilt release binary when available, else build from source.  |

## Outputs

| Output   | Description                                  |
| -------- | -------------------------------------------- |
| `passed` | `true` if every assertion passed.            |
| `junit`  | Path to the JUnit XML that was written.      |

## How it works

1. **Prefers a prebuilt binary.** When `prefer-prebuilt` is `true` (the
   default), the action downloads the `hauksbee-ci` binary from a matching
   GitHub Release asset (`hauksbee-<version>-<os>-<arch>.tar.gz`, produced by
   `.github/workflows/release.yml`). It maps the runner to the right
   os-arch label and uses `hauksbee-version`, or the release that matches a
   tagged `hauksbee-ref`, or the latest release. No compile, runs in seconds.
2. **Falls back to building from source** only when no matching prebuilt asset
   exists: it checks hauksbee out into `.hauksbee/` at `hauksbee-ref`, installs a
   stable Rust toolchain, caches `~/.cargo` and `.hauksbee/target` (keyed on the
   ref and `Cargo.lock`), and builds `hauksbee-ci` in release.
3. Runs your spec. Because `GITHUB_ACTIONS` is set, hauksbee-ci emits
   `::error` / `::notice` annotations inline, and the JUnit XML is published to
   the Checks tab via `mikepenz/action-junit-report`.
4. The job's exit code is hauksbee-ci's: green if every assertion passed.

To make the prebuilt path available, push a `vX.Y.Z` tag so the release
workflow attaches binaries. Until a release exists the action simply builds
from source, so it works on day one with no release published.
