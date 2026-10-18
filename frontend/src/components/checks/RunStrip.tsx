import type { RunResponse } from '../../types/report'
import type { LoadProgress } from '../../lib/api'
import { LoadBar } from '../LoadBar'
import { overallVerdict, type Overall } from '../../lib/report-view'

// The one-line state of the last run, above whichever surface is active. It says
// the verdict and how many checks landed where; each count jumps to the first
// such check inline (a gutter mark in the TOML, a row in the builder). The full
// account (remedies, evidence, the engine's explanation) is the details drawer,
// opened from here, so nothing about the run is listed beside the checks.

export type RunStatus = 'pass' | 'fail' | 'invalid' | 'measured'

const TONE: Record<Overall, React.CSSProperties> = {
  passed: { background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--ok)' },
  measured: { background: 'var(--surface-2)', border: '1px solid var(--hairline)', color: 'var(--silk)' },
  invalid: { background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--warn-strong)' },
  failed: { background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' },
}

const LINE: Record<Overall, string> = {
  passed: 'All checks passed.',
  measured: 'Measured only: no bound was set, so nothing was judged.',
  invalid: 'Some checks could not be judged.',
  failed: 'Checks failed.',
}

export const statusOf = (r: { invalid?: boolean; measured?: boolean; passed?: boolean }): RunStatus =>
  r.invalid ? 'invalid' : r.measured ? 'measured' : r.passed ? 'pass' : 'fail'

const COUNT_WORDS: Record<RunStatus, string> = {
  pass: 'passed', fail: 'failed', invalid: 'not judged', measured: 'measured',
}

export function RunStrip({
  result, stale, running, preparing, progress, onCancel, canRun, validationCount, reportSerious, detailsOpen, onToggleDetails, onJump,
}: {
  result: RunResponse | null
  stale: boolean
  running: boolean
  /** The starter supplies are still being worked out: a run waits for them. */
  preparing?: boolean
  /** How far the engine says the work has got, once it has said. */
  progress?: LoadProgress | null
  /** Stop waiting: ends the run (or the preparation) now. */
  onCancel?: () => void
  canRun: boolean
  /** Rows the preflight refused, for the "nothing was run" line. */
  validationCount: number
  /** Serious findings on the board report: a pass here does not clear them. */
  reportSerious: number
  detailsOpen: boolean
  onToggleDetails: () => void
  /** Jump to the first check with this status, inline in the active surface. */
  onJump: (status: RunStatus) => void
}) {
  const results = result?.ok ? result.results ?? [] : []
  const counts: Record<RunStatus, number> = { pass: 0, fail: 0, invalid: 0, measured: 0 }
  for (const r of results) counts[statusOf(r)] += 1
  const overall = result?.ok ? overallVerdict(result) : null
  const uncovered = overall === 'passed' && reportSerious > 0

  let tone: React.CSSProperties = { background: 'var(--surface-2)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)' }
  let line: React.ReactNode = canRun
    ? 'Not run yet. Each check gets its verdict beside it.'
    : 'Running from here needs the board uploaded in this session.'
  const working = running || !!preparing
  if (running) line = 'Running…'
  else if (preparing) line = 'Preparing the checks for this board…'
  else if (result && result.ok === false) {
    tone = TONE.failed
    line = result.error
  } else if (overall) {
    tone = TONE[uncovered ? 'invalid' : overall]
    line = uncovered
      ? 'These checks passed, but the board is not cleared.'
      : LINE[overall]
  }

  return (
    <div data-testid="run-summary" aria-live="polite" className="rounded-lg px-3 py-2 text-[13px]" style={tone}>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
        <span className="font-semibold min-w-0" style={{ overflowWrap: 'anywhere' }}>
          {line}
          {result?.ok && result.analog_abort ? ' (analog solve aborted, results not trustworthy)' : ''}
        </span>
        {overall && (['fail', 'invalid', 'pass', 'measured'] as const).filter(s => counts[s] > 0).map(s => (
          <button
            key={s}
            type="button"
            data-testid={`run-count-${s}`}
            onClick={() => onJump(s)}
            title="Jump to the first one"
            className="hb-chip hb-press px-2 py-0.5 text-[12px] tnum"
          >
            {counts[s]} {COUNT_WORDS[s]}
          </button>
        ))}
        {stale && result && (
          <span className="text-[11px] font-normal" style={{ color: 'var(--silk-dim)' }}>
            from a previous run; the spec has changed since
          </span>
        )}
        {result && (
          <button
            type="button"
            data-testid="run-details-toggle"
            aria-expanded={detailsOpen}
            onClick={onToggleDetails}
            className="hb-press text-[12px] cursor-pointer ml-auto underline"
            style={{ background: 'none', border: 'none', color: 'inherit' }}
          >
            {detailsOpen ? 'Hide details' : counts.invalid > 0 ? 'What blocks judgement' : 'Details'}
          </button>
        )}
      </div>
      {working && (
        <div className="flex flex-wrap items-center gap-x-4 gap-y-2 mt-2" data-testid="run-progress">
          <LoadBar
            progress={progress ?? null}
            delayMs={0}
            ariaLabel={running ? 'Running the checks' : 'Preparing the checks'}
            className="flex-1 min-w-[220px]"
            maxWidth={520}
          />
          {onCancel && (
            <button
              type="button"
              data-testid="run-cancel"
              onClick={onCancel}
              className="hb-btn hb-press px-3 py-1 text-[12px]"
            >
              {running ? 'Cancel the run' : 'Skip preparing'}
            </button>
          )}
        </div>
      )}
      {uncovered && (
        <div className="text-[12px] font-normal mt-1" data-testid="run-uncovered" style={{ color: 'var(--warn-strong)' }}>
          {reportSerious} serious issue{reportSerious === 1 ? '' : 's'} in the board report {reportSerious === 1 ? 'is' : 'are'} not
          covered by these checks, so this run does not clear the board.
        </div>
      )}
      {validationCount > 0 && (
        <div data-testid="builder-validation" className="text-[12px] font-normal mt-1" style={{ color: 'var(--err-strong)' }}>
          Nothing was run: {validationCount === 1 ? 'a check is' : `${validationCount} checks are`} missing
          values. The highlighted fields say which.
        </div>
      )}
    </div>
  )
}
