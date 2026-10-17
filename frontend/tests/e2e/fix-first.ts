#!/usr/bin/env bun
// End-to-end pass over the "Fix these first" hierarchy and the pre-layout state,
// in a real browser against the real engine.
//
// An unrouted board (stormduino) must show the verdict and the fix-first list in
// the first viewport, keep "Add more evidence" collapsed, say it isn't routed
// yet, and give one open-part count everywhere. A healthy board (blinky) shows
// no fix-first list.
//
// Usage:
//   HB_E2E_BASE=http://127.0.0.1:3917 HB_E2E_OUT=<dir> bun run tests/e2e/fix-first.ts

import { browserFlow, ok, openApp, settle, shotsInto, step } from './harness'
import type { Page } from 'playwright'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-fix-first')
const BOARDS = join(here, '../../../crates/hauksbee-ci/tests/fixtures')
const shoot = shotsInto(OUT)

async function load(page: Page, board: string) {
  await openApp(page)
  await page.setInputFiles('#board-file', join(BOARDS, board))
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 90_000 })
  await settle(page, 800)
}

const inFirstViewport = (page: Page, testid: string) => page.evaluate(id => {
  const r = document.querySelector(`[data-testid="${id}"]`)?.getBoundingClientRect()
  return !!r && r.top < window.innerHeight && r.bottom > 0
}, testid)

await browserFlow({ out: OUT, viewport: { width: 1280, height: 900 }, timeout: 90_000 }, async page => {
  for (const [name, viewport] of [['desktop', { width: 1280, height: 900 }], ['phone', { width: 390, height: 844 }]] as const) {
    await page.setViewportSize(viewport)
    step(`stormduino (unrouted) at ${name}`)
    await load(page, 'stormduino.kicad_pcb')
    ok('verdict is in the first viewport', await inFirstViewport(page, 'report-verdict'))
    ok('fix-first is in the first viewport', await inFirstViewport(page, 'fix-first'))
    const height = await page.locator('[data-testid="additional-evidence-report"]').evaluate(e => e.getBoundingClientRect().height)
    ok('additional evidence is collapsed', height < 80, String(height))
    const verdict = await page.locator('[data-testid="report-verdict"]').innerText()
    ok('the verdict frames the state and does not count it as an issue', /^Not routed yet: circuit checks found \d+ issues?, none serious\./.test(verdict), verdict.slice(0, 120))
    ok('the verdict does not repeat the finding', !verdict.includes('pad group'), verdict)
    const item = await page.locator('[data-testid="fix-first-item"]').first().innerText()
    ok('fix-first says what to do, in board-level words', item.startsWith('Route the board') && item.includes('Route all the nets') && !item.includes('netlist'), item)
    ok('the pad-group detail is said at most once on the page', (await page.locator('body').innerText()).split('pad group').length <= 2)
    ok('says the board is not routed yet', (await page.locator('body').innerText()).includes('Not routed yet'))
    const banner = Number(await page.locator('[data-testid="open-parts-banner"]').getAttribute('data-open-count'))
    const cards = await page.locator('[data-testid^="open-part-"]').count()
    ok('banner count equals the open-part cards', banner === cards && cards > 0, `${banner} vs ${cards}`)
    ok('no horizontal page scroll', await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth))
    await shoot(page, `stormduino-${name}`)

    step(`blinky (healthy) at ${name}`)
    await openApp(page)
    await page.click('[data-testid="sample-blinky"]')
    await page.waitForSelector('[data-testid="report-verdict"]')
    await settle(page, 800)
    ok('no fix-first list', await page.locator('[data-testid="fix-first"]').count() === 0)
    ok('verdict says it looks healthy', (await page.locator('[data-testid="report-verdict"]').innerText()).includes('Looks healthy'))
    await shoot(page, `blinky-${name}`)
  }
})
