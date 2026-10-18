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
    const counters = await card.locator('[data-testid="result-counts"] .tnum').allInnerTexts()
    const serious = Number(counters[0])
    const total = Number(counters[1])
    const tags = await card.locator('[data-testid="result-finding"]').filter({ hasText: /^\s*serious/i }).count()
    ok('the serious counter is not zero beside a SERIOUS finding', serious > 0 && tags > 0, `counter ${serious}, SERIOUS rows ${tags}`)
    ok('the card never says none serious', !/none serious|no findings/.test(text), text.slice(0, 200))
    const quoted = text.match(/(\d+) issues? found, (\d+) serious/)
    ok('the headline quotes the same numbers as the counters', !!quoted && Number(quoted[1]) === total && Number(quoted[2]) === serious, `${quoted?.[0]} vs ${serious}/${total}`)
    const section = (await page.locator('main, body').first().innerText()).replace(/\s+/g, ' ')
    ok('the section line agrees with the serious counter', new RegExp(`${total} issues? found, ${serious} serious\\.`).test(section), `serious ${serious} total ${total}`)
    ok('the card says what its rows leave out', /and \d+ more findings? in the sections below/.test(text))
    await shoot(page, `counters-${name}`)

    step(`the Checks page lists every failure it counts at ${name}px`)
    await page.click('[data-testid="nav-checks"]')
    await page.waitForSelector('[data-testid="checks-panel"]', { timeout: 20_000 })
    await page.click('[data-testid="run-checks"]')
    await page.waitForSelector('[data-testid="run-count-fail"]', { timeout: 60_000 })
    const failedByStrip = Number((await page.locator('[data-testid="run-count-fail"]').innerText()).match(/\d+/)?.[0])
    const chip = (await page.locator('header, [data-testid="header-chips"]').first().innerText().catch(() => '')) || ''
    // A FAIL chip rides on each spec row and on each board-gate result.
    const shownFails = await page.locator('[data-testid="row-result"]').filter({ hasText: /^FAIL$/ }).count()
    const gate = page.locator('[data-testid="board-gate-result"]')
    const gateFails = await gate.filter({ has: page.locator('[data-testid="row-result"]', { hasText: 'FAIL' }) }).count()
    const rowFails = shownFails - gateFails
    ok('every failure the strip counts is on the page', shownFails === failedByStrip && gateFails > 0, `${rowFails} on rows + ${gateFails} in the gate card vs ${failedByStrip} counted`)
    ok('the failure has a reason and a fix', (await gate.first().innerText()).includes('Fix:') && /not routed yet/.test(await gate.first().innerText()))
    ok('the top-bar chip does not outrun the page', !chip.includes('check failed') || gateFails + rowFails > 0, chip.slice(0, 80))
    ok('no horizontal page scroll on Checks', await noHorizontalScroll(page))
    await shoot(page, `checks-${name}`)
  }
})
