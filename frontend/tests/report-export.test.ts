// The exported report, checked for the two things a file that leaves the app
// has to get right: nothing in it executes, and nothing in it is fetched.
//
// Both are load-bearing rather than theoretical. Every string in that file comes
// from a board someone else made: net names, part values, a file name, and the
// spec text the user typed. The export is then emailed on and opened by a third
// person, so an unescaped `<` in a net name is a script running in their
// browser, from a file that looks like a report. And a single `https://` in it
// would mean the "standalone" claim on the menu item is false the first time
// someone opens it on a plane.

import { expect, test, beforeAll, afterAll } from 'bun:test'
import { chromium } from 'playwright'
import type { Browser } from 'playwright'
import { buildReportHtml, reportJson } from '../src/lib/report-export'
import type { WebReport } from '../src/types/report'

// One browser for the file, started outside the test: a cold chromium launch is
// a second or two on its own, and paying it inside a 5s test body is how a real
// assertion becomes a flaky timeout.
let browser: Browser
beforeAll(async () => { browser = await chromium.launch({ headless: true }) })
afterAll(async () => { await browser?.close() })

/** A report carrying, in every string field a board can influence, something
 *  that must not survive as markup. */
const hostile: WebReport = {
  ok: true,
  board_name: '<script>alert(1)</script>',
  file_name: '"><img src=x onerror=alert(2)>.kicad_pcb',
  num_components: 3,
  num_nets: 4,
  headline: 'A verdict with <b>markup</b> & an ampersand',
  serious: 1,
  total: 2,
  sections: [{
    title: 'Copper spacing <DRC>',
    verdict: 'one <thing> found',
    findings: [{
      level: 'serious',
      what: '"+3V3" and "/32K_P" are <1mm apart',
      why: 'why <em>this</em> matters',
      fix: "do <i>that</i>, and mind the 'quotes'",
    }],
    heads_up: [{ what: 'a note with <tags>' }],
  }],
  components: [{ reference: 'U1<x>', value: 'v', x: 0, y: 0, rot: 0 }],
  bind: {
    critical_parts_bound: '1/2',
    active_path_unresolved: ['U1<x>'],
    open_parts: [{
      reference: 'U1<x>',
      value: '<svg onload=alert(3)>',
      reason: 'no model',
      consequence: 'left <open>',
      active_ic: true,
      bound: false,
    }],
  },
  notes: [{ kind: 'other', message: 'a note <script>alert(4)</script>' }],
  refusal: {
    claim: 'a trustworthy <overall> conclusion',
    missing_prerequisite: 'a converged analog solve & real samples',
    valid_partial_conclusions: ['static copper findings remain <valid>'],
    next_action: 'fix the named net, then rerun <exactly>',
  },
  cosim: {
    ran: true,
    seconds_simulated: 0.5,
    uart_output: 'boot </pre><script>alert(5)</script>',
    findings: [],
    analog_valid: true,
    gpio_nets: [{ name: '<net>', volts: 3.3, driven: true }],
    timing_coverage: [{
      mcu_ref: 'U1', backend: 'simavr:atmega328p', cycle_exact: true,
      timestamp_precision_s: 0.000001, minimum_guaranteed_pulse_s: 0.000002,
      chunk_s: 0.001,
    }],
    timing_refusals: ['PWL replay refused on net /CLK: transition budget exceeded'],
    fallback_windows: [{
      start_s: 0.001, end_s: 0.002, method: 'backward-euler',
      fidelity_note: 'first-order and numerically dissipative', error_estimate_v: 0.012,
    }],
  },
}

const input = {
  report: hostile,
  boardLabel: '<b>label</b>',
  firmwareName: '<img src=x>.elf',
  analyzedAt: Date.UTC(2026, 7, 3, 12, 0, 0),
  engineVersion: '0.1.0',
  appVersion: '0.1.0',
  spec: { toml: 'name = "</pre><script>alert(6)</script>"\n[[assert]]\nkind = "no_faults"\n', fileName: 'x.toml' },
  checks: { passed: 1, failed: 0, invalid: 0 },
  sessionName: '<u>session</u>',
  restored: false,
}

