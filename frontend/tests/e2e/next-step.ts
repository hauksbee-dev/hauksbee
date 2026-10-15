#!/usr/bin/env bun
// The Next card follows the session, in a real browser against the real engine.
//
// Blinky alone says "add firmware"; once demo.hex has run it must never say so,
// and instead offers what the co-sim observed as checks. Adding the UART chip
// puts a "firmware must print" row in Checks, and running it passes. A rail and
// a blink chip carry their derivation, and the blink chip raises the run length.
//
// Usage (needs a `hauksbee serve` serving this build):
//   HB_E2E_BASE=http://127.0.0.1:3917 HB_E2E_OUT=<dir> bun run tests/e2e/next-step.ts

import { browserFlow, ok, openApp, settle, shotsInto, step } from './harness'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-next-step')
const ROOT = join(here, '../../..')
const shoot = shotsInto(OUT)

await browserFlow({ out: OUT, viewport: { width: 1280, height: 900 }, timeout: 90_000 }, async page => {
  const next = page.getByTestId('static-next-step')

  step('blinky without firmware')
  await openApp(page)
  await page.setInputFiles('#board-file', join(ROOT, 'frontend/public/samples/blinky.kicad_pcb'))
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 90_000 })
  ok('asks for firmware', /add firmware to see it run/i.test(await next.innerText()))
  await shoot(page, '01-no-firmware')

  step('add demo.hex')
  await page.getByTestId('open-evidence-next').click()
  await page.setInputFiles('#firmware-file', join(ROOT, 'testdata/firmware/demo/demo.hex'))
  await page.getByText('Turn what it did into checks').waitFor({ timeout: 90_000 })
  ok('never says add firmware once it ran', !/add firmware/i.test(await next.innerText()), await next.innerText())
  await page.getByTestId('suggest-uart').waitFor({ state: 'visible' })
  ok('the UART chip is there', await page.getByTestId('suggest-uart').isVisible())
  const text = await next.innerText()
  ok('it quotes what the firmware printed', text.includes('hauksbee-demo v1'), text)
  ok('the D13 chip says what was seen and that its run length is a suggestion', /D13 toggled once in 0\.2 s/.test(text) && /suggested/.test(text), text)
  ok('the rail chip carries its derived floor', /\+5V held 5\.000 V/.test(text) && /suggested min 4\.75 V/.test(text), text)
  await next.scrollIntoViewIfNeeded()
  await shoot(page, '02-firmware-ran')

  step('add the UART chip, run it')
  await page.getByTestId('suggest-uart').click()
  await page.waitForSelector('[data-testid="checks-panel"]', { timeout: 20_000 })
  const row = page.locator('.check-row', { hasText: 'The firmware must print' })
  ok('a UART row arrived in Checks', await row.count() === 1)
  await page.getByTestId('nav-board').click()
  await page.getByTestId('suggest-toggle-D13').click()
  await page.getByTestId('nav-board').click()
  await page.getByTestId('suggest-rail-+5V').click()
  const blink = page.locator('.check-row', { hasText: 'A net must blink' })
  ok('a blink row arrived too', await blink.count() === 1)
  await shoot(page, '03-checks-composed')
  await page.getByTestId('run-checks').click()
  await row.getByTestId('row-result').waitFor({ timeout: 90_000 })
  ok('the UART row passes', (await row.getByTestId('row-result').innerText()).trim() === 'PASS')
  ok('the blink row passes', (await blink.getByTestId('row-result').innerText()).trim() === 'PASS')
  const rail = page.locator('.check-row', { hasText: 'A net must sit at a voltage' })
  ok('the rail row passes', (await rail.getByTestId('row-result').innerText()).trim() === 'PASS')
  await shoot(page, '04-checks-ran')

  step('after a run the card points at CI')
  await page.getByTestId('nav-board').click()
  await settle(page, 500)
  const after = await next.innerText()
  ok('CI is the next step', /Run these in CI/.test(after) && /hauksbee-ci run ci\//.test(after), after)
  ok('no suggestions repeat', await page.getByTestId('suggest-uart').count() === 0)
  await next.scrollIntoViewIfNeeded()
  await shoot(page, '05-ci')
})
