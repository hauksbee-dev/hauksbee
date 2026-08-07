import type { ErrorBudget, ErrorBudgetWindow } from '../types/report'

const number = (value: number): string => value.toString()
const milliseconds = (seconds: number): string => (seconds * 1e3).toFixed(3)
const window = (span: ErrorBudgetWindow): string => `${milliseconds(span.start_s)}–${milliseconds(span.end_s)} ms`

/** Human-readable qualification of machine-readable numerical evidence.
 * Settings are labelled as settings; only a producer-measured residual is
 * called measured. Missing fields remain visibly unmeasured. */
export function summarizeErrorBudget(budget: ErrorBudget): string[] {
  const { tolerance } = budget
  const rows = [
    `Tolerance: rel ${number(tolerance.reltol)} · V ${number(tolerance.vntol)} V · I ${number(tolerance.abstol)} A · Q ${number(tolerance.chgtol)} C`,
    budget.residual
      ? `Residual: ${number(budget.residual.max_abs)} A at ${budget.residual.at}`
      : 'Residual: unmeasured by this solver path',
  ]
  const methods = [...new Set((budget.methods ?? []).map(entry => entry.method))]
  if (methods.length > 0) rows.push(`Methods: ${methods.join(', ')}`)
  if ((budget.failed_windows?.length ?? 0) > 0) {
    rows.push(`Invalid result spans: ${budget.failed_windows!.map(window).join(', ')}`)
  }
  if (budget.event_time_error_s !== undefined) {
    rows.push(`Event timing error: ≤${milliseconds(budget.event_time_error_s)} ms (chunk quantization)`)
  }
  if ((budget.model_uncertainty?.length ?? 0) > 0) {
    rows.push(`Model intervals: ${budget.model_uncertainty!.length} attached`)
  }
  return rows
}
