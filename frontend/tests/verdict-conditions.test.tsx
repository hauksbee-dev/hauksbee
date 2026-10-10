// The conditions of a conditional pass, listed under the verdict. The fixtures
// are real engine output (tests/visual-lint/fixtures, captured from
// `hauksbee serve` on the pMOS switch boards): a pass that rests on assumed
// corners carries `verdict_conditions`; a fail and an unconditional pass carry
// none and must show nothing extra.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { VerdictConditions } from '../src/components/report/VerdictConditions'
import { buildReportHtml } from '../src/lib/report-export'
import { reportView } from '../src/lib/report-view'
import type { WebReport } from '../src/types/report'

const fixture = (name: string): WebReport =>
  JSON.parse(readFileSync(new URL(`./visual-lint/fixtures/${name}`, import.meta.url), 'utf8')) as WebReport

const conditional = fixture('analyze-conditional-pass.json')
const failing = fixture('analyze-conditional-fail.json')
const unconditional = fixture('analyze-watchy.json')

const CONDITION = 'Q1 passes across assumed corners (generic_pmos_sot_corner_high_vth, generic_pmos_sot_corner_low_vth); put the MPN on Q1 to remove this assumption.'

const exportInput = (report: WebReport) => ({
  report,
  boardLabel: null,
  firmwareName: null,
  analyzedAt: Date.UTC(2026, 9, 10, 12, 0, 0),
  engineVersion: '0.1.0',
  appVersion: '0.1.0',
  spec: null,
  checks: null,
  sessionName: null,
  restored: false,
})

describe('verdict conditions', () => {
  test('the conditional fixture carries the engine sentence and the headline that says it', () => {
    expect(conditional.verdict_conditions).toEqual([CONDITION])
    expect(conditional.headline).toBe(`No blocking findings, on a condition: ${CONDITION}`)
  })

  test('each condition is its own list item, with the sentence verbatim', () => {
    const two = [CONDITION, 'U2 passes across assumed corners (a, b); put the MPN on U2 to remove this assumption.']
    const html = renderToStaticMarkup(<VerdictConditions conditions={two} />)
    expect(html).toContain('data-testid="report-verdict-conditions"')
    expect(html.match(/data-testid="report-verdict-condition"/g)).toHaveLength(2)
    expect(html).toContain(CONDITION)
    expect(html).toContain('U2 passes across assumed corners')
    expect(html).toContain('Conditions this pass rests on')
    // Quiet, not a failure: the note accent, never the error palette.
    expect(html).toContain('var(--note-accent)')
    expect(html).not.toContain('--err')
  })

  test('the fixture report projects to one condition and a lead-in headline that does not repeat it', () => {
    const view = reportView(conditional)
    expect(view.conditions).toEqual([CONDITION])
    expect(view.headline).toBe('No blocking findings, on a condition:')
    expect(view.tone).not.toBe('error')
    const html = renderToStaticMarkup(<VerdictConditions conditions={view.conditions} />)
    expect(html.split(CONDITION)).toHaveLength(2)
  })

  test('an unconditional or failing report shows nothing extra', () => {
    for (const report of [unconditional, failing]) {
      const view = reportView(report)
      expect(view.conditions).toEqual([])
      expect(view.headline).toBe(report.headline)
      expect(renderToStaticMarkup(<VerdictConditions conditions={view.conditions} />)).toBe('')
    }
    expect(reportView({ ...unconditional, verdict_conditions: [] }).conditions).toEqual([])
    expect(reportView({ ...unconditional, verdict_conditions: ['', '  '] }).conditions).toEqual([])
  })

  test('a headline that is not the conditional one is left alone', () => {
    const view = reportView({ ...unconditional, verdict_conditions: [CONDITION] })
    expect(view.headline).toBe(unconditional.headline)
    expect(view.conditions).toEqual([CONDITION])
  })

  test('the analysis view mounts the list inside the verdict card, before the inventory line', () => {
    const board = readFileSync(new URL('../src/components/BoardView.tsx', import.meta.url), 'utf8')
    const card = board.slice(board.indexOf('data-testid="report-verdict"'), board.indexOf('data-testid="report-inventory"'))
    expect(card).toContain('<VerdictConditions conditions={v.conditions} />')
  })

  test('the exported report lists the conditions and a clean one lists none', () => {
    const html = buildReportHtml(exportInput(conditional))
    expect(html).toContain(CONDITION)
    expect(html.split(CONDITION)).toHaveLength(2)
    expect(buildReportHtml(exportInput(unconditional))).not.toContain('assumed corners')
  })
})
