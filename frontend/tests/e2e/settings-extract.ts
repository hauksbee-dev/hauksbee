#!/usr/bin/env bun
// End-to-end pass over the Settings page's "Datasheet extraction" section, in
// a real browser.
//
// What this exists to prove, and what nothing else in the repo can:
//   - the Settings rail item opens a real page reading GET /api/settings/extract,
//     and it renders the server's own summary line;
//   - a preset's "Use" button really calls the server (POST .../preset/<id>)
//     and the page reflects what came back: the backend picker shows the new
//     backend active, and the model field's placeholder is the resolved
//     default for THAT backend;
//   - editing a field marks the draft dirty (Save goes from disabled to
//     enabled) and the terminal-equivalent line updates to match;
//   - Save really PUTs the draft and shows the server's own confirmation;
//   - Test backend streams a real SSE response into the log well and reports
//     the reply;
//   - none of it logs a console error.
//
// Usage:
//   HB_E2E_BASE=http://127.0.0.1:3001 bun run tests/e2e/settings-extract.ts
//   HB_E2E_EXTRACT_ONLY=1 HB_E2E_BASE=<serve with a fake agent> ...  # step 0 only
//   bun run tests/e2e/settings-extract.ts          # spawns the fixture server

import { browserFlow, ok, openApp, settle, shotsInto, step } from './harness'
import type { ExtractBackendsConfig } from '../../src/types/report'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const repo = join(here, '../../..')
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-settings-extract')

const shoot = shotsInto(OUT)

