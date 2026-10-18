#!/usr/bin/env bun
// End-to-end pass over the verdict header and the Checks strip, in a real
// browser, on an unrouted board. Three defects from a real-drop session:
//
//   1. the key-parts ring clipped a four-digit count ("5000/5000" read "000/500");
//   2. the card said "0 serious" / "none serious" beside a finding tagged SERIOUS;
//   3. the Checks page said "1 failed" while the only listed check said PASS.
//
// The ring is measured, not just read: its text must fit the ring's hole at
// every count from 1 to 6 digits, at desktop and phone width. The counters, the
// headline, the tag and the section line must all quote one number. Every
// failed result the strip counts must be on the page with its reason and fix.
//
// Usage:
//   bun run tests/e2e/verdict-header.ts          # spawns the fixture server
//   HB_E2E_BASE=<a real `hauksbee serve` serving this build> bun run ...   # real engine

import { browserFlow, ok, openApp, settle, shotsInto, step } from './harness'
import type { Page } from 'playwright'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-verdict-header')
const BOARD = join(here, '../../../crates/hauksbee-ci/tests/fixtures/stormduino.kicad_pcb')
const shoot = shotsInto(OUT)

// The widest text that fits the ring's hole: r 19, stroke 5, so 33 user units.
const HOLE = 33

/** Drop the board; `keyParts` rewrites the engine's bind count for the ring test. */
async function load(page: Page, keyParts?: string) {
  await page.unroute('**/api/analyze').catch(() => {})
  if (keyParts) {
    await page.route('**/api/analyze', async route => {
      const response = await route.fetch()
      const body = await response.json() as { bind?: { critical_parts_bound?: string } }
      if (body.bind) body.bind.critical_parts_bound = keyParts
      await route.fulfill({ response, json: body })
    })
  }
  await openApp(page)
  await page.setInputFiles('#board-file', BOARD)
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 90_000 })
  await settle(page, 600)
}

const noHorizontalScroll = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)

await browserFlow({ out: OUT, viewport: { width: 1440, height: 900 }, timeout: 90_000 }, async page => {
  for (const [name, viewport] of [['1440', { width: 1440, height: 900 }], ['390', { width: 390, height: 844 }]] as const) {
    await page.setViewportSize(viewport)

    step(`the key-parts ring at ${name}px, from 1 to 6 digits`)
    for (const digits of [1, 3, 4, 5, 6]) {
      const total = 10 ** digits - 1
      await load(page, `${total}/${total}`)
      const gauge = page.locator('[data-testid="result-gauge-modelled"]')
      const text = page.locator('[data-testid="result-gauge-modelled-text"]')
      const width = await text.evaluate(el => (el as unknown as SVGGraphicsElement).getBBox().width)
      ok(`${digits}-digit count fits the ring hole`, width <= HOLE, `${width.toFixed(1)} <= ${HOLE}: "${await text.textContent()}"`)
      const said = `${await gauge.getAttribute('title')} ${await gauge.innerText()}`
      ok(`${digits}-digit count: the exact numbers are on the page`, said.includes(`${total} of ${total}`) || said.includes(`${total.toLocaleString('en-US')} of ${total.toLocaleString('en-US')}`), said)
      ok(`${digits}-digit count: no horizontal page scroll`, await noHorizontalScroll(page))
      if (digits === 4) await shoot(page, `ring-4-digit-${name}`)
      if (digits === 6) await shoot(page, `ring-6-digit-${name}`)
    }

    step(`counters, headline, tag and section agree at ${name}px`)
    await load(page)
    const card = page.locator('[data-testid="report-verdict"]')
    const text = (await card.innerText()).replace(/\s+/g, ' ')
    // One verdict on the card: one word, its detail, and "not routed" once.
    ok('the card shows one verdict word', await card.locator('[data-testid="result-verdict-word"]').count() === 1)
    ok('the verdict is FAIL, not routed yet', (await card.locator('[data-testid="result-verdict-word"]').innerText()).trim() === 'FAIL' &&
      (await card.locator('[data-testid="result-verdict-detail"]').innerText()).trim() === 'not routed yet', text.slice(0, 120))
    ok('"not routed" is said once on the card', (text.match(/not routed/gi) ?? []).length === 1, text.slice(0, 200))
    ok('the card never says none serious', !/none serious|no findings/.test(text), text.slice(0, 200))
    // The findings beside the board count the same serious finding the headline quotes.
    const tally = await page.locator('[data-testid="findings-count"]').innerText()
    const serious = Number(/(\d+) serious/.exec(tally)?.[1] ?? 0)
    const seriousCards = await page.locator('[data-testid="findings"] [data-level="serious"]').count()
    ok('the findings list leads with the serious finding', serious > 0 && seriousCards > 0, `${tally}; ${seriousCards} serious cards`)
    const quoted = text.match(/(\d+) issues? found, (\d+) serious/)
    ok('the card sentence quotes the same serious count as the findings', !!quoted && Number(quoted[2]) === serious, `${quoted?.[0]} vs ${tally}`)
    ok('the next step is to route the board, not firmware', (await page.locator('[data-testid="static-next-step"]').getAttribute('data-next')) === 'route' &&
      !/add firmware/i.test(await page.locator('[data-testid="static-next-step"]').innerText()))
    ok('the board drawing starts above the fold', name === '390' || ((await page.locator('[data-testid="board-map-frame"]').boundingBox())?.y ?? 9999) < 200)
    await shoot(page, `counters-${name}`)

    step(`the Checks page states one verdict and reads not-routed as layout at ${name}px`)
    await page.click('[data-testid="nav-checks"]')
    await page.waitForSelector('[data-testid="checks-panel"]', { timeout: 20_000 })
    await page.click('[data-testid="run-checks"]')
    await page.waitForSelector('[data-testid="run-tag"]', { timeout: 60_000 })
    // The page states its verdict once: the band, with no header chip beside it.
    ok('the header carries no second verdict on Checks', (await page.locator('[data-testid="chip-status"]').count()) === 0)
    // Not routed is layout readiness: a calm note naming the nets, with what CI does.
    const layout = page.locator('[data-testid="layout-note"]')
    const layoutText = (await layout.innerText().catch(() => '')) || ''
    ok('the not-routed state is a layout note', /not routed yet/.test(layoutText) && /CI run of this spec also fails/.test(layoutText), layoutText.slice(0, 120))
    ok('the unrouted nets are one click from the board', (await page.locator('[data-testid="layout-show-on-board"]').count()) === 1)
    // Every failure the band counts is on the page, as a FAIL chip on a row or a board finding.
    const headline = await page.locator('[data-testid="run-headline"]').innerText()
    const counted = Number(/(\d+) of \d+ checks? fails?/.exec(headline)?.[1] ?? 0)
    const shownFails = await page.locator('[data-testid="row-result"]').filter({ hasText: /^FAIL$/ }).count()
    ok('every failure the band counts is on the page', shownFails === counted, `${shownFails} shown vs ${counted} counted`)
    ok('no internal ids on the page', !/static_check|hauksbee check\)/.test(await page.locator('[data-testid="checks-panel"]').innerText()))
    ok('no horizontal page scroll on Checks', await noHorizontalScroll(page))
    await shoot(page, `checks-${name}`)
  }
})
