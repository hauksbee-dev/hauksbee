#!/usr/bin/env bun
// The spec editor beside the builder, in a real browser against the real
// engine (it needs /api/ci/lint, /api/ci/schema and /api/ci/catalog):
//   - a comment above an assertion survives a builder edit to it;
//   - a TOML edit lands in the builder without taking focus from the editor;
//   - the last run's verdicts sit in the gutter, and a click jumps to the row;
//   - an alias kind gets the server's one-click fix within a second, and
//     applying it clears the marker;
//   - `net = "/V` completes from the board's nets;
//   - hovering a kind shows its one-liner;
//   - no confirm dialog, ever.
//
// Usage (needs a `hauksbee serve` serving this build), with a board that has
// /VBUS and /VIN nets (crates/hauksbee-ci/tests/fixtures/stormduino.kicad_pcb):
//   HB_E2E_BASE=http://127.0.0.1:3917 HB_E2E_OUT=<dir> bun run tests/e2e/toml-editor.ts <port> <board>

import type { Page } from 'playwright'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { browserFlow, note, ok, openApp, settle, shotsInto, step } from './harness'

const here = dirname(fileURLToPath(import.meta.url))
const OUT = process.env.DRIVE_OUT ?? process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-toml-editor')
const BASE = process.env.DRIVE_BASE ?? process.env.HB_E2E_BASE
const BOARD = process.argv[3] ?? join(here, '../../../crates/hauksbee-ci/tests/fixtures/stormduino.kicad_pcb')
const shoot = shotsInto(OUT)

const EDITOR = '[data-testid="toml-editor"]'

const editorText = (page: Page) => page.evaluate(sel => {
  const el = document.querySelector(`${sel} .cm-content`)
  return el ? Array.from(el.querySelectorAll('.cm-line')).map(l => l.textContent ?? '').join('\n') : ''
}, EDITOR)

/** Put the caret at the end of the editor's text. */
async function caretToEnd(page: Page) {
  await page.locator(`${EDITOR} .cm-content`).click()
  await page.keyboard.press('Control+End')
}

