#!/usr/bin/env bun
// End-to-end pass over the 3D viewer's render-on-demand loop, in a real browser.
//
// The loop used to schedule a frame unconditionally and run the whole composer,
// SSAO included, whether or not anything had moved. A still board cost the same
// as a spinning one: a fan on real hardware, and on the software GL this test
// runs under, a main thread so starved that a trivial page.evaluate() round-trip
// took the better part of a second.
//
// What this exists to prove, and what nothing else in the repo can:
//   - a still board draws NO frames at all: the loop suspends itself rather than
//     idling, which is the only version of this that saves any power;
//   - with it suspended, the main thread is as responsive as it is on the 2D
//     view, measured the same way in the same run;
//   - a drag and a wheel zoom still draw frames, and enough of them for the
//     motion to be continuous rather than a jump to the end state;
//   - the loop settles again afterwards, so one interaction does not re-arm it
//     forever;
//   - leaving the 3D view and coming back gets a live loop, not a dead canvas;
//   - none of it logs a console error.
//
// The frame counter is the viewer's own `window.__hb3dFrames`, a sibling of the
// `__hbBoard` and `__hbCam` hooks the 2D view already publishes. Without it
// there is no way to distinguish "suspended" from "rendering fast".
//
// Usage:
//   HB_E2E_BASE=http://127.0.0.1:3001 bun run tests/e2e/viewer-3d-idle.ts
//   bun run tests/e2e/viewer-3d-idle.ts          # spawns the fixture server
//
// Slow by nature: headless chromium has no GPU, so every composed frame is
// software-rasterised with SSAO on top. That is a feature here. The starvation
// this guards against is invisible on a fast GPU and glaring under software GL.

