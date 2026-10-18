import type { RunResponse } from '../../types/report'
import { explainRun, repeatedAssumptions, type Overall } from '../../lib/report-view'
import { useGlossary } from '../ui'
import { ExplainVerdict } from '../report/VerdictConditions'
import { AssertionEvidence, parseMeasures, plainDetail, ResultChip } from './pieces'

// The run's full account, behind the verdict band's "Details": the engine's
// reason and conditions, the explanation of the verdict, the run notes, and
// (in the TOML mode, which has no rows to annotate) every result listed. The
// band itself says the verdict and what to do; nothing here repeats it.

/** True when the details drawer has anything to show for this run. */
export function hasRunDetails(result: RunResponse, overall: Overall, rawMode: boolean): boolean {
  if (!result.ok) return false
  return rawMode
    || (overall === 'failed' && !!result.verdict_reason)
    || (result.verdict_conditions ?? []).some(c => c.trim() !== '')
    || (result.timing_refusals ?? []).length > 0
    || (result.substitutions ?? []).length + (result.coverage_warnings ?? []).length + (result.timing_coverage ?? []).length > 0
    || explainRun(result, overall, false) !== null
}

export function RunResults({ result, overall, stale, rawMode }: {
  result: RunResponse
  /** The verdict the band shows, which this account explains. */
  overall: Overall
  stale: boolean
  /** Raw mode has no builder rows to annotate, so results are listed here. */
  rawMode: boolean
}) {
  const results = result.ok ? result.results ?? [] : []
  const repeats = repeatedAssumptions(results)
  useGlossary(result.glossary)
  // A pass that rests on open parts the member sweep could not cross in full.
  const conditions = (result.verdict_conditions ?? []).filter(c => c.trim() !== '')
  const conditional = overall === 'passed' && conditions.length > 0
  const explanation = result.ok ? explainRun(result, overall, conditional) : null
  const notes = [
    ...(result.substitutions ?? []).map(s => `Substitute core: ${s}`),
    ...(result.coverage_warnings ?? []).map(s => `Coverage: ${s}`),
    ...(result.timing_coverage ?? []).map(t =>
      `Timing on ${t.mcu_ref} (${t.backend}): edges land within ±${(t.timestamp_precision_s * 1e6).toFixed(3)} µs, and pulses of ${(t.minimum_guaranteed_pulse_s * 1e6).toFixed(3)} µs or longer are always seen${t.cycle_exact ? ' (cycle-exact)' : ' (sampled at poll boundaries)'}.`),
  ]
  if (!result.ok) return null
  return (
    <div data-testid="check-results" data-verdict={overall} className="text-[12px]" style={{ color: 'var(--silk-dim)' }}>
      {overall === 'failed' && result.verdict_reason && (
        <div className="mt-1 text-[13px]" data-testid="checks-verdict-reason" style={{ color: 'var(--silk)' }}>
          {result.verdict_reason}
        </div>
      )}
      {conditions.map((c, i) => (
        <div key={i} className="mt-1" data-testid="run-condition" style={{ color: 'var(--warn-strong)' }}>On a condition: {c}</div>
      ))}
      {(result.timing_refusals ?? []).map((t, i) => (
        <div key={i} className="mt-1" style={{ color: 'var(--err)' }}>Timing not judged: {t}</div>
      ))}
      {explanation && <ExplainVerdict explanation={explanation} open={false} />}
      {notes.length > 0 && (
        <details className="mt-1.5" data-testid="run-notes">
          <summary className="cursor-pointer" style={{ color: 'var(--silk-faint)' }}>Run notes ({notes.length})</summary>
          {notes.map((n, i) => <div key={i} className="mt-1">{n}</div>)}
        </details>
      )}
      {rawMode && results.map((x, i) => (
        <div key={i} className="mt-1.5 rounded-lg px-3 py-2 text-[13px]" style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)' }}>
          <div className="flex flex-wrap items-center gap-2">
            <ResultChip result={x} stale={stale} />
            <span style={{ color: 'var(--silk)' }}>{x.label}</span>
            {parseMeasures(x.detail).length === 0 && !x.invalid && <span style={{ color: 'var(--silk-dim)' }}>{plainDetail(x.detail)}</span>}
          </div>
          <AssertionEvidence
            result={x}
            assumptions={result.assumptions ?? []}
            inventory={result.inventory ?? []}
            repeats={repeats[i] ?? new Set()}
            stale={stale}
          />
        </div>
      ))}
    </div>
  )
}