test('nothing in an exported report can execute', async () => {
  const html = buildReportHtml(input)
  // Asserted in a real parser, not with a regex over the source. Grepping for
  // `onerror=` says nothing: an escaped `&lt;img src=x onerror=alert(2)&gt;` is
  // TEXT that contains those characters and is exactly the right outcome. What
  // matters is whether the parser built an element out of it.
  const page = await browser.newPage()
  try {
    let dialogs = 0
    page.on('dialog', d => { dialogs++; void d.dismiss() })
    const errors: string[] = []
    page.on('pageerror', e => errors.push(e.message))
    await page.setContent(html, { waitUntil: 'load' })
    const built = await page.evaluate(() => ({
      scripts: document.scripts.length,
      images: document.images.length,
      svgs: document.querySelectorAll('svg').length,
      handlers: document.querySelectorAll('[onerror],[onload],[onclick]').length,
      // The hostile strings are all here, as readable text.
      text: document.body.innerText,
    }))
    expect(built.scripts).toBe(0)
    expect(built.images).toBe(0)
    expect(built.svgs).toBe(0)
    expect(built.handlers).toBe(0)
    expect(dialogs).toBe(0)
    expect(errors).toEqual([])
    expect(built.text).toContain('<script>alert(1)</script>')
    expect(built.text).toContain('<img src=x onerror=alert(2)>.kicad_pcb')
    expect(built.text).toContain('& an ampersand')
  } finally {
    await page.close()
  }
}, 30_000)

test('an exported report fetches nothing', () => {
  const html = buildReportHtml(input)
  expect(html).not.toMatch(/(?:src|href)\s*=\s*["'](?:https?:|\/\/)/i)
  expect(html).not.toMatch(/url\(\s*["']?https?:/i)
  expect(html).not.toContain('@import')
})

test('it still carries the report it is an export of', () => {
  const html = buildReportHtml(input)
  // The findings, the bind table, the spec and the provenance all survive the
  // escaping: an export that is safe and empty is not an export.
  expect(html).toContain('A verdict with')
  expect(html).toContain('Copper spacing')
  expect(html).toContain('Model binding')
  expect(html).toContain('[[assert]]')
  expect(html).toContain('0.1.0')
  expect(html).toContain('3 parts, 4 nets')
  expect(html).toContain('Refused claim')
  expect(html).toContain('a trustworthy &lt;overall&gt; conclusion')
  expect(html).toContain('static copper findings remain &lt;valid&gt;')
  expect(html).toContain('fix the named net, then rerun &lt;exactly&gt;')
  // A note that duplicates the bind section is dropped, as it is on screen.
  expect(html).toContain('a note &lt;script&gt;')
  expect(html).toContain('Timing coverage')
  expect(html).toContain('pulses &gt;= 2.000 us guaranteed')
  expect(html).toContain('TIMING INVALID')
  expect(html).toContain('PWL replay refused on net /CLK')
  expect(html).toContain('Fallback-qualified windows')
  expect(html).toContain('0.012 V')
})

test('the provenance block does not repeat one name three times', () => {
  const plain: WebReport = { ...hostile, board_name: 'watchy.kicad_pcb', file_name: 'watchy.kicad_pcb' }
  const html = buildReportHtml({ ...input, report: plain, boardLabel: 'watchy.kicad_pcb', sessionName: 'watchy.kicad_pcb' })
  const rows = [...html.matchAll(/<dt>([^<]*)<\/dt>/g)].map(m => m[1])
  expect(rows).toContain('Board')
  expect(rows).not.toContain('File')
  expect(rows).not.toContain('Session')
  expect(rows).not.toContain('Uploaded as')
})

test('the JSON export is the report, unchanged', () => {
  const round = JSON.parse(reportJson(hostile))
  expect(round).toEqual(hostile)
})
