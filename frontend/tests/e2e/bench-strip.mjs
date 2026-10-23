// End to end: the bench strip and the run view in a real browser.
//
// Serves the built app (frontend/dist) through the visual-lint fixture server,
// then replays the Watchy sample with a co-sim section added to its analysis
// answer. The bench fixtures name nets of their own; they are remapped, in
// order, onto Watchy nets so a chip click lands on a real net of the board.
//
// Run:  cd frontend && bun run build && node tests/e2e/bench-strip.mjs
// Playwright comes from the shared install in ~/.claude/tools/playwright.
// Screenshots go to $BENCH_SHOTS (default: the WP2 scratchpad folder).

import { spawn } from 'node:child_process'
import { mkdirSync, readFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const FRONTEND = join(here, '../..')
const SHOTS = process.env.BENCH_SHOTS
  ?? '/private/tmp/claude-501/-Users-hauksbee-user-public-public-repos-hauksbee-developer/5e3d9da3-a1f2-42a2-b4a1-484c1ce37da8/scratchpad/wp2'
mkdirSync(SHOTS, { recursive: true })

const { chromium } = await import(join(homedir(), '.claude/tools/playwright/node_modules/playwright/index.mjs'))

const PORT = Number(process.env.BENCH_PORT ?? 3487)
const BASE = `http://127.0.0.1:${PORT}`
const WATCHY_NETS = ['BUSY', 'DC', 'CS', 'MOSI', 'SCK', 'RES', 'EN', 'BTN1', 'BTN2', 'BTN3']

const fixture = name => JSON.parse(readFileSync(join(FRONTEND, 'tests/fixtures', name), 'utf8'))

/** Rename the fixture's nets onto Watchy nets (in order of first appearance). */
function remap(bench) {
  const names = new Map()
  const swap = net => {
    if (net == null) return net
    if (!names.has(net)) names.set(net, WATCHY_NETS[names.size % WATCHY_NETS.length])
    return names.get(net)
  }
  const out = structuredClone(bench)
  for (const m of out.mcus) {
    for (const p of m.pins) p.net = swap(p.net)
    for (const u of m.uarts ?? []) u.net = swap(u.net)
  }
  return { bench: out, names }
}

const results = []
const check = (ok, what) => {
  results.push({ ok, what })
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${what}`)
}

// The fixture server, as a child process (it is a bun script).
const server = spawn('bun', ['run', 'tests/visual-lint/fixture-server.ts', String(PORT)], { cwd: FRONTEND, stdio: ['ignore', 'pipe', 'inherit'] })
await new Promise((resolve, reject) => {
  const t = setTimeout(() => reject(new Error('fixture server did not start')), 20_000)
  server.stdout.on('data', d => { if (String(d).includes('http://')) { clearTimeout(t); resolve() } })
  server.on('exit', c => reject(new Error(`fixture server exited early (${c})`)))
})

const browser = await chromium.launch()
let failed = false

/** One scenario: open the sample with this co-sim section, hand the page back. */
async function open(section, { theme = 'dark', width = 1400, height = 1000, jobStub = null } = {}) {
  const ctx = await browser.newContext({ viewport: { width, height }, colorScheme: theme, baseURL: BASE })
  await ctx.addInitScript(t => { try { localStorage.setItem('hauksbee.theme', t) } catch { /* private window */ } }, theme)
  const page = await ctx.newPage()
  const errors = []
  // The fixture server has no /api/ci/scaffold answer (a gap in the shared
  // server, hit by the checks panel on load, nothing to do with the bench), so
  // that one 501 and the console line it produces are not counted.
  const gap = []
  page.on('console', m => { if (m.type() === 'error' && !/status of 501/.test(m.text())) errors.push(`console: ${m.text()}`) })
  page.on('pageerror', e => errors.push(`pageerror: ${e.message}`))
  page.on('response', r => {
    if (r.status() < 400) return
    if (r.status() === 501 && r.url().endsWith('/api/ci/scaffold')) gap.push(r.url())
    else errors.push(`http ${r.status()}: ${r.url()}`)
  })
  await page.route('**/api/analyze', async route => {
    const res = await route.fetch()
    const body = await res.json()
    await route.fulfill({ response: res, json: { ...body, cosim: section } })
  })
  if (jobStub) await jobStub(page)
  await page.goto('/', { waitUntil: 'domcontentloaded' })
  await page.waitForSelector('[data-testid="drop-zone"]', { timeout: 30_000 })
  await page.evaluate(t => { document.documentElement.dataset.theme = t }, theme)
  await page.click('[data-testid="sample-watchy"]')
  await page.waitForSelector('[data-testid="cosim-section"]', { timeout: 60_000 })
  await page.locator('[data-testid="cosim-section"]').scrollIntoViewIfNeeded()
  await page.waitForTimeout(500)
  return { page, ctx, errors }
}

const ranSection = bench => ({
  ran: true, seconds_simulated: bench.duration_s, uart_output: '', findings: [], analog_valid: true, bench,
})
const shot = (page, name) =>
  page.locator('[data-testid="cosim-section"]').screenshot({ path: join(SHOTS, `${name}.png`) })

try {
  // 1. Blinky: chips, click one, the lane and the board follow.
  {
    const { bench, names } = remap(fixture('bench-stm32-blinky.json'))
    const { page, ctx, errors } = await open(ranSection(bench), { theme: 'dark' })
    check(await page.locator('[data-testid="bench-strip"]').count() === 1, 'the bench strip replaces the table')
    check(await page.locator('[data-testid="cosim-gpio-table"], table[data-testid="gpio-table"]').count() === 0, 'the old gpio table is not shown')
    const chips = page.locator('[data-testid="bench-chip"]')
    check(await chips.count() >= 3, `one chip per pin (${await chips.count()})`)
    const unlinked = page.locator('[data-testid="bench-chip"][data-linked="false"]')
    check(await unlinked.count() >= 1 && !!(await unlinked.first().getAttribute('title')), 'a chip with no net is not clickable and has a title')
    check(await page.locator('[data-testid="bench-scope-canvas"]').count() === 1, 'the run view is drawn')
    await shot(page, '01-blinky-dark-initial')

    const ledNet = names.get('LED_PC13')
    const chip = page.locator(`[data-testid="bench-chip"][data-net="${ledNet}"]`).first()
    await chip.hover()
    await page.waitForTimeout(250)
    await chip.click()
    await page.waitForTimeout(500)
    check(await chip.getAttribute('aria-pressed') === 'true', 'the clicked chip is pressed')
    const focused = await page.locator('[data-testid="bench-scope-canvas"]').getAttribute('data-focused')
    check(focused === 'PC13', `the run view focuses its lane (data-focused=${focused})`)
    const card = await page.locator('[data-testid="selection-card"]').first().innerText().catch(() => '')
    check(card.includes(ledNet), `the board's selection card names ${ledNet}`)
    check(await page.locator('[data-testid="bench-show-board"]').count() === 1, 'the run view offers "Show it on the board"')
    await shot(page, '02-blinky-dark-focused')
    // The whole page, scrolled to the board, to judge the highlighted net as a user sees it.
    await page.evaluate(() => window.scrollTo(0, 0))
    await page.screenshot({ path: join(SHOTS, '02b-blinky-dark-board-top.png') })
    check(errors.length === 0, `blinky: no console or page errors ${errors.join(' | ')}`)
    await ctx.close()
  }

  // 2. Same in light mode.
  {
    const { bench } = remap(fixture('bench-stm32-blinky.json'))
    const { page, ctx, errors } = await open(ranSection(bench), { theme: 'light' })
    await page.locator('[data-testid="bench-chip"][data-linked="true"]').first().click()
    await page.waitForTimeout(400)
    await shot(page, '03-blinky-light-focused')
    check(errors.length === 0, `blinky light: no console or page errors ${errors.join(' | ')}`)
    await ctx.close()
  }

  // 3. Thermostat: the decoded bus track, the NACK in the error tone, a cut lane.
  {
    const { bench } = remap(fixture('bench-i2c-thermostat.json'))
    const { page, ctx, errors } = await open(ranSection(bench), { theme: 'dark' })
    const canvas = page.locator('[data-testid="bench-scope-canvas"]')
    check(Number(await canvas.getAttribute('data-decoded')) === bench.bus_log.length, 'every bus transaction is on the decoded track')
    check(Number(await canvas.getAttribute('data-events')) === bench.events.length, 'every event has a dashed rule')
    check(await page.locator('[data-testid="bench-uart"]').count() >= 1, 'the UART row is shown')
    await shot(page, '04-thermostat-dark')
    check(errors.length === 0, `thermostat: no console or page errors ${errors.join(' | ')}`)
    await ctx.close()
  }

  // 4. Parked: the warning is obvious in the strip.
  {
    const { bench } = remap(fixture('bench-parked.json'))
    const { page, ctx, errors } = await open(ranSection(bench), { theme: 'dark' })
    check(await page.locator('[data-testid="bench-issue"]').count() >= 1, 'a parked core shows an inline issue')
    check((await page.locator('[data-testid="bench-mcu"]').first().getAttribute('data-tone')) === 'err', 'the parked core is in the error tone')
    await shot(page, '05-parked-dark')
    await ctx.close()
    const light = await open(ranSection(bench), { theme: 'light' })
    await shot(light.page, '06-parked-light')
    check(errors.length + light.errors.length === 0, `parked: no console or page errors ${[...errors, ...light.errors].join(' | ')}`)
    await light.ctx.close()
  }

  // 5. Phone width: no horizontal page scroll.
  {
    const { bench } = remap(fixture('bench-i2c-thermostat.json'))
    const { page, ctx, errors } = await open(ranSection(bench), { theme: 'dark', width: 390, height: 844 })
    const over = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)
    check(over <= 1, `phone width: no horizontal page scroll (overflow ${over}px)`)
    await shot(page, '07-thermostat-phone')
    check(errors.length === 0, `phone: no console or page errors ${errors.join(' | ')}`)
    await ctx.close()
  }

  // 6. The emulator button: offered, runs the job, shows progress, replaces the section.
  {
    const { bench } = remap(fixture('bench-stm32-blinky.json'))
    const pending = { ran: false, seconds_simulated: 0, uart_output: '', findings: [], analog_valid: false, external_backend: true, notes: ['The STM32 needs Renode, which the synchronous analysis does not boot.'] }
    let polls = 0
    let startBodies = 0
    const stub = async page => {
      await page.route('**/api/cosim-run', async route => { startBodies++; await route.fulfill({ json: { job_id: 'job-1' } }) })
      await page.route('**/api/cosim-run/job-1', async route => {
        polls++
        if (polls === 1) return route.fulfill({ json: { state: 'running', progress_s: 3.2, total_s: 10 } })
        return route.fulfill({ json: { state: 'done', section: ranSection(bench) } })
      })
    }
    const { page, ctx, errors } = await open(pending, { theme: 'dark', jobStub: stub })
    const btn = page.locator('[data-testid="cosim-emulator-run"]')
    const offered = await btn.count()
    if (offered === 0) {
      console.log('NOTE  the emulator button is not offered for the bundled sample (its files are server preloaded, not retained)')
    } else {
      check((await btn.innerText()).includes('Renode/QEMU'), 'the button reads Run on Renode/QEMU')
      await btn.click()
      await page.waitForSelector('[data-testid="cosim-emulator-progress"]', { timeout: 5000 })
      const line = await page.locator('[data-testid="cosim-emulator-progress"]').innerText()
      check(/Booting the emulator/.test(line), `progress is shown (${line})`)
      await page.waitForSelector('[data-testid="bench-strip"]', { timeout: 10_000 })
      check(startBodies === 1, 'one job was started')
      check(await page.locator('[data-testid="bench-strip"]').count() === 1, 'the finished section replaced the notice')
    }
    check(errors.length === 0, `emulator path: no console or page errors ${errors.join(' | ')}`)
    await ctx.close()
  }
} catch (e) {
  failed = true
  console.error('FAIL  scenario threw:', e)
} finally {
  await browser.close()
  server.kill()
}

const bad = results.filter(r => !r.ok)
console.log(`\n${results.length - bad.length}/${results.length} checks passed; screenshots in ${SHOTS}`)
process.exit(failed || bad.length ? 1 : 0)
