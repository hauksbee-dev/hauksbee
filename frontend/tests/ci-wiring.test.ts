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
})
