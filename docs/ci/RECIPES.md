# CI recipes

Copy-paste pipeline recipes for CI systems other than GitHub Actions (GitHub
users: take `integrations/github-action` instead, see [CI.md](CI.md)). Every
recipe on this page runs against the published Docker image
`ghcr.io/hauksbee-dev/hauksbee@sha256:REPLACE_WITH_SLIM_DIGEST`, which carries `hauksbee` and
`hauksbee-ci` prebuilt on PATH plus the model database, so nothing is compiled
in the pipeline. Each recipe runs a checked-in spec with
`hauksbee-ci run ci/power-up.toml --junit report.xml` (specs conventionally
live in `ci/` in a hardware repo; `run` also accepts several specs at once and
writes one merged JUnit file, exiting with the worst code of the set), then
publishes the JUnit XML so the assertions show up as test results.

The GHCR image is private. Every recipe below obtains an authorized package-read
credential from its CI system's protected secret store; no token belongs in the
pipeline file or repository. A fine-grained PAT or GitHub App installation
token may be used, provided the repository has granted it access to the package.
Replace `REPLACE_WITH_SLIM_DIGEST` with the slim digest from the matching
private `container-digests-<tag>` prerelease's
`hauksbee-<version>-docker-digests.txt` asset. A moving tag is
not release evidence and is intentionally absent from these recipes.

## The exit-code contract

This is the canonical table. The other CI docs link here.

| exit | meaning |
|---|---|
| 0 | every assertion held (GREEN) |
| 1 | at least one assertion failed (RED) |
| 2 | spec / usage / board error |
| 3 | invalid for analysis: the analog co-sim did not converge, so the result is not trustworthy and the run refuses to pretend |

Exit 3 poses a policy question: it is not a hardware verdict (the board was
neither proven good nor proven bad), so teams usually surface it as an
"unstable" / soft-fail state rather than a plain red, and each recipe below
shows how its CI system expresses that mapping.

## GitLab CI

GitLab jobs run directly inside the image, so the binaries are simply on PATH
and the working directory is your checkout. `artifacts:reports:junit` feeds
the per-assertion results into the merge-request Tests tab, and
`artifacts:when: always` keeps the report on a red build, which is exactly
when you want it. `allow_failure:exit_codes` maps exit 3 to GitLab's orange
"passed with warnings" state while 1 and 2 stay red.
Because GitLab pulls the job image before `script`, configure a protected,
masked `DOCKER_AUTH_CONFIG` CI variable containing Docker auth JSON for
`ghcr.io` and an authorized package-read token. Generate and store that JSON in
GitLab's variable UI, never in this YAML.

```yaml
hauksbee:
  image: ghcr.io/hauksbee-dev/hauksbee@sha256:REPLACE_WITH_SLIM_DIGEST
  script:
    - hauksbee-ci run ci/power-up.toml --junit report.xml
  allow_failure:
    exit_codes: [3]
  artifacts:
    when: always
    reports:
      junit: report.xml
```

## Jenkins (declarative pipeline)

The `docker` agent runs the whole stage inside the image (Jenkins mounts the
workspace and matches the container user to the agent user automatically).
`sh(returnStatus: true)` captures the exit code instead of aborting, so the
pipeline can route it: 0 passes, 3 becomes UNSTABLE via `catchError`, and
anything else fails the build. `junit` in `post { always }` records the report
whatever the verdict.

```groovy
pipeline {
    agent {
        docker {
            image 'ghcr.io/hauksbee-dev/hauksbee@sha256:REPLACE_WITH_SLIM_DIGEST'
            registryUrl 'https://ghcr.io'
            registryCredentialsId 'hauksbee-ghcr-read'
        }
    }
    stages {
        stage('hauksbee-ci') {
            steps {
                script {
                    int code = sh(
                        returnStatus: true,
                        script: 'hauksbee-ci run ci/power-up.toml --junit report.xml'
                    )
                    if (code == 3) {
                        catchError(buildResult: 'UNSTABLE', stageResult: 'UNSTABLE') {
                            error 'exit 3: analog co-sim did not converge, no hardware verdict'
                        }
                    } else if (code != 0) {
                        error "hauksbee-ci failed with exit code ${code}"
                    }
                }
            }
        }
    }
    post {
        always {
            junit 'report.xml'
        }
    }
}
```

`hauksbee-ghcr-read` is a Jenkins username/password credential whose password
is the authorized token. Jenkins passes it to Docker without committing it.

## Azure DevOps

A container job runs every step inside the image, so the bash step calls
`hauksbee-ci` directly. The step captures the exit code itself: on 3 it emits
the `task.complete` logging command with `SucceededWithIssues` (the orange
partial-success state) and exits 0 so the pipeline continues, on anything
non-zero it fails. `PublishTestResults@2` with `condition: always()` uploads
the JUnit file on every verdict.

```yaml
resources:
  containers:
    - container: hauksbee
      image: ghcr.io/hauksbee-dev/hauksbee@sha256:REPLACE_WITH_SLIM_DIGEST
      endpoint: hauksbee-ghcr

jobs:
  - job: hauksbee
    pool:
      vmImage: ubuntu-latest
    container: hauksbee
    steps:
      - bash: |
          set +e
          hauksbee-ci run ci/power-up.toml --junit report.xml
          code=$?
          if [ "$code" -eq 3 ]; then
            echo "##vso[task.complete result=SucceededWithIssues;]analog co-sim did not converge, no hardware verdict"
            exit 0
          fi
          exit $code
        displayName: Run hauksbee-ci
      - task: PublishTestResults@2
        condition: always()
        inputs:
          testResultsFormat: JUnit
          testResultsFiles: report.xml
          testRunTitle: hauksbee-ci
```

`hauksbee-ghcr` is an Azure DevOps Docker Registry service connection for
`ghcr.io`, backed by an authorized package-read credential.

## Buildkite

The step logs in and runs Docker explicitly so the private pull cannot happen
before authentication. `--user` runs the container as the agent user so
`report.xml` is writable in the mounted checkout (the same ownership point
[DOCKER.md](DOCKER.md) makes). `soft_fail` on exit status 3
turns the non-verdict into a soft-failed (annotated, non-blocking) step while
1 and 2 stay hard failures, and `artifact_paths` uploads the report; add the
`junit-annotate` plugin in a follow-up step if you want the failures rendered
as a build annotation.

```yaml
steps:
  - label: "hauksbee-ci"
    command: |
      set +x
      export DOCKER_CONFIG="$(mktemp -d)"
      trap 'docker logout ghcr.io >/dev/null 2>&1 || true; find "$DOCKER_CONFIG" -depth -mindepth 1 -delete; rmdir "$DOCKER_CONFIG"' EXIT
      printf '%s' "$HAUKSBEE_GHCR_TOKEN" \
        | docker login ghcr.io --username "$HAUKSBEE_GHCR_USER" --password-stdin
      docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/work" \
        ghcr.io/hauksbee-dev/hauksbee@sha256:REPLACE_WITH_SLIM_DIGEST \
        hauksbee-ci run ci/power-up.toml --junit report.xml
    soft_fail:
      - exit_status: 3
    artifact_paths:
      - report.xml
```

Inject `HAUKSBEE_GHCR_USER` and `HAUKSBEE_GHCR_TOKEN` with Buildkite's secret
manager. The token must be redacted from logs and authorized for package read.

## Where to go next

The spec format (`[[supply]]`, `[fuzz]`, every `[[assert]]` kind) is in
[CI.md](CI.md). What the slim and full images contain, and when you need
`:full` (ESP32 / STM32 co-sim, autorouting), is in [DOCKER.md](DOCKER.md).
