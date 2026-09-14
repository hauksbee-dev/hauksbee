#!/usr/bin/env bun
// End-to-end pass over the Settings page's "Datasheet extraction" section, in
// a real browser.
//
// What this exists to prove, and what nothing else in the repo can:
//   - the Settings rail item opens a real page reading GET /api/settings/extract,
//     and it renders the server's own summary line;
//   - a preset's "Use" button really calls the server (POST .../preset/<id>)
//     and the page reflects what came back: the backend picker shows the new
//     backend active, and the model field's placeholder is the resolved
//     default for THAT backend;
//   - editing a field marks the draft dirty (Save goes from disabled to
//     enabled) and the terminal-equivalent line updates to match;
//   - Save really PUTs the draft and shows the server's own confirmation;
//   - Test backend streams a real SSE response into the log well and reports
//     the reply;
//   - none of it logs a console error.
//
// Usage:
//   HB_E2E_BASE=http://127.0.0.1:3001 bun run tests/e2e/settings-extract.ts
//   bun run tests/e2e/settings-extract.ts          # spawns the fixture server

import { chromium } from 'playwright'
import type { Browser, ConsoleMessage, Page } from 'playwright'
import { mkdirSync, rmSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-settings-extract')

let pass = 0
const failures: string[] = []

function ok(what: string, cond: boolean, detail = '') {
  if (cond) {
    pass++
    console.log(`  PASS  ${what}${detail ? ` :: ${detail}` : ''}`)
  } else {
    failures.push(`${what}${detail ? ` :: ${detail}` : ''}`)
    console.log(`  FAIL  ${what}${detail ? ` :: ${detail}` : ''}`)
  }
}
const step = (s: string) => console.log(`\n── ${s} ──`)

const consoleErrors: string[] = []
function watchConsole(page: Page, label: string) {
  page.on('console', (m: ConsoleMessage) => {
    if (m.type() !== 'error') return
    const text = m.text()
    if (/Failed to load resource|net::ERR_|the server does not|no fixture for/.test(text)) return
    consoleErrors.push(`[${label}] ${text}`)
  })
  page.on('pageerror', e => consoleErrors.push(`[${label}] pageerror: ${e.message}`))
}

const settle = (page: Page, ms = 400) => page.waitForTimeout(ms)

async function shoot(page: Page, name: string) {
  await settle(page, 500)
  await page.screenshot({ path: join(OUT, `${name}.png`), fullPage: false })
}

async function main() {
  rmSync(OUT, { recursive: true, force: true })
  mkdirSync(OUT, { recursive: true })

  const external = process.env.HB_E2E_BASE ?? null
  let fixture: Bun.Subprocess | null = null
  const port = Number(process.env.HB_E2E_PORT ?? 3492)
  const base = external ?? `http://127.0.0.1:${port}`
  if (!external) {
    fixture = Bun.spawn(
      ['bun', 'run', join(here, '../visual-lint/fixture-server.ts'), String(port)],
      { stdout: 'inherit', stderr: 'inherit' },
    )
  }
  for (let i = 0; i < 80; i++) {
    try {
      const r = await fetch(base, { signal: AbortSignal.timeout(1000) })
      if (r.ok) break
    } catch { /* not up yet */ }
    await Bun.sleep(250)
  }
  console.log(`base: ${base}${external ? ' (external)' : ' (fixture server)'}`)

  const browser: Browser = await chromium.launch({ headless: true })
  const ctx = await browser.newContext({ baseURL: base, viewport: { width: 1280, height: 900 } })
  const page = await ctx.newPage()
  page.setDefaultTimeout(20_000)
  watchConsole(page, 'app')

  // ── 1. Open the page, read the summary ──────────────────────────────────
  step('the Settings rail item opens the page')
  await page.goto('/', { waitUntil: 'domcontentloaded' })
  await page.waitForSelector('[data-testid="drop-zone"]', { timeout: 30_000 })
  await page.click('[data-testid="nav-settings"]')
  await page.waitForSelector('[data-testid="settings-status"]', { timeout: 20_000 })
  await settle(page, 700)
  const statusText = await page.locator('[data-testid="settings-status"]').innerText()
  ok('the status strip names the active backend', statusText.includes('Claude Code (claude-opus-5, high effort)'), statusText)
  ok('the status strip names where it came from', /extract\.toml/.test(statusText), statusText)
  await shoot(page, '01-settings-loaded')

  // ── 2. A preset's Use really applies it ──────────────────────────────────
  step('the agy preset\'s Use button applies it')
  await page.click('[data-testid="settings-preset-agy-use"]')
  await page.waitForSelector('[data-testid="settings-backend-agy"][aria-pressed="true"]', { timeout: 10_000 })
  ok('the backend picker now shows agy pressed/active', true)
  const modelPlaceholder = await page.locator('[data-testid="settings-model"]').getAttribute('placeholder')
  ok('the model field placeholder is agy\'s resolved default', modelPlaceholder === 'gemini-3.8-flash', String(modelPlaceholder))
  await shoot(page, '02-agy-preset-applied')

  // ── 3. Editing a field marks the draft dirty ────────────────────────────
  step('changing effort enables Save and updates the terminal line')
  ok('Save starts disabled (preset apply already saved server-side)',
    await page.locator('[data-testid="settings-save"]').isDisabled())
  await page.click('[data-testid="settings-effort-medium"]')
  await settle(page)
  ok('Save is now enabled', !(await page.locator('[data-testid="settings-save"]').isDisabled()))
  const terminalText = await page.locator('[data-testid="settings-terminal"]').innerText()
  ok('the terminal line reflects the unsaved effort change', terminalText.includes('agy.effort=medium'), terminalText)
  await shoot(page, '03-effort-changed')

  // ── 4. Save really writes it ────────────────────────────────────────────
  step('Save PUTs the draft and confirms')
  await page.click('[data-testid="settings-save"]')
  await page.waitForSelector('[data-testid="settings-saved"]', { timeout: 10_000 })
  const savedText = await page.locator('[data-testid="settings-saved"]').innerText()
  ok('the save confirmation names the path', /extract\.toml/.test(savedText), savedText)
  ok('Save is disabled again once the draft matches the saved config',
    await page.locator('[data-testid="settings-save"]').isDisabled())
  await shoot(page, '04-saved')

  // ── 5. Test backend streams and reports ─────────────────────────────────
  step('Test backend streams a reply')
  await page.click('[data-testid="settings-test"]')
  await page.waitForSelector('[data-testid="settings-test-done"]', { timeout: 15_000 })
  const logText = await page.locator('[data-testid="settings-test-log"]').innerText()
  ok('the log well shows progress lines', logText.length > 0, logText)
  const doneText = await page.locator('[data-testid="settings-test-done"]').innerText()
  ok('the result names the reply', doneText.includes('OK'), doneText)
  await shoot(page, '05-test-done')

  // ── 6. Console ───────────────────────────────────────────────────────────
  step('console')
  ok('no console errors anywhere in the run', consoleErrors.length === 0, consoleErrors.slice(0, 5).join(' | '))

  await ctx.close()
  await browser.close()
  fixture?.kill()

  console.log(`\n${pass} passed, ${failures.length} failed.`)
  for (const f of failures) console.log(`  FAILED: ${f}`)
  console.log(`screenshots: ${OUT}`)
  process.exit(failures.length > 0 ? 1 : 0)
}

await main()
