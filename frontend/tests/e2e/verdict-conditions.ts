#!/usr/bin/env bun
// End-to-end pass over the verdict card's conditions, in a real browser.
//
// A pass that rests on assumed corners ("Q1 passes across assumed corners ...;
// put the MPN on Q1 to remove this assumption.") must list that condition as
// its own item under the verdict, quietly, and a report with no condition (a
// fail, a clean pass) must show nothing extra. Run at a desktop and a phone
// width, with no horizontal overflow and no console errors.
//
// Usage:
//   HB_E2E_BASE=http://127.0.0.1:3001 bun run tests/e2e/verdict-conditions.ts
//   bun run tests/e2e/verdict-conditions.ts          # spawns the fixture server
//   HB_E2E_BASE=<a real `hauksbee serve` serving this build> bun run ...  # real engine

import { browserFlow, ok, openApp, settle, shotsInto, step } from './harness'
import type { Page } from 'playwright'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-verdict-conditions')
const BOARDS = join(here, '../../../crates/hauksbee-engine/tests/fixtures')
const shoot = shotsInto(OUT)

const verdictText = (page: Page) => page.locator('[data-testid="report-verdict"]').innerText()
const items = (page: Page) => page.locator('[data-testid="report-verdict-condition"]')

/** Drop a board and wait for its verdict card. */
async function load(page: Page, board: string) {
  await openApp(page)
  await page.setInputFiles('#board-file', join(BOARDS, board))
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 60_000 })
  await settle(page, 600)
}

const noHorizontalScroll = (page: Page) => page.evaluate(
  () => document.documentElement.scrollWidth <= window.innerWidth,
)

await browserFlow({ out: OUT, viewport: { width: 1440, height: 900 } }, async page => {
  step('a conditional pass lists its condition as an item')
  await load(page, 'generic_pmos_switch_gate_0v.kicad_pcb')
  ok('the conditions list is there', await page.locator('[data-testid="report-verdict-conditions"]').count() === 1)
  ok('one condition, one item', await items(page).count() === 1)
  const text = await items(page).first().innerText()
  ok('the item is the engine sentence', text.includes('Q1 passes across assumed corners') && text.includes('put the MPN on Q1'), text)
  const verdict = await verdictText(page)
  ok('the headline leads in, then the item says it once', verdict.includes('OK to order, conditionally') && verdict.split('Q1 passes across assumed corners').length === 2, verdict)
  ok('it does not read as a failure', !/serious|fix the serious/i.test(verdict), verdict)
  ok('the list sits inside the verdict card',
    await page.locator('[data-testid="report-verdict"] [data-testid="report-verdict-conditions"]').count() === 1)
  await shoot(page, '01-conditional-desktop')

  step('the same page at phone width')
  await page.setViewportSize({ width: 390, height: 844 })
  await settle(page, 400)
  ok('the item is still visible', await items(page).first().isVisible())
  ok('no horizontal page scroll', await noHorizontalScroll(page))
  // A long part-model id must wrap inside the card, not run past its edge.
  const inside = await page.evaluate(() => {
    const card = document.querySelector('[data-testid="report-verdict"]')?.getBoundingClientRect()
    const item = document.querySelector('[data-testid="report-verdict-condition"]')
    if (!card || !item) return false
    const range = document.createRange()
    range.selectNodeContents(item)
    return range.getBoundingClientRect().right <= card.right + 0.5
  })
  ok('the condition text stays inside the verdict card', inside)
  await shoot(page, '02-conditional-narrow')
  await page.setViewportSize({ width: 1440, height: 900 })

  step('a failing board shows no conditions')
  await load(page, 'generic_pmos_switch_gate_2v5.kicad_pcb')
  ok('no conditions list', await page.locator('[data-testid="report-verdict-conditions"]').count() === 0)
  ok('the failure keeps its own reason', await page.locator('[data-testid="report-verdict-reason"]').count() === 1)
  await shoot(page, '03-failing-desktop')
  await page.setViewportSize({ width: 390, height: 844 })
  await settle(page, 400)
  ok('no horizontal page scroll on the fail either', await noHorizontalScroll(page))
  await shoot(page, '04-failing-narrow')
  await page.setViewportSize({ width: 1440, height: 900 })

  step('an unconditional report shows nothing extra')
  await openApp(page)
  await page.click('[data-testid="sample-watchy"]')
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 60_000 })
  ok('no conditions list on the sample', await page.locator('[data-testid="report-verdict-conditions"]').count() === 0)
})
