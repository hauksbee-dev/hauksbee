#!/usr/bin/env bun
// End-to-end pass over the verdict card's qualifiers, in a real browser:
// conditions on a conditional pass, and the judged scope of a partly judged run.
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
  ok('the condition says how to remove it', /put the MPN on Q1/.test(await page.locator('[data-testid="report-verdict-condition-fix"]').innerText().catch(() => '')))
  ok('the tag is the CLI word', (await page.locator('[data-testid="result-tag"]').textContent())?.trim() === 'VERDICT: clean, conditionally')
  const addCheck = page.locator('[data-testid="report-verdict-condition"] [data-testid="result-add-check"]')
  if (await addCheck.count() > 0) {
    await addCheck.first().click()
    ok('"Add as check" opens that check in the board dock', await page.waitForSelector('[data-testid="check-this"] button[aria-expanded="true"]', { timeout: 5_000 }).then(() => true, () => false))
    ok('and no modal', await page.locator('[data-testid="constraint-modal"], [aria-modal="true"]').count() === 0)
    await settle(page, 300)
  }
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
  ok('the failure lists what it found as rows', await page.locator('[data-testid="result-finding"]').count() >= 1)
  ok('the failing tag is the CLI word', (await page.locator('[data-testid="result-tag"]').textContent())?.trim() === 'VERDICT: failing')
  await shoot(page, '03-failing-desktop')
  await page.setViewportSize({ width: 390, height: 844 })
  await settle(page, 400)
  ok('no horizontal page scroll on the fail either', await noHorizontalScroll(page))
  await shoot(page, '04-failing-narrow')
  await page.setViewportSize({ width: 1440, height: 900 })

  step('a partly judged run is its own non-pass state, naming what was judged')
  await load(page, 'verdict_fet_partial.kicad_pcb')
  const partly = page.locator('[data-testid="report-verdict"] [data-testid="report-partly-judged"]')
  ok('the partly judged block sits inside the verdict card', await partly.count() === 1)
  const partlyText = await partly.innerText()
  ok('it says it is not a pass', /not a pass: partly judged/i.test(partlyText), partlyText)
  ok('it names the part with no usable model', (await page.locator('[data-testid="partly-judged-open"]').innerText()).includes('Q1'))
  ok('it says what was not judged, once, in one row', await page.locator('[data-testid="partly-judged-unjudged"]').count() === 1 && await page.locator('[data-testid="partly-judged-wiring"], [data-testid="partly-judged-electrical"]').count() === 0)
  const unjudgedNames = await page.locator('[data-testid="partly-judged-unjudged"] [data-testid="result-link"]').evaluateAll(els => els.map(e => e.getAttribute('data-name')))
  ok('each unjudged net is named once', unjudgedNames.length > 0 && new Set(unjudgedNames).size === unjudgedNames.length, JSON.stringify(unjudgedNames))
  ok('it says what was judged', /Judged: \d+ of \d+ net checks/.test(await page.locator('[data-testid="partly-judged-judged"]').innerText()))
  ok('the headline says partly judged, not OK to order', /partly judged, not a pass/i.test(await verdictText(page)) && !/OK to order/.test(await verdictText(page)))
  ok('no conditions list on a partly judged run', await page.locator('[data-testid="report-verdict-conditions"]').count() === 0)
  await shoot(page, '05-partly-judged-desktop')
  await page.setViewportSize({ width: 390, height: 844 })
  await settle(page, 400)
  ok('no horizontal page scroll when partly judged', await noHorizontalScroll(page))
  await shoot(page, '06-partly-judged-narrow')
  await page.setViewportSize({ width: 1440, height: 900 })
  await settle(page, 400)

  step('the result reaches the board: overlay, hover and click')
  const drawn = () => page.evaluate(() => (window as unknown as {
    __hbOverlay?: { highlightNets: string[]; hoverRefs: string[]; result: { openRefs: string[]; unjudgedNets: string[] } | null }
  }).__hbOverlay ?? null)
  await page.waitForFunction(() => !!(window as unknown as { __hbOverlay?: unknown }).__hbOverlay, null, { timeout: 20_000 })
  let layer = await drawn()
  ok('the board draws the result layer: Q1 outlined as having no model', !!layer?.result?.openRefs.includes('Q1'), JSON.stringify(layer?.result))
  ok('and the unjudged nets marked', (layer?.result?.unjudgedNets.length ?? 0) > 0)
  ok('a legend says what the marks mean', await page.locator('[data-testid="result-legend"]').isVisible())
  await page.click('[data-testid="result-overlay-toggle"]')
  await settle(page, 300)
  layer = await drawn()
  ok('the overlay switch turns the layer off', layer?.result === null && await page.locator('[data-testid="result-legend"]').count() === 0)
  await page.click('[data-testid="result-overlay-toggle"]')
  await settle(page, 300)
  ok('and back on', !!(await drawn())?.result)

  const netChip = page.locator('[data-testid="partly-judged-unjudged"] [data-testid="result-link"]').first()
  const netName = await netChip.getAttribute('data-name')
  await netChip.hover()
  await settle(page, 250)
  ok('hovering a net in the result lights it on the board', !!netName && !!(await drawn())?.highlightNets.includes(netName), `${netName} vs ${JSON.stringify((await drawn())?.highlightNets)}`)
  const partChip = page.locator('[data-testid="partly-judged-open"] [data-testid="result-link"][data-name="Q1"]')
  await partChip.hover()
  await settle(page, 250)
  ok('hovering a part lights the part', !!(await drawn())?.hoverRefs.includes('Q1'))
  const camBefore = await page.evaluate(() => JSON.stringify((window as unknown as { __hbCam?: unknown }).__hbCam ?? null))
  await partChip.click()
  await page.waitForSelector('[data-testid="selection-card"]', { timeout: 5_000 }).catch(() => {})
  await settle(page, 700)
  ok('clicking a part selects it on the board (its card opens)', (await page.locator('[data-testid="selection-card"]').innerText().catch(() => '')).includes('Q1'))
  const camAfter = await page.evaluate(() => JSON.stringify((window as unknown as { __hbCam?: unknown }).__hbCam ?? null))
  ok('and frames it', camBefore !== camAfter, `${camBefore} -> ${camAfter}`)
  await shoot(page, '06b-partly-judged-linked')

  step('"Give Q1 a model" opens the model flow')
  const give = page.locator('[data-testid="result-give-model-Q1"]')
  ok('the row offers to give Q1 a model', await give.count() === 1)
  await give.click()
  await settle(page, 800)
  ok('the model flow opens for Q1', await page.locator('[data-testid="write-part"], [data-testid="datasheet-extract"], [data-testid="guessed-parts"]').first().isVisible().catch(() => false))
  await shoot(page, '06c-give-model')

  step('the result can be taken away')
  ok('the CLI command is shown', /hauksbee run \S+ --check/.test(await page.locator('[data-testid="result-cli"]').innerText()))
  ok('the panel says the verdict in the CLI words', (await page.locator('[data-testid="result-tag"]').textContent())?.trim() === 'VERDICT: PARTLY JUDGED')

  step('a conditional pass and a fail are not partly judged')
  await load(page, 'generic_pmos_switch_gate_0v.kicad_pcb')
  ok('no partly judged block on the conditional pass', await page.locator('[data-testid="report-partly-judged"]').count() === 0)
  await load(page, 'generic_pmos_switch_gate_2v5.kicad_pcb')
  ok('no partly judged block on the fail', await page.locator('[data-testid="report-partly-judged"]').count() === 0)

  step('an unconditional report shows nothing extra')
  await openApp(page)
  await page.click('[data-testid="sample-watchy"]')
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 60_000 })
  ok('no conditions list on the sample', await page.locator('[data-testid="report-verdict-conditions"]').count() === 0)
})
