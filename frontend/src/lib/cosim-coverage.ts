import type { WebFallbackWindow, WebTimingCoverage } from '../types/report'
import type { RefusalContract } from './refusal-contract'

function duration(value: number): string {
  if (value < 1e-3) return `${(value * 1e6).toFixed(3)} us`
  return `${(value * 1e3).toFixed(3)} ms`
}

export function timingCoverageLine(row: WebTimingCoverage): string {
  const stamps = row.cycle_exact ? 'cycle-exact stamps' : 'poll-boundary stamps'
  return `${row.mcu_ref} (${row.backend}): ${stamps}; edge uncertainty <= ${duration(row.timestamp_precision_s)}; pulses >= ${duration(row.minimum_guaranteed_pulse_s)} guaranteed; ${duration(row.chunk_s)} solver chunk.`
}

export function fallbackWindowLine(window: WebFallbackWindow): string {
  const estimate = window.error_estimate_v == null
    ? 'no measured error estimate'
    : `${window.error_estimate_v.toFixed(3)} V measured chunk-end error estimate`
  return `${(window.start_s * 1e3).toFixed(3)}-${(window.end_s * 1e3).toFixed(3)} ms: ${window.method}; ${estimate}; ${window.fidelity_note}.`
}

/** Keep the complete wire data while avoiding a second human rendering of the
 * exact diagnosis already carried by the typed analysis-refusal contract. */
export function uncoveredTimingRefusals(
  refusals: string[] | undefined,
  refusal: RefusalContract | null | undefined,
): string[] {
  return (refusals ?? []).filter(line => line !== refusal?.missing_prerequisite)
}
