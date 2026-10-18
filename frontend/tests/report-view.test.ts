// The report view's pure projections: the refusal contract's rows, the
// evidence counts and wording, and the numerical error budget. All three are
// the browser repeating the engine's words rather than inventing its own.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import {
  assumptionsForEvidence, briefReason, describeModelSource, fallbackWindowLine, overallVerdict, reportView, repeatedAssumptions, refusalLines, summarizeErrorBudget, summarizeEvidence,
} from '../src/lib/report-view'
import type { WebReport, CheckResult, ErrorBudget, ModelOnPath, ModelUncertainty, RefusalContract, RunResponse } from '../src/types/report'

describe('C5.3 refusal contract', () => {
  test('the browser preserves all four answers from the CI result', () => {
    const refusal: RefusalContract = {
      claim: 'overall CI verdict for boot checks',
      missing_prerequisite: 'a converged analog solve across the assertion window',
      valid_partial_conclusions: ['the UART assertion passed', 'the spec and board loaded'],
      next_action: 'fix the first named failed window, then rerun the same spec',
    }

    expect(refusalLines(refusal)).toEqual([
      ['Refused claim', 'overall CI verdict for boot checks'],
      ['Missing prerequisite', 'a converged analog solve across the assertion window'],
      ['Still valid', 'the UART assertion passed; the spec and board loaded'],
      ['Next action', 'fix the first named failed window, then rerun the same spec'],
    ])
  })

  test('both analysis and checks views render the shared contract', () => {
    const boardView = readFileSync(new URL('../src/components/BoardView.tsx', import.meta.url), 'utf8')
    const reportView = readFileSync(new URL('../src/lib/report-view.ts', import.meta.url), 'utf8')
    const checksRun = readFileSync(new URL('../src/components/checks/RunResults.tsx', import.meta.url), 'utf8')

    // The analysis view reads the rows off the derived report view, which is
    // where the one contract-to-rows projection lives.
    expect(reportView).toContain('refusalRows: r.refusal ? refusalLines(r.refusal) : null')
    expect(boardView).toContain('v.refusalRows.map(')
    expect(boardView).toContain('data-testid="analysis-refusal-contract"')
    // The Checks view shows the same contract rows inside Explain this verdict.
    expect(reportView).toContain('evidence: refusal ? refusalLines(refusal) : []')
    expect(checksRun).toContain('explainRun(result, overall, conditional)')
  })
})

test('the verdict reason is the engine sentence, shown in the card and the export', () => {
  const reason = '2 key parts have no usable model (U1, U2); next: draft a model per part, then rerun.'
  const base = { ok: true, file_name: 'b.kicad_pcb', num_components: 1, num_nets: 1, headline: 'Not cleared', serious: 0, total: 0 } as WebReport
  expect(reportView({ ...base, verdict_reason: reason }).reason).toBe(reason)
  expect(reportView(base).reason).toBeNull()
  const boardView = readFileSync(new URL('../src/components/report/ResultPanel.tsx', import.meta.url), 'utf8')
  const checksRun = readFileSync(new URL('../src/components/checks/RunResults.tsx', import.meta.url), 'utf8')
  expect(boardView).toContain('data-testid="report-verdict-reason"')
  expect(checksRun).toContain('result.verdict_reason')
})

test('the verdict card shows a headline of the reason, not the first finding in full', () => {
  const reason = '1 blocking problem(s), first: Two functions need the same block (U3). U3 (2X(PN1X20(F-D-1X20-LF))): net \'/A\' -> R10 -> net \'/B\' reaches J1 (3.5mm/PJ-W47S); next: Move one of the functions to a free block, or give up one. The datasheet pin-mux table shows which pins share which block.'
  const brief = briefReason(reason)
  expect(brief).toBe('1 blocking problem(s), first: Two functions need the same block (U3); next: Move one of the functions to a free block, or give up one.')
  expect(brief).not.toContain('PN1X20')
  expect(briefReason('3 keys have no model (U1); next: draft them.')).toBe('3 keys have no model (U1); next: draft them.')
  expect(briefReason('x '.repeat(200) + 'tail').length).toBeLessThan(150)
  const base = { ok: true, file_name: 'b.kicad_pcb', num_components: 1, num_nets: 1, headline: 'h', serious: 1, total: 1, verdict_reason: reason } as WebReport
  expect(reportView(base).reasonBrief).toBe(brief)
  expect(reportView(base).reason).toBe(reason)
})

test('repeated caveats preserve their id but not their explanation', () => {
  const rows = [{ evidence: { assumptions: ['a'] } }, { evidence: { assumptions: ['a'] } }] as CheckResult[]
  expect([...repeatedAssumptions(rows)[1]]).toEqual(['a'])
})

