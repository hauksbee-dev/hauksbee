// The board result as something to take away: the CI's verdict document, the
// Markdown summary and the reproduce command. The fixtures are real engine
// output (tests/visual-lint/fixtures). The JSON must be the shape `hauksbee run
// --check --json` writes (the schema in crates/hauksbee-engine/schemas), and the
// verdict words must be the CLI's own.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { checkReportDocument, checkReportJson, cliCommand, OMITTED_NOTE, REBUILT_NOTE, reportMarkdown, verdictTag } from '../src/lib/result-export'
import { entitiesIn, linkify, resultOverlay, vocabulary } from '../src/lib/result-links'
import { reportView } from '../src/lib/report-view'
import type { CheckReportJson, WebReport } from '../src/types/report'

const fixture = (name: string): WebReport =>
  JSON.parse(readFileSync(new URL(`./visual-lint/fixtures/${name}`, import.meta.url), 'utf8')) as WebReport

const schema = JSON.parse(readFileSync(
  new URL('../../crates/hauksbee-engine/schemas/hauksbee-run-report.schema.json', import.meta.url), 'utf8',
)) as { properties: Record<string, unknown>; definitions: { JsonFinding: { properties: Record<string, unknown>; required: string[] } } }

const conditional = fixture('analyze-conditional-pass.json')
const failing = fixture('analyze-conditional-fail.json')
const partly = fixture('analyze-partly-judged.json')
const clean = fixture('analyze-watchy.json')