await browserFlow({ out: OUT, viewport: { width: 1280, height: 900 } }, async (page, { base }) => {
  // ── 0. A datasheet extraction shows an estimated, honest progress bar ──
  step('extraction: phase, elapsed and an estimate that admits overrun')
  await page.goto('/', { waitUntil: 'domcontentloaded' })
  await page.waitForSelector('[data-testid="drop-zone"]', { timeout: 30_000 })
  await page.setInputFiles('#board-file', join(repo, 'crates/hauksbee-engine/tests/fixtures/plain_check_open_active_ic.kicad_pcb'))
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 30_000 })
  await page.locator('[data-testid^="extract-start-"]').first().dispatchEvent('click')
  await page.click('[data-testid="extract-consent-accept"]')
  await page.setInputFiles('[data-testid="extract-file"]', join(repo, 'testdata/datasheets/1N4148.pdf'))
  await page.click('[data-testid="extract-run"]')
  const progress = page.locator('[data-testid="extract-progress"]')
  const [bar, status] = [progress.locator('[role="progressbar"]'), progress.locator('[role="status"]')]
  const past = (pct: number) => page.waitForFunction(p => Number(document.querySelector('[role="progressbar"]')
    ?.getAttribute('aria-valuenow')) >= p, pct, { timeout: 60_000 })
  const both = async (name: string) => {
    for (const [width, height, suffix] of [[390, 844, '-narrow'], [1280, 900, '']] as const) {
      await page.setViewportSize({ width, height })
      await progress.scrollIntoViewIfNeeded()
      await shoot(page, `${name}${suffix}`)
    }
  }
  await past(1)
  await both('00a-extract-early')
  ok('early on, the status names a phase and an estimate', /left \(estimate\)/.test(await status.innerText()), await status.innerText())
  await past(75)
  await both('00b-extract-near-estimate')
  await status.filter({ hasText: 'taking longer than usual' }).waitFor({ timeout: 60_000 })
  await both('00c-extract-overrun')
  ok('past the estimate the bar stays short of full', Number(await bar.getAttribute('aria-valuenow')) < 100)
  ok('the log keeps the phases', (await page.locator('[data-testid="extract-log"]').innerText()).includes('phase: '))
  await page.waitForSelector('[role="progressbar"][aria-valuenow="100"]', { timeout: 120_000 })
  ok('the finished draft shows a full bar', (await status.innerText()) === 'Drafted')
  await shoot(page, '00d-extract-done')
  if (process.env.HB_E2E_EXTRACT_ONLY) return

  // ── 1. Open the page, read the summary ──────────────────────────────────
  step('the Settings rail item opens the page')
  await openApp(page)
  await page.click('[data-testid="nav-settings"]')
  await page.waitForSelector('[data-testid="settings-status"]', { timeout: 20_000 })
  await settle(page, 700)
  const statusText = await page.locator('[data-testid="settings-status"]').innerText()
  ok('the status strip names the active backend', statusText.includes('Claude Code (claude-opus-5-5, high effort)'), statusText)
  ok('the status strip names where it came from', /extract\.toml/.test(statusText), statusText)
  await shoot(page, '01-settings-loaded')

  // ── 2. A preset's Use really applies it ──────────────────────────────────
  step('the agy preset\'s Use button applies it')
  await page.click('[data-testid="settings-preset-agy-use"]')
  await page.waitForSelector('[data-testid="settings-backend-agy"][aria-pressed="true"]', { timeout: 10_000 })
  ok('the backend picker now shows agy pressed/active', true)
  const modelPlaceholder = await page.locator('[data-testid="settings-model"]').getAttribute('placeholder')
  ok('the model field placeholder is agy\'s resolved default', modelPlaceholder === 'gemini-3.8-flash', String(modelPlaceholder))
  await shoot(page, '02-agy-preset-applied')

  // ── 3. Editing a field marks the draft dirty ────────────────────────────
  step('changing effort enables Save and updates the terminal line')
  ok('Save starts disabled (preset apply already saved server-side)',
    await page.locator('[data-testid="settings-save"]').isDisabled())
  await page.click('[data-testid="settings-effort-medium"]')
  await page.click('[data-testid="settings-advanced-toggle"]')
  await page.locator('[data-testid="settings-command"]').fill('/tmp/custom agent')
  await settle(page)
  ok('Save is now enabled', !(await page.locator('[data-testid="settings-save"]').isDisabled()))
  const terminalText = await page.locator('[data-testid="settings-terminal"]').innerText()
  ok('the terminal line preserves effort and quotes the custom path',
    terminalText.includes('agy.effort=medium') && terminalText.includes("'agy.command=/tmp/custom agent'"), terminalText)
  await shoot(page, '03-effort-changed')

  // ── 4. Save really writes it ────────────────────────────────────────────
  step('Save PUTs the draft and confirms')
  await page.click('[data-testid="settings-save"]')
  await page.waitForSelector('[data-testid="settings-saved"]', { timeout: 10_000 })
  const savedText = await page.locator('[data-testid="settings-saved"]').innerText()
  ok('the save confirmation names the path', /extract\.toml/.test(savedText), savedText)
  ok('Save is disabled again once the draft matches the saved config',
    await page.locator('[data-testid="settings-save"]').isDisabled())
  const savedConfig = await (await fetch(`${base}/api/settings/extract`)).json()
  ok('custom executable survives saving with spaces intact', savedConfig.config.agy.command === '/tmp/custom agent')
  await page.locator('[data-testid="settings-command"]').fill('')
  ok('the terminal line also clears an existing command',
    (await page.locator('[data-testid="settings-terminal"]').innerText()).includes('agy.command='))
  const cleared = page.waitForResponse(r => r.url().endsWith('/api/settings/extract') && r.request().method() === 'PUT')
  await page.click('[data-testid="settings-save"]')
  ok('clearing the command finishes before testing', !(await (await cleared).json()).config.agy.command)
  await shoot(page, '04-saved')

  // ── 5. Codex permission mode is editable and persists ───────────────────
  step('Codex approve-for-me saves and reloads')
  await page.click('[data-testid="settings-backend-codex"]')
  await page.locator('[data-testid="settings-model-pick-gpt-6.1-sol"]').waitFor()
  const picks = (await page.locator('[data-testid="settings-model-picks"] button').allInnerTexts()).map(t => t.replace(/\s+/g, ' '))
  ok('the picker lists Sol first, recommended', picks[0]?.includes('gpt-6.1-sol') && picks[0].includes('Recommended'), picks.join(' | '))
  ok('Luna is next, labelled as the cheaper pick', picks[1]?.includes('gpt-6-luna') && picks[1].includes('much cheaper'), picks.join(' | '))
  ok('no outdated model is offered', picks.length === 2 && !/gpt-5|gpt-6-sol/.test(picks.join(' ')), picks.join(' | '))
  ok('the unset model reads Sol as the default', await page.locator('[data-testid="settings-model"]').getAttribute('placeholder') === 'gpt-6.1-sol')
  await page.click('[data-testid="settings-model-pick-gpt-6-luna"]')
  ok('choosing Luna fills the model field', await page.locator('[data-testid="settings-model"]').inputValue() === 'gpt-6-luna')
  await page.locator('[data-testid="settings-model"]').fill('gpt-5.6-sol')
  ok('an older name typed in is accepted and gets a hint, not a refusal',
    (await page.locator('[data-testid="settings-model-hint"]').innerText()).includes('gpt-6.1-sol is recommended'))
  await shoot(page, '05a-model-picker')
  await page.locator('[data-testid="settings-model"]').fill('')
  const permission = page.locator('[data-testid="settings-permission-mode"]')
  ok('Codex renders the server-populated permission selector', await permission.isVisible())
  ok('blank Codex permission uses the resolved default',
    await permission.inputValue() === '' && (await permission.locator('option').allTextContents()).includes('approve-for-me'))
  await permission.selectOption('approve-for-me')
  const codexSave = page.waitForResponse(r => r.url().endsWith('/api/settings/extract') && r.request().method() === 'PUT')
  await page.click('[data-testid="settings-save"]')
  const codexResponse = await codexSave
  const codexPayload = codexResponse.request().postDataJSON() as ExtractBackendsConfig
  ok('Save sends approve-for-me in the Codex block', codexPayload.codex?.permission_mode === 'approve-for-me')
  await page.reload({ waitUntil: 'domcontentloaded' })
  await page.click('[data-testid="nav-settings"]')
  await page.waitForSelector('[data-testid="settings-status"]')
  await page.click('[data-testid="settings-backend-codex"]')
  await page.click('[data-testid="settings-advanced-toggle"]')
  ok('approve-for-me remains selected after reload',
    await page.locator('[data-testid="settings-permission-mode"]').inputValue() === 'approve-for-me')
  await page.click('[data-testid="settings-backend-api"]')
  ok('the API backend has no permission selector',
    await page.locator('[data-testid="settings-permission-mode"]').count() === 0)
  await shoot(page, '05-codex-permission-saved')

  // ── 6. Test backend streams and reports ─────────────────────────────────
  step('Test backend streams a reply')
  await page.click('[data-testid="settings-test"]')
  await page.waitForSelector('[data-testid="settings-test-done"]', { timeout: 15_000 })
  const logText = await page.locator('[data-testid="settings-test-log"]').innerText()
  ok('the log well shows progress lines', logText.length > 0, logText)
  const doneText = await page.locator('[data-testid="settings-test-done"]').innerText()
  ok('the result names the reply', doneText.includes('OK'), doneText)
  await shoot(page, '05-test-done')
})
