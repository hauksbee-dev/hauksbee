import { describe, expect, test } from 'bun:test'
import { existsSync, readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const frontend = join(dirname(fileURLToPath(import.meta.url)), '..')
const repository = join(frontend, '..')

type PackageManifest = { scripts?: Record<string, string> }

describe('frontend release gates', () => {
  const manifest = JSON.parse(
    readFileSync(join(frontend, 'package.json'), 'utf8'),
  ) as PackageManifest
  const workflow = readFileSync(join(repository, '.github/workflows/ci.yml'), 'utf8')

  test('package scripts expose the unit and end-to-end suites', () => {
    expect(manifest.scripts?.['test:unit']).toBe('bun test')
    expect(manifest.scripts?.['test:e2e']).toBe('bun run tests/e2e/run.ts')
  })

  test('the end-to-end runner exists and owns every release flow', () => {
    const runner = join(frontend, 'tests/e2e/run.ts')
    expect(existsSync(runner)).toBe(true)
    const source = readFileSync(runner, 'utf8')
    for (const flow of ['layers-dismiss.ts', 'sessions-export.ts', 'viewer-3d-idle.ts']) {
      expect(source).toContain(flow)
    }
  })

  test('CI runs lint, unit tests, and the complete browser suite', () => {
    expect(workflow).toContain('run: bun run lint')
    expect(workflow).toContain('run: bun run test:unit')
    expect(workflow).toContain('run: bun run test:e2e')
  })

  test('every CI frontend build binds generated workflows to its exact source commit', () => {
    const frontendBuildSteps = workflow.match(
      /- name: (?:build the embedded browser front door|build the frontend)[\s\S]*?run: (?:\|\n[\s\S]*?)?\s*bun run build/g,
    ) ?? []
    expect(frontendBuildSteps).toHaveLength(2)
    for (const step of frontendBuildSteps) {
      expect(step).toContain('HAUKSBEE_RELEASE_COMMIT: ${{ github.sha }}')
    }
  })

  test('the generated workflow can publish its default Checks report', async () => {
    Object.assign(globalThis, {
      __APP_VERSION__: '0.1.0',
      __RELEASE_COMMIT__: '0123456789abcdef0123456789abcdef01234567',
    })
    const { specStemFor, workflowYaml } = await import('../src/lib/ci-workflow')
    const generated = workflowYaml('power-up')
    expect(generated).toContain('permissions:\n  contents: read\n  checks: write')
    expect(generated).toContain('cancel-in-progress: true')
    expect(generated).toContain('timeout-minutes: 45')
    expect(generated.match(/persist-credentials: false/g)).toHaveLength(2)
    expect(generated).toContain("publish-report: ${{ github.event_name != 'pull_request'")
    expect(generated).toContain('ref: 0123456789abcdef0123456789abcdef01234567')
    expect(generated).toContain('hauksbee-ref: 0123456789abcdef0123456789abcdef01234567')
    expect(generated).toContain('hauksbee-version: v0.1.0')
    expect(generated).not.toContain('ref: v0.1.0')
    expect(specStemFor('my # board.kicad_pcb')).toBe('my-board')
    const injected = specStemFor('board\nspecs: stolen.kicad_pcb')
    expect(injected).toBe('board-specs-stolen')
    expect(workflowYaml(injected)).toContain('spec: ci/board-specs-stolen.toml')
    expect(workflowYaml(injected)).not.toContain('\nspecs:')
  })

  test('every default-feature Rust job installs the immutable simavr prerequisite', () => {
    const setup = readFileSync(join(repository, 'scripts/setup-ci-simavr.sh'), 'utf8')
    const installer = readFileSync(join(repository, 'scripts/install-sims.sh'), 'utf8')
    expect(setup).toContain('scripts/install-sims.sh" --avr')
    expect(setup).toContain('SIMAVR_INCLUDE_DIR')
    expect(setup).toContain('SIMAVR_LIB_DIR')
    expect(setup).toContain('SIMAVR_COMMIT')
    expect(setup).toContain('cat "$prefix/.hauksbee-simavr-commit"')
    expect(setup).toContain('Version: $simavr_tag')
    expect(installer).toContain('SIMAVR_COMMIT="f44723e8c42431136d5b4de81f789ded56d7e8fa"')
    expect(installer).toContain('fetch --depth 1 origin "$SIMAVR_COMMIT"')
    expect(installer).toContain('"refs/tags/$SIMAVR_TAG:refs/tags/$SIMAVR_TAG"')
    expect(installer).toContain('[ "$tagged_commit" = "$SIMAVR_COMMIT" ]')
    expect(installer).toContain('[ "$fetched_commit" = "$SIMAVR_COMMIT" ]')
    expect(installer).toContain('[ "$installed_commit" = "$SIMAVR_COMMIT" ]')
    expect(installer).toContain('Version: $SIMAVR_TAG')
    expect(installer).toContain('refusing to overwrite or trust an unidentified library')
    expect(installer).not.toContain('git clone --depth 1 --branch "$SIMAVR_TAG"')
    for (const job of ['clippy', 'test', 'docs', 'scenario-qc']) {
      const start = workflow.indexOf(`  ${job}:`)
      expect(start).toBeGreaterThan(-1)
      const rest = workflow.slice(start + 3)
      const nextJob = rest.search(/^ {2}[a-z][a-z0-9-]*:\n/m)
      const block = nextJob === -1 ? rest : rest.slice(0, nextJob)
      expect(block).toContain('uses: actions/cache@0057852bfaa89a56745cba8c7296529d2fc39830')
      expect(block).toContain("hashFiles('scripts/install-sims.sh', 'scripts/setup-ci-simavr.sh', 'scripts/simavr-payload-provenance.sh')")
      expect(block).toContain('run: scripts/setup-ci-simavr.sh')
    }
    const release = readFileSync(join(repository, '.github/workflows/release.yml'), 'utf8')
    const dockerWorkflow = readFileSync(join(repository, '.github/workflows/docker.yml'), 'utf8')
    const buildRs = readFileSync(join(repository, 'crates/hauksbee-mcu/build.rs'), 'utf8')
    const bundle = readFileSync(join(repository, 'scripts/bundle.sh'), 'utf8')
    const docker = readFileSync(join(repository, 'docker/Dockerfile.slim'), 'utf8')
    expect(release).toContain("hashFiles('hauksbee/Cargo.lock', 'hauksbee/scripts/install-sims.sh', 'hauksbee/scripts/simavr-payload-provenance.sh', 'hauksbee/crates/hauksbee-mcu/build.rs')")
    expect(release).not.toContain('restore-keys: |\n            release-${{ matrix.label }}-')
    expect(buildRs).toContain('cargo:rerun-if-env-changed=SIMAVR_COMMIT')
    expect(buildRs).toContain('cargo:rustc-env=HAUKSBEE_SIMAVR_COMMIT=')
    expect(bundle).toContain('source commit $SIMAVR_COMMIT')
    expect(bundle.indexOf('export SIMAVR_COMMIT')).toBeLessThan(
      bundle.indexOf('"$CARGO" build --locked --release'),
    )
    expect(bundle).toContain('.hauksbee-simavr-commit')
    expect(docker).toContain('/usr/local/.hauksbee-simavr-commit')
    expect(dockerWorkflow).toContain('workspace_version=')
    expect(dockerWorkflow).toContain('tag version $version does not match workspace package $workspace_version')
    expect(dockerWorkflow).toContain('hauksbee --version')
    expect(dockerWorkflow).toContain('hauksbee-ci --version')

    const appBuilder = readFileSync(join(repository, 'app/macos/build-app.sh'), 'utf8')
    expect(appBuilder).toContain('source commit $SIMAVR_COMMIT')
    expect(appBuilder).not.toContain('(commit: see scripts/install-sims.sh)')
    expect(appBuilder).toContain('PERMISSIVE=1')
    expect(appBuilder).toContain('BUNDLE_FLAGS+=(--shape permissive)')
    expect(appBuilder).not.toContain('BUNDLE_FLAGS+=(--shape permissive --no-default-features)')
    expect(appBuilder).toContain('NAME="hauksbee-${VERSION}-${TARGET}-permissive-app"')
    expect(appBuilder).toContain('--no-default-features app unexpectedly contains AVR')
    expect(appBuilder.indexOf('if [ "$PERMISSIVE" -eq 1 ]')).toBeLessThan(
      appBuilder.indexOf('source commit $SIMAVR_COMMIT'),
    )
    expect(buildRs).toContain('SIMAVR_INCLUDE_DIR and SIMAVR_LIB_DIR must share one prefix')

    const compliance = readFileSync(join(repository, 'COMPLIANCE.md'), 'utf8')
    expect(compliance).toContain('permissive-app.zip')
    expect(compliance).toContain('non-release app shape')
    expect(compliance).not.toContain('There is no permissive app shape')
  })
})
