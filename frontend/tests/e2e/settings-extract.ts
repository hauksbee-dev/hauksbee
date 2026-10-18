#!/usr/bin/env bun
// End-to-end pass over the Settings page's "Datasheet extraction" section, in
// a real browser.
//
// What this exists to prove, and what nothing else in the repo can:
//   - the Settings rail item opens a real page reading GET /api/settings/extract,
//     and it renders the server's own summary line;
//   - "Change" opens the one place to choose; a preset's "Use" button really
//     calls the server (POST .../preset/<id>) and the page reflects what came
//     back: the row reads Active, the Custom backend list shows the new backend
//     selected, and the model field's placeholder is the resolved default for
//     THAT backend;
//   - exactly one setup is marked Recommended, and the escalation help agrees
//     with the escalation option that is selected;
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
  // The guessed-parts group starts collapsed; open it as a user would.
  const closedParts = page.locator('[data-testid="guessed-parts"]:not([open]) > summary')
  if (await closedParts.count()) await closedParts.click()
  await page.locator('[data-testid="write-part-open"]').dispatchEvent('click')
  await page.locator('[data-testid="model-tab-draft"]').dispatchEvent('click')
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

  // ── 0b. The live agent panel follows `activity:` events ─────────────────
  step('extraction: activity events fill the agent panel and stay out of the log')
  const activity = (e: object) => ({ event: 'log', data: `activity: ${JSON.stringify(e)}` })
  const usage = (extra: object) => activity({
    t: 3, kind: 'usage', model: 'claude-haiku-5-5', context: 9000, input: 1, output: 1, cache_read: 0, cache_write: 0,
    cost_usd: 0.004, total_tokens: 12345, total_cost_usd: 0.0123, ...extra,
  })
  /** Runs one extraction against a canned stream that ends in an error, so the
   *  panel and the log are both on screen, and returns the panel. */
  const runCanned = async (frames: { event: string; data: string }[]) => {
    await page.route('**/api/models/extract', route => route.fulfill({
      status: 200,
      headers: { 'content-type': 'text/event-stream' },
      body: frames.map(f => `event: ${f.event}\ndata: ${f.data}\n\n`).join(''),
    }))
    await page.setViewportSize({ width: 1280, height: 900 })
    await page.goto('/', { waitUntil: 'domcontentloaded' })
    await page.waitForSelector('[data-testid="drop-zone"]', { timeout: 30_000 })
    await page.setInputFiles('#board-file', join(repo, 'crates/hauksbee-engine/tests/fixtures/plain_check_open_active_ic.kicad_pcb'))
    await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 30_000 })
    // The guessed-parts group starts collapsed; open it as a user would.
    const closedParts = page.locator('[data-testid="guessed-parts"]:not([open]) > summary')
    if (await closedParts.count()) await closedParts.click()
    await page.locator('[data-testid="write-part-open"]').dispatchEvent('click')
    await page.locator('[data-testid="model-tab-draft"]').dispatchEvent('click')
    await page.click('[data-testid="extract-consent-accept"]')
    await page.setInputFiles('[data-testid="extract-file"]', join(repo, 'testdata/datasheets/1N4148.pdf'))
    await page.click('[data-testid="extract-run"]')
    await page.waitForSelector('[data-testid="extract-failed"]', { timeout: 15_000 })
    await page.unroute('**/api/models/extract')
    return page.locator('[data-testid="agent-activity"]')
  }
  const tail = [{ event: 'error', data: 'the fixture stops here' }]
  const spendOf = async (panel: ReturnType<typeof page.locator>) =>
    (await panel.locator('[data-testid="agent-activity-spend"]').innerText()).replace(/\s+/g, ' ')

  const panel = await runCanned([
    { event: 'log', data: 'phase: reading the datasheet' },
    activity({ t: 0.5, kind: 'tier', model: 'claude-haiku-5-5', tier: 1, tiers: 3, harness: 'staged', budget: 100000, label: 'Haiku 5.5', badge: 'below Luna baseline' }),
    activity({ t: 1, kind: 'stage', stage: 'locate', attempt: 1, attempts: 2 }),
    activity({ t: 2, kind: 'read', path: 'pages/p7.png', page: 7 }),
    { event: 'log', data: 'activity: {this is not json' },
    usage({ billing: 'metered' }),
    activity({ t: 4, kind: 'validator', ok: false, error: 'pin 3 missing' }),
    activity({ t: 5, kind: 'escalate', from: 'claude-haiku-5-5', to: 'claude-sonnet-5-5', reason: 'validator failed' }),
    activity({ t: 6, kind: 'tier', model: 'claude-sonnet-5-5', tier: 2, tiers: 3, harness: 'single', budget: 0 }),
    ...tail,
  ])
  const panelText = await panel.innerText()
  ok('the panel shows the current tier and harness', /claude-sonnet-5-5/.test(panelText) && /Attempt 2 of 3/.test(panelText), panelText)
  ok('the panel lists the page read', (await panel.locator('[data-testid="agent-activity-pages"]').innerText()).includes('7'))
  ok('API billing shows tokens and the list-price dollar cost', /12\.3k tokens/.test(panelText) && panelText.includes('$0.0123 list price'), panelText)
  ok('the panel shows the escalation', /claude-haiku-5-5\s*→\s*claude-sonnet-5-5: validator failed/.test(
    await panel.locator('[data-testid="agent-activity-escalations"]').innerText()))
  ok('no activity line leaks into the plain log', !(await page.locator('body').innerText()).includes('activity: '))
  await shoot(page, '00e-agent-activity')

  // A tier the server labels and flags shows both, as sent.
  const flagged = await runCanned([
    activity({ t: 0.5, kind: 'tier', model: 'claude-haiku-5-5', tier: 1, tiers: 2, harness: 'staged', budget: 100000, label: 'Haiku 5.5', badge: 'below Luna baseline' }),
    ...tail,
  ])
  const tierText = await flagged.locator('[data-testid="agent-activity-tier"]').innerText()
  ok('a server label and baseline badge are shown as sent', tierText.includes('Haiku 5.5') && tierText.includes('below Luna baseline'), tierText)

  // Every billing state: dollars only for metered billing, never as a fallback.
  // The server also leaves cost null unless metered; a stray number is still not shown.
  const plan = (extra: object) => activity({
    t: 9, kind: 'plan', tool: 'claude', available: true,
    before: { week: 46, session: 4 }, after: { week: 47, session: 6 }, ...extra,
  })
  const billingStates: { name: string; frames: { event: string; data: string }[]; shows: RegExp; never?: RegExp }[] = [
    { name: 'metered billing', frames: [usage({ billing: 'metered' })], shows: /\$0\.0123 list price/, never: /from your plan|Plan usage/ },
    { name: 'a plan run with both windows', frames: [usage({ billing: 'plan', cost_usd: null, total_cost_usd: null }), plan({})],
      shows: /from your plan.*Plan usage: week 46% \u2192 47% \(\+1\), 5-hour 4% \u2192 6% \(\+2\)/, never: /\$/ },
    { name: 'a plan run with the 5-hour window missing', frames: [usage({ billing: 'plan' }), plan({ before: { week: 46, session: null }, after: { week: 47, session: null } })],
      shows: /Plan usage: week 46% \u2192 47% \(\+1\)/, never: /\$|5-hour/ },
    { name: 'a plan run whose usage is unavailable', frames: [usage({ billing: 'plan' }), plan({ available: false, before: undefined, after: undefined })],
      shows: /from your plan.*usage unavailable/, never: /\$|%/ },
    { name: 'a plan run before its plan event', frames: [usage({ billing: 'plan' })], shows: /from your plan/, never: /\$|%/ },
    { name: 'a server that says billing is unknown', frames: [usage({ billing: 'unknown' })], shows: /billing unknown/, never: /\$/ },
    { name: 'a server that reports a cost but no billing mode', frames: [usage({})], shows: /billing unknown/, never: /\$/ },
  ]
  for (const { name, frames: stateFrames, shows, never } of billingStates) {
    const p = await runCanned([...stateFrames, ...tail])
    const spend = await spendOf(p)
    ok(`${name}: ${shows}`, shows.test(spend) && (!never || !never.test(spend)), spend)
    await shoot(page, `00f-spend-${name.replace(/[^a-z]+/gi, '-').toLowerCase()}`)
  }
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
  step('Change opens the one chooser; the agy preset\'s Use button applies it')
  ok('the page starts with the current choice only, no chooser', await page.locator('[data-testid="settings-choose"]').count() === 0)
  await page.click('[data-testid="settings-change"]')
  const tags = await page.locator('[data-testid="settings-choose"] [data-testid$="-tag"]').allInnerTexts()
  ok('exactly one setup is marked Recommended', tags.filter(t => /recommended/i.test(t)).length === 1, tags.join(' | '))
  ok('the rest of the setups are behind More setups', await page.locator('[data-testid="settings-preset-agy"]').count() === 0)
  await page.click('[data-testid="settings-more-toggle"]')
  await page.click('[data-testid="settings-preset-agy-use"]')
  await page.waitForSelector('[data-testid="settings-preset-agy-use"]:disabled', { timeout: 10_000 })
  await page.click('[data-testid="settings-custom-toggle"]')
  await page.waitForSelector('[data-testid="settings-backend-agy"][aria-pressed="true"]', { timeout: 10_000 })
  ok('the custom backend list now shows agy selected', true)
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
  ok('the picker lists Sol first, as the most accurate', picks[0]?.includes('gpt-6.1-sol') && picks[0].includes('Most accurate'), picks.join(' | '))
  ok('a model note never adds a second Recommended marker', !picks.join(' ').includes('Recommended'), picks.join(' | '))
  ok('Luna is next, labelled as the cheaper pick', picks[1]?.includes('gpt-6-luna') && picks[1].includes('much cheaper'), picks.join(' | '))
  ok('no outdated model is offered', picks.length === 2 && !/gpt-5|gpt-6-sol/.test(picks.join(' ')), picks.join(' | '))
  ok('the unset model reads Sol as the default', await page.locator('[data-testid="settings-model"]').getAttribute('placeholder') === 'gpt-6.1-sol')
  await page.click('[data-testid="settings-model-pick-gpt-6-luna"]')
  ok('choosing Luna fills the model field', await page.locator('[data-testid="settings-model"]').inputValue() === 'gpt-6-luna')
  await page.locator('[data-testid="settings-model"]').fill('gpt-5.6-sol')
  ok('an older name typed in is accepted and gets a hint, not a refusal',
    (await page.locator('[data-testid="settings-model-hint"]').innerText()).includes('gpt-6.1-sol is the default'))
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
  await page.click('[data-testid="settings-change"]')
  await page.click('[data-testid="settings-custom-toggle"]')
  await page.click('[data-testid="settings-backend-codex"]')
  await page.click('[data-testid="settings-advanced-toggle"]')
  ok('approve-for-me remains selected after reload',
    await page.locator('[data-testid="settings-permission-mode"]').inputValue() === 'approve-for-me')
  await page.click('[data-testid="settings-backend-api"]')
  ok('the API backend has no permission selector',
    await page.locator('[data-testid="settings-permission-mode"]').count() === 0)
  await shoot(page, '05-codex-permission-saved')

  // ── 5b. Escalation ladder: shown on presets, editable on agent backends ──
  step('the escalate ladder shows on a preset and is editable')
  const ladderPreset = await page.locator('[data-testid="settings-preset-claude-code"]').innerText()
  ok('the balanced preset names its ladder', /Haiku 5\.5 → Sonnet 5\.5 → Opus 5\.5/.test(ladderPreset), ladderPreset)
  ok('a preset the server flags shows its baseline badge as sent',
    (await page.locator('[data-testid="settings-preset-claude-code-haiku-badge"]').innerText()).toLowerCase().includes('below luna baseline'))
  await page.click('[data-testid="settings-backend-claude-code"]')
  ok('a model the server flags shows its badge',
    (await page.locator('[data-testid="settings-model-badge-claude-haiku-5-5"]').innerText()).includes('below Luna baseline'))
  ok('a harnessed model is marked with its stage size',
    (await page.locator('[data-testid="settings-model-pick-claude-haiku-5-5"]').innerText()).includes('harnessed: ~100k-token stages'))
  const terminal = () => page.locator('[data-testid="settings-terminal"]').innerText()
  await page.click('[data-testid="settings-escalate-none"]')
  ok('No escalation is the terminal spelling of an empty ladder', (await terminal()).includes('claude-code.escalate=none'))
  ok('the escalation help agrees with No escalation',
    /^No escalation/.test(await page.locator('[data-testid="settings-escalate-effective"]').innerText()))
  await page.click('[data-testid="settings-escalate-pick-claude-sonnet-5-5"]')
  await page.click('[data-testid="settings-escalate-pick-claude-haiku-5-5"]')
  ok('the terminal line spells the ladder comma-separated, in the order picked',
    (await terminal()).includes('claude-code.escalate=claude-sonnet-5-5,claude-haiku-5-5'))
  ok('a picked model shows its position', (await page.locator('[data-testid="settings-escalate-pick-claude-haiku-5-5"]').innerText()).includes('2.'))
  await shoot(page, '05b-escalate')
  await page.click('[data-testid="settings-escalate-default"]')
  ok('Default leaves the setting blank', !(await terminal()).includes('claude-code.escalate='))
  // The fixture's default for Claude Code is no ladder: the option's name and the
  // help must then both say so (they used to say "Default ladder" and "no escalation").
  const defaultName = await page.locator('[data-testid="settings-escalate-default"]').innerText()
  const defaultHelp = await page.locator('[data-testid="settings-escalate-effective"]').innerText()
  ok('the Default option and its help say the same thing about escalation',
    /no escalation/i.test(defaultName) === /no escalation/i.test(defaultHelp) && !/ladder/i.test(defaultName), `${defaultName} | ${defaultHelp}`)

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
