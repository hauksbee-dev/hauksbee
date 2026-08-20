#!/usr/bin/env bun
// End-to-end pass over saved sessions and report export, in a real browser.
//
// What this exists to prove, and what nothing else in the repo can:
//   - a session is written, survives a full page reload, and comes back with
//     its composed checks attached;
//   - the portable exports produce real files, and the HTML one renders STANDALONE:
//     it is opened again from disk in a fresh browser context, with no server
//     and no network, and has to still be a styled report;
//   - the named-session operations (switch, rename, delete) do what they say
//     after a reload, not just in the open tab;
//   - none of it logs a console error, and all of it behaves under
//     prefers-reduced-motion.
//
// Usage:
//   HB_E2E_BASE=http://127.0.0.1:3457 bun run tests/e2e/sessions-export.ts
//   bun run tests/e2e/sessions-export.ts          # spawns the fixture server
//
// The fixture replays one captured engine report but preserves each uploaded
// filename. That distinction is enough to exercise the complete multi-session
// flow without inventing analysis results for a second board.

import { chromium } from 'playwright'
import type { Browser, ConsoleMessage, Page } from 'playwright'
import { mkdirSync, readFileSync, rmSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { pathToFileURL } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e')
const DL = join(OUT, 'downloads')
const expectWorkflowExport = process.env.HB_EXPECT_WORKFLOW_EXPORT === '1'

let pass = 0
const failures: string[] = []
const notes: string[] = []

function ok(what: string, cond: boolean, detail = '') {
  if (cond) {
    pass++
    console.log(`  PASS  ${what}${detail ? ` :: ${detail}` : ''}`)
  } else {
    failures.push(`${what}${detail ? ` :: ${detail}` : ''}`)
    console.log(`  FAIL  ${what}${detail ? ` :: ${detail}` : ''}`)
  }
}
const note = (s: string) => { notes.push(s); console.log(`  note  ${s}`) }
const step = (s: string) => console.log(`\n── ${s} ──`)

/** Console output that counts as a defect. A failed fetch the app HANDLES (the
 *  fixture server's 501s, a live launch it reports in the UI) still logs a
 *  network error in Chromium, so only real page-level errors are collected. */
const consoleErrors: string[] = []
function watchConsole(page: Page, label: string) {
  const onMsg = (m: ConsoleMessage) => {
    if (m.type() !== 'error') return
    const text = m.text()
    // Network-level noise from a request the app itself surfaces to the user.
    if (/Failed to load resource|net::ERR_|the server does not|no fixture for/.test(text)) return
    consoleErrors.push(`[${label}] ${text}`)
  }
  page.on('console', onMsg)
  page.on('pageerror', e => consoleErrors.push(`[${label}] pageerror: ${e.message}`))
}

const settle = (page: Page, ms = 400) => page.waitForTimeout(ms)

async function shoot(page: Page, name: string) {
  // Long enough for every entry animation to have finished: a shot taken mid
  // fade shows a half-transparent panel, which is a picture of the harness's
  // timing rather than of the UI.
  await settle(page, 500)
  await page.screenshot({ path: join(OUT, `${name}.png`), fullPage: false })
}

/** Read the whole session index out of the page's localStorage. */
async function sessionIndex(page: Page): Promise<{ id: string; name: string; hasReport: boolean; checkCount: number }[]> {
  return await page.evaluate(() => {
    try {
      return JSON.parse(localStorage.getItem('hauksbee.sessions.v1') ?? '[]')
    } catch {
      return []
    }
  })
}

/** Compose two checks and run them, so the session has a real spec in it. */
async function composeChecks(page: Page) {
  await page.click('[data-testid="nav-checks"]')
  await page.waitForSelector('[data-testid="checks-panel"]', { timeout: 20_000 })
  const empty = page.locator('[data-testid="checks-empty-add"]')
  if (await empty.count() > 0) await empty.click()
  await page.click('[data-testid="add-check"]')
  await page.locator('button', { hasText: 'A net must sit at a voltage' }).first().click()
  await page.locator('input[list="net-options"]').last().fill('GND')
  await page.locator('label', { hasText: 'min V' }).locator('input').last().fill('-0.1')
  await page.locator('label', { hasText: 'max V' }).locator('input').last().fill('0.1')
  await settle(page, 700)
}

async function main() {
  rmSync(OUT, { recursive: true, force: true })
  mkdirSync(DL, { recursive: true })

  const external = process.env.HB_E2E_BASE ?? null
  let fixture: Bun.Subprocess | null = null
  const port = Number(process.env.HB_E2E_PORT ?? 3488)
  const base = external ?? `http://127.0.0.1:${port}`
  if (!external) {
    fixture = Bun.spawn(
      ['bun', 'run', join(here, '../visual-lint/fixture-server.ts'), String(port)],
      { stdout: 'inherit', stderr: 'inherit' },
    )
  }
  for (let i = 0; i < 80; i++) {
    try {
      const r = await fetch(base, { signal: AbortSignal.timeout(1000) })
      if (r.ok) break
    } catch { /* not up yet */ }
    await Bun.sleep(250)
  }
  console.log(`base: ${base}${external ? ' (external)' : ' (fixture server)'}`)

  const browser: Browser = await chromium.launch({ headless: true })
  const ctx = await browser.newContext({
    baseURL: base,
    viewport: { width: 1440, height: 900 },
    acceptDownloads: true,
  })
  const page = await ctx.newPage()
  page.setDefaultTimeout(20_000)
  watchConsole(page, 'app')

  // ── 1. A fresh visit has nothing to resume ──────────────────────────────
  step('a first visit saves a session')
  await page.goto('/', { waitUntil: 'domcontentloaded' })
  await page.waitForSelector('[data-testid="drop-zone"]', { timeout: 30_000 })
  ok('a first visit offers nothing to resume',
    await page.locator('[data-testid="session-resume"]').count() === 0)
  ok('the rail shows the session indicator with nothing saved',
    await page.locator('[data-testid="session-indicator"]').count() === 1)

  await page.click('[data-testid="sample-watchy"]')
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 60_000 })
  await settle(page, 1500)
  const boardName = (await page.locator('[data-testid="sidebar-board"]').first().innerText()).split('\n')[0]

  let idx = await sessionIndex(page)
  ok('one session was written by the report landing', idx.length === 1, JSON.stringify(idx[0]?.name))
  ok('the saved session carries the report', idx[0]?.hasReport === true)

  await composeChecks(page)
  await page.click('[data-testid="run-checks"]')
  await page.waitForSelector('[data-testid="check-results"], [data-testid="builder-validation"]', { timeout: 90_000 })
  await settle(page, 900)
  const specText = await page.locator('[data-testid="spec-preview"]').innerText()
  ok('the composed spec holds both assertions',
    (specText.match(/\[\[assert\]\]/g) ?? []).length === 2,
    `${(specText.match(/\[\[assert\]\]/g) ?? []).length} asserts`)

  idx = await sessionIndex(page)
  ok('the session records the composed check count', idx[0]?.checkCount === 2, `checkCount=${idx[0]?.checkCount}`)
  await shoot(page, '01-checks-composed')

  // ── 2. Export ───────────────────────────────────────────────────────────
  step('the export menu writes real files')
  await page.click('[data-testid="nav-board"]')
  await page.locator('[data-testid="export-open"]').scrollIntoViewIfNeeded()
  await page.click('[data-testid="export-open"]')
  await page.waitForSelector('[data-testid="export-menu"]')
  const itemCount = await page.locator('[data-testid="export-menu"] [role="menuitem"]').count()
  const expectedItems = expectWorkflowExport ? 4 : 3
  ok(expectWorkflowExport
    ? 'a release build offers all four files once a spec exists'
    : 'a development build omits the credential-bearing workflow export',
  itemCount === expectedItems, `${itemCount} items`)
  await shoot(page, '02-export-menu')

  const grab = async (testid: string): Promise<string> => {
    const [download] = await Promise.all([
      page.waitForEvent('download'),
      page.click(`[data-testid="${testid}"]`),
    ])
    const name = download.suggestedFilename()
    const to = join(DL, name)
    await download.saveAs(to)
    return to
  }

  const htmlPath = await grab('export-html')
  ok('the HTML export downloaded with a .html name', htmlPath.endsWith('.html'), htmlPath)
  await page.waitForSelector('[data-testid="export-done"]')
  ok('the app confirms what it wrote',
    (await page.locator('[data-testid="export-done"]').innerText()).includes('.html'))

  await page.click('[data-testid="export-open"]')
  const jsonPath = await grab('export-json')
  await page.click('[data-testid="export-open"]')
  const tomlPath = await grab('export-toml')
  let ymlPath: string | null = null
  if (expectWorkflowExport) {
    await page.click('[data-testid="export-open"]')
    ymlPath = await grab('export-workflow')
  }

  // ── 3. The exported HTML is self-contained ──────────────────────────────
  step('the exported HTML renders standalone, off a file:// URL, with no network')
  const html = readFileSync(htmlPath, 'utf8')
  const externalRefs = [
    ...html.matchAll(/(?:src|href)\s*=\s*["'](https?:|\/\/)[^"']*/gi),
    ...html.matchAll(/url\(\s*["']?(https?:|\/\/)/gi),
    ...html.matchAll(/@import\b/gi),
  ].map(m => m[0])
  ok('no external stylesheet, script, font or image reference',
    externalRefs.length === 0, externalRefs.slice(0, 3).join(' | '))
  ok('no script at all', !/<script/i.test(html))
  ok('the palette is inlined as literal values', /--surface:\s*#/.test(html))

  // A context that CANNOT reach the network: any request the page tries is
  // aborted, so "it rendered" is proof it needed nothing.
  const offline = await browser.newContext({ viewport: { width: 1100, height: 900 } })
  await offline.route('**/*', async route => {
    if (route.request().url().startsWith('file://')) await route.continue()
    else await route.abort()
  })
  const exported = await offline.newPage()
  const exportErrors: string[] = []
  exported.on('pageerror', e => exportErrors.push(e.message))
  exported.on('console', m => { if (m.type() === 'error') exportErrors.push(m.text()) })
  await exported.goto(pathToFileURL(htmlPath).href, { waitUntil: 'load' })

  const shown = await exported.evaluate(() => {
    const body = document.body
    const s = getComputedStyle(body)
    const verdict = document.querySelector('.verdict')
    return {
      title: document.title,
      bg: s.backgroundColor,
      font: s.fontFamily,
      verdictText: verdict?.textContent?.trim().slice(0, 120) ?? '',
      verdictBorder: verdict ? getComputedStyle(verdict).borderTopWidth : '',
      cards: document.querySelectorAll('.card').length,
      tables: document.querySelectorAll('table').length,
      pres: document.querySelectorAll('pre').length,
      metaRows: document.querySelectorAll('dl.meta dt').length,
      text: document.body.innerText,
      scrollW: document.documentElement.scrollWidth,
      clientW: document.documentElement.clientWidth,
    }
  })
  ok('it has a real title', shown.title.startsWith('hauksbee report:'), shown.title)
  ok('the styles applied (a themed background, not the UA default)',
    shown.bg !== 'rgba(0, 0, 0, 0)' && shown.bg !== 'rgb(255, 255, 255)', shown.bg)
  ok('the verdict block is drawn with its tone border',
    shown.verdictBorder !== '' && shown.verdictBorder !== '0px', shown.verdictBorder)
  ok('the verdict text survived', shown.verdictText.length > 20, shown.verdictText.slice(0, 60))
  ok('the findings came with it', shown.cards >= 3, `${shown.cards} cards`)
  ok('the bind table came with it', shown.tables >= 1, `${shown.tables} tables`)
  ok('the composed spec TOML came with it',
    shown.pres >= 1 && shown.text.includes('[[assert]]'))
  ok('the provenance block names the board, the time and the binary',
    shown.metaRows >= 6 && /hauksbee/.test(shown.text), `${shown.metaRows} meta rows`)
  ok('a timestamp is in the file', /\d{1,4}[/.-]\d{1,2}[/.-]\d{1,4}/.test(shown.text))
  ok('the exported page does not scroll sideways',
    shown.scrollW <= shown.clientW + 1, `${shown.scrollW} vs ${shown.clientW}`)
  ok('the exported page logged nothing', exportErrors.length === 0, exportErrors.join(' | '))
  await exported.screenshot({ path: join(OUT, '03-exported-html.png'), fullPage: false })
  // A phone-width look at the same file, since it is a page someone will open
  // on their phone from an email.
  await exported.setViewportSize({ width: 360, height: 780 })
  await settle(exported, 200)
  const narrow = await exported.evaluate(() => ({
    scrollW: document.documentElement.scrollWidth,
    clientW: document.documentElement.clientWidth,
  }))
  ok('and does not scroll sideways at 360px either',
    narrow.scrollW <= narrow.clientW + 1, `${narrow.scrollW} vs ${narrow.clientW}`)
  await exported.screenshot({ path: join(OUT, '04-exported-html-360.png'), fullPage: false })
  await offline.close()

  // ── 4. The JSON export is the API shape ─────────────────────────────────
  step('the JSON export is the engine\'s own shape')
  const parsed = JSON.parse(readFileSync(jsonPath, 'utf8')) as Record<string, unknown>
  for (const key of ['ok', 'board_name', 'file_name', 'num_components', 'num_nets', 'headline', 'serious', 'total', 'sections', 'components']) {
    ok(`the JSON carries \`${key}\``, key in parsed)
  }
  ok('the JSON findings count matches the report headline count',
    Array.isArray(parsed.sections)
    && (parsed.sections as { findings: unknown[] }[]).reduce((n, s) => n + s.findings.length, 0) === parsed.total,
    `${parsed.total} total`)

  const toml = readFileSync(tomlPath, 'utf8')
  ok('the exported spec is the one the pane showed', toml.trim() === specText.trim())
  if (ymlPath) {
    const yml = readFileSync(ymlPath, 'utf8')
    ok('the workflow fetches the private Action at the exact release commit',
      /hauksbee-ref:\s*[0-9a-f]{40}/.test(yml)
        && /hauksbee-version:\s*v\d/.test(yml)
        && /persist-credentials:\s*false/.test(yml)
        && /uses:\s*hauksbee-dev\/hauksbee\/integrations\/github-action@[0-9a-f]{40}/.test(yml)
        && /permissions:\s*\n\s*contents:\s*read\s*\n\s*checks:\s*write/.test(yml))
  }

  // ── 5. Restore across a full reload ─────────────────────────────────────
  step('the session survives a reload and comes back with its checks')
  await page.reload({ waitUntil: 'domcontentloaded' })
  await page.waitForSelector('[data-testid="session-resume"]', { timeout: 30_000 })
  const resumeText = await page.locator('[data-testid="session-resume"]').innerText()
  ok('the resume card names the session and its board',
    resumeText.includes(boardName.split('.')[0]) || resumeText.includes(idx[0].name),
    resumeText.split('\n').slice(0, 3).join(' / '))
  ok('the resume card says what cannot come back',
    /board file itself does not/.test(resumeText))
  ok('the resume card counts the composed checks', /2 checks composed/.test(resumeText))
  await shoot(page, '05-resume-offer')

  await page.click('[data-testid="session-resume-open"]')
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 30_000 })
  const reanalyzed = await page.locator('[data-testid="restored-notice"]').count() === 0
  ok('the live /boards route returns retained bytes and Resume re-runs them', reanalyzed)
  await shoot(page, '06-resumed-report')

  await page.click('[data-testid="nav-checks"]')
  await page.waitForSelector('[data-testid="checks-panel"]')
  await settle(page, 700)
  const restoredSpec = await page.locator('[data-testid="spec-preview"]').innerText()
  ok('the composed checks came back with the session',
    (restoredSpec.match(/\[\[assert\]\]/g) ?? []).length === 2,
    `${(restoredSpec.match(/\[\[assert\]\]/g) ?? []).length} asserts`)
  ok('and the restored spec is byte-identical to the exported one',
    restoredSpec.trim() === specText.trim())
  await shoot(page, '07-restored-checks')

  // The export path works from a restored session too, which is the whole point
  // of keeping the report rather than only the metadata.
  await page.click('[data-testid="nav-board"]')
  await page.locator('[data-testid="export-open"]').scrollIntoViewIfNeeded()
  await page.click('[data-testid="export-open"]')
  const restoredHtml = await grab('export-html')
  const rh = readFileSync(restoredHtml, 'utf8')
  ok('a restored session still exports a full report',
    rh.includes('[[assert]]') && /class="card"/.test(rh))
  ok('the re-analyzed export is not mislabeled as a stored report',
    !/restored from a saved browser session/i.test(rh))

  // ── 6. Named sessions: a second board, switch, rename, delete ───────────
  step('named sessions: a second board, the switcher, rename and delete')
  await page.click('[data-testid="header-another-board"]')
  await page.waitForSelector('[data-testid="drop-zone"]', { timeout: 20_000 })
  await page.click('[data-testid="sample-blinky"]')
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 60_000 })
  await settle(page, 1500)
  idx = await sessionIndex(page)
  const multi = idx.length >= 2
  if (!multi) {
    note('the server returned the same board identity for two uploads; '
      + 'multi-session behavior cannot be exercised against this server')
  }
  ok('a second board makes a second session', multi, `${idx.length} sessions`)

  await page.click('[data-testid="session-indicator"]')
  await page.waitForSelector('[data-testid="session-switcher"]')
  const rowCount = await page.locator('[data-testid="session-row"]').count()
  ok('the switcher lists every saved session', rowCount === idx.length, `${rowCount} rows`)
  ok('the switcher marks which one is open',
    await page.locator('[data-testid="session-row-current"]').count() === 1)
  ok('the switcher states what a session cannot hold',
    /cannot hold the board file itself/.test(await page.locator('[data-testid="session-switcher"]').innerText()))
  await shoot(page, '08-session-switcher')

  // Rename the open one (it is the row whose name is visible without scrolling).
  await page.locator('[data-testid="session-rename"]').first().click()
  await page.waitForSelector('[data-testid="session-rename-input"]')
  await page.fill('[data-testid="session-rename-input"]', 'bench rev C')
  await shoot(page, '09-session-rename')
  await page.click('[data-testid="session-rename-save"]')
  await settle(page)
  ok('the rename shows in the list at once',
    (await page.locator('[data-testid="session-row-name"]').first().innerText()) === 'bench rev C')
  idx = await sessionIndex(page)
  ok('the rename is in storage', idx.some(r => r.name === 'bench rev C'))

  await page.reload({ waitUntil: 'domcontentloaded' })
  await page.waitForSelector('[data-testid="session-indicator"]', { timeout: 30_000 })
  await page.click('[data-testid="session-indicator"]')
  await page.waitForSelector('[data-testid="session-switcher"]')
  ok('the rename survived the reload',
    (await page.locator('[data-testid="session-switcher"]').innerText()).includes('bench rev C'))

  if (multi) {
    // Open the OTHER session from the switcher: the report on screen has to
    // become that board's.
    const openedName = await page.locator('[data-testid="session-row"]')
      .filter({ has: page.locator('[data-testid="session-open"]') })
      .first().locator('[data-testid="session-row-name"]').innerText()
    // After the reload nothing is loaded, so there is no board card to read.
    const wasShowing = await page.locator('[data-testid="sidebar-board"]').count() > 0
      ? (await page.locator('[data-testid="sidebar-board"]').first().innerText()).split('\n')[0]
      : '(nothing loaded)'
    await page.locator('[data-testid="session-open"]').first().click()
    await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 30_000 })
    await settle(page, 900)
    const nowShowing = (await page.locator('[data-testid="sidebar-board"]').first().innerText()).split('\n')[0]
    ok('opening a session from the switcher loads THAT board',
      nowShowing !== wasShowing && openedName.includes(nowShowing.replace(/\.[^.]+$/, '')),
      `${wasShowing} -> ${nowShowing} (row: ${openedName})`)
    await shoot(page, '10-session-switched')
  }

  const countBefore = (await sessionIndex(page)).length
  await page.click('[data-testid="session-indicator"]')
  await page.waitForSelector('[data-testid="session-switcher"]')
  await page.locator('[data-testid="session-delete"]').last().click()
  await page.waitForSelector('[data-testid="session-delete-confirm"]')
  ok('delete asks first, in-app', true)
  await page.locator('[data-testid="session-delete-confirm"]').click()
  await settle(page)
  const countAfter = (await sessionIndex(page)).length
  ok('delete removes exactly one session', countAfter === countBefore - 1,
    `${countBefore} -> ${countAfter}`)
  const rowsNow = await page.locator('[data-testid="session-row"]').count()
  ok('and the list agrees immediately', rowsNow === countAfter, `${rowsNow} rows`)

  // ── 7. Reduced motion ───────────────────────────────────────────────────
  step('prefers-reduced-motion: nothing animates, everything still works')
  const rm = await browser.newContext({
    baseURL: base, viewport: { width: 1440, height: 900 }, reducedMotion: 'reduce',
    acceptDownloads: true,
  })
  const rmPage = await rm.newPage()
  rmPage.setDefaultTimeout(20_000)
  watchConsole(rmPage, 'reduced-motion')
  await rmPage.goto('/', { waitUntil: 'domcontentloaded' })
  await rmPage.waitForSelector('[data-testid="drop-zone"]', { timeout: 30_000 })
  await rmPage.click('[data-testid="sample-watchy"]')
  await rmPage.waitForSelector('[data-testid="report-verdict"]', { timeout: 60_000 })
  await settle(rmPage, 1200)
  await rmPage.locator('[data-testid="export-open"]').scrollIntoViewIfNeeded()
  await rmPage.click('[data-testid="export-open"]')
  await rmPage.waitForSelector('[data-testid="export-menu"]')
  // No settle: under reduced motion the panel must be fully there on the frame
  // it appears, not fading in over 180ms.
  const menuNow = await rmPage.evaluate(() => {
    const el = document.querySelector('[data-testid="export-menu"]') as HTMLElement | null
    if (!el) return null
    const s = getComputedStyle(el)
    return { opacity: Number(s.opacity), transform: s.transform }
  })
  ok('the export menu is at full opacity immediately', menuNow?.opacity === 1, JSON.stringify(menuNow))
  ok('and is not mid-translate', menuNow?.transform === 'none' || /matrix\(1, 0, 0, 1, 0, 0\)/.test(menuNow?.transform ?? ''),
    menuNow?.transform)
  await rmPage.keyboard.press('Escape')
  await rmPage.click('[data-testid="session-indicator"]')
  await rmPage.waitForSelector('[data-testid="session-switcher"]')
  const panelNow = await rmPage.evaluate(() => {
    const el = document.querySelector('[data-testid="session-switcher"]') as HTMLElement | null
    return el ? Number(getComputedStyle(el).opacity) : null
  })
  ok('the session switcher is at full opacity immediately', panelNow === 1, String(panelNow))
  await rmPage.keyboard.press('Escape')
  await settle(rmPage, 200)
  ok('Escape closes the switcher',
    await rmPage.locator('[data-testid="session-switcher"]').count() === 0)
  await settle(rmPage, 300)
  await rmPage.screenshot({ path: join(OUT, '11-reduced-motion.png') })
  await rm.close()

  // ── 8. Narrow viewport walk of the new surfaces ─────────────────────────
  step('the new surfaces at 320px')
  // The SAME context, resized: a new context has its own empty localStorage, so
  // it would have no session to resume and the walk would test nothing.
  const sp = page
  await sp.setViewportSize({ width: 320, height: 568 })
  await sp.goto('/', { waitUntil: 'domcontentloaded' })
  await sp.waitForSelector('[data-testid="session-resume"]', { timeout: 30_000 })
  await settle(sp, 500)
  await sp.screenshot({ path: join(OUT, '12-resume-320.png') })
  await sp.click('[data-testid="session-resume-open"]')
  await sp.waitForSelector('[data-testid="report-verdict"]', { timeout: 30_000 })
  await settle(sp, 800)
  await sp.locator('[data-testid="export-open"]').scrollIntoViewIfNeeded()
  await sp.click('[data-testid="export-open"]')
  await sp.waitForSelector('[data-testid="export-menu"]')
  const fits = await sp.evaluate(() => {
    const el = document.querySelector('[data-testid="export-menu"]')!.getBoundingClientRect()
    return { left: el.left, right: el.right, vw: document.documentElement.clientWidth,
      pageScroll: document.documentElement.scrollWidth }
  })
  ok('the export menu stays inside a 320px window',
    fits.left >= -1 && fits.right <= fits.vw + 1, JSON.stringify(fits))
  await settle(sp, 500)
  await sp.screenshot({ path: join(OUT, '13-export-320.png') })
  await sp.keyboard.press('Escape')
  await sp.click('[data-testid="session-indicator"]')
  await sp.waitForSelector('[data-testid="session-switcher"]')
  const panelFits = await sp.evaluate(() => {
    const el = document.querySelector('[data-testid="session-switcher"]')!.getBoundingClientRect()
    return { left: el.left, right: el.right, top: el.top, bottom: el.bottom,
      vw: document.documentElement.clientWidth, vh: document.documentElement.clientHeight }
  })
  ok('the switcher stays inside a 320x568 window',
    panelFits.left >= -1 && panelFits.right <= panelFits.vw + 1
    && panelFits.top >= -1 && panelFits.bottom <= panelFits.vh + 1,
    JSON.stringify(panelFits))
  await settle(sp, 500)
  await sp.screenshot({ path: join(OUT, '14-switcher-320.png') })

  // ── 9. Console ───────────────────────────────────────────────────────────
  step('console')
  ok('no console errors anywhere in the run', consoleErrors.length === 0,
    consoleErrors.slice(0, 5).join(' | '))

  await ctx.close()
  await browser.close()
  fixture?.kill()

  console.log(`\n${pass} passed, ${failures.length} failed, ${notes.length} note(s).`)
  for (const f of failures) console.log(`  FAILED: ${f}`)
  console.log(`screenshots + downloads: ${OUT}`)
  process.exit(failures.length > 0 ? 1 : 0)
}

await main()