test('evidence summary counts each derived status and every caveated assertion', () => {
  const summary = summarizeEvidence([
    { assertion: 'VBUS', status: 'clean' },
    { assertion: '3V3', status: 'qualified', assumptions: ['reader:a'] },
    { assertion: 'RESET', status: 'undermined', assumptions: ['open-part:U1'] },
  ])

  expect(summary).toEqual({ clean: 1, qualified: 1, undermined: 1, caveated: 2 })
})

/** A datasheet-derived, bounds-only model from `<model_id>.toml` with one
 *  uncertainty record. */
function boundedModel(reference: string, model_id: string, uncertainty: ModelUncertainty): ModelOnPath {
  return {
    reference,
    model_id,
    confidence: 'exact',
    source: {
      tier: 'datasheet-derived',
      layer: 'user-dir',
      origin: `${model_id}.toml`,
      validation: 'physical-bounds-only',
      uncertainty: [uncertainty],
    },
  }
}

test('frontend renders canonical model source and unknown accuracy without guessing a range', () => {
  expect(describeModelSource(boundedModel('U2', 'xc6206', {
    status: 'unknown',
    parameter: 'U2.model',
    reason: 'the source publishes no validated error interval',
  }))).toContain('datasheet-derived · physical-bounds-only · uncertainty unknown')
})

test('frontend never presents a typical-only range as validated accuracy', () => {
  expect(describeModelSource(boundedModel('U1', 'switch', {
    status: 'interval',
    parameter: 'U1.ilim',
    low: 0.75,
    high: 1.0,
    unit: 'A',
    kind: 'typical-range',
    basis: 'datasheet min/typ row with no maximum',
  }))).toContain('non-guaranteed typical/estimated range')
})

test('CI assertion evidence resolves canonical assumption records without inventing prose', () => {
  const map = {
    assertion: 'RAIL stays up',
    status: 'qualified' as const,
    assumptions: ['reduced-fidelity:RAIL', 'missing:id'],
  }
  const registry = [{
    id: 'reduced-fidelity:RAIL',
    kind: 'reduced_fidelity',
    source: 'check',
    scope: { type: 'nets' },
    statement: 'Net RAIL is held by an ideal source.',
    because: 'Nothing on the board sets its voltage in this run: a stimulus does.',
    consequence: 'Passing vouches for nothing about the board.',
    replacement: 'Model the real supply path and re-run.',
  }]

  expect(assumptionsForEvidence(map, registry)).toEqual(registry)
})

const budget: ErrorBudget = {
  tolerance: { reltol: 1e-3, vntol: 1e-6, abstol: 1e-12, chgtol: 1e-14 },
  methods: [
    { window: { start_s: 0, end_s: 0.004 }, method: 'trapezoidal' },
    { window: { start_s: 0.004, end_s: 0.005 }, method: 'backward-euler' },
  ],
  residual: { max_abs: 2.5e-10, at: 'VCC' },
  failed_windows: [{ start_s: 0.008, end_s: 0.009 }],
  event_time_error_s: 0.001,
  model_uncertainty: [],
}

describe('summarizeErrorBudget', () => {
  test('shows measured qualification and invalid spans without inventing accuracy', () => {
    const rows = summarizeErrorBudget(budget)
    expect(rows).toContain('Tolerance: rel 0.001 · V 0.000001 V · I 1e-12 A · Q 1e-14 C')
    expect(rows).toContain('Residual: 2.5e-10 A at VCC')
    expect(rows).toContain('Methods: Trapezoidal, Backward Euler')
    expect(rows).toContain('Invalid result spans: 8.000–9.000 ms')
    expect(rows).toContain('Event timing error: ≤1.000 ms (chunk quantization)')
    expect(rows.join(' ')).not.toMatch(/accuracy|%/i)
  })

  test('calls an absent residual unmeasured, never zero', () => {
    expect(summarizeErrorBudget({ ...budget, residual: undefined })).toContain(
      'Residual: unmeasured by this solver path',
    )
  })
})

test('a run that judged nothing is measured, never passed', () => {
  const row = (measured: boolean, invalid = false) => ({ passed: true, invalid, measured })
  const verdict = (...results: object[]) => overallVerdict({ ok: true, passed: true, results } as RunResponse)
  expect(verdict(row(true), row(true))).toBe('measured')
  expect(verdict(row(true), row(false))).toBe('passed')
  expect(verdict(row(true), row(true, true))).toBe('invalid')
})

test('a cold-start fallback window states the jump from the carried state', () => {
  const window = {
    start_s: 0.001, end_s: 0.002, method: 'cold-start-backward-euler',
    fidelity_note: 'restarted from a fresh operating point', error_estimate_v: 0.012,
  }
  expect(fallbackWindowLine(window)).not.toContain('jump')
  expect(fallbackWindowLine({ ...window, state_jump_v: 1.25 })).toBe(
    '1.000-2.000 ms: Restarted from rest (Backward Euler); 0.012 V measured chunk-end error estimate; '
    + 'carried state discarded, 1.250 V jump at the window start; restarted from a fresh operating point.',
  )
})
