# CI recipes

Copy-paste pipeline recipes for CI systems other than GitHub Actions (GitHub
users: take `integrations/github-action` instead, see [CI.md](CI.md)). Every
recipe on this page runs against the published Docker image
`ghcr.io/hauksbee-dev/hauksbee:slim`, which carries `hauksbee` and
`hauksbee-ci` prebuilt on PATH plus the model database, so nothing is compiled
in the pipeline. Each recipe runs a checked-in spec with
`hauksbee-ci run ci/power-up.toml --junit report.xml` (specs conventionally
live in `ci/` in a hardware repo; `run` also accepts several specs at once and
writes one merged JUnit file, exiting with the worst code of the set), then
publishes the JUnit XML so the assertions show up as test results.

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

```yaml
hauksbee:
  image: ghcr.io/hauksbee-dev/hauksbee:slim
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
        docker { image 'ghcr.io/hauksbee-dev/hauksbee:slim' }
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

## Azure DevOps

A container job runs every step inside the image, so the bash step calls
`hauksbee-ci` directly. The step captures the exit code itself: on 3 it emits
the `task.complete` logging command with `SucceededWithIssues` (the orange
partial-success state) and exits 0 so the pipeline continues, on anything
non-zero it fails. `PublishTestResults@2` with `condition: always()` uploads
the JUnit file on every verdict.

```yaml
jobs:
  - job: hauksbee
    pool:
      vmImage: ubuntu-latest
    container: ghcr.io/hauksbee-dev/hauksbee:slim
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

## Buildkite

The docker plugin runs the command inside the image with the checkout
mounted; `propagate-uid-gid: true` runs the container as the agent user so
`report.xml` is writable in the mounted checkout (the same ownership point
[DOCKER.md](DOCKER.md) makes about `--user`). `soft_fail` on exit status 3
turns the non-verdict into a soft-failed (annotated, non-blocking) step while
1 and 2 stay hard failures, and `artifact_paths` uploads the report; add the
`junit-annotate` plugin in a follow-up step if you want the failures rendered
as a build annotation.

```yaml
steps:
  - label: "hauksbee-ci"
    command: hauksbee-ci run ci/power-up.toml --junit report.xml
    soft_fail:
      - exit_status: 3
    artifact_paths:
      - report.xml
    plugins:
      - docker#v5.12.0:
          image: ghcr.io/hauksbee-dev/hauksbee:slim
          propagate-uid-gid: true
```

## Where to go next

The spec format (`[[supply]]`, `[fuzz]`, every `[[assert]]` kind) is in
[CI.md](CI.md). What the slim and full images contain, and when you need
`:full` (ESP32 / STM32 co-sim, autorouting), is in [DOCKER.md](DOCKER.md).