import { chromium } from 'playwright'
import type { Browser, ConsoleMessage, Page } from 'playwright'
import { mkdirSync, rmSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-3d')

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

const consoleErrors: string[] = []
function watchConsole(page: Page, label: string) {
  page.on('console', (m: ConsoleMessage) => {
    if (m.type() !== 'error') return
    const text = m.text()
    if (/Failed to load resource|net::ERR_|the server does not|no fixture for/.test(text)) return
    consoleErrors.push(`[${label}] ${text}`)
  })
  page.on('pageerror', e => consoleErrors.push(`[${label}] pageerror: ${e.message}`))
}

/** Frames the viewer has composed since the page loaded, or null if not mounted. */
const frames = (page: Page) =>
  page.evaluate(() => (window as unknown as { __hb3dFrames?: number }).__hb3dFrames ?? null)

/**
 * Wait until the frame counter stops advancing. Everything timed in this file is
 * timed against a loop that has actually gone quiet, so the numbers are the
 * steady state and not the one-off cost of the first SSAO shader compile.
 */
async function waitForQuiet(page: Page, budgetMs = 180_000): Promise<{ quiet: boolean; secs: number; frames: number }> {
  const t0 = Date.now()
  let last = await frames(page)
  while (Date.now() - t0 < budgetMs) {
    await page.waitForTimeout(2000)
    const now = await frames(page)
    if (now === last) return { quiet: true, secs: (Date.now() - t0) / 1000, frames: now ?? -1 }
    last = now
  }
  return { quiet: false, secs: (Date.now() - t0) / 1000, frames: last ?? -1 }
}

/** Round-trip time for a no-op evaluate: how starved is the main thread? */
async function roundTrips(page: Page, n = 20): Promise<number[]> {
  const out: number[] = []
  for (let i = 0; i < n; i++) {
    const t0 = performance.now()
    await page.evaluate(() => 1)
    out.push(performance.now() - t0)
  }
  return out
}
const median = (xs: number[]) => {
  const s = [...xs].sort((a, b) => a - b)
  return s[Math.floor(s.length / 2)]
}
const stat = (xs: number[]) => {
  const s = [...xs].sort((a, b) => a - b)
  return `min ${s[0].toFixed(1)}ms median ${median(s).toFixed(1)}ms max ${s[s.length - 1].toFixed(1)}ms`
}

async function shoot(page: Page, name: string) {
  await page.waitForTimeout(400)
  await page.screenshot({ path: join(OUT, `${name}.png`), fullPage: false })
}

async function main() {
  rmSync(OUT, { recursive: true, force: true })
  mkdirSync(OUT, { recursive: true })

  const external = process.env.HB_E2E_BASE ?? null
  let fixture: Bun.Subprocess | null = null
  const port = Number(process.env.HB_E2E_PORT ?? 3492)
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
  const ctx = await browser.newContext({ baseURL: base, viewport: { width: 1440, height: 900 } })
  const page = await ctx.newPage()
  page.setDefaultTimeout(30_000)
  watchConsole(page, 'app')

  await page.goto('/', { waitUntil: 'domcontentloaded' })
  await page.waitForSelector('[data-testid="drop-zone"]', { timeout: 30_000 })
  await page.click('[data-testid="sample-watchy"]')
  await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 90_000 })
  await page.click('[data-testid="nav-board"]')
  await page.waitForSelector('[data-testid="view-3d"]', { timeout: 30_000 })
  await page.waitForTimeout(2500)

  // ── 1. The 2D baseline, measured in this same run ────────────────────────
  step('the 2D view is the yardstick')
  const twoD = await roundTrips(page)
  console.log(`     2D idle round-trip: ${stat(twoD)}`)
  ok('the 2D view leaves the main thread responsive', median(twoD) < 50, stat(twoD))
  await shoot(page, '01-2d')

  // ── 2. A still 3D board draws nothing ───────────────────────────────────
  step('a still 3D board draws no frames at all')
  await page.click('[data-testid="view-3d"]')
  // The model has to build and the first frames (shader compile included) have
  // to land before "quiet" means anything.
  await page.waitForTimeout(8000)
  const settled = await waitForQuiet(page)
  ok('the render loop suspends itself once the board is still', settled.quiet,
    `${settled.secs.toFixed(1)}s to quiet, ${settled.frames} frames drawn`)
  ok('and it drew a bounded number of frames getting there, not hundreds',
    settled.frames > 0 && settled.frames < 100, `${settled.frames} frames`)

  const before = await frames(page)
  await page.waitForTimeout(5000)
  const idleDrawn = (await frames(page))! - before!
  ok('nothing at all is drawn over 5 idle seconds', idleDrawn === 0, `${idleDrawn} frames`)
  await shoot(page, '02-3d-idle')

  const threeD = await roundTrips(page)
  console.log(`     3D idle round-trip: ${stat(threeD)}`)
  ok('an idle 3D view is as responsive as the 2D view', median(threeD) <= median(twoD) + 20,
    `3D ${stat(threeD)} vs 2D ${stat(twoD)}`)

  // ── 3. Interaction still draws ──────────────────────────────────────────
  step('orbiting and zooming still draw frames')
  const box = await page.locator('canvas').last().boundingBox()
  if (!box) throw new Error('the 3D canvas has no box to drag')
  const cx = box.x + box.width / 2
  const cy = box.y + box.height / 2

  const dragBefore = await frames(page)
  await page.mouse.move(cx, cy)
  await page.mouse.down()
  for (let i = 1; i <= 12; i++) {
    await page.mouse.move(cx + i * 8, cy + i * 3)
    await page.waitForTimeout(60)
  }
  await page.mouse.up()
  await page.waitForTimeout(1500)
  const dragDrawn = (await frames(page))! - dragBefore!
  // A dozen frames over a two-second window is the floor for "this moved rather
  // than jumped". Under software GL a frame costs hundreds of milliseconds, so
  // the count is bounded by the rasteriser, not by the loop's willingness.
  ok('a drag draws a continuous run of frames, not one', dragDrawn >= 12, `${dragDrawn} frames`)
  await shoot(page, '03-3d-after-drag')

  const afterDrag = await waitForQuiet(page)
  ok('the loop settles again after the drag', afterDrag.quiet,
    `${afterDrag.secs.toFixed(1)}s to quiet`)
  note(`the drag and its glide ran ${afterDrag.frames - dragBefore!} frames in total; OrbitControls `
    + 'damping at dampingFactor 0.06 decays over roughly 190 frames, so a released drag is still '
    + 'visibly moving for most of that. That is the pre-existing feel of the control, not something '
    + 'render-on-demand adds; what changed is that the frames stop when the movement does.')

  const wheelBefore = await frames(page)
  await page.mouse.move(cx, cy)
  await page.mouse.wheel(0, -300)
  await page.waitForTimeout(1500)
  const wheelDrawn = (await frames(page))! - wheelBefore!
  ok('a wheel zoom wakes the loop and draws', wheelDrawn > 0, `${wheelDrawn} frames`)
  await shoot(page, '04-3d-after-wheel')

  const afterWheel = await waitForQuiet(page)
  ok('and it settles again after the zoom', afterWheel.quiet, `${afterWheel.secs.toFixed(1)}s to quiet`)
  const postBefore = await frames(page)
  await page.waitForTimeout(5000)
  const postIdle = (await frames(page))! - postBefore!
  ok('nothing is drawn over 5 idle seconds after interacting', postIdle === 0, `${postIdle} frames`)
  const postTrips = await roundTrips(page)
  console.log(`     3D post-interaction idle round-trip: ${stat(postTrips)}`)
  ok('and the main thread is free again', median(postTrips) <= median(twoD) + 20, stat(postTrips))

  // ── 4. A resize has to wake it ──────────────────────────────────────────
  step('a resize wakes the loop')
  const resizeBefore = await frames(page)
  await page.setViewportSize({ width: 1200, height: 780 })
  await page.waitForTimeout(3000)
  const resizeDrawn = (await frames(page))! - resizeBefore!
  ok('resizing redraws rather than leaving a stretched frame', resizeDrawn > 0, `${resizeDrawn} frames`)
  await shoot(page, '05-3d-resized')
  await page.setViewportSize({ width: 1440, height: 900 })
  await waitForQuiet(page)

  // ── 5. Leaving and coming back ──────────────────────────────────────────
  step('leaving 3D and coming back gets a live loop')
  await page.click('[data-testid="view-2d"]')
  await page.waitForTimeout(1500)
  await page.click('[data-testid="view-3d"]')
  await page.waitForTimeout(8000)
  const reentry = await waitForQuiet(page)
  ok('the rebuilt viewer draws the board and then goes quiet', reentry.quiet && reentry.frames > 0,
    `${reentry.frames} frames, quiet after ${reentry.secs.toFixed(1)}s`)
  const reBox = await page.locator('canvas').last().boundingBox()
  const reBefore = await frames(page)
  if (reBox) {
    await page.mouse.move(reBox.x + reBox.width / 2, reBox.y + reBox.height / 2)
    await page.mouse.wheel(0, -240)
    await page.waitForTimeout(1500)
  }
  ok('and it still responds to the wheel after the round trip',
    (await frames(page))! - reBefore! > 0, `${(await frames(page))! - reBefore!} frames`)
  await shoot(page, '06-3d-reentry')

  // ── 6. Console ──────────────────────────────────────────────────────────
  step('console')
  ok('no console errors anywhere in the run', consoleErrors.length === 0,
    consoleErrors.slice(0, 5).join(' | '))

  await ctx.close()
  await browser.close()
  fixture?.kill()

  console.log(`\n${pass} passed, ${failures.length} failed, ${notes.length} note(s).`)
  for (const f of failures) console.log(`  FAILED: ${f}`)
  console.log(`screenshots: ${OUT}`)
  process.exit(failures.length > 0 ? 1 : 0)
}

await main()
