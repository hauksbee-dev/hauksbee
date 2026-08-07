import { describe, expect, test } from 'bun:test'
import { summarizeErrorBudget } from '../src/lib/error-budget'
import type { ErrorBudget } from '../src/types/report'

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
