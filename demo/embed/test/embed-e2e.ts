#!/usr/bin/env bun
/**
 * End-to-end pass over the embeddable widget, in a real browser, against the
 * built bundle and the recorded assets. What it exists to prove:
 *
 *   - the compact state renders a REAL, interactive board map fast, from the
 *     cache, with no engine anywhere;
 *   - clicking a net answers with a recorded measurement;
 *   - the host's contract fires: ready, engaged, requestExpand, requestCollapse,
 *     and the commands (expand/collapse/reset/loadBoard) work from the host side;
 *   - the checks panel runs and returns the RECORDED verdict, green on the green
 *     spec and red on the red one;
 *   - the two-sided firmware story is real: the same gate check goes red with the
 *     firmware unstaged;
 *   - nothing is fetched but the widget's own files and the recorded assets;
 *   - no console errors anywhere in it.
 *
 * Usage:  bun demo/embed/test/embed-e2e.ts            (serves demo/embed-dist)
 *         HB_EMBED_BASE=http://127.0.0.1:4173 bun demo/embed/test/embed-e2e.ts
 */

import { chromium } from 'playwright'
import type { ConsoleMessage, Frame, Page, Request } from 'playwright'
import { existsSync, mkdirSync, statSync } from 'node:fs'
import { extname, join, resolve } from 'node:path'

const REPO = resolve(import.meta.dir, '../../..')
const DIST = resolve(REPO, 'demo/embed-dist')
const OUT = process.env.HB_EMBED_OUT ?? resolve(REPO, 'demo/embed-dist/../embed-test-results')

let pass = 0
const failures: string[] = []
const notes: string[] = []
const ok = (what: string, cond: boolean, detail = '') => {
  if (cond) { pass++; console.log(`  PASS  ${what}${detail ? ` :: ${detail}` : ''}`) }
  else { failures.push(`${what}${detail ? ` :: ${detail}` : ''}`); console.log(`  FAIL  ${what}${detail ? ` :: ${detail}` : ''}`) }
}
const note = (s: string) => { notes.push(s); console.log(`  note  ${s}`) }
const step = (s: string) => console.log(`\n── ${s} ──`)

// ── A static server over demo/embed-dist, because that is all the widget needs.
const MIME: Record<string, string> = {
  '.html': 'text/html', '.js': 'text/javascript', '.json': 'application/json',
  '.css': 'text/css', '.svg': 'image/svg+xml', '.jsonl': 'application/x-ndjson',
  '.kicad_pcb': 'text/plain', '.hex': 'text/plain', '.bin': 'application/octet-stream',
}

function serve(dir: string) {
  return Bun.serve({
    port: 0,
    async fetch(req) {
      const url = new URL(req.url)
      const p = url.pathname === '/' ? '/test.html' : decodeURIComponent(url.pathname)
      const file = join(dir, p)
      if (!file.startsWith(dir) || !existsSync(file) || !statSync(file).isFile()) {
        return new Response('not found', { status: 404 })
      }
      return new Response(Bun.file(file), {
        headers: { 'Content-Type': MIME[extname(file)] ?? 'application/octet-stream' },
      })
    },
  })
}

const consoleErrors: string[] = []
function watchConsole(page: Page) {
  page.on('console', (m: ConsoleMessage) => {
    if (m.type() !== 'error') return
    consoleErrors.push(m.text())
  })
  page.on('pageerror', e => consoleErrors.push(`pageerror: ${e.message}`))
}

const requests: { url: string; method: string }[] = []

/** The widget's frame (it iframes itself by default). */
async function widgetFrame(page: Page): Promise<Frame> {
  const handle = await page.waitForSelector('#demo iframe', { timeout: 15_000 })
  const frame = await handle.contentFrame()
  if (!frame) throw new Error('the widget iframe has no content frame')
  return frame
}

const shot = async (page: Page, name: string) => {
  mkdirSync(OUT, { recursive: true })
  await page.screenshot({ path: join(OUT, `${name}.png`), fullPage: false })
  console.log(`  shot  ${join(OUT, `${name}.png`)}`)
}

