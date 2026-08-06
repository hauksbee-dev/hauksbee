#!/usr/bin/env bun
// One release gate for the browser flows that are too stateful for unit tests.
// A single fixture server gets an OS-selected port and is always reaped. Each
// flow remains independently runnable, while CI has one exhaustive command.

import { mkdirSync } from 'node:fs'
import { basename, dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { terminateProcess, waitForFixtureBase } from './harness'

const here = dirname(fileURLToPath(import.meta.url))
const frontend = join(here, '../..')
const results = join(frontend, 'test-results/e2e')

export const E2E_FLOWS = [
  'layers-dismiss.ts',
  'sessions-export.ts',
  'viewer-3d-idle.ts',
] as const

async function main(): Promise<void> {
  mkdirSync(results, { recursive: true })

  const fixture = Bun.spawn(
    ['bun', 'run', join(here, '../visual-lint/fixture-server.ts'), '0'],
    { cwd: frontend, stdout: 'pipe', stderr: 'inherit' },
  )
  if (!(fixture.stdout instanceof ReadableStream)) {
    throw new Error('fixture server stdout was not captured')
  }

  let running: Bun.Subprocess | null = null
  let interrupted = false
  const stopChildren = () => {
    interrupted = true
    running?.kill()
    fixture.kill()
  }
  process.once('SIGINT', stopChildren)
  process.once('SIGTERM', stopChildren)

  try {
    const base = await waitForFixtureBase(
      fixture.stdout,
      fixture.exited,
      20_000,
      chunk => process.stdout.write(chunk),
    )
    console.log(`[e2e] shared fixture: ${base}`)

    for (const flow of E2E_FLOWS) {
      if (interrupted) throw new Error('end-to-end run interrupted')
      console.log(`\n[e2e] ${flow}`)
      running = Bun.spawn(['bun', 'run', join(here, flow)], {
        cwd: frontend,
        env: {
          ...process.env,
          HB_E2E_BASE: base,
          HB_E2E_OUT: join(results, basename(flow, '.ts')),
        },
        stdout: 'inherit',
        stderr: 'inherit',
      })
      const code = await running.exited
      running = null
      if (code !== 0) {
        throw new Error(`${flow} failed with exit code ${code}`)
      }
    }
  } finally {
    process.off('SIGINT', stopChildren)
    process.off('SIGTERM', stopChildren)
    if (running) await terminateProcess(running)
    await terminateProcess(fixture)
  }
}

if (import.meta.main) {
  await main().catch(error => {
    console.error(`[e2e] ${error instanceof Error ? error.message : String(error)}`)
    process.exitCode = 1
  })
}