await browserFlow({
  out: OUT,
  viewport: { width: 1440, height: 1000 },
  timeout: 30_000,
  host: async run => { if (!BASE) throw new Error('set DRIVE_BASE or HB_E2E_BASE'); await run(BASE) },
}, async page => {
  const dialogs: string[] = []
  page.on('dialog', d => { dialogs.push(`${d.type()}: ${d.message()}`); void d.dismiss() })

  step('open the board and the Checks view')
  await openApp(page)
  await page.setInputFiles('#board-file', BOARD)
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 120_000 })
  await page.click('[data-testid="nav-checks"]')
  await page.waitForSelector('[data-testid="checks-panel"]')
  ok('one surface at a time: the builder is up first, the editor is not mounted', await page.locator(`${EDITOR} .cm-editor`).count() === 0)
  await page.click('[data-testid="mode-toml"]')
  await page.waitForSelector(`${EDITOR} .cm-content`, { timeout: 20_000 })
  ok('the editor loaded (lazily) in the spec pane', await page.locator(`${EDITOR} .cm-editor`).count() === 1)
  ok('and the builder stepped aside', await page.locator('[data-testid="add-check"]').count() === 0)
  ok('the old raw-mode toggle is gone', await page.locator('[data-testid="raw-toggle"]').count() === 0)
  await settle(page, 1_000) // the scaffolded supplies land
  await shoot(page, '01-editor-dark')

  step('a comment above an assertion survives a builder edit')
  await page.click('[data-testid="mode-builder"]')
  await page.click('[data-testid="add-check"]')
  await page.click('[data-testid="kind-voltage"]')
  await page.fill('[data-testid="check-editor"] [data-testid="field-net"]', 'GND')
  await page.fill('[data-testid="check-editor"] [data-testid="field-max"]', '0.1')
  await page.click('[data-testid="editor-add"]')
  await settle(page)
  // Put "# my note" right above that (last) [[assert]] header in the TOML.
  await page.click('[data-testid="mode-toml"]')
  await page.locator(`${EDITOR} .cm-line`, { hasText: '[[assert]]' }).last().click()
  await page.keyboard.press('Home')
  await page.keyboard.insertText('# my note\n')
  await settle(page)
  await page.click('[data-testid="mode-builder"]')
  await page.locator('label', { hasText: 'max V' }).locator('input').last().fill('0.25')
  await settle(page)
  await page.click('[data-testid="mode-toml"]')
  await page.waitForSelector(`${EDITOR} .cm-content`)
  const afterBuilder = await editorText(page)
  const at = afterBuilder.lastIndexOf('# my note')
  ok('the comment is still directly above the assertion, and only max changed',
    /# my note\n\[\[assert\]\]\nkind = "voltage"\nnet = "GND"\nmax = 0\.25/.test(afterBuilder),
    JSON.stringify(afterBuilder.slice(at, at + 80)))
  await shoot(page, '02-comment-kept')

  step('editing max in the TOML updates the builder without taking focus')
  await page.locator(`${EDITOR} .cm-line`, { hasText: 'max = 0.25' }).last().click()
  await page.keyboard.press('End')
  await page.keyboard.press('Backspace')
  await page.keyboard.press('Backspace')
  await page.keyboard.insertText('75')
  await settle(page)
  ok('focus stayed in the editor', await page.evaluate(sel => !!document.activeElement?.closest(sel), EDITOR))
  await page.keyboard.insertText(' # tuned')
  await settle(page)
  ok('typing carries on where it was', (await editorText(page)).includes('max = 0.75 # tuned'))
  await page.click('[data-testid="mode-builder"]')
  const builderMax = await page.locator('label', { hasText: 'max V' }).locator('input').last().inputValue()
  ok('the builder shows 0.75', builderMax === '0.75', builderMax)
  await page.click('[data-testid="mode-toml"]')
  await page.waitForSelector(`${EDITOR} .cm-content`)

  step('run, and the gutter carries each verdict')
  await page.getByTestId('run-checks').click()
  await page.getByTestId('run-count-pass').or(page.getByTestId('run-count-fail')).or(page.getByTestId('run-count-invalid'))
    .first().waitFor({ timeout: 120_000 }).catch(() => undefined)
  const marks = page.locator(`${EDITOR} [data-testid="gutter-result"]`)
  await marks.first().waitFor({ timeout: 10_000 }).catch(() => undefined)
  const asserts = ((await editorText(page)).match(/^\[\[assert\]\]/gm) ?? []).length
  ok('the gutter shows a verdict per [[assert]]', (await marks.count()) === asserts, `${await marks.count()} marks, ${asserts} asserts`)
  note(`verdicts: ${(await marks.evaluateAll(els => els.map(e => e.getAttribute('title')))).join(' | ')}`)
  await page.locator(EDITOR).scrollIntoViewIfNeeded()
  await shoot(page, '03-gutter-dark')
  if (await marks.count() > 0) {
    await marks.last().click()
    await settle(page, 600)
    const picked = await page.evaluate(() => (window.getSelection()?.toString() ?? ''))
    ok('clicking a verdict dot selects that assertion in the editor', /\[\[assert\]\]/.test(picked) || (await page.locator(`${EDITOR} .cm-selectionBackground`).count()) > 0, JSON.stringify(picked.slice(0, 40)))
    ok('the run summary names the verdicts and jumps to them', (await page.locator('[data-testid^="run-count-"]').count()) > 0)
    await shoot(page, '04-gutter-jump')
  }

  step('alias kind: the server lint offers its fix within a second')
  await caretToEnd(page)
  await page.keyboard.insertText('\n[[assert]]\nkind = "boot-coverage"\n')
  await page.keyboard.press('Escape')
  const t0 = Date.now()
  const alias = page.locator(`${EDITOR} .cm-lintRange`, { hasText: 'boot-coverage' })
  await alias.first().waitFor({ timeout: 3_000 }).catch(() => undefined)
  const markedIn = Date.now() - t0
  ok('a lint marker on the alias appears within 1 s of typing', (await alias.count()) > 0 && markedIn <= 1_000, `${markedIn} ms`)
  if (await alias.count() > 0) await alias.first().hover()
  const tip = page.locator('.cm-tooltip-lint')
  await tip.first().waitFor({ timeout: 3_000 }).catch(() => undefined)
  note(`lint tooltip: ${(await tip.count()) ? (await tip.first().innerText()).replace(/\n/g, ' / ') : '(none)'}`)
  const fix = page.locator('.cm-diagnosticAction', { hasText: 'Replace with boot_coverage' })
  ok('the fix reads "Replace with boot_coverage"', (await fix.count()) > 0)
  await shoot(page, '05-lint-fix-offered')
  if (await fix.count() > 0) {
    await fix.first().click()
    await settle(page, 1_000)
    const text = await editorText(page)
    ok('applying the fix rewrote the kind', text.includes('kind = "boot_coverage"') && !text.includes('boot-coverage'), text.slice(-60))
    // A missing-`net` diagnostic may still sit on the kind; only the alias one must go.
    const left = page.locator(`${EDITOR} .cm-lintRange`, { hasText: 'boot' })
    let aliasLeft = ''
    if (await left.count() > 0) {
      await left.first().hover()
      await tip.first().waitFor({ timeout: 2_000 }).catch(() => undefined)
      aliasLeft = (await tip.count()) ? await tip.first().innerText() : ''
    }
    ok('the alias marker cleared', !/boot-coverage|Replace with boot_coverage|alias/i.test(aliasLeft), aliasLeft.replace(/\n/g, ' / '))
  }
  await page.mouse.move(5, 5)

  step('net completion from the board')
  await caretToEnd(page)
  await page.keyboard.insertText('\n[[assert]]\nkind = "voltage"\nnet = "/V')
  const popup = page.locator('.cm-tooltip-autocomplete')
  await popup.waitFor({ timeout: 1_500 }).catch(() => undefined)
  if (await popup.count() === 0) {
    note('completion did not open on typing; opened explicitly with Ctrl+Space')
    await page.keyboard.press('Control+Space')
    await popup.waitFor({ timeout: 2_000 }).catch(() => undefined)
  }
  const labels = (await popup.count()) ? (await popup.locator('li').allInnerTexts()).map(s => s.split('\n')[0].trim()) : []
  ok('/VBUS is offered', labels.includes('/VBUS'), labels.slice(0, 8).join(', '))
  ok('/VIN is offered', labels.includes('/VIN'), labels.slice(0, 8).join(', '))
  await shoot(page, '06-net-completion')
  await page.keyboard.press('Escape')
  await page.keyboard.insertText('IN"\nmax = 12\n') // close the value so the text parses again

  step('hover help for a kind')
  await caretToEnd(page)
  await page.keyboard.insertText('\n[[assert]]\nkind = "rail_window"\nnet = "/VIN"\n')
  await page.keyboard.press('Escape')
  await settle(page, 300)
  const line = page.locator(`${EDITOR} .cm-line`, { hasText: 'kind = "rail_window"' }).last()
  await line.hover({ position: { x: 100, y: 8 } })
  const hover = page.getByTestId('toml-hover')
  await hover.waitFor({ timeout: 3_000 }).catch(() => undefined)
  const hoverText = (await hover.count()) ? await hover.innerText() : ''
  ok('hovering rail_window shows its one-liner', hoverText.trim().length > 10, hoverText.replace(/\n/g, ' / ').slice(0, 160))
  await shoot(page, '07-hover')
  await page.mouse.move(5, 5)

  step('light theme')
  await page.evaluate(() => { document.documentElement.dataset.theme = 'light' })
  await page.locator(EDITOR).scrollIntoViewIfNeeded()
  await shoot(page, '08-editor-light')
  await caretToEnd(page)
  await page.keyboard.insertText('\n[[assert]]\nkind = "voltage"\nnet = "/VB')
  await popup.waitFor({ timeout: 1_500 }).catch(() => undefined)
  await shoot(page, '09-completion-light')
  await page.keyboard.press('Escape')

  ok('no dialog was ever opened', dialogs.length === 0, dialogs.join(' | '))
})