async function main() {
  if (!existsSync(join(DIST, 'test.html'))) {
    throw new Error(`no built widget at ${DIST}; run demo/embed/build.sh first`)
  }
  const server = process.env.HB_EMBED_BASE ? null : serve(DIST)
  const base = process.env.HB_EMBED_BASE ?? `http://127.0.0.1:${server!.port}`
  const browser = await chromium.launch()
  const ctx = await browser.newContext({ viewport: { width: 1320, height: 900 } })
  const page = await ctx.newPage()
  watchConsole(page)
  page.on('request', (r: Request) => requests.push({ url: r.url(), method: r.method() }))

  try {
    // ── 1. Compact, from cold, fast.
    step('compact state')
    const t0 = Date.now()
    await page.goto(`${base}/test.html`, { waitUntil: 'domcontentloaded' })
    const frame = await widgetFrame(page)
    await frame.waitForSelector('[data-testid="embed-compact"] canvas', { timeout: 15_000 })
    // Interactive means the map has real geometry AND the readout carries a real
    // measurement off a recorded frame: a canvas alone could be an empty box.
    await frame.waitForFunction(() => {
      const el = document.querySelector('[data-testid="embed-readout"]')
      return !!el && /\d\.\d{3} V/.test(el.textContent ?? '')
    }, undefined, { timeout: 15_000 })
    const ms = Date.now() - t0
    ok('compact is interactive within 1s of navigation', ms < 1000, `${ms} ms`)
    if (ms >= 1000) note(`compact took ${ms} ms; the budget in the brief is 1000 ms`)

    const prompt = (await frame.textContent('[data-testid="embed-prompt"]')) ?? ''
    ok('the invitation says what it is and what to do', /real .*\.\s*Click a net\./i.test(prompt), prompt.trim())
    const honesty = (await frame.textContent('[data-testid="embed-honesty"]')) ?? ''
    ok(
      'the honesty line is on screen in the compact state',
      honesty.includes('A recorded run of the real engine on this board. Your boards run locally.'),
    )
    await shot(page, '01-compact')

    // ── 2. A net click answers with a recorded measurement.
    step('net click')
    const before = (await frame.textContent('[data-testid="embed-readout"]')) ?? ''
    const box = await (await frame.waitForSelector('[data-testid="embed-compact"] canvas')).boundingBox()
    if (!box) throw new Error('the map canvas has no box')
    // Sweep for a click that lands on copper: the map is mostly substrate, and a
    // click on nothing is a legitimate answer the viewer gives (null).
    let clicked = ''
    for (const [dx, dy] of [[0.5, 0.5], [0.42, 0.46], [0.58, 0.54], [0.5, 0.38], [0.35, 0.6], [0.65, 0.42], [0.46, 0.62], [0.54, 0.35]]) {
      await page.mouse.click(box.x + box.width * dx, box.y + box.height * dy)
      await page.waitForTimeout(180)
      const now = (await frame.textContent('[data-testid="embed-readout"]')) ?? ''
      const net = (await frame.textContent('.hb-embed-readout-net')) ?? ''
      if (net && now !== before) { clicked = net; break }
    }
    ok('clicking the map selects a real net and reads it out', clicked !== '', clicked)
    const events1 = await page.evaluate(() => (window as unknown as { __hbEvents: { type: string }[] }).__hbEvents.map(e => e.type))
    ok('the host was told the visitor engaged', events1.includes('engaged'), events1.join(','))
    ok('the host was told the widget was ready', events1.includes('ready'))
    await shot(page, '02-net-clicked')

    // ── 3. Expand, and the real app.
    step('expand into the app')
    await frame.click('[data-testid="embed-expand"]')
    const sawExpand = await page.waitForFunction(
      () => (window as unknown as { __hbEvents: { type: string }[] }).__hbEvents
        .some(e => e.type === 'requestExpand'),
      undefined, { timeout: 5000 },
    ).then(() => true).catch(() => false)
    ok('requestExpand fired', sawExpand)
    await frame.waitForSelector('[data-testid="embed-app"]', { timeout: 10_000 })
    await frame.waitForSelector('[data-testid="report"]', { timeout: 20_000 })
    const verdict = (await frame.textContent('[data-testid="report-verdict"]')) ?? ''
    ok('the app shows the recorded report for the board', verdict.trim().length > 0, verdict.trim().slice(0, 90))
    // The host animates the box (420 ms here); wait for it rather than racing it.
    await page.waitForFunction(() => {
      const el = document.getElementById('demo')
      return !!el && el.getBoundingClientRect().height > 600
    }, undefined, { timeout: 5000 }).catch(() => {})
    ok('the host box grew to the expanded height', await page.evaluate(() => {
      const el = document.getElementById('demo')
      return el ? Math.round(el.getBoundingClientRect().height) : 0
    }) > 600)
    await shot(page, '03-expanded-report')

    // ── 4. The checks panel: the recorded verdict, green.
    step('checks: the recorded green run')
    await frame.click('[data-testid="nav-checks"]')
    await frame.waitForSelector('[data-testid="checks-panel"]', { timeout: 10_000 })
    await frame.click('[data-testid="run-checks"]')
    await frame.waitForSelector('[data-testid="check-results"]', { timeout: 20_000 })
    const green = (await frame.textContent('[data-testid="check-results"]')) ?? ''
    ok('the green spec comes back green, from the recording', /All checks passed\./.test(green), green.trim().slice(0, 80))
    await shot(page, '04-checks-green')

    // ── 5. The same panel, the red spec.
    step('checks: the recorded red run')
    const redPresets = await frame.$$('button[data-testid^="embed-preset-"]')
    ok('the recorded specs are offered as chips', redPresets.length >= 2, `${redPresets.length} chips`)
    // The second chip on every board is the deliberately-red one.
    await redPresets[1].click()
    // The panel restores its rows at mount, so a spec switch remounts the app;
    // the widget puts the visitor back on the surface they were reading.
    await frame.waitForSelector('[data-testid="run-checks"]:not([disabled])', { timeout: 20_000 })
    await frame.waitForFunction(() => {
      const el = document.querySelector('[data-testid="checks-panel"]') as HTMLElement | null
      return !!el && el.offsetParent !== null
    }, undefined, { timeout: 20_000 })
    await frame.click('[data-testid="run-checks"]')
    await frame.waitForSelector('[data-testid="check-results"]', { timeout: 20_000 })
    const red = (await frame.textContent('[data-testid="check-results"]')) ?? ''
    ok('the red spec comes back red, from the recording', /Checks failed\./.test(red), red.trim().slice(0, 80))
    await shot(page, '05-checks-red')

    // ── 6. The live surface: the recorded session, replaying.
    step('the live surface replays the recorded session')
    await frame.click('[data-testid="nav-sim"]')
    await frame.waitForSelector('[data-testid="replay-scrub"]', { timeout: 20_000 })
    const scrubMax = await frame.getAttribute('[data-testid="replay-scrub"]', 'max')
    ok('the transport offers the recording as a scrubbable timeline', Number(scrubMax) > 0, `${scrubMax}s`)
    // It is playing, not sitting there: the recording pages itself along its own
    // sim-time axis the moment the surface opens.
    const at0 = Number(await frame.getAttribute('[data-testid="replay-scrub"]', 'value'))
    await frame.waitForTimeout(1200)
    const at1 = Number(await frame.getAttribute('[data-testid="replay-scrub"]', 'value'))
    ok('the recording is already playing when the surface opens', at1 > at0, `${at0.toFixed(2)}s -> ${at1.toFixed(2)}s`)
    // Scrubbing is the visitor's, and it is instant: the state is rebuilt from
    // the recording rather than re-simulated.
    await frame.fill('[data-testid="replay-scrub"]', String(Number(scrubMax) * 0.5))
    await frame.waitForTimeout(150)
    const scrubbed = Number(await frame.getAttribute('[data-testid="replay-scrub"]', 'value'))
    ok('the timeline can be scrubbed', Math.abs(scrubbed - Number(scrubMax) * 0.5) < Number(scrubMax) * 0.2,
      `${scrubbed.toFixed(2)}s`)
    await shot(page, '06-live-replay')

    // ── 6b. 3D is retired in the widget, deliberately (demo/EMBED.md, "About
    // 3D"): the hide is a structural CSS rule over the app's own toolbar, and
    // this is the guard that it still matches. A 3D button coming back is either
    // that rule going stale or someone re-enabling it without measuring.
    step('3D is not offered')
    await frame.click('[data-testid="nav-board"]')
    await frame.waitForTimeout(400)
    const threeDVisible = await frame.evaluate(() => Array
      .from(document.querySelectorAll('[data-testid="embed-app"] button'))
      .filter(b => b.textContent?.trim() === '3D')
      .some(b => (b as HTMLElement).offsetParent !== null))
    ok('the 3D control is hidden in the widget', !threeDVisible)
    const compactThreeD = await frame.evaluate(() => Array
      .from(document.querySelectorAll('.hb-embed-compact button'))
      .filter(b => b.textContent?.trim() === '3D')
      .some(b => (b as HTMLElement).offsetParent !== null))
    ok('and hidden in the compact state too', !compactThreeD)
    note('the three.js chunk therefore never loads; it stays in the build for when 3D is re-enabled')

    // ── 7. The two-sided firmware story on boot_gate.
    step('boot_gate: the same check, with and without the firmware')
    await page.click('#board-boot_gate')
    await frame.waitForSelector('[data-testid="embed-preset-gate-with-firmware"]', { timeout: 20_000 })
    await frame.click('[data-testid="embed-preset-gate-with-firmware"]')
    await frame.click('[data-testid="nav-checks"]')
    await frame.waitForSelector('[data-testid="run-checks"]:not([disabled])', { timeout: 20_000 })
    await frame.click('[data-testid="run-checks"]')
    await frame.waitForSelector('[data-testid="check-results"]', { timeout: 20_000 })
    const withFw = (await frame.textContent('[data-testid="check-results"]')) ?? ''
    ok('the gate check passes with the firmware staged', /All checks passed\./.test(withFw), withFw.trim().slice(0, 70))
    await shot(page, '07-boot-gate-green')

    await frame.click('[data-testid="embed-preset-gate-no-firmware"]')
    await frame.waitForSelector('[data-testid="run-checks"]:not([disabled])', { timeout: 20_000 })
    await frame.waitForFunction(() => {
      const el = document.querySelector('[data-testid="checks-panel"]') as HTMLElement | null
      return !!el && el.offsetParent !== null
    }, undefined, { timeout: 20_000 })
    await frame.click('[data-testid="run-checks"]')
    await frame.waitForSelector('[data-testid="check-results"]', { timeout: 20_000 })
    const noFw = (await frame.textContent('[data-testid="check-results"]')) ?? ''
    ok('the same check goes red with no firmware staged', /Checks failed\./.test(noFw), noFw.trim().slice(0, 70))
    await shot(page, '08-boot-gate-red')

    // ── 7b. A spec that was never recorded refuses honestly. This is the
    // dead-end test: the panel is a real editor, so an edited spec is one click
    // away, and what comes back must be a refusal that says why, not a spinner
    // and not a plausible-looking verdict.
    step('an unrecorded spec is refused, in words')
    await frame.click('[data-testid="raw-toggle"]')
    await frame.waitForSelector('[data-testid="raw-toml"]', { timeout: 10_000 })
    const raw = (await frame.inputValue('[data-testid="raw-toml"]')) ?? ''
    ok('the panel round-trips the recorded spec into raw TOML', /\[\[assert\]\]/.test(raw))
    const stale = (await frame.textContent('[data-testid="check-results"]')) ?? ''
    await frame.fill('[data-testid="raw-toml"]', raw.replace('min = 3.0', 'min = 2.71828'))
    await frame.click('[data-testid="run-checks"]')
    // The previous run's results are still on screen (flagged stale) until this
    // one lands: wait for the text to actually change.
    await frame.waitForFunction(
      prev => (document.querySelector('[data-testid="check-results"]')?.textContent ?? '') !== prev,
      stale, { timeout: 20_000 },
    )
    const refused = (await frame.textContent('[data-testid="check-results"]')) ?? ''
    ok(
      'an unrecorded spec comes back as an honest refusal',
      /recorded runs of the real engine/.test(refused) && /install hauksbee/i.test(refused),
      refused.trim().slice(0, 140),
    )
    ok('the refusal is not a verdict', !/All checks passed\.|Checks failed\./.test(refused))
    await shot(page, '10-unrecorded-spec')

    // ── 8. Collapse, and the host commands.
    step('collapse and the host commands')
    await frame.click('[data-testid="embed-collapse"]')
    // postMessage is asynchronous: wait for the host to have been told, rather
    // than reading the log in the same tick as the click.
    const sawCollapse = await page.waitForFunction(
      () => (window as unknown as { __hbEvents: { type: string }[] }).__hbEvents
        .some(e => e.type === 'requestCollapse'),
      undefined, { timeout: 5000 },
    ).then(() => true).catch(() => false)
    ok('requestCollapse fired', sawCollapse)
    await frame.waitForSelector('[data-testid="embed-compact"]', { timeout: 10_000 })
    await page.click('#expand')
    await frame.waitForSelector('[data-testid="embed-app"]', { timeout: 10_000 })
    ok('the host command expand() works', true)
    await page.click('#collapse')
    await frame.waitForSelector('[data-testid="embed-compact"]', { timeout: 10_000 })
    ok('the host command collapse() works', true)
    await page.click('#board-blinky')
    await frame.waitForFunction(() => {
      const el = document.querySelector('[data-testid="embed-readout"]')
      return !!el && /\d\.\d{3} V/.test(el.textContent ?? '')
    }, undefined, { timeout: 20_000 })
    ok('the host command loadBoard() switches board', true)
    await shot(page, '09-blinky-compact')

    // ── 9. Nothing but the widget's own assets crossed the network.
    step('network')
    const foreign = requests.filter(r => !r.url.startsWith(base) && !r.url.startsWith('data:') && !r.url.startsWith('blob:'))
    ok('no request left the widget\'s own origin', foreign.length === 0, foreign.map(r => r.url).join(', '))
    // Object-URL reads (the board layout the app holds as a File) are not
    // network traffic; they are the page reading its own memory.
    const http = requests.filter(r => /^https?:/.test(r.url))
    const blobReads = requests.length - http.length
    note(`${blobReads} object-URL reads (the board files the app holds in memory)`)
    const apiCalls = http.filter(r => /\/api\//.test(new URL(r.url).pathname))
    ok('no engine API was called (every answer came from the cache)', apiCalls.length === 0,
      apiCalls.map(r => `${r.method} ${r.url}`).join(', '))
    const paths = [...new Set(http.map(r => new URL(r.url).pathname))]
    const unexpected = paths.filter(p => !(
      p === '/test.html' || p === '/favicon.svg'
      || p.startsWith('/sessions/') || p.startsWith('/chunks/')
      || p === '/hauksbee-embed.js' || p === '/iframe.html' || p === '/iframe.js'
      || p.startsWith('/assets/')
    ))
    ok('every request is a widget file or a recorded asset', unexpected.length === 0, unexpected.join(', '))
    note(`requested paths: ${paths.sort().join(' ')}`)

    // ── 9b. The other shape: mounted straight into the host document.
    step('inline mount (no iframe)')
    const inlinePage = await ctx.newPage()
    watchConsole(inlinePage)
    await inlinePage.goto(`${base}/test.html?inline=1`, { waitUntil: 'domcontentloaded' })
    await inlinePage.waitForSelector('[data-testid="embed-compact"] canvas', { timeout: 20_000 })
    const inlineReady = await inlinePage.waitForFunction(() => {
      const el = document.querySelector('[data-testid="embed-readout"]')
      return !!el && /\d\.\d{3} V/.test(el.textContent ?? '')
    }, undefined, { timeout: 20_000 }).then(() => true).catch(() => false)
    ok('the inline mount renders the same interactive map, in the host document', inlineReady)
    ok('the inline mount used no iframe', await inlinePage.$$('#demo iframe').then(f => f.length === 0))
    await inlinePage.click('[data-testid="embed-expand"]')
    await inlinePage.waitForSelector('[data-testid="report"]', { timeout: 20_000 })
    ok('the inline mount expands into the app', true)
    mkdirSync(OUT, { recursive: true })
    await inlinePage.screenshot({ path: join(OUT, '11-inline.png') })
    console.log(`  shot  ${join(OUT, '11-inline.png')}`)
    await inlinePage.close()

    // ── 10. Console.
    step('console')
    ok('no console errors anywhere in the pass', consoleErrors.length === 0,
      consoleErrors.slice(0, 6).join(' | '))
  } finally {
    await browser.close()
    server?.stop(true)
  }

  console.log(`\n${pass} passed, ${failures.length} failed`)
  if (notes.length > 0) console.log(`\nnotes:\n${notes.map(n => `  - ${n}`).join('\n')}`)
  if (failures.length > 0) {
    console.log(`\nfailures:\n${failures.map(f => `  - ${f}`).join('\n')}`)
    process.exit(1)
  }
}

main().catch(e => { console.error(e); process.exit(1) })
