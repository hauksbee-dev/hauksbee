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
    ok('the next step is in the first viewport', await inFirstViewport(page, 'static-next-step'))
    const height = await page.locator('[data-testid="additional-evidence-report"]').evaluate(e => e.getBoundingClientRect().height)
    ok('additional evidence is collapsed', height < 80, String(height))
    const verdict = await page.locator('[data-testid="report-verdict"]').innerText()
    ok('the verdict frames the state and counts it as the serious finding it is listed as', /FAIL\s+not routed yet\b.*(\d+ issues? found, [1-9]\d* serious, including the missing routing|the board cannot be ordered as is)/.test(verdict.replace(/\s+/g, ' ')), verdict.slice(0, 160))
    ok('the verdict does not repeat the finding', !verdict.includes('pad group'), verdict)
    const nextText = await page.locator('[data-testid="static-next-step"]').innerText()
    ok('the one next step is to route the board, with the re-drop button', /Route the board in your layout editor/.test(nextText) && await page.locator('[data-testid="next-redrop"]').count() === 1, nextText)
    ok('the next step does not offer firmware', !/add firmware/i.test(nextText))
    const routeCard = await page.locator('[data-testid="findings"] [data-level="serious"]').first().innerText()
    ok('the serious finding says what to do, in board-level words', routeCard.includes('Route the board') && routeCard.includes('Route all the nets'), routeCard)
    ok('the pad-group detail is said at most once on the page', (await page.locator('body').innerText()).split('pad group').length <= 2)
    ok('says the board is not routed yet', /not routed yet/i.test(await page.locator('body').innerText()))
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
