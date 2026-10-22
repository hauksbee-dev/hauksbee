#!/usr/bin/env bun
// "Assert this" / "Assert what I expected", driven in a real browser against a
// real engine: the blinky board runs with its firmware through the Checks view
// (so the spec holds `firmware =`), a 5 Hz D13 trace and the banner on the UART
// are played into the Live Sim over a scripted socket, and each observation is
// turned into a check. The added checks are then run by the engine and must be
// green; the expectation that does not hold must say so before it is added.
//
//   HB_E2E_BASE=http://127.0.0.1:5199 bun run tests/e2e/assert-this.ts
//
// The base must serve THIS frontend and proxy /api to a `hauksbee serve` engine
// (vite dev does, to :3001). It is not in the default fixture list because the
// fixture server cannot run a spec.

import { browserFlow, ok, openApp, settle, shotsInto, step } from './harness'
import { dirname, join, resolve } from 'node:path'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const ROOT = resolve(here, '../../..')
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-assert-this')
const shoot = shotsInto(OUT)
const BOARD = join(ROOT, 'frontend/public/samples/blinky.kicad_pcb')

if (!process.env.HB_E2E_BASE) {
  console.log('skipped: set HB_E2E_BASE to a frontend that proxies /api to a running `hauksbee serve`')
  process.exit(0)
}

/** One live frame: D13 is a 5 Hz square wave, the banner is printed once. */
const frame = (t: number, banner: boolean) => JSON.stringify({
  type: 'SimFrame', t, realtime_factor: 1,
  net_voltages: { D13: Math.floor(t * 10) % 2 === 0 ? 0 : 5, '+5V': 5, GND: 0 },
  component_states: {}, uart: banner ? { U1: [...new TextEncoder().encode('hauksbee-demo v1\n')] } : {}, faults: [],
})

await browserFlow({ out: OUT, viewport: { width: 1440, height: 1000 }, timeout: 90_000, ignore: /Failed to load resource|net::ERR_/ }, async page => {
  await page.route('**/api/live/status', route => route.fulfill({ json: { active: true, board_name: 'blinky-live' } }))
  await page.route('**/boards/blinky-live.kicad_pcb', route => route.fulfill({
    contentType: 'text/plain', body: readFileSync(BOARD, 'utf8'),
  }))
  let socket: import('playwright').WebSocketRoute | null = null
  await page.routeWebSocket('**/ws', ws => {
    socket = ws
    ws.send(JSON.stringify({
      type: 'BoardInfo', name: 'blinky-live', board_url: '/boards/blinky-live.kicad_pcb', num_components: 8, num_nets: 6,
      nets: ['D13', '+5V', 'GND', 'LED_A'], component_kinds: { U1: 'mcu' }, mcus: [['U1', 'avr']],
    }))
    ws.send(JSON.stringify({ type: 'Backlog' }))
  })

  step('the engine reads the board and the firmware, so the spec holds `firmware =`')
  await openApp(page)
  await page.setInputFiles('#board-file', BOARD)
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 90_000 })
  await page.getByTestId('open-evidence-next').click()
  await page.setInputFiles('#firmware-file', join(ROOT, 'testdata/firmware/demo/demo.hex'))
  await page.getByText('Turn what it did into checks').waitFor({ timeout: 90_000 })

  step('the live view plays a 5 Hz D13 trace and the banner')
  await page.getByTestId('nav-sim').click()
  // The server reports a live session (scripted below), so the app asks which to use.
  await page.getByRole('button', { name: 'Open the running session' }).click({ timeout: 5000 }).catch(() => {})
  await page.waitForSelector('[data-testid="sim-scope"]', { timeout: 8000 })
  await page.getByTestId('probe-add').click()
  await page.getByTestId('probe-add-option').filter({ hasText: /^D13$/ }).click()
  // Paced: the app coalesces frames that arrive faster than it paints, and a
  // coalesced trace would lose the edges this flow is about.
  for (let i = 0; i < 160; i++) {
    socket!.send(frame(i * 0.01, i === 0))
    await page.waitForTimeout(22)
  }
  await settle(page, 400)
  await shoot(page, '01-live-scope')

  step('Assert this on the D13 trace')
  await page.getByTestId('assert-this-scope').click()
  const editor = page.locator('[data-testid="check-editor"]').first()
  await editor.waitFor()
  ok('the toggle check opens', await editor.getAttribute('data-kind') === 'toggle')
  const freq = Number(await editor.locator('[data-testid="field-freq_hz"]').inputValue())
  ok('with the observed rate filled in (about 5 Hz)', Math.abs(freq - 5) < 0.5, String(freq))
  ok('and says where it came from', /this trace/.test(await editor.innerText()))
  await shoot(page, '02-assert-this-trace')
  await editor.locator('[data-testid="editor-add"]').click()
  await page.getByTestId('assert-added').waitFor()

  step('Assert what I expected on the trace: a wrong expectation says so')
  await page.getByTestId('assert-expected-scope').click()
  const expected = page.locator('[data-testid="check-editor"]').first()
  await expected.waitFor()
  ok('the rate is left empty for the user', await expected.locator('[data-testid="field-freq_hz"]').inputValue() === '')
  await expected.locator('[data-testid="field-freq_hz"]').fill('20')
  await page.getByTestId('preview-would-fail').waitFor()
  ok('the preview says it would fail today', await page.getByTestId('preview-would-fail').isVisible())
  await shoot(page, '03-expected-would-fail')
  await expected.locator('[data-testid="field-freq_hz"]').fill('5')
  await page.getByTestId('preview-would-pass').waitFor()
  await page.getByTestId('assert-expected-scope').click()

  step('Assert this on the UART line')
  await page.getByTestId('sim-dock-open').click().catch(() => {})
  await page.getByText('Serial', { exact: true }).first().click().catch(() => {})
  await page.getByTestId('assert-this').click()
  const uart = page.locator('[data-testid="check-editor"][data-kind="uart"]')
  await uart.waitFor()
  ok('the uart check opens with the printed line', await uart.locator('[data-testid="field-contains"]').inputValue() === 'hauksbee-demo v1')
  await shoot(page, '04-assert-this-uart')
  await uart.locator('[data-testid="editor-add"]').click()

  step('the checks are in the spec and the engine runs them green')
  await page.getByTestId('assert-added-open').first().click()
  await page.waitForSelector('[data-testid="checks-panel"]')
  const toggle = page.locator('.check-row', { hasText: 'A net toggles at a rate' })
  const uartRow = page.locator('.check-row', { hasText: 'Firmware prints a string' })
  ok('a toggle row arrived', await toggle.count() >= 1)
  ok('a uart row arrived', await uartRow.count() >= 1)
  await page.getByTestId('run-checks').click()
  await toggle.first().getByTestId('row-result').waitFor({ timeout: 90_000 })
  await uartRow.first().getByTestId('row-result').waitFor({ timeout: 90_000 })
  ok('the toggle row passes', (await toggle.first().getByTestId('row-result').innerText()).trim() === 'PASS')
  ok('the uart row passes', (await uartRow.first().getByTestId('row-result').innerText()).trim() === 'PASS')
  await shoot(page, '05-checks-green')
})
