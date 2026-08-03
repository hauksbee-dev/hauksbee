#!/usr/bin/env bun
// Visual lint: every surface, at every viewport, measured in a real browser.
//
// This exists because the defects it catches are invisible to every other gate
// in the repo. `tsc` and eslint are happy with a card whose button hangs 40px
// off its right edge; `bun test` never renders anything; a screenshot taken at
// 1440x900 while building the feature never shows what 320px does to it. So
// the layout is measured instead: page-level horizontal scroll, children
// escaping their container, single-line text cut off, images broken or
// stretched, and sticky chrome sitting on top of a control.
//
//   bun run visual-lint                      # builds nothing; serves dist/ from fixtures
//   HB_LINT_BASE=http://127.0.0.1:3001 \
//     bun run visual-lint                    # against a real `hauksbee serve`
//   HB_LINT_SITE=0 bun run visual-lint       # skip the marketing site pass
//
// Exit code is 1 if anything at `error` severity fired. `info` findings
// (deliberate ellipsis truncation) are printed and do not fail.

import { chromium } from 'playwright'
import type { Browser, Page } from 'playwright'
import { mkdirSync, rmSync, existsSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { auditPage, TOLERANCE } from './audit'
import type { Finding } from './audit'
import { APP_SURFACES, SITE_SURFACES, VIEWPORTS } from './surfaces'
import type { Surface, Viewport } from './surfaces'

const here = dirname(fileURLToPath(import.meta.url))
const SHOTS = join(here, '../../test-results/visual-lint')

type Row = Finding & { surface: string; viewport: string; shot: string | null }

const rows: Row[] = []
let ran = 0

/** Serve a directory of static files, SPA-style. Used for the site pass; the
 *  app pass uses fixture-server.ts, which also answers /api. */
function staticServer(root: string, port: number) {
  return Bun.serve({
    port,
    idleTimeout: 60,
    async fetch(req) {
      const p = new URL(req.url).pathname
      const f = Bun.file(join(root, p === '/' ? '/index.html' : decodeURIComponent(p)))
      if (await f.exists()) return new Response(f)
      return new Response(Bun.file(join(root, 'index.html')), { headers: { 'content-type': 'text/html' } })
    },
  })
}

async function waitForServer(base: string, tries = 60) {
  for (let i = 0; i < tries; i++) {
    try {
      const res = await fetch(base, { signal: AbortSignal.timeout(1000) })
      if (res.ok) return
    } catch { /* not up yet */ }
    await Bun.sleep(250)
  }
  throw new Error(`nothing answering at ${base}`)
}

/** Run one surface chain at one viewport, auditing after every surface. */
async function runPass(
  browser: Browser,
  base: string,
  label: string,
  surfaces: Surface[],
  viewport: Viewport,
) {
  const ctx = await browser.newContext({
    baseURL: base,
    viewport: { width: viewport.width, height: viewport.height },
    deviceScaleFactor: 1,
    // Deterministic layout: motion-driven transforms mid-flight would be
    // measured as escapes, and the lint is about the resting state.
    reducedMotion: 'reduce',
  })
  const page = await ctx.newPage()
  // Short, because a locator that never appears is the failure, and waiting
  // half a minute per viewport to learn that makes the lint too slow to run.
  page.setDefaultTimeout(12_000)
  page.setDefaultNavigationTimeout(30_000)
  const pageErrors: string[] = []
  page.on('pageerror', e => pageErrors.push(e.message))

  for (const surface of surfaces) {
    const id = `${label}${surface.id}`
    try {
      await surface.reach(page)
    } catch (e) {
      // A surface that cannot be reached at this viewport IS a finding: it
      // means a control the flow depends on is not clickable at that size.
      // The chain carries on, so one blocked step does not hide the surfaces
      // after it; a genuine cascade reports as one line per surface, which is
      // the right amount of loud.
      const shot = await shoot(page, id, viewport)
      rows.push({
        surface: id, viewport: viewport.name, shot,
        rule: 'unreachable', severity: 'error',
        selector: '-', detail: e instanceof Error ? e.message.split('\n')[0] : String(e),
      })
      continue
    }

    const found = await page.evaluate(auditPage, TOLERANCE) as Finding[]
    ran++
    const graded = found.map(f => {
      const allowed = surface.allow?.find(a => a.rule === f.rule && a.selector.test(f.selector))
      return allowed
        ? { ...f, severity: 'info' as const, detail: `${f.detail} [allowed: ${allowed.why}]` }
        : f
    })
    const shot = graded.some(f => f.severity === 'error') ? await shoot(page, id, viewport) : null
    for (const f of graded) rows.push({ ...f, surface: id, viewport: viewport.name, shot })
  }

  for (const msg of pageErrors) {
    rows.push({
      surface: `${label}(page)`, viewport: viewport.name, shot: null,
      rule: 'page-error', severity: 'info', selector: '-', detail: msg.slice(0, 200),
    })
  }
  await ctx.close()
}

/** Screenshot the state, with every violating element outlined and the first of
 *  them scrolled into view. The marks come from the audit (`data-visual-lint`)
 *  and are removed afterwards so they cannot leak into the next surface. */
async function shoot(page: Page, id: string, viewport: Viewport): Promise<string | null> {
  const name = `${id.replace(/\//g, '-')}__${viewport.name}.png`
  try {
    await page.evaluate(() => {
      const marked = document.querySelectorAll('[data-visual-lint]')
      if (marked.length === 0) return
      const style = document.createElement('style')
      style.id = 'visual-lint-marks'
      style.textContent = '[data-visual-lint]{outline:2px solid #ff3b30 !important;outline-offset:1px}'
      document.head.append(style)
      // Scrolling changes what a sticky layer covers, so the scroll state of
      // every scroller is put back afterwards: the surfaces run as a chain, and
      // a screenshot must not change the layout the NEXT one is measured in.
      // Every scroller, not just the ones currently scrolled: scrollIntoView
      // moves each ancestor scroller, including those sitting at zero.
      const scrolls = Array.from(document.querySelectorAll('*'))
        .filter(el => el.scrollHeight > el.clientHeight || el.scrollWidth > el.clientWidth)
        .map(el => ({ el, top: el.scrollTop, left: el.scrollLeft }))
      ;(window as unknown as { __vlScrolls: typeof scrolls }).__vlScrolls = scrolls
      marked[0].scrollIntoView({ block: 'center', behavior: 'instant' })
    })
    await page.waitForTimeout(120)
    await page.screenshot({ path: join(SHOTS, name), fullPage: true })
    await page.evaluate(() => {
      document.getElementById('visual-lint-marks')?.remove()
      for (const el of Array.from(document.querySelectorAll('[data-visual-lint]'))) {
        el.removeAttribute('data-visual-lint')
      }
      const w = window as unknown as { __vlScrolls?: { el: Element; top: number; left: number }[] }
      for (const s of w.__vlScrolls ?? []) {
        s.el.scrollTop = s.top
        s.el.scrollLeft = s.left
      }
      delete w.__vlScrolls
    })
    return name
  } catch {
    return null
  }
}

// ── Report ────────────────────────────────────────────────────────────────────

/** Persistent chrome (the header, the sidebar) is on every surface, so one
 *  broken button in it would otherwise be reported eleven times. Identical
 *  measurements at the same viewport collapse to one line naming the surfaces. */
function collapse(rs: Row[]): { row: Row; surfaces: string[] }[] {
  const groups = new Map<string, { row: Row; surfaces: string[] }>()
  for (const r of rs) {
    const key = `${r.viewport}|${r.rule}|${r.selector}|${r.detail}`
    const g = groups.get(key)
    if (g) {
      if (!g.surfaces.includes(r.surface)) g.surfaces.push(r.surface)
      g.row.shot ??= r.shot
    } else {
      groups.set(key, { row: { ...r }, surfaces: [r.surface] })
    }
  }
  return [...groups.values()]
}

function report(): number {
  const errors = collapse(rows.filter(r => r.severity === 'error'))
  const infos = collapse(rows.filter(r => r.severity === 'info'))

  const line = ({ row: r, surfaces }: { row: Row; surfaces: string[] }) =>
    `${r.severity === 'error' ? 'FAIL' : 'info'} ${surfaces.join(',')} @ ${r.viewport} `
    + `[${r.rule}] ${r.selector} :: ${r.detail}${r.shot ? ` (shot: ${r.shot})` : ''}`

  console.log(`\n── visual lint: ${ran} surface x viewport combinations audited ──`)
  if (errors.length === 0 && infos.length === 0) {
    console.log('no findings.')
  }
  for (const g of errors) console.log(line(g))
  if (infos.length > 0) {
    console.log('')
    for (const g of infos) console.log(line(g))
  }

  const byRule = new Map<string, number>()
  for (const g of errors) byRule.set(g.row.rule, (byRule.get(g.row.rule) ?? 0) + 1)
  console.log('')
  console.log(`${errors.length} distinct violation(s), ${infos.length} note(s).`)
  for (const [rule, n] of [...byRule].sort((a, b) => b[1] - a[1])) console.log(`  ${rule}: ${n}`)
  if (errors.length > 0) console.log(`screenshots: ${SHOTS}`)
  return errors.length > 0 ? 1 : 0
}

// ── Main ──────────────────────────────────────────────────────────────────────

const APP_PORT = Number(process.env.HB_LINT_PORT ?? 3479)
const SITE_PORT = APP_PORT + 1

rmSync(SHOTS, { recursive: true, force: true })
mkdirSync(SHOTS, { recursive: true })

const externalBase = process.env.HB_LINT_BASE ?? null
let fixtureProc: Bun.Subprocess | null = null
let siteServer: ReturnType<typeof staticServer> | null = null

const appBase = externalBase ?? `http://127.0.0.1:${APP_PORT}`
if (!externalBase) {
  fixtureProc = Bun.spawn(['bun', 'run', join(here, 'fixture-server.ts'), String(APP_PORT)], {
    stdout: 'inherit', stderr: 'inherit',
  })
}

const browser = await chromium.launch({
  // No 3D anywhere in the lint (the surfaces never open the 3D tab), so no GPU
  // flags and no software-GL workarounds are needed. If a surface is ever added
  // that mounts Board3DViewer, it will hang here: don't.
  headless: true,
})

try {
  await waitForServer(appBase)
  for (const vp of VIEWPORTS) {
    await runPass(browser, appBase, '', APP_SURFACES, vp)
  }

  // ── The site: optional, tolerant ────────────────────────────────────────
  // A separate build owned separately. If it is not built, say so and move on;
  // the app's lint result must not depend on it.
  const siteDist = join(here, '../../../site/dist')
  const doSite = process.env.HB_LINT_SITE !== '0'
  if (doSite && existsSync(join(siteDist, 'index.html'))) {
    siteServer = staticServer(siteDist, SITE_PORT)
    const siteBase = `http://127.0.0.1:${SITE_PORT}`
    await waitForServer(siteBase)
    for (const vp of VIEWPORTS) {
      await runPass(browser, siteBase, 'site/', SITE_SURFACES, vp)
    }
  } else if (doSite) {
    console.log(`\n[skipped] site pass: no build at ${siteDist} (run \`bun run build\` in site/)`)
  }
} finally {
  await browser.close()
  fixtureProc?.kill()
  await siteServer?.stop(true)
}

process.exit(report())
