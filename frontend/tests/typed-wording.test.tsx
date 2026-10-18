// Words the engine now sends already plain: the verdict's why (no terminal
// command), and the part lists that read as a sentence.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { ResultPanel } from '../src/components/report/ResultPanel'
import { ChipList, partsOf } from '../src/components/report/ResultLinks'
import { explainReport, reportView } from '../src/lib/report-view'
import type { WebReport } from '../src/types/report'

const fixture = (name: string): WebReport =>
  JSON.parse(readFileSync(new URL(`./visual-lint/fixtures/${name}`, import.meta.url), 'utf8')) as WebReport

const CLI = 'draft their models with `hauksbee models extract --unmodelled <board>`, then rerun.'
const refused = {
  ...fixture('analyze-openparts.json'),
  verdict: 'invalid', partly_judged: false, fix_first: undefined, refusal: undefined,
  verdict_reason: `2 key parts have no usable model (U1, U2), so the checks cannot vouch for this board; next: ${CLI}`,
} as WebReport

const card = (r: WebReport) =>
  renderToStaticMarkup(<ResultPanel report={r} view={reportView(r)} explanation={explainReport(r)} exportSlot={null} />)

describe('the verdict card shows the why, not the terminal step', () => {
  test('verdict_why is read when the engine sends it', () => {
    const r = { ...refused, verdict_why: '2 key parts have no usable model (U1, U2), so the checks cannot vouch for this board.' }
    expect(reportView(r).reason).toBe(r.verdict_why)
    const html = card(r)
    expect(html).not.toContain('hauksbee models extract')
    expect(html).not.toContain('--unmodelled')
  })

  test('verdict_reason stands in when it is absent', () => {
    expect(reportView(refused).reason).toBe(refused.verdict_reason!)
  })

  test('the explanation reads the why too, so no command reaches "what to do"', () => {
    const r = { ...refused, verdict_why: '2 key parts have no usable model (U1, U2), so the checks cannot vouch for this board.' }
    expect(JSON.stringify(explainReport(r))).not.toContain('hauksbee models')
  })
})

describe('parts in a sentence read as a list', () => {
  const text = (html: string) => html.replace(/<[^>]+>/g, '').replace(/\s+/g, ' ').trim()
  test('two chips are joined by "and", three by commas then "and"', () => {
    expect(text(renderToStaticMarkup(<ChipList joined entities={partsOf(['Q1', 'U4'])} />))).toBe('Q1 and U4')
    expect(text(renderToStaticMarkup(<ChipList joined entities={partsOf(['Q1', 'U4', 'U5'])} />))).toBe('Q1, U4 and U5')
    expect(text(renderToStaticMarkup(<ChipList joined entities={partsOf(['Q1'])} />))).toBe('Q1')
  })
  test('a plain chip list is unchanged', () => {
    expect(text(renderToStaticMarkup(<ChipList entities={partsOf(['Q1', 'U4'])} />))).toBe('Q1U4')
  })
})
