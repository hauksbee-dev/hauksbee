import { fileURLToPath } from 'node:url'
import { join } from 'node:path'
import type { ConsoleMessage, Page } from 'playwright'

const FIXTURE_URL = /\[visual-lint fixture server\]\s+(http:\/\/127\.0\.0\.1:\d+)/

/**
 * Wait until the fixture server announces the actual port selected by the OS.
 * Keeping the server on port 0 avoids collisions with local development and
 * parallel CI jobs; watching both stdout and process exit prevents a dead
 * child from consuming the whole job timeout.
 */
export function waitForFixtureBase(
  stdout: ReadableStream<Uint8Array>,
  exited: Promise<number>,
  timeoutMs: number,
  onOutput: (chunk: string) => void,
): Promise<string> {
  return new Promise((resolve, reject) => {
    let settled = false
    let transcript = ''
    const decoder = new TextDecoder()

    const timer = setTimeout(() => {
      if (settled) return
      settled = true
      reject(new Error(`fixture server did not announce its URL within ${timeoutMs}ms`))
    }, timeoutMs)

    const succeed = (base: string) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      resolve(base)
    }

    const fail = (error: Error) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      reject(error)
    }

    void exited.then(code => {
      fail(new Error(`fixture server exited with code ${code} before announcing its URL`))
    })

    void (async () => {
      const reader = stdout.getReader()
      try {
        while (true) {
          const { done, value } = await reader.read()
          if (done) break
          const chunk = decoder.decode(value, { stream: true })
          onOutput(chunk)
          transcript += chunk
          const match = transcript.match(FIXTURE_URL)
          if (match?.[1]) succeed(match[1])
          if (transcript.length > 4_096) transcript = transcript.slice(-2_048)
        }
        const tail = decoder.decode()
        if (tail) {
          onOutput(tail)
          transcript += tail
          const match = transcript.match(FIXTURE_URL)
          if (match?.[1]) succeed(match[1])
        }
        fail(new Error('fixture server closed stdout before announcing its URL'))
      } catch (error) {
        fail(error instanceof Error ? error : new Error(String(error)))
      } finally {
        reader.releaseLock()
      }
    })()
  })
}

type KillableProcess = {
  exited: Promise<number>
  kill(signal?: number): void
}

/** Stop a child and wait for it, escalating only when graceful shutdown stalls. */
export async function terminateProcess(child: KillableProcess, timeoutMs = 5_000): Promise<void> {
  let exited = false
  const observedExit = child.exited.then(() => { exited = true })
  child.kill()
  let timer: ReturnType<typeof setTimeout> | undefined
  await Promise.race([
    observedExit,
    new Promise<void>(resolve => { timer = setTimeout(resolve, timeoutMs) }),
  ])
  if (timer) clearTimeout(timer)
  if (!exited) {
    child.kill(9)
    await child.exited
  }
}

/** Own a fixture until the whole browser flow ends, including launch failure. */
export async function withFixture(
  run: (base: string) => Promise<void>,
  external: string | null = process.env.HB_E2E_BASE ?? null,
  port = Number(process.env.HB_E2E_PORT ?? 0),
  tries = 80,
) {
  const fixture = external
    ? null
    : Bun.spawn(
        [
          'bun',
          'run',
          fileURLToPath(new URL('../visual-lint/fixture-server.ts', import.meta.url)),
          String(port),
        ],
        { stdout: 'pipe', stderr: 'inherit' },
      )
  try {
    const base = fixture
      ? await waitForFixtureBase(fixture.stdout, fixture.exited, 20_000, (chunk) =>
          process.stdout.write(chunk),
        )
      : external!
    await waitForServer(base, tries)
    console.log(`base: ${base}${external ? ' (external)' : ' (fixture server)'}`)
    await run(base)
  } finally {
    if (fixture) await terminateProcess(fixture)
  }
}

export async function waitForServer(base: string, tries = 80) {
  for (let i = 0; i < tries; i++) {
    try {
      const res = await fetch(base, { signal: AbortSignal.timeout(1000) })
      if (res.ok) return
    } catch {
      /* not up yet */
    }
    await Bun.sleep(250)
  }
  throw new Error(`nothing answering at ${base}`)
}

// ── Flow reporting ───────────────────────────────────────────────────────────
// Every browser flow keeps the same tally and prints the same PASS/FAIL lines.
// One copy here means a reader moving between flows reads one format, and a new
// flow cannot quietly invent a laxer console filter or a quieter summary.

let passed = 0
export const failures: string[] = []
export const notes: string[] = []

/** Record one expectation, naming it whether it held or not. */
export function ok(what: string, cond: boolean, detail = '') {
  const line = `${what}${detail ? ` :: ${detail}` : ''}`
  if (cond) {
    passed++
    console.log(`  PASS  ${line}`)
  } else {
    failures.push(line)
    console.log(`  FAIL  ${line}`)
  }
}

/** An observation worth printing that is neither a pass nor a failure. */
export const note = (s: string) => { notes.push(s); console.log(`  note  ${s}`) }

export const step = (s: string) => console.log(`\n── ${s} ──`)

/** Print the tally and set the exit code; `tail` names where evidence landed. */
export function summarise(tail: string, withNotes = false) {
  console.log(`\n${passed} passed, ${failures.length} failed${withNotes ? `, ${notes.length} note(s)` : ''}.`)
  for (const f of failures) console.log(`  FAILED: ${f}`)
  console.log(tail)
  process.exitCode = failures.length > 0 ? 1 : 0
}

/** Console output that counts as a defect. A failed fetch the app HANDLES (the
 *  fixture server's 501s, a live launch it reports in the UI) still logs a
 *  network error in Chromium, so only real page-level errors are collected. */
export const HANDLED_CONSOLE_NOISE = /Failed to load resource|net::ERR_|the server does not|no fixture for/

export const consoleErrors: string[] = []

/** Collect page-level errors under `label`, ignoring `ignore` matches. */
export function watchConsole(page: Page, label: string, ignore: RegExp = HANDLED_CONSOLE_NOISE) {
  page.on('console', (m: ConsoleMessage) => {
    if (m.type() !== 'error') return
    const text = m.text()
    if (ignore.test(text)) return
    consoleErrors.push(`[${label}] ${text}`)
  })
  page.on('pageerror', e => consoleErrors.push(`[${label}] pageerror: ${e.message}`))
}

export const settle = (page: Page, ms = 400) => page.waitForTimeout(ms)

/** Screenshot taker for one output directory. It settles first: a shot taken
 *  mid fade shows a half-transparent panel, which is a picture of the
 *  harness's timing rather than of the UI. */
export function shotsInto(dir: string) {
  return async (page: Page, name: string) => {
    await settle(page, 500)
    await page.screenshot({ path: join(dir, `${name}.png`), fullPage: false })
  }
}