/** Markdown text with its escapes undone, to compare against plain sentences. */
const plain = (markdown: string) => markdown.replace(/\\([\\`*_[\]<>|])/g, '$1')

describe('verdict tag', () => {
  test('is the word the CLI prints after VERDICT:', () => {
    expect(verdictTag(conditional)).toBe('clean, conditionally')
    expect(verdictTag(failing)).toBe('failing')
    expect(verdictTag(partly)).toBe('PARTLY JUDGED')
    expect(verdictTag({ ...partly, partly_judged: false })).toBe('inconclusive')
    expect(verdictTag({ ...conditional, verdict_conditions: [] })).toBe('clean')
    // A pass with items to inspect is never "clean": the engine's tag is used.
    const inspect = { ...conditional, verdict_inspect: 'inspect before ordering (2 items)' }
    expect(verdictTag(inspect)).toBe('inspect before ordering (2 items), conditionally')
    expect(verdictTag({ ...inspect, verdict_conditions: [] })).toBe('inspect before ordering (2 items)')
  })
})

describe('the CI verdict JSON', () => {
  test('passes the engine document through untouched when the server sent it', () => {
    const engine: CheckReportJson = {
      ok: false, verdict: 'fail', verdict_reason: 'r', serious_count: 1, actionable_count: 1,
      schema_version: 9, board: 'b', bind: {}, findings: [],
    }
    const { doc, exact } = checkReportDocument({ ...failing, check_report: engine })
    expect(exact).toBe(true)
    expect(doc).toBe(engine)
    expect(JSON.parse(checkReportJson({ ...failing, check_report: engine }))).toEqual(engine)
  })

  test('a large board whose document was left out says so instead of claiming an old engine', () => {
    const { doc, exact } = checkReportDocument({ ...failing, check_report: undefined, check_report_omitted_bytes: 12_000_000 })
    expect(exact).toBe(false)
    expect(doc.export_note).toBe(OMITTED_NOTE)
    expect(doc.verdict).toBe(failing.verdict ?? 'fail')
  })

  test('a rebuilt document uses only the CLI fields, says it is rebuilt, and keeps the verdict', () => {
    for (const report of [conditional, failing, partly, clean]) {
      const { doc, exact } = checkReportDocument({ ...report, check_report: undefined })
      expect(exact).toBe(false)
      for (const key of Object.keys(doc)) if (key !== 'export_note') expect(Object.keys(schema.properties)).toContain(key)
      expect(doc.export_note).toBe(REBUILT_NOTE)
      expect(doc.verdict).toBe(report.verdict ?? (report.serious > 0 ? 'fail' : 'invalid'))
      expect(doc.ok).toBe(report.verdict === 'pass')
      expect(doc.serious_count).toBe(report.serious)
      for (const f of (doc.findings as Record<string, unknown>[])) {
        for (const key of Object.keys(f)) expect(Object.keys(schema.definitions.JsonFinding.properties)).toContain(key)
        for (const key of schema.definitions.JsonFinding.required) expect(f).toHaveProperty(key)
      }
    }
  })

  test('the CLI drops verdict_conditions from a non-pass, and so does the rebuilt document', () => {
    expect(checkReportDocument(conditional).doc.verdict_conditions).toEqual(conditional.verdict_conditions)
    expect(checkReportDocument({ ...failing, verdict_conditions: ['x'] }).doc).not.toHaveProperty('verdict_conditions')
  })

  test('a partly judged run carries its flag and scope', () => {
    const { doc } = checkReportDocument(partly)
    expect(doc.partly_judged).toBe(true)
    expect(doc.judgement_scope).toEqual(partly.judgement_scope)
    expect(doc.verdict_reason).toBe(partly.verdict_reason)
  })
})

describe('the reproduce command', () => {
  test('is the CLI invocation, quoting only what needs it', () => {
    expect(cliCommand({ board: 'b.kicad_pcb' })).toBe('hauksbee run b.kicad_pcb --check')
    expect(cliCommand({ board: 'my board.kicad_pcb', firmware: 'fw.elf', json: true }))
      .toBe("hauksbee run 'my board.kicad_pcb' --check --firmware fw.elf --json")
    expect(cliCommand({ board: "it's.kicad_pcb" })).toBe("hauksbee run 'it'\\''s.kicad_pcb' --check")
  })
})

describe('the Markdown summary', () => {
  const md = (report: WebReport) => reportMarkdown({
    report, boardLabel: 'board.kicad_pcb', firmwareName: null, engineVersion: '0.1.0', analyzedAt: Date.UTC(2026, 9, 15, 9, 0, 0),
  })

  test('a conditional pass: the CLI verdict, the headline and each condition verbatim', () => {
    const text = plain(md(conditional))
    expect(text).toContain('**VERDICT: clean, conditionally**')
    for (const c of conditional.verdict_conditions ?? []) expect(text).toContain(`- ${c}`)
    expect(text).toContain('### Condition this pass rests on')
    expect(text).toContain('hauksbee run board.kicad_pcb --check --json')
    expect(text).toContain('analyzed 2026-10-15T09:00:00Z')
  })

  test('a partly judged run: not a pass, its scope and what to do', () => {
    const text = plain(md(partly))
    const scope = reportView(partly).partlyJudged!
    expect(text).toContain('**VERDICT: PARTLY JUDGED**')
    expect(text).toContain(`**Reason:** ${partly.verdict_reason}`)
    expect(text).toContain('### Judged scope (not a pass: partly judged)')
    expect(text).toContain(`- Judged: ${scope.judged} of ${scope.total} net checks`)
    expect(text).toContain('- Q1 (key part) has no usable model.')
    expect(text).toContain('give Q1 a model.')
  })

  test('a failing run lists its findings, the serious ones first', () => {
    const text = md(failing)
    expect(text).toContain('**VERDICT: failing**')
    expect(text).toContain('| Severity | Where | What | Fix |')
    const rows = text.split('\n').filter(l => /^\| (serious|warning|info|note)/.test(l))
    expect(rows.length).toBeGreaterThan(0)
    const firstNonSerious = rows.findIndex(l => !l.startsWith('| serious'))
    if (firstNonSerious >= 0) expect(rows.slice(firstNonSerious).some(l => l.startsWith('| serious'))).toBe(false)
  })

  test('markup in a board string cannot break out of the table', () => {
    const hostile = { ...failing, verdict_reason: 'a | b <script>' }
    expect(md(hostile)).toContain('a \\| b \\<script\\>')
  })
})

describe('result links', () => {
  const vocab = { refs: new Set(['Q1', 'U2']), nets: new Set(['VBUS', '+3V3', 'in']) }

  test('names the report lists become links; ordinary words and possessives do not', () => {
    expect(linkify("Q1's gate sits on VBUS, not in +3V3.", vocab)).toEqual([
      { kind: 'part', ref: 'Q1' }, "'s gate sits on ", { kind: 'net', net: 'VBUS' }, ', not in ', { kind: 'net', net: '+3V3' }, '.',
    ])
    expect(entitiesIn('VBUS feeds Q1 and Q1 again', vocab)).toEqual([{ kind: 'part', ref: 'Q1' }, { kind: 'net', net: 'VBUS' }])
  })

  test('the overlay of a partly judged run outlines the part and marks the unjudged nets', () => {
    const o = resultOverlay(partly, reportView(partly))
    expect(o.openRefs).toContain('Q1')
    const scope = partly.judgement_scope!
    for (const n of [...scope.wiring_dependent, ...scope.electrical_dependent]) expect(o.unjudgedNets).toContain(n)
    expect(vocabulary(partly).refs.has('Q1')).toBe(true)
  })

  test('machine findings colour their nets at the worst level that names them', () => {
    const o = resultOverlay({
      ...clean,
      check_report: {
        ok: false, verdict: 'fail', serious_count: 1, actionable_count: 2,
        findings: [
          { check: 'a', kind: 'k', severity: 'warning', nets: ['N1'], refs: ['R1'], actionable: true, message: 'm', plain: 'm', location_mm: [1, 2] },
          { check: 'b', kind: 'k', severity: 'serious', nets: ['N1'], refs: [], actionable: true, message: 'm', plain: 'm' },
        ],
      },
    }, reportView(clean))
    expect(o.findingNets).toEqual({ N1: 'serious' })
    expect(o.findingRefs).toEqual({ R1: 'warning' })
    expect(o.points).toEqual([{ x: 1, y: 2, level: 'warning' }])
  })
})
