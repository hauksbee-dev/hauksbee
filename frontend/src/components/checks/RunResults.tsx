import type { RunResponse } from '../../types/report'
import { refusalLines } from '../../lib/report-view'
import { ArriveOnce } from '../../motion'
import { AssertionEvidence } from './pieces'

type Overall = 'passed' | 'invalid' | 'failed'

/** The run's one-word verdict, from the per-assertion results when there are
 *  any and from the runner's own exit otherwise. */
function overallVerdict(result: RunResponse): Overall {
  const results = result.results ?? []
  if (results.length === 0) return result.passed ? 'passed' : 'failed'
  if (results.some(x => !x.invalid && !x.passed)) return 'failed'
  return results.some(x => x.invalid) ? 'invalid' : 'passed'
}

const VERDICT_STYLE: Record<Overall, React.CSSProperties> = {
  passed: { background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--ok)' },
  invalid: { background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--warn-strong)' },
  failed: { background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' },
}

const VERDICT_LINE: Record<Overall, string> = {
  passed: 'All checks passed.',
  invalid: 'Some checks could not be judged.',
  failed: 'Checks failed.',
}

/** Overall result. It fades up once when the run lands; it does NOT re-animate
 *  when the spec is edited afterwards, because the panel then goes stale rather
 *  than becoming new. */
export function RunResults({ result, runKey, stale, rawMode }: {
  result: RunResponse
  /** Changes only when a new run lands, so the arrival plays once per run. */
  runKey: string | undefined
  stale: boolean
  /** Raw mode has no builder rows to annotate, so results are listed here. */
  rawMode: boolean
}) {
  const results = result.ok ? result.results ?? [] : []
  const overall = overallVerdict(result)
  const noteLines = (key: string, lines: string[] | undefined, prefix: string, color: string) =>
    (lines ?? []).map((line, i) => (
      <div key={`${key}${i}`} className="mt-1 text-[12px]" style={{ color }}>
        {prefix}: {line}
      </div>
    ))
  return (
    <ArriveOnce key={runKey} className="mt-3">
      <div data-testid="check-results" aria-live="polite">
        {result.ok === false ? (
          // The server's vocabulary errors list the accepted values as one
          // unbroken pipe-separated run ("voltage|uart|..."), which has no
          // space to break at; `anywhere` is what lets a single long token wrap.
          <div
            className="rounded-lg px-3 py-2.5 text-[13px]"
            style={{
              background: 'var(--err-bg)', border: '1px solid var(--err-border)',
              color: 'var(--err-strong)', overflowWrap: 'anywhere',
            }}
          >
            {result.error}
          </div>
        ) : (
          <>
            <div className="rounded-lg px-3 py-2.5 text-[14px] font-semibold" style={VERDICT_STYLE[overall]}>
              {VERDICT_LINE[overall]}
              {result.analog_abort && ' (analog solve aborted, results not trustworthy)'}
              {stale && (
                <div className="text-[11px] font-normal mt-0.5" style={{ color: 'var(--silk-dim)' }}>
                  from a previous run; the spec has changed since
                </div>
              )}
            </div>
            {overall === 'invalid' && result.refusal && (
              <div
                className="mt-2 rounded-lg px-3 py-2.5 text-[12px]"
                data-testid="refusal-contract"
                style={{ background: 'var(--warn-bg)', border: '1px solid var(--warn-border)' }}
              >
                {refusalLines(result.refusal).map(([label, value]) => (
                  <div key={label} className="mt-1 first:mt-0" style={{ color: 'var(--silk)' }}>
                    <strong style={{ color: 'var(--warn-strong)' }}>{label}:</strong> {value}
                  </div>
                ))}
              </div>
            )}
            {rawMode && results.map((x, i) => (
              <div
                key={i}
                className="mt-1.5 rounded-lg px-3 py-2 text-[13px]"
                style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)' }}
              >
                <div className="flex flex-wrap gap-2">
                  <span style={{ color: x.invalid ? 'var(--warn)' : x.passed ? 'var(--ok)' : 'var(--err)', fontWeight: 700 }}>
                    {x.invalid ? 'INVALID' : x.passed ? 'PASS' : 'FAIL'}
                  </span>
                  <span style={{ color: 'var(--silk)' }}>{x.label}</span>
                  <span style={{ color: 'var(--silk-faint)' }}>{x.detail}</span>
                </div>
                <AssertionEvidence
                  result={x}
                  assumptions={result.assumptions ?? []}
                  inventory={result.inventory ?? []}
                  stale={stale}
                />
              </div>
            ))}
            {noteLines('s', result.substitutions, 'substitute core', 'var(--warn)')}
            {noteLines('c', result.coverage_warnings, 'coverage', 'var(--warn)')}
            {(result.timing_coverage ?? []).map((t, i) => (
              <div key={`t${i}`} className="mt-1 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
                timing {t.mcu_ref} ({t.backend}): edge ±{(t.timestamp_precision_s * 1e6).toFixed(3)} µs;
                {' '}pulses ≥ {(t.minimum_guaranteed_pulse_s * 1e6).toFixed(3)} µs guaranteed;
                {' '}{t.cycle_exact ? 'cycle-exact' : 'poll-boundary'}
              </div>
            ))}
            {noteLines('tr', result.timing_refusals, 'timing invalid', 'var(--err)')}
          </>
        )}
      </div>
    </ArriveOnce>
  )
}
