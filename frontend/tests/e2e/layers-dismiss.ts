#!/usr/bin/env bun
// End-to-end pass over dismissing the board viewer's Layers panel, in a real
// browser.
//
// What this exists to prove, and what nothing else in the repo can:
//   - Escape closes the panel and hands focus back to the trigger, so a keyboard
//     reader who opened it is not stranded at the top of the document;
//   - a click anywhere outside the panel closes it, because it covers a third of
//     the map and re-finding the one button that opened it is not a way out;
//   - a click INSIDE the panel does not close it, so toggling two layers in a
//     row still works;
//   - with the map expanded, Escape closes the panel FIRST and only collapses
//     the view on a second press: the innermost surface takes the first Escape;
//   - none of it logs a console error.
//
// Usage:
//   HB_E2E_BASE=http://127.0.0.1:3001 bun run tests/e2e/layers-dismiss.ts
//   bun run tests/e2e/layers-dismiss.ts          # spawns the fixture server

import { chromium } from 'playwright'
import type { Browser, ConsoleMessage, Page } from 'playwright'
import { mkdirSync, rmSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-layers')

let pass = 0
const failures: string[] = []
const notes: string[] = []

function ok(what: string, cond: boolean, detail = '') {
  if (cond) {
    pass++
    console.log(`  PASS  ${what}${detail ? ` :: ${detail}` : ''}`)
  } else {
    failures.push(`${what}${detail ? ` :: ${detail}` : ''}`)
    console.log(`  FAIL  ${what}${detail ? ` :: ${detail}` : ''}`)
  }
}
const note = (s: string) => { notes.push(s); console.log(`  note  ${s}`) }
const step = (s: string) => console.log(`\n── ${s} ──`)

/** Console output that counts as a defect, same filter as the sessions pass. */
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
const panelOpen = (page: Page) => page.locator('[data-testid="layers-panel"]').count().then(n => n > 0)

/** Is the Layers trigger the focused element? */
const triggerFocused = (page: Page) => page.evaluate(() =>
  document.activeElement?.getAttribute('data-testid') === 'layers-toggle')

async function shoot(page: Page, name: string) {
  await settle(page, 500)
  await page.screenshot({ path: join(OUT, `${name}.png`), fullPage: false })
}

async function main() {
  rmSync(OUT, { recursive: true, force: true })
  mkdirSync(OUT, { recursive: true })

  const external = process.env.HB_E2E_BASE ?? null
  let fixture: Bun.Subprocess | null = null
  const port = Number(process.env.HB_E2E_PORT ?? 3491)
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
  const ctx = await browser.newContext({ baseURL: base, viewport: { width: 1440, height: 900 } })
  const page = await ctx.newPage()
  page.setDefaultTimeout(20_000)
  watchConsole(page, 'app')

  await page.goto('/', { waitUntil: 'domcontentloaded' })
  await page.waitForSelector('[data-testid="drop-zone"]', { timeout: 30_000 })
  await page.click('[data-testid="sample-watchy"]')
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 60_000 })
  await page.click('[data-testid="nav-board"]')
  await page.waitForSelector('[data-testid="layers-toggle"]', { timeout: 30_000 })
  await settle(page, 900)

  // ── 1. The trigger still opens and closes it ────────────────────────────
  step('the trigger opens the panel')
  ok('the panel starts closed', !(await panelOpen(page)))
  await page.click('[data-testid="layers-toggle"]')
  await page.waitForSelector('[data-testid="layers-panel"]')
  ok('clicking the trigger opens the panel', await panelOpen(page))
  ok('and the trigger reports itself expanded',
    await page.locator('[data-testid="layers-toggle"]').getAttribute('aria-expanded') === 'true')
  await shoot(page, '01-layers-open')

  await page.click('[data-testid="layers-toggle"]')
  await settle(page)
  ok('re-clicking the trigger closes it (the behaviour that already worked)',
    !(await panelOpen(page)))

  // ── 2. Escape ───────────────────────────────────────────────────────────
  step('Escape closes the panel and returns focus to the trigger')
  await page.click('[data-testid="layers-toggle"]')
  await page.waitForSelector('[data-testid="layers-panel"]')
  // Move focus off the trigger first, the way tabbing into the panel would, so
  // the focus assertion is about the Escape handler and not about the click.
  await page.locator('[data-testid="layers-panel"] button').first().focus()
  ok('focus is inside the panel before Escape', !(await triggerFocused(page)))
  await page.keyboard.press('Escape')
  await settle(page)
  ok('Escape closes the panel', !(await panelOpen(page)))
  ok('and focus lands back on the Layers trigger', await triggerFocused(page))
  await shoot(page, '02-after-escape')

  // ── 3. Outside click ────────────────────────────────────────────────────
  step('a click outside closes the panel, a click inside does not')
  await page.click('[data-testid="layers-toggle"]')
  await page.waitForSelector('[data-testid="layers-panel"]')

  // Inside first: toggling a layer has to leave the panel up, or the second
  // toggle of a pair is impossible.
  const rows = page.locator('[data-testid="layers-panel"] button')
  const rowCount = await rows.count()
  ok('the panel has layer rows to click', rowCount > 0, `${rowCount} rows`)
  await rows.first().click()
  await settle(page)
  ok('toggling a layer inside the panel leaves it open', await panelOpen(page))
  await rows.first().click()
  await settle(page)
  ok('and so does toggling it back', await panelOpen(page))

  // Outside: the map canvas itself, bottom left, well clear of the panel in the
  // top right. Clamped into the viewport, because a click dispatched below the
  // window lands on nothing and would pass without ever touching the map.
  const mapBox = await page.locator('canvas').first().boundingBox()
  if (!mapBox) throw new Error('the map canvas has no box to click')
  const vp = page.viewportSize()!
  const ox = Math.min(mapBox.x + 80, vp.width - 8)
  const oy = Math.min(mapBox.y + mapBox.height - 80, vp.height - 8)
  ok('the outside-click point is inside the map and inside the window',
    ox > mapBox.x && ox < mapBox.x + mapBox.width
    && oy > mapBox.y && oy < mapBox.y + mapBox.height
    && ox < vp.width && oy < vp.height,
    `${Math.round(ox)},${Math.round(oy)} in map ${JSON.stringify(mapBox)} window ${vp.width}x${vp.height}`)
  await page.mouse.click(ox, oy)
  await settle(page)
  ok('a click on the map closes the panel', !(await panelOpen(page)), `at ${Math.round(ox)},${Math.round(oy)}`)
  await shoot(page, '03-after-outside-click')

  // And a click on a surface outside the viewer entirely.
  await page.click('[data-testid="layers-toggle"]')
  await page.waitForSelector('[data-testid="layers-panel"]')
  await page.click('[data-testid="nav-board"]')
  await settle(page)
  ok('a click on the nav rail closes the panel too', !(await panelOpen(page)))

  // ── 4. Escape precedence against the expanded view ──────────────────────
  step('the panel takes the first Escape, the expanded view takes the second')
  const fsCount = await page.locator('[data-testid="viewer-fullscreen"]').count()
  if (fsCount === 0) {
    note('this surface has no fullscreen toggle; Escape precedence not exercised here')
  } else {
    await page.click('[data-testid="viewer-fullscreen"]')
    await settle(page, 600)
    const expanded = async () =>
      await page.locator('[data-testid="viewer-fullscreen"]').getAttribute('aria-pressed') === 'true'
    ok('the map is expanded', await expanded())
    await page.click('[data-testid="layers-toggle"]')
    await page.waitForSelector('[data-testid="layers-panel"]')
    await shoot(page, '04-expanded-with-panel')

    await page.keyboard.press('Escape')
    await settle(page, 500)
    ok('the first Escape closes the panel', !(await panelOpen(page)))
    ok('and leaves the map expanded', await expanded())

    await page.keyboard.press('Escape')
    await settle(page, 600)
    ok('the second Escape collapses the map', !(await expanded()))
    await shoot(page, '05-collapsed')
  }

  // ── 5. Console ──────────────────────────────────────────────────────────
  step('console')
  ok('no console errors anywhere in the run', consoleErrors.length === 0,
    consoleErrors.slice(0, 5).join(' | '))

  await ctx.close()
  await browser.close()
  fixture?.kill()

  console.log(`\n${pass} passed, ${failures.length} failed, ${notes.length} note(s).`)
  for (const f of failures) console.log(`  FAILED: ${f}`)
  console.log(`screenshots: ${OUT}`)
  process.exit(failures.length > 0 ? 1 : 0)
}

await main()
