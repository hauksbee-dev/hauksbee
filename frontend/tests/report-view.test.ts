// The report view's pure projections: the refusal contract's rows, the
// evidence counts and wording, and the numerical error budget. All three are
// the browser repeating the engine's words rather than inventing its own.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import {
  assumptionsForEvidence, describeModelSource, refusalLines, summarizeErrorBudget, summarizeEvidence,
} from '../src/lib/report-view'
import type { ErrorBudget, RefusalContract } from '../src/types/report'

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
    expect(checksRun).toContain('refusalLines(result.refusal)')
  })
})

test('evidence summary counts each derived status and every caveated assertion', () => {
  const summary = summarizeEvidence([
    { assertion: 'VBUS', status: 'clean' },
    { assertion: '3V3', status: 'qualified', assumptions: ['reader:a'] },
    { assertion: 'RESET', status: 'undermined', assumptions: ['open-part:U1'] },
  ])

  expect(summary).toEqual({ clean: 1, qualified: 1, undermined: 1, caveated: 2 })
})

test('frontend renders canonical model source and unknown accuracy without guessing a range', () => {
  expect(describeModelSource({
    reference: 'U2',
    model_id: 'xc6206',
    confidence: 'exact',
    source: {
      tier: 'datasheet-derived',
      layer: 'user-dir',
      origin: 'xc6206.toml',
      validation: 'physical-bounds-only',
      uncertainty: [{
        status: 'unknown',
        parameter: 'U2.model',
        reason: 'the source publishes no validated error interval',
      }],
    },
  })).toContain('datasheet-derived · physical-bounds-only · uncertainty unknown')
})

test('frontend never presents a typical-only range as validated accuracy', () => {
  expect(describeModelSource({
    reference: 'U1',
    model_id: 'switch',
    confidence: 'exact',
    source: {
      tier: 'datasheet-derived',
      layer: 'user-dir',
      origin: 'switch.toml',
      validation: 'physical-bounds-only',
      uncertainty: [{
        status: 'interval',
        parameter: 'U1.ilim',
        low: 0.75,
        high: 1.0,
        unit: 'A',
        kind: 'typical-range',
        basis: 'datasheet min/typ row with no maximum',
      }],
    },
  })).toContain('non-guaranteed typical/estimated range')
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
    expect(rows).toContain('Methods: trapezoidal, backward-euler')
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
