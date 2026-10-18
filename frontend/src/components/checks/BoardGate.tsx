import type { CheckResult } from '../../types/report'
import { plural } from '../../lib/report-view'
import { ResultChip } from './pieces'
import { ToBoard } from './ChecksLinks'

// The checks a run adds that no row of the spec asked for: the board's own
// static check fails a run on a serious finding (an unrouted board, a short),
// and that verdict is counted in the strip and in the top bar like any other.
// Everything counted is shown where it is counted, so each such result is
// listed here with its reason and its fix, instead of only behind "Details".

/** The leading "FAIL: " the engine puts on a failed row's detail; the chip
 *  already says it. */
const withoutVerdictWord = (detail: string): string => detail.replace(/^(FAIL|INVALID|MEASURED|PASS):\s*/, '')

export function BoardGate({ results, stale }: {
  /** The results no spec row owns (see `resultsWithoutRows`). */
  results: readonly CheckResult[]
  stale: boolean
}) {
  if (results.length === 0) return null
  const failing = results.filter(r => !r.invalid && !r.passed && !r.measured).length
  return (
    <div
      data-testid="board-gate"
      className="hb-card mt-3 px-3 py-2.5"
      style={{ borderLeft: `4px solid ${failing > 0 ? 'var(--err)' : 'var(--hairline)'}` }}
    >
      <div className="text-[11px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>
        Added by the board&apos;s own check
      </div>
      <div className="text-[12px] mt-0.5" style={{ color: 'var(--silk-dim)' }}>
        {plural(results.length, 'result')} from the board itself, not from a row below. It counts in the run
        {failing > 0 ? ` (${failing} failed)` : ''}. <ToBoard>Open the board</ToBoard>
      </div>
      <ul className="list-none p-0 m-0">
        {results.map((r, i) => (
          <li key={i} data-testid="board-gate-result" className="mt-2 text-[13px]" style={{ color: 'var(--silk)' }}>
            <div className="flex flex-wrap items-start gap-2">
              <ResultChip result={r} stale={stale} />
              <span className="min-w-0" style={{ overflowWrap: 'anywhere' }}>{withoutVerdictWord(r.detail)}</span>
            </div>
            <div className="mt-0.5 text-[11px]" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)', overflowWrap: 'anywhere' }}>
              {r.label}
            </div>
            {r.all_subjects && r.all_subjects.length > 0 && (
              <details data-testid="board-gate-all" className="mt-0.5 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                <summary className="cursor-pointer" style={{ color: 'var(--silk-dim)' }}>
                  Show all {r.all_subjects.length}
                </summary>
                <div className="mt-0.5" style={{ fontFamily: 'var(--font-mono)', overflowWrap: 'anywhere' }}>
                  {r.all_subjects.join(', ')}
                </div>
              </details>
            )}
            {r.why && (
              <div className="mt-0.5 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
                <span className="font-semibold">Fix: </span>{r.why}
              </div>
            )}
          </li>
        ))}
      </ul>
    </div>
  )
}
