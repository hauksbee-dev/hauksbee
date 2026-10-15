// Terms show the engine's glossary words (fed in here), a refused verdict is
// explained in three lines, and the export carries the glossary it uses.
import { afterEach, describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { AssertionEvidence } from '../src/components/checks/pieces'
import { ExplainVerdict } from '../src/components/report/VerdictConditions'
import { publishGlossary, resetGlossary, Term } from '../src/components/ui'
import { buildReportHtml } from '../src/lib/report-export'
import { explainReport, explainRun } from '../src/lib/report-view'
import type { CheckResult, GlossaryRow, RunResponse, WebReport } from '../src/types/report'

const rows: GlossaryRow[] = [
  { term: 'invalid', label: 'INVALID', short: 'Hauksbee refused to give this a pass or fail.', what_to_do: 'Close the named gap and rerun.' },
  { term: 'open_part', label: 'Open part', short: 'A part with no model.', what_to_do: 'Give it a model.' },
]
const fixture = (name: string) =>
  JSON.parse(readFileSync(new URL(`./visual-lint/fixtures/${name}`, import.meta.url), 'utf8')) as WebReport
const invalid = { ...fixture('analyze-watchy.json'), glossary: rows }

afterEach(resetGlossary)

describe('Term', () => {
  test('is its plain words until the engine sends the glossary, then a dotted button; bare has no underline', () => {
    expect(renderToStaticMarkup(<Term id="invalid">INVALID</Term>)).toBe('INVALID')
    publishGlossary(rows)
    const html = renderToStaticMarkup(<Term id="open_part" />)
    expect(html).toContain('data-testid="term-open_part"')
    expect(html).toContain('decoration-dotted')
    expect(html).toContain('>Open part<')
    expect(renderToStaticMarkup(<Term id="invalid" bare>x</Term>)).not.toContain('decoration-dotted')
  })
})

describe('explaining a verdict', () => {
  test('a refused board says what was not judged; a refusal contract leads', () => {
    const e = explainReport(invalid)!
    expect(e.why).toStartWith('Not judged because 1 key part has no usable model (U1)')
    expect(e.doThis).toContain('hauksbee models extract --unmodelled')
    const refusal = { claim: 'a verdict', missing_prerequisite: 'The firmware never booted', valid_partial_conclusions: ['static'], next_action: 'Flash it.' }
    const r = explainReport({ ...invalid, refusal })!
    expect([r.why, r.doThis]).toEqual(['Not judged because the firmware never booted.', 'Flash it.'])
    expect(r.evidence.map(([label]) => label)).toEqual(['Refused claim', 'Missing prerequisite', 'Still valid', 'Next action'])
  })

  test('a failing, unrouted, conditional or clean board has no panel: its headline already says it', () => {
    expect(explainReport(fixture('analyze-conditional-fail.json'))).toBeNull()
    expect(explainReport(fixture('analyze-conditional-pass.json'))).toBeNull()
    expect(explainReport({ ...invalid, fix_first: { notice: 'not routed', items: [], more: 0 } })).toBeNull()
  })

  test('a checks run explains refused, failed and conditional runs, not a clean or measured one', () => {
    const run = { ok: true, verdict_reason: 'U2 has no supply; next: add a supply on /VIN.' } as RunResponse
    expect(explainRun(run, 'invalid', false)).toMatchObject({ why: 'Not judged because U2 has no supply.', doThis: 'add a supply on /VIN.' })
    expect(explainRun(run, 'failed', false)!.term).toBe('fail')
    expect(explainRun(run, 'passed', true)!.term).toBe('conditional_pass')
    expect([explainRun(run, 'passed', false), explainRun(run, 'measured', false)]).toEqual([null, null])
  })

  test('ExplainVerdict: glossary meaning, why, do this; evidence behind a closed details', () => {
    publishGlossary(rows)
    const html = renderToStaticMarkup(
      <ExplainVerdict explanation={{ term: 'invalid', why: 'Not judged because U1 has no model.', doThis: null, evidence: [['Refused claim', 'x']] }} />,
    )
    for (const text of ['Explain this verdict', 'refused to give this a pass or fail', 'Not judged because U1', 'Close the named gap']) {
      expect(html).toContain(text)
    }
    expect(html).toMatch(/<details[^>]*data-testid="explain-evidence"(?![^>]*open)/)
    resetGlossary()
    expect(renderToStaticMarkup(<ExplainVerdict explanation={{ term: 'invalid', why: null, doThis: null, evidence: [] }} />)).toBe('')
  })
})

test('the error budget sits in a numeric-details block that is closed by default', () => {
  const evidence = { assertion: 'no_faults', status: 'qualified', assumptions: [], error_budget: { tolerance: { reltol: 1, vntol: 1, abstol: 1, chgtol: 1 } } }
  const result = { label: 'x', passed: true, detail: '', evidence } as unknown as CheckResult
  const html = renderToStaticMarkup(<AssertionEvidence result={result} assumptions={[]} inventory={[]} repeats={new Set()} stale={false} />)
  expect(html).toMatch(/<details[^>]*data-testid="numeric-details"(?![^>]*open)/)
  expect(html).toContain('data-testid="error-budget-summary"')
})

describe('the exported report', () => {
  const input = (report: WebReport, checks: { passed: number; failed: number; invalid: number } | null) => ({
    report, boardLabel: null, firmwareName: null, analyzedAt: 0, engineVersion: '0', appVersion: '0',
    spec: null, checks, sessionName: null, restored: false,
  })

  test('ends with a glossary of the terms it used, and has none from an engine without one', () => {
    const html = buildReportHtml(input(invalid, { passed: 0, failed: 0, invalid: 1 }))
    expect(html).toContain('<section id="glossary">')
    expect(html).toContain('id="term-invalid"')
    expect(html).toContain('refused to give this a pass or fail')
    expect(buildReportHtml(input(fixture('analyze-watchy.json'), null))).not.toContain('id="glossary"')
  })
})
